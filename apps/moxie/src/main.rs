//! moxie —— 小学生默写工具后端（单 Lambda 打天下）。
//!
//! 家长在手机上把当天要默写的词语下发到孩子的平板，平板按顺序逐个报词，
//! 孩子写完一个点一下屏幕再报下一个。
//!
//! 模块结构：
//! - `main.rs`     入口：run_with_setup + Router 注册
//! - `models.rs`   模型层：DynamoDB 实体 + API 请求/响应
//! - `handlers.rs` 路由层：认证、多租户校验、下发编排
//! - `auth.rs`     账号体系：argon2 密码哈希、设备令牌
//! - `tts.rs`      Polly 语音合成：拼音 SSML + 内容寻址
//! - `ocr.rs`      拍照识别词表：Bedrock → GLM-4V 降级
//!
//! 路由：
//! - `GET    /health`                     健康检查
//! - `POST   /api/auth/register|login`    注册 / 登录
//! - `GET    /api/kids`                   孩子列表（家长）
//! - `POST   /api/kids`                   新建孩子（家长）
//! - `POST   /api/kids/{id}/bind-code`    生成绑定码（家长）
//! - `DELETE /api/kids/{id}/device`       解绑设备（家长）
//! - `GET    /api/kids/{id}/words`        取词表含汉字（家长）
//! - `POST   /api/kids/{id}/push`         下发词表 + 生成音频（家长）
//! - `POST   /api/preview`                试听单个词（家长）
//! - `POST   /api/ocr`                    拍照识别（家长）
//! - `POST   /api/kid/bind`               绑定码换设备令牌（公开）
//! - `GET    /api/kid/current`            当前词表，**不含汉字**（孩子）
//! - `GET    /api/kid/answers`            答案（孩子）

mod auth;
mod handlers;
mod models;
mod ocr;
mod tts;

use axum::extract::Extension;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use operon_core::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run_with_setup(|state| async move {
        let router = Router::new()
            // `/health` 供直连 Function URL 用；`/api/health` 才是走 CloudFront 能到达的路径
            // （CloudFront 只把 /api/* 路由到 Lambda，其余落到 S3 前端桶 → 403）
            .route(
                "/health",
                get(|| async {
                    Json(serde_json::json!({ "status": "ok", "service": "operon-moxie" }))
                }),
            )
            .route(
                "/api/health",
                get(|| async {
                    Json(serde_json::json!({ "status": "ok", "service": "operon-moxie" }))
                }),
            )
            // ---- 认证 ----
            .route("/api/auth/register", post(handlers::register))
            .route("/api/auth/login", post(handlers::login))
            // ---- 家长端 ----
            .route(
                "/api/kids",
                get(handlers::list_kids).post(handlers::create_kid),
            )
            .route(
                "/api/kids/{kid_id}/bind-code",
                post(handlers::create_bind_code),
            )
            .route(
                "/api/kids/{kid_id}/device",
                delete(handlers::unbind_device),
            )
            .route("/api/kids/{kid_id}/words", get(handlers::get_words))
            .route("/api/kids/{kid_id}/push", post(handlers::push_words))
            .route("/api/preview", post(handlers::preview))
            .route("/api/ocr", post(handlers::ocr_words))
            // ---- 孩子端 ----
            .route("/api/kid/bind", post(handlers::kid_bind))
            .route("/api/kid/current", get(handlers::kid_current))
            .route("/api/kid/answers", get(handlers::kid_answers));

        // axum 0.8：AppState 用 Extension 注入（serve 只接受 Router<()>）
        Ok(router.layer(Extension(state)).with_operon_defaults())
    })
    .await
}
