//! SQS 消费运行时（对应文章 FR-1.4 的 `operon::sqs::run_sqs_with_setup`）。
//!
//! 处理 SQS 相关的脏活累活：消息接收、反序列化前处理、可见性超时、错误重试约定。
//! 业务只需实现 `SqsHandler` trait。错误时**不删除消息**，交给 SQS 可见性超时自然重试。

use aws_sdk_sqs::Client as SqsClient;

use crate::auth::{resolve_jwt_seed, Jwt};
use crate::config::ConfigLoader;
use crate::AppState;

/// 消息处理器：`process` 返回 Err 时消息不会被删除（可见性超时后重试，SQS 内置最多 N 次）。
#[async_trait::async_trait]
pub trait SqsHandler: Send + Sync + 'static {
    async fn process(&self, body: &str) -> Result<(), String>;
}

/// SQS Worker 入口：初始化（日志/配置/密钥）→ 构建 handler → 长轮询消费。
///
/// `queue_url` 为 SQS 队列 URL；`setup` 与 `run_with_setup` 一致，冷启动构建一次 handler。
pub async fn run_sqs_with_setup<F, Fut, H>(queue_url: &str, setup: F) -> anyhow::Result<()>
where
    F: FnOnce(AppState) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<H>>,
    H: SqsHandler,
{
    crate::server::init_tracing();

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let config = ConfigLoader::load(&aws_config).await?;
    let seed = resolve_jwt_seed(&config)?;
    let jwt = Jwt::from_seed(&seed)?;
    let state = AppState {
        config,
        jwt,
        aws_config: aws_config.clone(),
    };
    let handler = setup(state).await?;

    let sqs = SqsClient::new(&aws_config);
    tracing::info!(queue_url, "SQS worker started");

    loop {
        let out = sqs
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(10)
            .wait_time_seconds(20) // 长轮询，省请求费
            .send()
            .await?;
        for msg in out.messages() {
            let handle = msg.receipt_handle().unwrap_or_default();
            let body = msg.body().unwrap_or_default();
            match handler.process(body).await {
                Ok(()) => {
                    // 处理成功 → 删除消息
                    sqs.delete_message()
                        .queue_url(queue_url)
                        .receipt_handle(handle)
                        .send()
                        .await?;
                    tracing::info!("message processed and deleted");
                }
                Err(e) => {
                    // 处理失败 → 不删除，可见性超时后自动重试（幂等由业务保证）
                    tracing::warn!(error = %e, "handler error, leaving message for retry");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;

    #[async_trait::async_trait]
    impl SqsHandler for EchoHandler {
        async fn process(&self, body: &str) -> Result<(), String> {
            if body.is_empty() {
                Err("empty body".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn handler_ok_and_err() {
        let h = EchoHandler;
        assert!(h.process("hello").await.is_ok());
        assert!(h.process("").await.is_err());
    }
}
