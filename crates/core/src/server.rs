//! 运行时引导。
//!
//! 对应文章 `operon::run(router())` 一行启动的 API。
//! 本地开发与 Lambda 部署共用同一份代码：
//! - 本地：直接监听 `PORT`（默认 8080）；
//! - Lambda：通过 Web Adapter 把请求转发进这个端口，二进制完全一样。
//!
//! 启动顺序：初始化日志 → 加载配置（环境变量 + SSM 批量拉取）→
//! 解析 JWT 密钥 → 构建路由 → serve。

use axum::Router;

use crate::auth::{resolve_jwt_seed, Jwt};
use crate::config::ConfigLoader;
use crate::AppState;

/// 一行启动：`operon::run_with_setup(|state| async move { Ok(router(state)) }).await`
///
/// 注意：axum 0.8 的 serve 只接受 `Router<()>`，应用应把 AppState 通过
/// **`Extension(state)`** 注入（而非 `with_state`），handlers 用 `Extension<AppState>` 提取。
pub async fn run_with_setup<F, Fut>(setup: F) -> anyhow::Result<()>
where
    F: FnOnce(AppState) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Router>>,
{
    init_tracing();

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await;
    let config = ConfigLoader::load(&aws_config).await?;
    let seed = resolve_jwt_seed(&config)?;
    let jwt = Jwt::from_seed(&seed)?;
    let state = AppState {
        config,
        jwt,
        aws_config,
    };

    let router = setup(state).await?;

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "operon server listening");
    axum::serve(listener, router).await?;
    Ok(())
}

pub(crate) fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if std::env::var("RUST_LOG").is_ok() {
            EnvFilter::from_env("RUST_LOG")
        } else {
            EnvFilter::new("info")
        }
    });
    // Lambda 里结构化 JSON 日志（对应文章「对 AI 友好的调试闭环」），本地用可读格式。
    if std::env::var("AWS_LAMBDA_FUNCTION_NAME").is_ok() {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}
