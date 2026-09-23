//! `/v1/messages`：网关主路径。
//!
//! 流程：鉴权 → 解析 → 并发闸门 → 提取会话/能力需求 → 选候选 → WebSearch 预处理
//! → 转发 → （如果需要）接上搜索循环 → 回程。错误一律是 Anthropic 信封。

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
use crate::websearch::{self, backends as search_backends, search_loop::SearchState};

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = authenticate(&state, &headers).await?;
    let started = Instant::now();

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

    // ---- WebSearch 预处理（D13）----
    let settings = state.pool.load().settings.clone();
    let (mut upstream_body, search_plan) = if settings.search.enabled {
        websearch::history::prepare_request(&parsed, settings.search.max_uses)
    } else {
        (parsed.clone(), Default::default())
    };
    let mut search_state = SearchState {
        plan: search_plan,
        backends: Vec::new(),
        settings: settings.clone(),
        request_id: request_id.clone(),
    };
    let mut pre_search: Option<(String, Vec<search_backends::SearchItem>, String)> = None;
    if let Some(query) = search_state.plan.fast_path_query.clone() {
        // 快速路径：Claude Code 的固定形态搜索请求，先自己搜，省掉一次上游调用。
        search_state.backends = load_search_backends(&state).await;
        match websearch::search_loop::run_search(Arc::clone(&state), &search_state, &query).await {
            Ok((items, text)) => {
                websearch::history::rewrite_for_fast_path(&mut upstream_body, &query, &text);
                pre_search = Some((query, items, text));
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request_id,
                    "快速路径搜索失败，回退到让模型自己调用工具：{}",
                    truncate_error(&e)
                );
            }
        }
    }

    tracing::debug!(
        request_id = %request_id,
        key = %key.name,
        tier = plan.tier,
        candidates = plan.candidates.len(),
        est_tokens = needs.est_tokens,
        vision = needs.vision,
        web_search = search_state.plan.has_tool,
        fast_path = pre_search.is_some(),
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
        body: upstream_body,
        streaming,
        plan,
        client_anthropic_version,
        requested_model,
        show_thinking,
        log_template: Some(log_template.clone()),
    };
    let outcome = forward::run(Arc::clone(&state), ctx.clone()).await;
    let elapsed_ms = started.elapsed().as_millis() as i64;

    match outcome {
        Outcome::NonStreaming { mut body, meta } => {
            // 非流式路径里的 WebSearch：把模型的工具调用换成服务端工具块
            // （不续写；非流式本来就不该承载搜索请求）。
            if search_state.plan.has_tool {
                if let Some(query) = first_web_search_query(&body) {
                    if search_state.backends.is_empty() {
                        search_state.backends = load_search_backends(&state).await;
                    }
                    match websearch::search_loop::run_search(
                        Arc::clone(&state),
                        &search_state,
                        &query,
                    )
                    .await
                    {
                        Ok((items, _)) => {
                            websearch::search_loop::replace_tool_use_in_message(
                                &mut body, &query, &items,
                            );
                        }
                        Err(e) => tracing::warn!(
                            request_id = %request_id,
                            "非流式搜索失败：{}",
                            truncate_error(&e)
                        ),
                    }
                }
            }
            write_request_log(&state, &log_template, &meta, 200, None, elapsed_ms);
            Ok(Json(body).into_response())
        }
        Outcome::Streaming { stream, meta } => {
            let stream = if search_state.plan.has_tool {
                if search_state.backends.is_empty() {
                    search_state.backends = load_search_backends(&state).await;
                }
                websearch::search_loop::wrap(
                    Arc::clone(&state),
                    ctx.clone(),
                    search_state,
                    stream,
                    meta,
                    pre_search,
                )
            } else {
                stream
            };
            // 流式请求的明细由管线在收尾/断开时写（meta.log_written 已置位），
            // meta 在上面按需交给搜索循环了。
            Ok(sse_response(stream))
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

fn sse_response(
    stream: impl futures::Stream<Item = std::io::Result<Bytes>> + Send + 'static,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream; charset=utf-8")
        .header("cache-control", "no-cache")
        // 双保险：万一反代开了缓冲，这个头会让它别缓冲。
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .expect("构造流式响应失败")
}

async fn load_search_backends(state: &Arc<AppState>) -> Vec<search_backends::SearchBackend> {
    state
        .db
        .read(search_backends::load_backends)
        .await
        .unwrap_or_default()
}

/// 响应里第一个 `web_search` 工具调用的查询词。
fn first_web_search_query(value: &Value) -> Option<String> {
    value
        .get("content")?
        .as_array()?
        .iter()
        .find(|b| {
            b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && b.get("name").and_then(|n| n.as_str()) == Some(websearch::TOOL_NAME)
        })
        .and_then(|b| {
            b.get("input")
                .and_then(|i| i.get("query"))
                .and_then(|q| q.as_str())
                .map(|q| q.to_string())
        })
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
