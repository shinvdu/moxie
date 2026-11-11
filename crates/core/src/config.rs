//! 混合配置策略：环境变量 + SSM 参数存储。
//!
//! 对应文章 4.1 节的设计：
//! - 非敏感配置（项目名、环境、表前缀等）走环境变量注入；
//! - 敏感配置（JWT 密钥等）存在 SSM 参数存储，冷启动时**一次** `GetParametersByPath`
//!   批量拉取全部密钥，缓存在内存里，热请求零开销。
//!
//! 本地开发模式（未设置 `OPERON_SECRETS_PATH`）自动降级：密钥从
//! `OPERON_DEV_JWT_SEED` 环境变量读取，无需任何 AWS 依赖即可跑通整个框架。

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub project: String,
    pub environment: String,
    pub region: String,
    pub table_prefix: String,
    /// 从 SSM 批量拉取的密钥，key 为参数名最后一段（如 `jwt_seed`）。
    pub secrets: HashMap<String, String>,
    /// 是否本地开发模式（无 SSM）。
    pub dev_mode: bool,
}

impl AppConfig {
    /// 取一个密钥；SSM 里不存在时返回 None。
    pub fn secret(&self, key: &str) -> Option<&str> {
        self.secrets.get(key).map(|s| s.as_str())
    }
}

pub struct ConfigLoader;

impl ConfigLoader {
    /// 加载配置。SSM 参数路径形如 `/operon-example/dev/`，参数名取最后一段。
    pub async fn load(aws_config: &aws_config::SdkConfig) -> anyhow::Result<AppConfig> {
        let project = std::env::var("OPERON_PROJECT").unwrap_or_else(|_| "operon".into());
        let environment =
            std::env::var("OPERON_ENV").unwrap_or_else(|_| "dev".into());
        let region = std::env::var("AWS_REGION")
            .ok()
            .or_else(|| {
                aws_config
                    .region()
                    .map(|r| r.as_ref().to_string())
            })
            .unwrap_or_else(|| "us-west-2".into());
        let table_prefix =
            std::env::var("OPERON_TABLE_PREFIX").unwrap_or_else(|_| format!("{project}-{environment}-"));

        // 敏感配置：SSM 批量拉取（一次调用拿全部，冷启动缓存）
        let mut secrets = HashMap::new();
        let secrets_path = std::env::var("OPERON_SECRETS_PATH").ok();
        if let Some(path) = &secrets_path {
            let client = aws_sdk_ssm::Client::new(aws_config);
            let mut next_token: Option<String> = None;
            loop {
                let out = client
                    .get_parameters_by_path()
                    .path(path)
                    .recursive(true)
                    .with_decryption(true)
                    .set_next_token(next_token.clone())
                    .send()
                    .await?;
                for p in out.parameters() {
                    let key = p
                        .name()
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let value = p.value().unwrap_or("").to_string();
                    if !key.is_empty() {
                        secrets.insert(key, value);
                    }
                }
                next_token = out.next_token().map(|t| t.to_string());
                if next_token.is_none() {
                    break;
                }
            }
            tracing::info!(
                path = %path,
                count = secrets.len(),
                "loaded secrets from SSM (single GetParametersByPath)"
            );
        }

        Ok(AppConfig {
            project,
            environment,
            region,
            table_prefix,
            secrets,
            dev_mode: secrets_path.is_none(),
        })
    }
}
