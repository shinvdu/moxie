//! 拍照识别词表：Bedrock（可选）→ GLM-4V（默认可用路径）。
//!
//! 家长常拿到老师发的图片，手打 20 个词很烦。这里把图片里的词语抽出来。
//!
//! ## 实测结论（决定了默认走哪条路）
//!
//! 部分 AWS 账号默认**未开通 Bedrock**：调用时报
//! "you must provide further information so we can verify you are a corporate customer"，
//! 需要先提交工单做 allowlisting；且 Anthropic 系模型另有地区限制
//! （"Access to Anthropic models is not allowed from unsupported countries"）。
//!
//! 因此 Bedrock 改为**显式 opt-in**：只有配了 `OPERON_MOXIE_BEDROCK_MODEL_ID`
//! 才尝试，否则直接走 GLM-4V——避免每次 OCR 都白跑一次注定失败的请求。
//! 若账号已通过 allowlisting，导出该环境变量即可启用（模型用 Amazon Nova，
//! 无地区限制且比 Claude 便宜）。
//!
//! **图片大小**：Lambda Function URL 有 6MB payload 上限，手机原图会超。
//! 前端负责用 canvas 压缩到长边 ≤1600px / JPEG 0.75（约 200–400KB）再上传，
//! 这里再做一次 base64 解码体积兜底检查。

use base64::Engine as _;
use operon_core::AppError;

/// 上传图片的 base64 解码后体积上限（约 4MB，给 6MB payload 留余量）。
const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;

/// 给视觉模型的提示词。
///
/// **必须强调「一个格子里可能有多个词语」**：实测不强调时，
/// 模型只取每行的第一个词（19 个词只认出 8 个，"谈吐"还被认成"谈话"）。
/// 明确举出「繁 → 繁星 繁华」这个例子后，glm-4v-plus 达到 19/19。
const PROMPT: &str = "这是一张小学语文生字表。表格里有「生字」和「组词」两列。\n\
请把「组词」列里的每一个词语都提取出来。\n\
重要：一个生字对应的组词格子里可能有**两个或更多词语**\
（例如生字「繁」对应「繁星 繁华」），这些词**全部都要**提取，不要只取第一个。\n\
不要把「生字」列的字单独提取出来。不要拼音，不要序号，不要解释。\n\
每行只输出一个词语。";

/// GLM 视觉模型。可用 `OPERON_MOXIE_GLM_MODEL` 覆盖。
///
/// 选 `glm-4v-plus` 而非免费档 `glm-4v-flash`：实测同一张生字表，
/// flash 只有 14/19（且出现"谈吐→谈话"这类错字），plus 是 19/19；
/// 更准的 `glm-4.5v` 同样满分但慢约 4 倍（10.8s vs 2.5s），不划算。
fn glm_model() -> String {
    std::env::var("OPERON_MOXIE_GLM_MODEL").unwrap_or_else(|_| "glm-4v-plus".into())
}

/// 是否 CJK 汉字（含扩展 A 区与兼容区）。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK 统一表意文字
        | '\u{3400}'..='\u{4DBF}' // 扩展 A
        | '\u{F900}'..='\u{FAFF}' // 兼容表意文字
    )
}

/// 从模型返回的自由文本中解析出干净的词语列表。
///
/// 容错点：去掉行首序号/项目符号、按空白再切一次、**只保留含汉字的 token**、去重。
///
/// 「必须含汉字」这条比「丢弃纯 ASCII」更严：模型常把拼音一起吐出来，
/// 而带声调的拼音（`fán huá`）里的 `á` 并非 ASCII，用 ASCII 判断会漏掉它们，
/// 结果把拼音当成待默写的词语。
pub fn parse_words(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        // 去掉行首的 "1." "1、" "-" "•" "*" 等
        let trimmed = line
            .trim()
            .trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | '、' | ')' | '）' | '-' | '•' | '*' | ' ')
            })
            .trim();

        for token in trimmed.split_whitespace() {
            let t = token.trim_matches(|c: char| {
                matches!(c, ',' | '，' | '。' | '、' | ';' | '；' | ':' | '：' | '"' | '\'' | '“' | '”')
            });
            if t.is_empty() {
                continue;
            }
            // 只保留含汉字的 token，挡掉拼音（含带调符号）、编号、英文说明
            if !t.chars().any(is_cjk) {
                continue;
            }
            if t.chars().count() > 30 {
                continue;
            }
            if !out.iter().any(|x| x == t) {
                out.push(t.to_string());
            }
        }
    }
    out
}

/// 校验并解码前端上传的 base64 图片。
pub fn decode_image(image_base64: &str) -> Result<Vec<u8>, AppError> {
    // 容忍前端误带 data: 前缀
    let b64 = image_base64
        .split_once("base64,")
        .map(|(_, rest)| rest)
        .unwrap_or(image_base64);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| AppError::BadRequest("图片 base64 解码失败".into()))?;
    if bytes.is_empty() {
        return Err(AppError::BadRequest("图片为空".into()));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AppError::BadRequest(format!(
            "图片过大（{}KB），请重新拍摄或换一张",
            bytes.len() / 1024
        )));
    }
    Ok(bytes)
}

/// 识别图片中的词语。优先 Bedrock，失败则尝试 GLM-4V。
pub async fn recognize(
    aws_config: &aws_config::SdkConfig,
    config: &operon_core::AppConfig,
    image_base64: &str,
    media_type: &str,
) -> Result<Vec<String>, AppError> {
    decode_image(image_base64)?;

    // 1) Bedrock —— 仅当显式配置了模型 ID 才尝试（见模块文档）
    if std::env::var("OPERON_MOXIE_BEDROCK_MODEL_ID").is_ok() {
        match bedrock_recognize(aws_config, image_base64, media_type).await {
            Ok(raw) => {
                let words = parse_words(&raw);
                if !words.is_empty() {
                    return Ok(words);
                }
                tracing::warn!("bedrock 返回了空词表，改用 GLM");
            }
            Err(e) => tracing::warn!("bedrock 识别失败，改用 GLM: {e}"),
        }
    }

    // 2) GLM-4V（默认路径，需 SSM 配 zhipu_api_key）
    let words = glm_recognize(config, image_base64).await?;
    if words.is_empty() {
        return Err(AppError::BadRequest(
            "没有从图片中识别出词语，请换一张更清晰的图片或改用手动输入".into(),
        ));
    }
    Ok(words)
}

/// `image/jpeg` → Nova 要的 `jpeg`
fn image_format(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "jpeg",
    }
}

/// Bedrock 调用（Amazon Nova 的 Converse 风格请求体）。
///
/// 选 Nova 而非 Claude：Anthropic 系模型在 Bedrock 上有地区限制
/// （"Access to Anthropic models is not allowed from unsupported countries"），
/// Nova 没有这个限制，且视觉能力足够、价格更低。
///
/// 模型 ID 由 `OPERON_MOXIE_BEDROCK_MODEL_ID` 指定，例如 `amazon.nova-lite-v1:0`。
async fn bedrock_recognize(
    aws_config: &aws_config::SdkConfig,
    image_base64: &str,
    media_type: &str,
) -> Result<String, AppError> {
    let model_id = std::env::var("OPERON_MOXIE_BEDROCK_MODEL_ID")
        .map_err(|_| AppError::Internal("未配置 OPERON_MOXIE_BEDROCK_MODEL_ID".into()))?;

    let body = serde_json::json!({
        "messages": [{
            "role": "user",
            "content": [
                {
                    "image": {
                        "format": image_format(media_type),
                        "source": { "bytes": image_base64 }
                    }
                },
                { "text": PROMPT }
            ]
        }],
        "inferenceConfig": { "maxTokens": 2048 }
    });

    let client = aws_sdk_bedrockruntime::Client::new(aws_config);
    let out = client
        .invoke_model()
        .model_id(model_id)
        .content_type("application/json")
        .accept("application/json")
        .body(aws_sdk_bedrockruntime::primitives::Blob::new(
            serde_json::to_vec(&body).map_err(|e| AppError::Internal(e.to_string()))?,
        ))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("bedrock 调用失败: {e}")))?;

    let resp: serde_json::Value = serde_json::from_slice(out.body().as_ref())
        .map_err(|e| AppError::Internal(format!("bedrock 响应解析失败: {e}")))?;

    // Nova 响应：{"output":{"message":{"content":[{"text":"..."}]}}}
    let text = resp["output"]["message"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if text.is_empty() {
        return Err(AppError::Internal(format!("bedrock 响应中没有文本: {resp}")));
    }
    Ok(text)
}

/// GLM-4V 调用（降级路径，key 从 SSM 的 `zhipu_api_key` 读）。
async fn glm_recognize(
    config: &operon_core::AppConfig,
    image_base64: &str,
) -> Result<Vec<String>, AppError> {
    let key = config
        .secret("zhipu_api_key")
        .map(str::to_string)
        .or_else(|| std::env::var("OPERON_DEV_ZHIPU_API_KEY").ok())
        .ok_or_else(|| {
            AppError::Internal(
                "拍照识别暂不可用：未配置 zhipu_api_key（也未启用 Bedrock）。\
                 请改用「粘贴文本」录入词表。"
                    .into(),
            )
        })?;

    let body = serde_json::json!({
        "model": glm_model(),
        "messages": [{
            "role": "user",
            "content": [
                { "type": "image_url", "image_url": { "url": image_base64 } },
                { "type": "text", "text": PROMPT }
            ]
        }]
    });

    let resp = reqwest::Client::new()
        .post("https://open.bigmodel.cn/api/paas/v4/chat/completions")
        .bearer_auth(key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("GLM 请求失败: {e}")))?;

    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("GLM 响应解析失败: {e}")))?;

    if !status.is_success() {
        return Err(AppError::Internal(format!("GLM 返回 {status}: {json}")));
    }

    let text = json["choices"][0]["message"]["content"].as_str().unwrap_or("");
    Ok(parse_words(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_one_word_per_line() {
        let raw = "繁星\n繁华\n怀抱\n怀旧";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华", "怀抱", "怀旧"]);
    }

    #[test]
    fn strips_numbering_and_bullets() {
        let raw = "1. 繁星\n2、繁华\n- 怀抱\n• 怀旧\n* 寂寞";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华", "怀抱", "怀旧", "寂寞"]);
    }

    #[test]
    fn splits_multiple_words_on_one_line() {
        let raw = "繁星 繁华\n怀抱 怀旧";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华", "怀抱", "怀旧"]);
    }

    /// 模型有时会连拼音一起吐出来，拼音必须被丢掉（否则会被当成词语默写）。
    #[test]
    fn discards_pinyin_and_ascii() {
        let raw = "繁星 fan2 xing1\n繁华\nfán huá";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华"]);
    }

    /// 回归测试：**带声调的拼音含非 ASCII 字符（á）**，
    /// 早期用「丢弃纯 ASCII」实现的过滤挡不住它，会把拼音当词语下发。
    #[test]
    fn discards_tone_marked_pinyin() {
        let raw = "fán huá\nmó hú\nhú tu\n繁星";
        let out = parse_words(raw);
        assert_eq!(out, vec!["繁星"], "带调拼音被误当成词语: {out:?}");
        for w in &out {
            assert!(w.chars().any(is_cjk), "非汉字混入词表: {w}");
        }
    }

    #[test]
    fn is_cjk_covers_common_and_extension_ranges() {
        assert!(is_cjk('繁'));
        assert!(is_cjk('星'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('á')); // 带调拼音
        assert!(!is_cjk('1'));
        assert!(!is_cjk(' '));
        assert!(!is_cjk('，'));
    }

    /// 纯标点行不应产出词语。
    #[test]
    fn punctuation_only_lines_yield_nothing() {
        assert!(parse_words("，。、；：\n——\n?!").is_empty());
    }

    #[test]
    fn dedupes_preserving_order() {
        let raw = "繁星\n繁华\n繁星\n模糊\n繁华";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华", "模糊"]);
    }

    #[test]
    fn strips_trailing_punctuation() {
        let raw = "繁星，\n繁华。\n怀抱、";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华", "怀抱"]);
    }

    #[test]
    fn ignores_blank_lines_and_whitespace() {
        let raw = "\n\n  繁星  \n\n   \n繁华\n";
        assert_eq!(parse_words(raw), vec!["繁星", "繁华"]);
    }

    #[test]
    fn drops_overlong_tokens() {
        let long = "字".repeat(40);
        let raw = format!("繁星\n{long}\n繁华");
        assert_eq!(parse_words(&raw), vec!["繁星", "繁华"]);
    }

    #[test]
    fn empty_input_gives_empty_list() {
        assert!(parse_words("").is_empty());
        assert!(parse_words("\n\n  \n").is_empty());
        assert!(parse_words("1. \n2. \n").is_empty());
    }

    /// 提示词里这两条是准确率的关键，不能被无意改掉：
    /// 少了「全部都要」，模型只取每行第一个词（实测 8/19）。
    #[test]
    fn prompt_demands_all_words_from_each_cell() {
        assert!(PROMPT.contains("全部都要"), "提示词丢失了「全部都要」的关键要求");
        assert!(PROMPT.contains("繁星 繁华"), "提示词丢失了多词示例，模型会只取第一个词");
        assert!(PROMPT.contains("每行只输出一个词语"), "缺少输出格式约束");
        assert!(PROMPT.contains("不要拼音"), "缺少排除拼音的要求");
    }

    /// 免费档 `glm-4v-flash` 在真实生字表上只有 14/19（还有"谈吐→谈话"这类错字），
    /// 默认必须是 `glm-4v-plus`。
    #[test]
    fn glm_model_defaults_to_plus() {
        if std::env::var("OPERON_MOXIE_GLM_MODEL").is_ok() {
            return; // 显式覆盖时不测默认值
        }
        assert_eq!(glm_model(), "glm-4v-plus");
    }

    #[test]
    fn decode_image_accepts_data_url_prefix() {
        // "繁星" 的 base64 只是为了拿到一段合法 base64
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"hello");
        assert!(decode_image(&b64).is_ok());
        assert!(decode_image(&format!("data:image/jpeg;base64,{b64}")).is_ok());
    }

    #[test]
    fn decode_image_rejects_garbage_and_empty() {
        assert!(decode_image("!!!not base64!!!").is_err());
        assert!(decode_image("").is_err());
    }

    #[test]
    fn decode_image_rejects_oversized() {
        let big = base64::engine::general_purpose::STANDARD.encode(vec![0u8; MAX_IMAGE_BYTES + 1]);
        let err = decode_image(&big).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)), "应为 400: {err:?}");
    }
}
