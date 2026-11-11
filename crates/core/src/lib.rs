//! operon core —— 一人公司无服务器框架的运行时核心。
//!
//! 对应文章第三篇「打造低成本无服务器框架」：单 Lambda 打天下 +
//! axum Router 分发 + 默认中间件 + 统一错误 + JWT 认证 + 混合配置。

pub mod auth;
pub mod config;
pub mod error;
pub mod oidc;
pub mod server;
pub mod sqs;

pub use auth::{resolve_jwt_seed, unix_now, ApiKeyAuth, Jwt, JwtAuth, JwtClaims};
pub use config::{AppConfig, ConfigLoader};
pub use error::AppError;
pub use oidc::{OidcAuthHandler, OidcProviderConfig, OidcRouter, OidcUserInfo, TokenDelivery};
pub use server::run_with_setup;
pub use sqs::{run_sqs_with_setup, SqsHandler};

use axum::Router;

/// 应用状态：框架注入，业务代码在 handler 里取用。
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub jwt: Jwt,
    pub aws_config: aws_config::SdkConfig,
}

// 让 `Jwt` / `AppConfig` / `SdkConfig` 都可以通过 axum 的 State 提取器按需取出。
impl axum::extract::FromRef<AppState> for Jwt {
    fn from_ref(state: &AppState) -> Self {
        state.jwt.clone()
    }
}
impl axum::extract::FromRef<AppState> for AppConfig {
    fn from_ref(state: &AppState) -> Self {
        state.config.clone()
    }
}
impl axum::extract::FromRef<AppState> for aws_config::SdkConfig {
    fn from_ref(state: &AppState) -> Self {
        state.aws_config.clone()
    }
}

/// 默认中间件层：request-id + tracing + CORS。
/// 对应文章 `OLayer::new().request_id().tracing().cors(...)`。
pub trait OperonRouterExt {
    fn with_operon_defaults(self) -> Self;
}

impl<S> OperonRouterExt for Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    fn with_operon_defaults(self) -> Self {
        use axum::http::HeaderName;
        use tower_http::cors::CorsLayer;
        use tower_http::request_id::{MakeRequestUuid, SetRequestIdLayer};
        use tower_http::trace::TraceLayer;

        self.layer(TraceLayer::new_for_http())
            .layer(SetRequestIdLayer::new(
                HeaderName::from_static("x-request-id"),
                MakeRequestUuid,
            ))
            .layer(CorsLayer::permissive())
    }
}

/// 用户代码的一站式导入：`use operon_core::prelude::*;`
pub mod prelude {
    pub use crate::{
        AppConfig, AppError, AppState, ApiKeyAuth, Jwt, JwtAuth, JwtClaims,
        OidcAuthHandler, OidcProviderConfig, OidcRouter, OidcUserInfo,
        OperonRouterExt, SqsHandler, TokenDelivery, run_sqs_with_setup, run_with_setup,
    };
}
