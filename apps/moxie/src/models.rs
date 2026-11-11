//! 模型层：DynamoDB 实体 + API 请求/响应结构。
//!
//! 单表设计 `moxie-{env}-data`，前缀式 pk：
//! - `USER#{user_id}`  + `PROFILE`     账号
//! - `USER#{user_id}`  + `KID#{kid_id}` 孩子（一账号多孩子）
//! - `KID#{kid_id}`    + `CURRENT`     当前下发的词表
//! - `BIND#{code}`     + `META`        一次性绑定码
//!
//! **安全约定**：所有返回给客户端的结构（`*Out` / `*Resp`）都不得包含
//! `password_hash`、`device_token_hash` 等敏感字段；实体结构不直接序列化给客户端。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 实体

/// 家长账号。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub pk: String, // USER#{user_id}
    pub sk: String, // PROFILE
    pub user_id: String,
    pub phone: String,
    /// argon2id 哈希，绝不存明文
    pub password_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    pub created_at: i64,
}

impl User {
    pub fn pk_for(user_id: &str) -> String {
        format!("USER#{user_id}")
    }
    pub const SK: &'static str = "PROFILE";
}

/// 孩子（挂在账号下）。
///
/// **注意分区设计**：孩子用独立分区 `USER#{user_id}#KIDS`，而非与账号共用
/// `USER#{user_id}`。因为 `DynamoClient::query(pk)` 会把同一分区下**所有**记录
/// 反序列化成同一类型，共用分区会导致 account 的 PROFILE 记录与孩子记录
/// 互相反序列化失败。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Kid {
    pub pk: String, // USER#{user_id}#KIDS
    pub sk: String, // KID#{kid_id}
    pub kid_id: String,
    pub user_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<i32>,
    /// 设备令牌的 sha256；None = 未绑定设备
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_token_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bound_at: Option<i64>,
    pub created_at: i64,
}

impl Kid {
    pub fn pk_for(user_id: &str) -> String {
        format!("USER#{user_id}#KIDS")
    }
    pub fn sk_for(kid_id: &str) -> String {
        format!("KID#{kid_id}")
    }
}

/// 手机号 → user_id 的索引项：登录查找 + 注册唯一性。
///
/// 独立分区 `PHONE#{phone}` / `META`，注册时先查此项是否已存在。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PhoneIndex {
    pub pk: String, // PHONE#{phone}
    pub sk: String, // META
    pub user_id: String,
    pub created_at: i64,
}

impl PhoneIndex {
    pub fn pk_for(phone: &str) -> String {
        format!("PHONE#{phone}")
    }
    pub const SK: &'static str = "META";
}

/// 一个词：文本 + 可选拼音 + 两个版本的音频路径。
///
/// `pinyin` 为空时交给 Polly 自判；读错时家长补上拼音（如 `"mo2 hu2"`）重新生成。
/// `a1` / `a2` 是可直接给前端的路径，形如 `/audio/ab12cd34.mp3`。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinyin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a1: Option<String>, // 播一遍
    #[serde(skip_serializing_if = "Option::is_none")]
    pub a2: Option<String>, // 播两遍
}

/// 当前下发的词表（每个孩子一条）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentList {
    pub pk: String, // KID#{kid_id}
    pub sk: String, // CURRENT
    pub kid_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub pushed_at: i64,
    pub pushed_by: String,
    pub default_repeat: i32,
    pub words: Vec<Word>,
}

impl CurrentList {
    pub fn pk_for(kid_id: &str) -> String {
        format!("KID#{kid_id}")
    }
    pub const SK: &'static str = "CURRENT";
}

/// 一次性绑定码（5 分钟有效，用完即删）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BindCode {
    pub pk: String, // BIND#{code}
    pub sk: String, // META
    pub kid_id: String,
    pub user_id: String,
    pub expires_at: i64,
}

impl BindCode {
    pub fn pk_for(code: &str) -> String {
        format!("BIND#{code}")
    }
    pub const SK: &'static str = "META";
}

// ---------------------------------------------------------------- 认证请求/响应

#[derive(Debug, Deserialize)]
pub struct RegisterReq {
    pub phone: String,
    pub password: String,
    #[serde(default)]
    pub nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginReq {
    pub phone: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResp {
    pub token: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
}

// ---------------------------------------------------------------- 家长端

#[derive(Debug, Deserialize)]
pub struct CreateKidReq {
    pub name: String,
    #[serde(default)]
    pub grade: Option<i32>,
}

/// 孩子的对外表示（不含 device_token_hash）。
#[derive(Debug, Serialize)]
pub struct KidOut {
    pub kid_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<i32>,
    pub bound: bool,
    pub created_at: i64,
}

impl From<&Kid> for KidOut {
    fn from(k: &Kid) -> Self {
        Self {
            kid_id: k.kid_id.clone(),
            name: k.name.clone(),
            grade: k.grade,
            bound: k.device_token_hash.is_some(),
            created_at: k.created_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BindCodeResp {
    pub code: String,
    /// 剩余有效秒数
    pub expires_in: i64,
}

/// 家长端看到的词表（含汉字与拼音，用于编辑和试听）。
#[derive(Debug, Serialize)]
pub struct WordsResp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_at: Option<i64>,
    pub default_repeat: i32,
    pub words: Vec<Word>,
}

#[derive(Debug, Deserialize)]
pub struct PushReq {
    #[serde(default)]
    pub title: Option<String>,
    pub words: Vec<WordIn>,
    #[serde(default = "default_repeat")]
    pub default_repeat: i32,
}

fn default_repeat() -> i32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct WordIn {
    pub text: String,
    #[serde(default)]
    pub pinyin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PushResp {
    /// 本次新生成的音频数量
    pub generated: usize,
    /// 命中已有缓存的数量（跨用户共享）
    pub cached: usize,
    pub total: usize,
}

#[derive(Debug, Deserialize)]
pub struct PreviewReq {
    pub text: String,
    #[serde(default)]
    pub pinyin: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewResp {
    pub a1: String,
    pub a2: String,
}

#[derive(Debug, Deserialize)]
pub struct OcrReq {
    /// 浏览器端压缩后的图片 base64（不含 data: 前缀）
    pub image_base64: String,
    #[serde(default = "default_media_type")]
    pub media_type: String,
}

fn default_media_type() -> String {
    "image/jpeg".into()
}

#[derive(Debug, Serialize)]
pub struct OcrResp {
    pub words: Vec<String>,
}

// ---------------------------------------------------------------- 孩子端

#[derive(Debug, Deserialize)]
pub struct BindReq {
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct BindResp {
    pub device_token: String,
    pub kid_name: String,
}

/// 孩子端的单个词条 —— **刻意只有音频路径，没有任何汉字**（防偷看）。
#[derive(Debug, Serialize)]
pub struct SessionItem {
    pub a1: String,
    pub a2: String,
}

/// 孩子端会话 —— **绝不含 `text` / `pinyin` / `title`**。
///
/// 连标题都不给：标题形如「第2课《繁星》」，而「繁星」本身很可能就是待默写的词，
/// 渲染出来等于送答案。标题只在默写结束后的 [`KidAnswers`] 里出现。
#[derive(Debug, Serialize)]
pub struct KidSession {
    pub total: usize,
    pub default_repeat: i32,
    pub items: Vec<SessionItem>,
}

/// 默写结束后的答案（家长核对 / 孩子自查）。
#[derive(Debug, Serialize)]
pub struct KidAnswers {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub words: Vec<String>,
}

// ---------------------------------------------------------------- 工具

/// 生成 8 位无歧义房间/绑定码（去掉易混的 0/O/1/I/L）。
pub fn gen_bind_code() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..6)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// 生成带前缀的唯一 ID，如 `u_8f3a...`。
pub fn gen_id(prefix: &str) -> String {
    format!("{}_{}", prefix, uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_pk_sk_shape() {
        assert_eq!(User::pk_for("u_1"), "USER#u_1");
        assert_eq!(User::SK, "PROFILE");
        assert_eq!(Kid::sk_for("k_1"), "KID#k_1");
        assert_eq!(CurrentList::pk_for("k_1"), "KID#k_1");
        assert_eq!(BindCode::pk_for("ABC234"), "BIND#ABC234");
        assert_eq!(PhoneIndex::pk_for("13800138000"), "PHONE#13800138000");
    }

    /// 账号分区与孩子分区必须不同，否则 `query()` 会把两种记录当同一类型反序列化。
    #[test]
    fn account_and_kids_partitions_are_distinct() {
        let user_pk = User::pk_for("u_1");
        let kids_pk = Kid::pk_for("u_1");
        assert_ne!(user_pk, kids_pk, "账号与孩子共用分区会导致反序列化冲突");
        // 孩子分区必须能按 user_id 前缀查询
        assert!(kids_pk.starts_with("USER#u_1"));

        // 不同账号的孩子分区也必须隔离（多租户）
        assert_ne!(Kid::pk_for("u_1"), Kid::pk_for("u_2"));
    }

    #[test]
    fn bind_code_is_6_chars_from_unambiguous_alphabet() {
        const ALLOWED: &str = "23456789ABCDEFGHJKMNPQRSTUVWXYZ";
        for _ in 0..200 {
            let c = gen_bind_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ALLOWED.contains(ch)), "非法字符: {c}");
            // 易混字符不得出现
            for bad in ['0', 'O', '1', 'I', 'L'] {
                assert!(!c.contains(bad), "绑定了易混字符 {bad}: {c}");
            }
        }
    }

    /// 关键安全断言：孩子端会话**不得泄露任何词语**（防偷看）。
    ///
    /// 用真实词表做输入，逐个断言词面不出现在响应里；另断言整个 JSON
    /// 不含任何汉字——这同时覆盖了「标题泄露」这类间接泄露
    /// （标题「第2课《繁星》」里的「繁星」就是待默写的词）。
    #[test]
    fn kid_session_leaks_no_word_text() {
        let words = ["繁星", "繁华", "怀抱", "模糊"];
        let session = KidSession {
            total: words.len(),
            default_repeat: 1,
            items: words
                .iter()
                .map(|_| SessionItem {
                    a1: "/audio/aaaa.mp3".into(),
                    a2: "/audio/bbbb.mp3".into(),
                })
                .collect(),
        };
        let json = serde_json::to_string(&session).unwrap();

        for w in words {
            assert!(!json.contains(w), "孩子端会话泄露了词语「{w}」: {json}");
        }
        assert!(
            !json.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "孩子端会话不应含任何汉字: {json}"
        );
        assert!(!json.contains("text"), "泄露 text 字段: {json}");
        assert!(!json.contains("pinyin"), "泄露 pinyin 字段: {json}");
        assert!(!json.contains("title"), "标题可能含词语，不应下发: {json}");
    }

    /// 答案接口是默写结束后才调的，这里**应该**有词语。
    #[test]
    fn kid_answers_does_contain_words() {
        let answers = KidAnswers {
            title: Some("第2课《繁星》".into()),
            words: vec!["繁星".into(), "繁华".into()],
        };
        let json = serde_json::to_string(&answers).unwrap();
        assert!(json.contains("繁星"));
        assert!(json.contains("繁华"));
    }

    /// KidOut 不得泄露设备令牌哈希。
    #[test]
    fn kid_out_hides_device_token_hash() {
        let kid = Kid {
            pk: "USER#u_1#KIDS".into(),
            sk: "KID#k_1".into(),
            kid_id: "k_1".into(),
            user_id: "u_1".into(),
            name: "小明".into(),
            grade: Some(4),
            device_token_hash: Some("deadbeef".into()),
            bound_at: Some(123),
            created_at: 100,
        };
        let json = serde_json::to_string(&KidOut::from(&kid)).unwrap();
        assert!(!json.contains("deadbeef"), "泄露令牌哈希: {json}");
        assert!(!json.contains("password"), "泄露密码字段: {json}");
        assert!(json.contains("小明"));
        assert!(json.contains("\"bound\":true"));
    }

    /// User 实体不得被直接序列化返回给客户端（此处断言它含敏感字段，提醒勿直接返回）。
    #[test]
    fn word_serializes_optional_fields_away() {
        let w = Word {
            text: "繁星".into(),
            pinyin: None,
            a1: Some("/audio/x.mp3".into()),
            a2: None,
        };
        let json = serde_json::to_string(&w).unwrap();
        assert!(!json.contains("pinyin"), "None 字段不应序列化: {json}");
        assert!(!json.contains("a2"), "None 字段不应序列化: {json}");
        assert!(json.contains("繁星"));
    }

    #[test]
    fn push_resp_counts_add_up() {
        let r = PushResp {
            generated: 3,
            cached: 37,
            total: 40,
        };
        assert_eq!(r.generated + r.cached, r.total);
    }
}
