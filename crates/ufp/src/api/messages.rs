//! `/v1/messages`：网关主路径。
//!
//! 流程：鉴权 → 解析 → 并发闸门 → 提取会话/能力需求 → 选候选 → 转发 → 回程。
//! 错误一律是 Anthropic 信封（见 `api/error.rs`），不把上游格式泄漏给客户端。

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

use super::auth::authenticate;
use super::error::ApiError;
use super::AppState;
use crate::forward::{self, ForwardCtx, Meta, Outcome};
use crate::pipeline::{self, truncate_error};
use crate::router::{extract_session_id, select};
use crate::store::{RequestLogRow, Write};
use crate::tokens;

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = authenticate(&state, &headers).await?;

    if body.len() > state.cfg.max_body_bytes {
        return Err(ApiError::too_large(format!(
            "请求体超过网关上限（{} 字节）",
            state.cfg.max_body_bytes
        )));
    }
    let parsed: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid_request(format!("请求体不是合法 JSON：{e}")))?;
    if parsed.get("messages").and_then(|m| m.as_array()).is_none() {
        return Err(ApiError::invalid_request("messages: field required"));
    }

    // 并发闸门：转换要解析完整 JSON，每个在途请求都占着内存，不能无限接。
    let _permit = match Arc::clone(&state.inflight).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => return Err(ApiError::overloaded("网关并发已满，请稍后重试")),
    };

    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let started = Instant::now();
    let session_id = extract_session_id(&headers, &parsed);
    let requested_model = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let streaming = parsed
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let show_thinking = pipeline::wants_thinking(&parsed);
    let needs = analyze(&parsed);
    let client_anthropic_version = headers
        .get("anthropic-version")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());

    let plan = {
        let pool = state.pool.load();
        select::plan(
            &pool,
            &state.breakers,
            &state.cooldowns,
            &state.sessions,
            session_id.as_deref(),
            needs,
        )
    }
    .map_err(select_error)?;

    tracing::debug!(
        request_id = %request_id,
        key = %key.name,
        tier = plan.tier,
        candidates = plan.candidates.len(),
        est_tokens = needs.est_tokens,
        vision = needs.vision,
        pdf = needs.pdf,
        "开始转发"
    );

    let log_template = RequestLogRow {
        request_id: request_id.clone(),
        downstream_key_id: Some(key.id),
        session_id: session_id.clone(),
        requested_model: requested_model.clone(),
        streaming,
        created_ms: chrono::Utc::now().timestamp_millis(),
        ..Default::default()
    };

    let ctx = ForwardCtx {
        request_id: request_id.clone(),
        session_id,
        downstream_key_id: key.id,
        body: parsed,
        streaming,
        plan,
        client_anthropic_version,
        requested_model,
        show_thinking,
        log_template: Some(log_template.clone()),
    };
    let outcome = forward::run(Arc::clone(&state), ctx).await;
    let elapsed_ms = started.elapsed().as_millis() as i64;

    match outcome {
        Outcome::NonStreaming { body, meta } => {
            write_request_log(&state, &log_template, &meta, 200, None, elapsed_ms);
            Ok(Json(body).into_response())
        }
        Outcome::Streaming { stream, meta } => {
            // 流式请求的明细由管线在收尾/断开时写（meta.log_written）。
            let _ = &meta;
            let response = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream; charset=utf-8")
                .header("cache-control", "no-cache")
                // 双保险：万一反代开了缓冲，这个头会让它别缓冲。
                .header("x-accel-buffering", "no")
                .body(Body::from_stream(stream))
                .expect("构造流式响应失败");
            Ok(response)
        }
        Outcome::Failed { error, meta } => {
            write_request_log(
                &state,
                &log_template,
                &meta,
                error.status.as_u16(),
                Some((error.kind.to_string(), error.message.clone())),
                elapsed_ms,
            );
            Err(error)
        }
    }
}

/// 把选择失败翻译成客户端能看懂的错误。
fn select_error(e: select::SelectError) -> ApiError {
    match e {
        select::SelectError::NoEntries => ApiError::overloaded("网关还没有配置任何上游条目"),
        select::SelectError::TooLong => ApiError::prompt_too_long(),
        select::SelectError::NoCapability(what) => {
            ApiError::invalid_request(format!("请求包含 {what}，但当前池里没有支持它的模型"))
        }
        select::SelectError::AllUnavailable => {
            ApiError::overloaded("所有上游条目都在冷却或熔断中，请稍后重试")
        }
    }
}

/// 分析请求：本地 token 估算 + 是否含图片/文档。
fn analyze(body: &Value) -> select::RequestNeeds {
    let mut vision = false;
    let mut pdf = false;
    walk_content(body.get("messages"), &mut vision, &mut pdf);
    select::RequestNeeds {
        est_tokens: tokens::estimate_request(body),
        vision,
        pdf,
    }
}

fn walk_content(content: Option<&Value>, vision: &mut bool, pdf: &mut bool) {
    let Some(content) = content else {
        return;
    };
    match content {
        Value::Array(items) => {
            for item in items {
                walk_content(Some(item), vision, pdf);
            }
        }
        Value::Object(map) => {
            // 消息对象：看 content 字段
            if let Some(inner) = map.get("content") {
                walk_content(Some(inner), vision, pdf);
            }
            match map.get("type").and_then(|t| t.as_str()) {
                Some("image") => *vision = true,
                Some("document") => *pdf = true,
                _ => {}
            }
        }
        _ => {}
    }
}

/// 写请求明细（非流式与失败路径；流式路径由管线负责）。
fn write_request_log(
    state: &Arc<AppState>,
    template: &RequestLogRow,
    meta: &Meta,
    status: u16,
    error: Option<(String, String)>,
    elapsed_ms: i64,
) {
    let mut row = template.clone();
    row.upstream_model = meta.upstream_model.clone();
    row.channel_id = meta.channel_id;
    row.key_id = meta.key_id;
    row.http_status = status as i64;
    row.stop_reason = meta.stop_reason.clone();
    row.input_tokens = meta.usage.input_tokens as i64;
    row.output_tokens = meta.usage.output_tokens as i64;
    row.cache_read_tokens = meta.usage.cache_read_tokens as i64;
    row.cache_creation_tokens = meta.usage.cache_creation_tokens as i64;
    row.search_requests = meta.usage.web_search_requests as i64;
    row.attempts = meta.attempts as i64;
    row.first_content_ms = meta.first_content_ms;
    row.total_ms = elapsed_ms;
    if let Some((kind, message)) = error {
        row.error_type = Some(kind);
        row.error_message = Some(truncate_error(&message));
    }
    state.db.write(Write::RequestLog(Box::new(row)));
}
