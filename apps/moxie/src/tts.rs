//! AWS Polly 中文语音合成：拼音 SSML + 内容寻址 key。
//!
//! ## 设计要点
//!
//! 1. **内容寻址**：音频 key 由 `(文本, 拼音, 遍数, 音色)` 哈希得出。同一个词在全平台
//!    只生成一次，跨用户、跨课文、跨天复用——全国小学生学同一套教材，命中率极高，
//!    这让 Polly 成本趋近于零，也让下发的响应从秒级降到毫秒级。
//!
//! 2. **拼音 SSML**：`<phoneme alphabet="x-amazon-pinyin" ph="mo2 hu2">模糊</phoneme>`
//!    用于纠正多音字（"模糊"读 mó 不读 mú）。实测同一文本用 `mo2` / `mu2` 生成的
//!    音频哈希不同，证明该标签确实在改变发音。
//!    声调数字：1 阴平 / 2 阳平 / 3 上声 / 4 去声 / 0 轻声。
//!
//! 3. **每词两个文件**：念一遍 / 念两遍。两遍版用 `<break>` 分隔，比客户端连播两次
//!    停顿更自然。孩子端切换遍数时只是换个 url，无需重新生成。

use std::time::Duration;

use aws_sdk_polly::types::{Engine, OutputFormat, TextType, VoiceId};
use operon_core::AppError;
use sha2::{Digest, Sha256};

/// cmn-CN 目前唯一的中文音色（女声），支持 neural 引擎。
pub const VOICE: &str = "Zhiyu";
/// 念两遍时中间的停顿。
pub const REPEAT_BREAK: &str = "900ms";
/// 音频在 S3 中的前缀。前端拿到的路径是 `/audio/xxx.mp3`（去掉这个前缀前的斜杠差异）。
pub const AUDIO_PREFIX: &str = "audio/";

/// 稳定地喂入一个字符串：带长度前缀，避免拼接歧义（如 `"ab"+"c"` 与 `"a"+"bc"`）。
fn feed(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_le_bytes());
    h.update(s.as_bytes());
}

/// 内容寻址 key。相同输入必定得到相同 key；任一字段变化 key 必变。
///
/// 返回形如 `audio/1a2b3c4d5e6f7a8b.mp3`。
pub fn audio_key(text: &str, pinyin: Option<&str>, repeat: u8) -> String {
    let mut h = Sha256::new();
    feed(&mut h, text);
    feed(&mut h, pinyin.unwrap_or("").trim());
    feed(&mut h, &repeat.to_string());
    feed(&mut h, VOICE);
    let digest = h.finalize();
    format!("{}{}.mp3", AUDIO_PREFIX, hex::encode(&digest[..8]))
}

/// S3 key → 前端可直接使用的路径。`audio/x.mp3` → `/audio/x.mp3`
pub fn key_to_path(key: &str) -> String {
    format!("/{key}")
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 把单个词渲染成 SSML 片段：有拼音就用 `<phoneme>` 强制读音。
fn render_word(text: &str, pinyin: Option<&str>) -> String {
    match pinyin.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!(
            r#"<phoneme alphabet="x-amazon-pinyin" ph="{}">{}</phoneme>"#,
            escape_xml(p),
            escape_xml(text)
        ),
        None => escape_xml(text),
    }
}

/// 构造完整 SSML。`repeat >= 2` 时念两遍，中间加 `<break>`。
pub fn build_ssml(text: &str, pinyin: Option<&str>, repeat: u8) -> String {
    let one = render_word(text, pinyin);
    if repeat >= 2 {
        format!(
            r#"<speak>{}<break time="{}"/>{}</speak>"#,
            one, REPEAT_BREAK, one
        )
    } else {
        format!("<speak>{one}</speak>")
    }
}

/// Polly 的瞬时故障重试次数。
///
/// 实测：38 次并发生成里会有 1–2 次返回通用的 `service error`（服务端 500），
/// 重试即可成功。不重试的话整个下发会因为这 1 次失败而全盘失败。
pub const MAX_ATTEMPTS: u32 = 3;

/// 第 `attempt` 次失败后的退避时长（毫秒），指数增长。
fn backoff_ms(attempt: u32) -> u64 {
    200 * 2u64.pow(attempt)
}

/// 对可重试操作做指数退避。
///
/// 抽成泛型是为了能用一个假的操作去测重试行为本身，而不必真的调 Polly。
async fn with_retry<F, Fut, T>(mut op: F) -> Result<T, AppError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    let mut last: Option<AppError> = None;
    for attempt in 0..MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    tracing::warn!(attempt, error = %e, "合成失败，准备重试");
                    tokio::time::sleep(Duration::from_millis(backoff_ms(attempt))).await;
                }
                last = Some(e);
            }
        }
    }
    Err(last.expect("MAX_ATTEMPTS 至少为 1"))
}

/// 调 Polly 合成一段音频，返回 MP3 字节。带瞬时故障重试。
pub async fn synthesize(
    aws_config: &aws_config::SdkConfig,
    text: &str,
    pinyin: Option<&str>,
    repeat: u8,
) -> Result<Vec<u8>, AppError> {
    if text.trim().is_empty() {
        return Err(AppError::BadRequest("词语为空".into()));
    }
    with_retry(|| synthesize_once(aws_config, text, pinyin, repeat)).await
}

async fn synthesize_once(
    aws_config: &aws_config::SdkConfig,
    text: &str,
    pinyin: Option<&str>,
    repeat: u8,
) -> Result<Vec<u8>, AppError> {
    let client = aws_sdk_polly::Client::new(aws_config);
    let ssml = build_ssml(text, pinyin, repeat);

    let out = client
        .synthesize_speech()
        .voice_id(VoiceId::from(VOICE))
        .engine(Engine::Neural)
        .output_format(OutputFormat::Mp3)
        .text_type(TextType::Ssml)
        .text(ssml)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("polly 合成失败: {e}")))?;

    let bytes = out
        .audio_stream
        .collect()
        .await
        .map_err(|e| AppError::Internal(format!("polly 读取音频流失败: {e}")))?
        .into_bytes();

    if bytes.is_empty() {
        return Err(AppError::Internal("polly 返回了空音频".into()));
    }
    Ok(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_key_is_deterministic() {
        let a = audio_key("模糊", Some("mo2 hu2"), 1);
        let b = audio_key("模糊", Some("mo2 hu2"), 1);
        assert_eq!(a, b, "同样的输入必须得到同样的 key（内容寻址的前提）");
        assert!(a.starts_with("audio/"));
        assert!(a.ends_with(".mp3"));
    }

    /// 这是整个方案的核心断言：拼音不同 → key 必须不同。
    /// 否则纠错后仍会命中旧的错误音频。
    #[test]
    fn audio_key_differs_by_pinyin() {
        let plain = audio_key("模糊", None, 1);
        let mo = audio_key("模糊", Some("mo2 hu2"), 1);
        let mu = audio_key("模糊", Some("mu2 hu2"), 1);
        assert_ne!(plain, mo);
        assert_ne!(mo, mu, "拼音不同必须产生不同 key，否则纠错无效");
        assert_ne!(plain, mu);
    }

    #[test]
    fn audio_key_differs_by_repeat() {
        assert_ne!(
            audio_key("繁星", None, 1),
            audio_key("繁星", None, 2),
            "念一遍和念两遍必须是两个文件"
        );
    }

    #[test]
    fn audio_key_differs_by_text() {
        assert_ne!(audio_key("繁星", None, 1), audio_key("繁华", None, 1));
    }

    /// 长度前缀必须在场，否则 ("ab","c") 与 ("a","bc") 会碰撞。
    #[test]
    fn audio_key_has_no_concatenation_collision() {
        assert_ne!(audio_key("ab", Some("c"), 1), audio_key("a", Some("bc"), 1));
    }

    #[test]
    fn ssml_uses_phoneme_when_pinyin_present() {
        let s = build_ssml("模糊", Some("mo2 hu2"), 1);
        assert!(s.starts_with("<speak>"));
        assert!(s.ends_with("</speak>"));
        assert!(s.contains(r#"alphabet="x-amazon-pinyin""#), "SSML: {s}");
        assert!(s.contains(r#"ph="mo2 hu2""#), "SSML: {s}");
        assert!(s.contains("模糊"));
        assert!(!s.contains("<break"), "念一遍不应有 break: {s}");
    }

    #[test]
    fn ssml_has_no_phoneme_without_pinyin() {
        let s = build_ssml("繁星", None, 1);
        assert!(!s.contains("phoneme"), "无拼音不应加 phoneme: {s}");
        assert_eq!(s, "<speak>繁星</speak>");
    }

    /// 空串或纯空白拼音应视同没有拼音。
    #[test]
    fn blank_pinyin_is_treated_as_absent() {
        assert_eq!(build_ssml("繁星", Some(""), 1), "<speak>繁星</speak>");
        assert_eq!(build_ssml("繁星", Some("   "), 1), "<speak>繁星</speak>");
        assert_eq!(audio_key("繁星", Some(""), 1), audio_key("繁星", None, 1));
    }

    #[test]
    fn ssml_repeats_twice_with_break() {
        let s = build_ssml("繁星", None, 2);
        assert!(s.contains("<break"), "念两遍应有 break: {s}");
        assert_eq!(s.matches("繁星").count(), 2, "应出现两遍: {s}");
    }

    #[test]
    fn ssml_repeat_with_phoneme_applies_to_both() {
        let s = build_ssml("模糊", Some("mo2 hu2"), 2);
        // 数开标签而非 "phoneme"（后者会把 </phoneme> 也算上）
        assert_eq!(s.matches("<phoneme").count(), 2, "两遍都应有注音: {s}");
        assert_eq!(s.matches("模糊").count(), 2);
    }

    #[test]
    fn ssml_escapes_xml_specials() {
        let s = build_ssml("a<b&c", None, 1);
        assert!(s.contains("&lt;"), "SSML: {s}");
        assert!(s.contains("&amp;"), "SSML: {s}");
        assert!(!s.contains("a<b"), "未转义会破坏 SSML: {s}");
    }

    #[test]
    fn backoff_grows_exponentially() {
        assert_eq!(backoff_ms(0), 200);
        assert_eq!(backoff_ms(1), 400);
        assert_eq!(backoff_ms(2), 800);
        assert!(backoff_ms(2) > backoff_ms(1));
    }

    /// 首次就成功 → 不该有重试。
    #[tokio::test]
    async fn retry_returns_immediately_on_success() {
        use std::cell::Cell;
        use std::rc::Rc;
        let calls = Rc::new(Cell::new(0usize));
        let c = calls.clone();
        let r: Result<u8, AppError> = with_retry(|| {
            c.set(c.get() + 1);
            async { Ok(7) }
        })
        .await;
        assert_eq!(r.unwrap(), 7);
        assert_eq!(calls.get(), 1, "成功就不该重试");
    }

    /// 回归测试：实测 Polly 会偶发返回通用 `service error`，
    /// 前两次失败、第三次成功是真实会发生的场景——必须能恢复。
    #[tokio::test]
    async fn retry_recovers_from_transient_failures() {
        use std::cell::Cell;
        use std::rc::Rc;
        let calls = Rc::new(Cell::new(0usize));
        let c = calls.clone();
        let r: Result<&str, AppError> = with_retry(|| {
            let n = c.get();
            c.set(n + 1);
            async move {
                if n < 2 {
                    Err(AppError::Internal("service error".into()))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), "ok", "第三次应成功");
        assert_eq!(calls.get(), 3);
    }

    /// 一直失败 → 重试到上限后放弃，不能无限重试。
    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        use std::cell::Cell;
        use std::rc::Rc;
        let calls = Rc::new(Cell::new(0usize));
        let c = calls.clone();
        let r: Result<(), AppError> = with_retry(|| {
            c.set(c.get() + 1);
            async { Err(AppError::Internal("always".into())) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.get(), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn key_to_path_prefixes_slash() {
        let key = audio_key("繁星", None, 1);
        let path = key_to_path(&key);
        assert_eq!(path, format!("/{key}"));
        assert!(path.starts_with("/audio/"));
    }
}
