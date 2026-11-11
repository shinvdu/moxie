//! 统一的 API 错误类型。
//!
//! 所有错误最终都映射为结构化的 JSON 响应，格式统一：
//! `{"error": {"code": "NOT_FOUND", "message": "..."}}`。
//! 对应文章 operon 框架的 `error_format()` 中间件设计。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// 任何 `anyhow::Error` 都能直接 `?` 冒泡成内部错误。
impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let (code, message) = match &self {
            Self::Unauthorized(m) => ("UNAUTHORIZED", m.clone()),
            Self::Forbidden(m) => ("FORBIDDEN", m.clone()),
            Self::NotFound(m) => ("NOT_FOUND", m.clone()),
            Self::BadRequest(m) => ("BAD_REQUEST", m.clone()),
            Self::Conflict(m) => ("CONFLICT", m.clone()),
            Self::Internal(m) => ("INTERNAL", m.clone()),
        };
        tracing::warn!(code, message = %message, "request failed");
        (
            status,
            Json(ErrorBody {
                error: ErrorDetail {
                    code: code.to_string(),
                    message,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn status_code_mapping() {
        assert_eq!(AppError::Unauthorized("x".into()).status(), StatusCode::UNAUTHORIZED);
        assert_eq!(AppError::Forbidden("x".into()).status(), StatusCode::FORBIDDEN);
        assert_eq!(AppError::NotFound("x".into()).status(), StatusCode::NOT_FOUND);
        assert_eq!(AppError::BadRequest("x".into()).status(), StatusCode::BAD_REQUEST);
        assert_eq!(AppError::Conflict("x".into()).status(), StatusCode::CONFLICT);
        assert_eq!(AppError::Internal("x".into()).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn json_error_format() {
        let resp = AppError::NotFound("用户不存在".into()).into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "NOT_FOUND");
        assert_eq!(json["error"]["message"], "用户不存在");
    }
}
