//! Anthropic 形态的错误响应。
//!
//! 下游只认这一种错误信封：`{"type":"error","error":{"type":"…","message":"…"}}`。
//! 上游的错误（OpenAI/Gemini 各自的形状）在转发层就被翻译成这里的类型，
//! 避免把上游格式泄漏给 Claude Code。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    /// Anthropic 的 error.type 取值。
    pub kind: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found_error", message)
    }

    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
    }

    pub fn permission(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "permission_error", message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", message)
    }

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", message)
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "api_error", message)
    }

    /// 池子整体不可用（所有条目都在冷却或熔断）。
    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::from_u16(529).unwrap(),
            "overloaded_error",
            message,
        )
    }

    pub fn prompt_too_long() -> Self {
        Self::invalid_request(
            "prompt is too long: the model's context window is smaller than the request",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "type": "error",
            "error": { "type": self.kind, "message": self.message },
        });
        (self.status, axum::Json(body)).into_response()
    }
}
