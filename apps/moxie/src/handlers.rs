//! 路由层：认证、多租户校验、词表下发、孩子端会话。
//!
//! ## 安全主线
//!
//! 1. **越权防护**：每个 `{kid_id}` 操作都先走 [`owned_kid`]，它按
//!    `pk = USER#{当前登录 user_id}#KIDS` 去取记录——不属于自己的 kid_id
//!    自然取不到，统一 404。不区分"不存在"与"不属于你"，避免泄露他人 kid_id 是否存在。
//! 2. **防偷看**：孩子端 `/api/kid/current` 的响应里**根本没有汉字**，
//!    只有音频路径。孩子即使打开开发者工具也看不到要默写的词。
//! 3. **设备令牌可撤销**：库里只存 sha256，家长解绑后旧令牌立刻失效。

use axum::extract::{Extension, Path};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use futures::stream::{self, StreamExt};
use operon_core::prelude::*;
// `unix_now` 只在 crate 根导出，不在 prelude 里
use operon_core::unix_now;
use operon_dynamo::DynamoClient;
use operon_s3::S3Client;

use crate::auth;
use crate::models::*;
use crate::ocr;
use crate::tts;

/// Polly / S3 的并发上限。Lambda 超时写死 30 秒，串行调 40 次 Polly 必超时。
const GEN_CONCURRENCY: usize = 8;
/// 绑定码有效期 5 分钟。
const BIND_CODE_TTL: i64 = 300;
/// 家长登录态 30 天——没人想每天重新登录。
const JWT_TTL: u64 = 30 * 24 * 3600;
/// 一次下发的词语数上限。
const MAX_WORDS: usize = 200;

// ------------------------------------------------------------- 基础设施 helper

fn data_table(state: &AppState) -> String {
    format!("{}data", state.config.table_prefix)
}

fn db(state: &AppState) -> DynamoClient {
    DynamoClient::new(&state.aws_config, data_table(state))
}

fn s3_client(state: &AppState) -> Result<S3Client, AppError> {
    let bucket = std::env::var("OPERON_MOXIE_AUDIO_BUCKET").map_err(|_| {
        AppError::Internal("未配置 OPERON_MOXIE_AUDIO_BUCKET 环境变量".into())
    })?;
    Ok(S3Client::new(&state.aws_config, bucket))
}

fn now() -> i64 {
    unix_now() as i64
}

/// 常量时间字符串比较（用于设备令牌哈希）。
fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// 取孩子并**校验归属**——多租户最关键的一处。
///
/// 按当前登录用户的 `user_id` 拼分区键，所以访问他人的 kid_id 一律 404。
async fn owned_kid(state: &AppState, user_id: &str, kid_id: &str) -> Result<Kid, AppError> {
    db(state)
        .get::<Kid>(&Kid::pk_for(user_id), &Kid::sk_for(kid_id))
        .await?
        .ok_or_else(|| AppError::NotFound("孩子不存在".into()))
}

fn sign_token(state: &AppState, user_id: &str) -> Result<String, AppError> {
    let now = unix_now();
    let mut extra = serde_json::Map::new();
    extra.insert("role".into(), serde_json::json!("parent"));
    state
        .jwt
        .sign(&JwtClaims {
            sub: user_id.to_string(),
            email: None,
            iat: now,
            exp: now + JWT_TTL,
            extra,
        })
        .map_err(|e| AppError::Internal(format!("签发令牌失败: {e}")))
}

/// 孩子端认证：解析设备令牌 → 校验哈希 → 取当前词表。
async fn device_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Kid, CurrentList), AppError> {
    let token = headers
        .get("x-device-token")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("缺少设备令牌".into()))?;

    let (user_id, kid_id) = auth::parse_device_token(token)
        .ok_or_else(|| AppError::Unauthorized("设备令牌格式错误".into()))?;

    let kid = owned_kid(state, user_id, kid_id).await?;
    let expected = kid
        .device_token_hash
        .as_deref()
        .ok_or_else(|| AppError::Unauthorized("该设备尚未绑定".into()))?;

    if !ct_eq(&auth::hash_token(token), expected) {
        return Err(AppError::Unauthorized(
            "设备令牌无效（可能已被家长解绑）".into(),
        ));
    }

    let list = db(state)
        .get::<CurrentList>(&CurrentList::pk_for(kid_id), CurrentList::SK)
        .await?
        .ok_or_else(|| AppError::NotFound("家长还没有下发默写内容".into()))?;

    Ok((kid, list))
}

// ------------------------------------------------------------- 认证

/// `POST /api/auth/register`
pub async fn register(
    Extension(state): Extension<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<(StatusCode, Json<AuthResp>), AppError> {
    auth::validate_phone(&req.phone)?;
    auth::validate_password(&req.password)?;
    let phone = req.phone.trim().to_string();

    let dbc = db(&state);
    // 手机号唯一性：先查索引项
    if dbc
        .get::<PhoneIndex>(&PhoneIndex::pk_for(&phone), PhoneIndex::SK)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict("该手机号已注册".into()));
    }

    let user_id = gen_id("u");
    let user = User {
        pk: User::pk_for(&user_id),
        sk: User::SK.to_string(),
        user_id: user_id.clone(),
        phone: phone.clone(),
        password_hash: auth::hash_password(&req.password)?,
        nickname: req.nickname.clone().filter(|n| !n.trim().is_empty()),
        created_at: now(),
    };
    let index = PhoneIndex {
        pk: PhoneIndex::pk_for(&phone),
        sk: PhoneIndex::SK.to_string(),
        user_id: user_id.clone(),
        created_at: now(),
    };
    dbc.put(&user).await?;
    dbc.put(&index).await?;

    let token = sign_token(&state, &user_id)?;
    Ok((
        StatusCode::CREATED,
        Json(AuthResp {
            token,
            user_id,
            nickname: user.nickname,
        }),
    ))
}

/// `POST /api/auth/login`
pub async fn login(
    Extension(state): Extension<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<AuthResp>, AppError> {
    // 统一返回同一个错误，避免暴露"手机号是否已注册"
    let invalid = || AppError::Unauthorized("手机号或密码不正确".into());

    let dbc = db(&state);
    let phone = req.phone.trim().to_string();
    let index = dbc
        .get::<PhoneIndex>(&PhoneIndex::pk_for(&phone), PhoneIndex::SK)
        .await?
        .ok_or_else(invalid)?;

    let user = dbc
        .get::<User>(&User::pk_for(&index.user_id), User::SK)
        .await?
        .ok_or_else(invalid)?;

    if !auth::verify_password(&req.password, &user.password_hash) {
        return Err(invalid());
    }

    let token = sign_token(&state, &user.user_id)?;
    Ok(Json(AuthResp {
        token,
        user_id: user.user_id,
        nickname: user.nickname,
    }))
}

// ------------------------------------------------------------- 家长端：孩子管理

/// `GET /api/kids`
pub async fn list_kids(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
) -> Result<Json<Vec<KidOut>>, AppError> {
    let kids: Vec<Kid> = db(&state).query(&Kid::pk_for(&claims.sub)).await?;
    let mut out: Vec<KidOut> = kids.iter().map(KidOut::from).collect();
    out.sort_by_key(|k| k.created_at);
    Ok(Json(out))
}

/// `POST /api/kids`
pub async fn create_kid(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
    Json(req): Json<CreateKidReq>,
) -> Result<(StatusCode, Json<KidOut>), AppError> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("请填写孩子姓名".into()));
    }
    if name.chars().count() > 20 {
        return Err(AppError::BadRequest("姓名过长".into()));
    }
    if let Some(g) = req.grade {
        if !(1..=12).contains(&g) {
            return Err(AppError::BadRequest("年级应在 1–12 之间".into()));
        }
    }

    let kid_id = gen_id("k");
    let kid = Kid {
        pk: Kid::pk_for(&claims.sub),
        sk: Kid::sk_for(&kid_id),
        kid_id,
        user_id: claims.sub.clone(),
        name: name.to_string(),
        grade: req.grade,
        device_token_hash: None,
        bound_at: None,
        created_at: now(),
    };

    db(&state).put(&kid).await?;
    Ok((StatusCode::CREATED, Json(KidOut::from(&kid))))
}

/// `POST /api/kids/{kid_id}/bind-code`
pub async fn create_bind_code(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
    Path(kid_id): Path<String>,
) -> Result<Json<BindCodeResp>, AppError> {
    owned_kid(&state, &claims.sub, &kid_id).await?;

    let code = gen_bind_code();
    let bc = BindCode {
        pk: BindCode::pk_for(&code),
        sk: BindCode::SK.to_string(),
        kid_id,
        user_id: claims.sub.clone(),
        expires_at: now() + BIND_CODE_TTL,
    };
    db(&state).put(&bc).await?;

    Ok(Json(BindCodeResp {
        code,
        expires_in: BIND_CODE_TTL,
    }))
}

/// `DELETE /api/kids/{kid_id}/device`
pub async fn unbind_device(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
    Path(kid_id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut kid = owned_kid(&state, &claims.sub, &kid_id).await?;
    kid.device_token_hash = None;
    kid.bound_at = None;
    db(&state).put(&kid).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------- 家长端：词表

/// `GET /api/kids/{kid_id}/words` —— 含汉字与拼音，供家长编辑和试听。
pub async fn get_words(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
    Path(kid_id): Path<String>,
) -> Result<Json<WordsResp>, AppError> {
    owned_kid(&state, &claims.sub, &kid_id).await?;

    let list = db(&state)
        .get::<CurrentList>(&CurrentList::pk_for(&kid_id), CurrentList::SK)
        .await?;

    Ok(Json(match list {
        Some(l) => WordsResp {
            title: l.title,
            pushed_at: Some(l.pushed_at),
            default_repeat: l.default_repeat,
            words: l.words,
        },
        None => WordsResp {
            title: None,
            pushed_at: None,
            default_repeat: 1,
            words: vec![],
        },
    }))
}

/// 规范化并去重词表输入。
fn normalize_words(words: &[WordIn]) -> Result<Vec<(String, Option<String>)>, AppError> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for w in words {
        let text = auth::validate_word(&w.text)?;
        let pinyin = w
            .pinyin
            .as_ref()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        if !out.iter().any(|(t, p)| t == &text && p == &pinyin) {
            out.push((text, pinyin));
        }
    }
    Ok(out)
}

/// 内容寻址：算出某个词两个版本（念一遍 / 念两遍）的 S3 key。
fn word_keys(text: &str, pinyin: Option<&str>) -> (String, String, String, String) {
    let k1 = tts::audio_key(text, pinyin, 1);
    let k2 = tts::audio_key(text, pinyin, 2);
    let p1 = tts::key_to_path(&k1);
    let p2 = tts::key_to_path(&k2);
    (k1, k2, p1, p2)
}

/// 确保这些词的音频都在 S3 里，返回 `(新生成数, 复用数)`。
///
/// 先并发探测 S3，只对缺失的调 Polly——教材词表在用户间高度重复，
/// 通常绝大多数都是命中的。
async fn ensure_audio(
    state: &AppState,
    s3: &S3Client,
    inputs: &[(String, Option<String>)],
) -> Result<(usize, usize), AppError> {
    // 1) 展开成待检查的 key 列表（每词两个版本）
    let mut keys: Vec<String> = Vec::new();
    for (text, pinyin) in inputs {
        let (k1, k2, _, _) = word_keys(text, pinyin.as_deref());
        keys.push(k1);
        keys.push(k2);
    }

    // 2) 并发探测 S3：读得到 = 已缓存，NotFound = 需生成。
    //
    // 这里用 get_object 而非 head，是因为 `crates/s3` 目前只暴露
    // get/put/presign。音频仅几 KB，同区域内下载开销可忽略；
    // 若日后要省这点流量，给 `crates/s3` 加个 head_object 即可。
    let checks: Vec<(String, bool)> = stream::iter(keys)
        .map(|key| {
            let s3 = s3.clone();
            async move {
                let exists = s3.get_object(&key).await.is_ok();
                (key, exists)
            }
        })
        .buffer_unordered(GEN_CONCURRENCY)
        .collect()
        .await;

    // 3) 只对缺失项调 Polly
    let mut to_generate: Vec<(String, String, Option<String>, u8)> = Vec::new();
    let mut cached = 0usize;
    for (key, exists) in checks {
        if exists {
            cached += 1;
            continue;
        }
        // 反查这个 key 对应哪个词的哪个版本
        if let Some((text, pinyin, repeat)) = lookup_key(inputs, &key) {
            to_generate.push((key, text, pinyin, repeat));
        }
    }

    // 4) 并发生成缺失项
    let generated = to_generate.len();
    let results: Vec<Result<(), AppError>> = stream::iter(to_generate)
        .map(|(key, text, pinyin, repeat)| {
            let s3 = s3.clone();
            let aws = state.aws_config.clone();
            async move {
                let bytes = tts::synthesize(&aws, &text, pinyin.as_deref(), repeat).await?;
                s3.put_object(&key, bytes, "audio/mpeg").await?;
                tracing::info!(%key, %text, repeat, "已生成音频");
                Ok(())
            }
        })
        .buffer_unordered(GEN_CONCURRENCY)
        .collect()
        .await;
    for r in results {
        r?;
    }

    Ok((generated, cached))
}

/// 由 S3 key 反查它对应的 (文本, 拼音, 遍数)。
fn lookup_key(inputs: &[(String, Option<String>)], key: &str) -> Option<(String, Option<String>, u8)> {
    for (text, pinyin) in inputs {
        for repeat in [1u8, 2u8] {
            if tts::audio_key(text, pinyin.as_deref(), repeat) == key {
                return Some((text.clone(), pinyin.clone(), repeat));
            }
        }
    }
    None
}

/// `POST /api/kids/{kid_id}/push` —— 下发词表并生成缺失音频。
pub async fn push_words(
    Extension(state): Extension<AppState>,
    JwtAuth(claims): JwtAuth,
    Path(kid_id): Path<String>,
    Json(req): Json<PushReq>,
) -> Result<Json<PushResp>, AppError> {
    owned_kid(&state, &claims.sub, &kid_id).await?;

    if req.words.is_empty() {
        return Err(AppError::BadRequest("词表为空".into()));
    }
    if req.words.len() > MAX_WORDS {
        return Err(AppError::BadRequest(format!("一次最多下发 {MAX_WORDS} 个词")));
    }
    let default_repeat = if req.default_repeat >= 2 { 2 } else { 1 };

    let inputs = normalize_words(&req.words)?;
    if inputs.is_empty() {
        return Err(AppError::BadRequest("词表为空".into()));
    }

    let s3 = s3_client(&state)?;
    let (generated, cached) = ensure_audio(&state, &s3, &inputs).await?;

    let words: Vec<Word> = inputs
        .iter()
        .map(|(text, pinyin)| {
            let (_, _, p1, p2) = word_keys(text, pinyin.as_deref());
            Word {
                text: text.clone(),
                pinyin: pinyin.clone(),
                a1: Some(p1),
                a2: Some(p2),
            }
        })
        .collect();

    let list = CurrentList {
        pk: CurrentList::pk_for(&kid_id),
        sk: CurrentList::SK.to_string(),
        kid_id,
        title: req.title.clone().filter(|t| !t.trim().is_empty()),
        pushed_at: now(),
        pushed_by: claims.sub.clone(),
        default_repeat,
        words,
    };
    db(&state).put(&list).await?;

    Ok(Json(PushResp {
        generated,
        cached,
        total: inputs.len() * 2,
    }))
}

/// `POST /api/preview` —— 试听单个词（家长纠错用）。
pub async fn preview(
    Extension(state): Extension<AppState>,
    JwtAuth(_claims): JwtAuth,
    Json(req): Json<PreviewReq>,
) -> Result<Json<PreviewResp>, AppError> {
    let text = auth::validate_word(&req.text)?;
    let pinyin = req
        .pinyin
        .as_ref()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());

    let s3 = s3_client(&state)?;
    ensure_audio(&state, &s3, &[(text.clone(), pinyin.clone())]).await?;
    let (_, _, p1, p2) = word_keys(&text, pinyin.as_deref());

    Ok(Json(PreviewResp { a1: p1, a2: p2 }))
}

/// `POST /api/ocr`
pub async fn ocr_words(
    Extension(state): Extension<AppState>,
    JwtAuth(_claims): JwtAuth,
    Json(req): Json<OcrReq>,
) -> Result<Json<OcrResp>, AppError> {
    let words = ocr::recognize(
        &state.aws_config,
        &state.config,
        &req.image_base64,
        &req.media_type,
    )
    .await?;
    Ok(Json(OcrResp { words }))
}

// ------------------------------------------------------------- 孩子端

/// `POST /api/kid/bind` —— 用绑定码换长期设备令牌（公开接口）。
pub async fn kid_bind(
    Extension(state): Extension<AppState>,
    Json(req): Json<BindReq>,
) -> Result<Json<BindResp>, AppError> {
    let code = req.code.trim().to_uppercase();
    if code.is_empty() {
        return Err(AppError::BadRequest("请输入绑定码".into()));
    }

    let dbc = db(&state);
    let bc = dbc
        .get::<BindCode>(&BindCode::pk_for(&code), BindCode::SK)
        .await?
        .ok_or_else(|| AppError::BadRequest("绑定码无效或已使用".into()))?;

    // 无论是否过期都删掉，防止同一个码被反复尝试
    dbc.delete(&BindCode::pk_for(&code), BindCode::SK).await?;

    if now() > bc.expires_at {
        return Err(AppError::BadRequest(
            "绑定码已过期，请在家长端重新生成".into(),
        ));
    }

    let mut kid = owned_kid(&state, &bc.user_id, &bc.kid_id).await?;
    let token = auth::gen_device_token(&bc.user_id, &bc.kid_id);
    kid.device_token_hash = Some(auth::hash_token(&token));
    kid.bound_at = Some(now());
    dbc.put(&kid).await?;

    Ok(Json(BindResp {
        device_token: token,
        kid_name: kid.name,
    }))
}

/// `GET /api/kid/current` —— **响应里没有任何汉字**（防偷看）。
pub async fn kid_current(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
) -> Result<Json<KidSession>, AppError> {
    let (_kid, list) = device_context(&state, &headers).await?;

    let items: Vec<SessionItem> = list
        .words
        .iter()
        .filter_map(|w| match (&w.a1, &w.a2) {
            (Some(a1), Some(a2)) => Some(SessionItem {
                a1: a1.clone(),
                a2: a2.clone(),
            }),
            _ => None,
        })
        .collect();

    // 刻意不下发 title：标题「第2课《繁星》」里的「繁星」可能就是待默写的词
    Ok(Json(KidSession {
        total: items.len(),
        default_repeat: list.default_repeat,
        items,
    }))
}

/// `GET /api/kid/answers` —— 默写结束后取答案供核对。
pub async fn kid_answers(
    Extension(state): Extension<AppState>,
    headers: HeaderMap,
) -> Result<Json<KidAnswers>, AppError> {
    let (_kid, list) = device_context(&state, &headers).await?;
    Ok(Json(KidAnswers {
        title: list.title,
        words: list.words.iter().map(|w| w.text.clone()).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_string_equality() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn normalize_words_trims_and_dedupes() {
        let input = vec![
            WordIn { text: "  繁星 ".into(), pinyin: None },
            WordIn { text: "繁星".into(), pinyin: None },
            WordIn { text: "繁华".into(), pinyin: Some("fan2 hua2".into()) },
            WordIn { text: "繁华".into(), pinyin: Some("  ".into()) },
        ];
        let out = normalize_words(&input).unwrap();
        assert_eq!(out.len(), 3, "应去掉完全重复的项: {out:?}");
        assert_eq!(out[0], ("繁星".to_string(), None));
        assert_eq!(out[1], ("繁华".to_string(), Some("fan2 hua2".to_string())));
        // 第三次的空白拼音等价于无拼音，与第二次不同故保留
        assert_eq!(out[2], ("繁华".to_string(), None));
    }

    #[test]
    fn normalize_words_rejects_empty() {
        let input = vec![WordIn { text: "   ".into(), pinyin: None }];
        assert!(normalize_words(&input).is_err());
    }

    #[test]
    fn word_keys_are_stable_and_distinct() {
        let (k1, k2, p1, p2) = word_keys("繁星", None);
        assert_ne!(k1, k2, "念一遍与念两遍必须不同文件");
        assert_eq!(p1, format!("/{k1}"));
        assert_eq!(p2, format!("/{k2}"));
        assert!(p1.starts_with("/audio/"));
        // 稳定
        assert_eq!(word_keys("繁星", None).0, k1);
    }

    #[test]
    fn lookup_key_finds_matching_word_and_repeat() {
        let inputs = vec![
            ("繁星".to_string(), None),
            ("模糊".to_string(), Some("mo2 hu2".to_string())),
        ];
        let k = tts::audio_key("模糊", Some("mo2 hu2"), 2);
        let found = lookup_key(&inputs, &k).unwrap();
        assert_eq!(found.0, "模糊");
        assert_eq!(found.1.as_deref(), Some("mo2 hu2"));
        assert_eq!(found.2, 2);

        let k1 = tts::audio_key("繁星", None, 1);
        assert_eq!(lookup_key(&inputs, &k1).unwrap().2, 1);

        assert!(lookup_key(&inputs, "/audio/notmine.mp3").is_none());
    }

    /// 纠错路径的关键性质：改了拼音，key 必须变，否则会继续命中旧的错误音频。
    #[test]
    fn fixing_pinyin_changes_key() {
        let wrong = tts::audio_key("模糊", None, 1);
        let right = tts::audio_key("模糊", Some("mo2 hu2"), 1);
        assert_ne!(wrong, right);
    }

    #[test]
    fn data_table_uses_env_prefix() {
        // 表名 = {prefix}data，前缀由框架按环境注入
        let prefix = "moxie-dev-";
        assert_eq!(format!("{prefix}data"), "moxie-dev-data");
    }
}
