//! `/v1/models` 与 `/v1/messages/count_tokens`。
//!
//! 对外只暴露一个模型 id（`settings.public_model_id`）：Claude Code 请求里发什么
//! 模型名都不校验，全部进同一个池；但模型列表要能回答出来，计数要走本地估算
//! （cc-switch 是直接 404，Claude Code 能否容忍这个失败没实测过，不如给个值）。

use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use super::auth::authenticate;
use super::error::ApiError;
use super::AppState;
use crate::tokens;

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers).await?;
    let pool = state.pool.load();
    let id = pool.settings.public_model_id.clone();
    let created = chrono::Utc::now().timestamp();
    Ok(Json(json!({
        "type": "list",
        "data": [{
            "type": "model",
            "id": id,
            "display_name": format!("{id} (ufp 统一池)"),
            "created_at": created,
        }],
        "has_more": false,
        "first_id": id,
        "last_id": id,
    })))
}

pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers).await?;
    if body.get("messages").and_then(|m| m.as_array()).is_none() {
        return Err(ApiError::invalid_request("messages: field required"));
    }
    // 与 Anthropic 一致：只返回输入侧的计量。
    Ok(Json(
        json!({ "input_tokens": tokens::estimate_request(&body) }),
    ))
}
