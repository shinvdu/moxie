//! 账号体系：密码哈希、设备令牌、输入校验。
//!
//! **为什么不复用 operon 现有的登录**：`apps/site` 的管理员登录是把 SSM 里的明文
//! 密码做常量时间比对，那套只适合单一内置账号，**绝不能用于用户账号**。
//! 这里用 argon2id（密码哈希的当前推荐算法）。
//!
//! **设备令牌**：孩子平板绑定后拿到的长期凭证。刻意用**不透明随机串**而非 JWT，
//! 因为 JWT 无状态、签发后在过期前无法撤销；家长点「解绑」时我们希望旧令牌立刻失效。
//! 令牌格式 `{kid_id}.{64位hex}`，服务端只存它的 sha256。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use operon_core::AppError;
use rand::Rng;
use sha2::{Digest, Sha256};

/// 密码长度下限。太短容易被猜，太长（>128）会拖慢 argon2。
const PASSWORD_MIN: usize = 6;
const PASSWORD_MAX: usize = 128;

/// argon2id 哈希密码。每次调用盐都不同，所以同一密码两次哈希结果不同。
pub fn hash_password(password: &str) -> Result<String, AppError> {
    validate_password(password)?;
    let mut salt_bytes = [0u8; 16];
    rand::thread_rng().fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AppError::Internal(format!("生成盐失败: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(format!("密码哈希失败: {e}")))
}

/// 校验密码是否匹配。哈希串损坏时返回 false 而非报错（避免泄露内部状态）。
pub fn verify_password(password: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 生成设备令牌：`{user_id}.{kid_id}.{64位随机hex}`。
///
/// **为什么令牌里要带 user_id 和 kid_id**：孩子记录存在
/// `pk = USER#{user_id}#KIDS` / `sk = KID#{kid_id}` 分区下，
/// 光凭 kid_id 定位不到记录，需要 user_id 才能拼出分区键。
/// 两者都是 UUID，不是秘密；真正的凭证是最后那段 32 字节随机数，
/// 服务端只存它的 sha256，所以令牌本身可撤销。
pub fn gen_device_token(user_id: &str, kid_id: &str) -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill(&mut buf);
    format!("{user_id}.{kid_id}.{}", hex::encode(buf))
}

/// 从设备令牌中解析出 `(user_id, kid_id)`。
///
/// 只做格式解析，**不做合法性校验**——合法性靠与库里存的哈希比对。
pub fn parse_device_token(token: &str) -> Option<(&str, &str)> {
    let mut parts = token.splitn(3, '.');
    let user_id = parts.next()?;
    let kid_id = parts.next()?;
    let _secret = parts.next()?;
    if user_id.is_empty() || kid_id.is_empty() {
        return None;
    }
    Some((user_id, kid_id))
}

/// 令牌的 sha256 十六进制。数据库里只存这个，不存明文。
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// 中国大陆手机号：11 位数字，以 1 开头。
pub fn validate_phone(phone: &str) -> Result<(), AppError> {
    let p = phone.trim();
    let ok = p.len() == 11
        && p.starts_with('1')
        && p.chars().all(|c| c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(AppError::BadRequest("手机号格式不正确（应为 11 位数字）".into()))
    }
}

pub fn validate_password(password: &str) -> Result<(), AppError> {
    let n = password.chars().count();
    if n < PASSWORD_MIN {
        return Err(AppError::BadRequest(format!(
            "密码至少 {PASSWORD_MIN} 位"
        )));
    }
    if n > PASSWORD_MAX {
        return Err(AppError::BadRequest(format!(
            "密码最多 {PASSWORD_MAX} 位"
        )));
    }
    Ok(())
}

/// 词语文本校验：非空、长度合理、不含控制字符。
pub fn validate_word(text: &str) -> Result<String, AppError> {
    let t = text.trim();
    if t.is_empty() {
        return Err(AppError::BadRequest("词语不能为空".into()));
    }
    if t.chars().count() > 30 {
        return Err(AppError::BadRequest(format!("词语过长: {t}")));
    }
    if t.chars().any(|c| c.is_control()) {
        return Err(AppError::BadRequest("词语含非法字符".into()));
    }
    Ok(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let hash = hash_password("hunter2!").unwrap();
        assert!(verify_password("hunter2!", &hash));
        assert!(!verify_password("wrong", &hash));
    }

    /// 同一密码两次哈希必须不同（盐随机），否则可被彩虹表攻击。
    #[test]
    fn password_hash_is_salted() {
        let a = hash_password("samepassword").unwrap();
        let b = hash_password("samepassword").unwrap();
        assert_ne!(a, b, "两次哈希相同说明没有随机盐");
        assert!(verify_password("samepassword", &a));
        assert!(verify_password("samepassword", &b));
    }

    #[test]
    fn password_hash_is_not_plaintext() {
        let hash = hash_password("hunter2!").unwrap();
        assert!(!hash.contains("hunter2"), "哈希串里不能出现明文密码");
        assert!(hash.starts_with("$argon2"), "应为 argon2 格式: {hash}");
    }

    #[test]
    fn corrupt_hash_verifies_false_not_panic() {
        assert!(!verify_password("x", "not-a-hash"));
        assert!(!verify_password("x", ""));
    }

    #[test]
    fn password_length_is_enforced() {
        assert!(validate_password("12345").is_err(), "5 位应被拒");
        assert!(validate_password("123456").is_ok(), "6 位应通过");
        assert!(validate_password(&"a".repeat(129)).is_err(), "129 位应被拒");
        assert!(hash_password("12345").is_err(), "hash_password 也应校验长度");
    }

    #[test]
    fn phone_validation() {
        assert!(validate_phone("13800138000").is_ok());
        assert!(validate_phone(" 13800138000 ").is_ok(), "应容忍首尾空格");
        assert!(validate_phone("1380013800").is_err(), "10 位应被拒");
        assert!(validate_phone("138001380000").is_err(), "12 位应被拒");
        assert!(validate_phone("23800138000").is_err(), "不以 1 开头应被拒");
        assert!(validate_phone("1380013800a").is_err(), "含字母应被拒");
        assert!(validate_phone("").is_err());
    }

    #[test]
    fn device_token_roundtrip_and_uniqueness() {
        let t1 = gen_device_token("u_abc", "k_abc");
        let t2 = gen_device_token("u_abc", "k_abc");
        assert_ne!(t1, t2, "两次生成的令牌必须不同");
        assert_eq!(parse_device_token(&t1), Some(("u_abc", "k_abc")));
        assert!(t1.starts_with("u_abc.k_abc."));
        // 32 字节 → 64 个 hex 字符
        let rand_part = t1.rsplit_once('.').unwrap().1;
        assert_eq!(rand_part.len(), 64);
    }

    /// 不同账号下的同名 kid_id 必须解析出各自的 user_id，
    /// 否则跨账号的设备令牌会互相能定位到对方的记录。
    #[test]
    fn device_token_keeps_tenant_separation() {
        let a = gen_device_token("u_a", "k_1");
        let b = gen_device_token("u_b", "k_1");
        assert_eq!(parse_device_token(&a).unwrap().0, "u_a");
        assert_eq!(parse_device_token(&b).unwrap().0, "u_b");
        assert_ne!(parse_device_token(&a), parse_device_token(&b));
    }

    #[test]
    fn parse_device_token_handles_garbage() {
        assert_eq!(parse_device_token("nodot"), None);
        assert_eq!(parse_device_token("only.two"), None);
        assert_eq!(parse_device_token(".abc.secret"), None);
        assert_eq!(parse_device_token("u_1..secret"), None);
        assert_eq!(parse_device_token(""), None);
    }

    #[test]
    fn token_hash_is_stable_and_hides_token() {
        let t = gen_device_token("u_1", "k_1");
        let h = hash_token(&t);
        assert_eq!(h, hash_token(&t), "同一令牌哈希必须稳定");
        assert!(!h.contains(&t), "哈希串不得包含原令牌");
        assert_eq!(h.len(), 64);
        assert_ne!(h, hash_token(&gen_device_token("u_1", "k_1")));
    }

    #[test]
    fn word_validation() {
        assert_eq!(validate_word("  繁星 ").unwrap(), "繁星", "应 trim");
        assert!(validate_word("").is_err());
        assert!(validate_word("   ").is_err());
        assert!(validate_word("繁星\n繁华").is_err(), "控制字符应被拒");
        assert!(validate_word(&"字".repeat(31)).is_err(), "超长应被拒");
        assert!(validate_word(&"字".repeat(30)).is_ok());
    }
}
