//! 转发：候选循环、错误分类、熔断/冷却记账（D7/D10/D11）。
//!
//! 一次客户端请求按选择结果依次尝试候选；每次尝试的结果分成几类，各走各的账：
//!
//! | 结果 | 换候选 | 记熔断 | 记冷却 | 备注 |
//! |---|---|---|---|---|
//! | 2xx 且拿到内容 | 结束 | 成功 | | |
//! | 2xx 但空流 / 首内容超时 / 提交前流内错误 | 是 | 失败 | | 「免费模型先 200 再出错」的主战场 |
//! | 429 / 402 | 是 | | 是（按 Retry-After / retryDelay / 日配额） | 额度问题换 key 就好 |
//! | 401 / 403 | 是 | | | 禁用该 key，后台标红 |
//! | 5xx / 408 / 网络错误 | 是 | 失败 | | |
//! | 400/413/422（可矫正） | 先矫正重试，再换候选 | | | 不计熔断 |
//! | 上下文超长 | 是 | | | 不算故障，换上下文更大的条目 |
//! | 其他 4xx | 否 | | | 翻译成 Anthropic 错误返回 |

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use ufp_convert::usage::parser::TokenUsage;

use crate::api::error::ApiError;
use crate::api::AppState;
use crate::health::cooldown::cooldown_from_response;
use crate::pipeline::{truncate_error, Pipeline, PipelineCfg, Usage};
use crate::rectify::{apply_builtin, BuiltinRectifier};
use crate::router::select::Plan;
use crate::store::{AttemptLogRow, Candidate, ModelEcho, RequestLogRow, Settings, Write};
use crate::upstream::{self, AnthropicToolSchemaHints, BuildCtx};

/// 一次调用的上下文。
#[derive(Clone)]
pub struct ForwardCtx {
    pub request_id: String,
    pub session_id: Option<String>,
    pub downstream_key_id: i64,
    /// 客户端原始请求体。
    pub body: Value,
    pub streaming: bool,
    pub plan: Plan,
    pub client_anthropic_version: Option<String>,
    pub requested_model: String,
    /// 客户端是否要求显示思考。
    pub show_thinking: bool,
    /// 流式请求的明细模板（非流式由 handler 自己写）。
    pub log_template: Option<RequestLogRow>,
}

#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub attempts: u32,
    pub channel_id: Option<i64>,
    pub key_id: Option<i64>,
    pub upstream_model: String,
    pub first_content_ms: Option<i64>,
    pub usage: Usage,
    pub stop_reason: Option<String>,
    /// 流式请求的明细由管线负责写，handler 不要再写一次。
    pub log_written: bool,
    pub degraded_from_tier: Option<i32>,
}

pub enum Outcome {
    NonStreaming {
        body: Value,
        meta: Meta,
    },
    Streaming {
        stream: BoxStream<'static, std::io::Result<Bytes>>,
        meta: Meta,
    },
    Failed {
        error: ApiError,
        meta: Meta,
    },
}

/// 一次尝试所用的输入（矫正器会改写 `body`）。
struct AttemptInput {
    body: Value,
    streaming: bool,
    client_anthropic_version: Option<String>,
    show_thinking: bool,
    hints: Arc<AnthropicToolSchemaHints>,
    log_template: Option<RequestLogRow>,
}

/// 每次尝试的结果分类。
enum AttemptResult {
    Done {
        body: Value,
        usage: Usage,
        stop_reason: Option<String>,
    },
    Committed {
        stream: BoxStream<'static, std::io::Result<Bytes>>,
        usage: Usage,
        stop_reason: Option<String>,
        first_content_ms: i64,
    },
    /// 上游给了 200，但没等到任何内容就结束了/超时了。
    /// `replay` 是「万一所有候选都没内容」时可以直接交付给客户端的空消息字节。
    Empty {
        detail: String,
        replay: Vec<Bytes>,
        channel_id: i64,
        key_id: i64,
        upstream_model: String,
    },
    Retryable {
        kind: &'static str,
        message: String,
        status: Option<u16>,
    },
    CoolKey {
        kind: &'static str,
        message: String,
        status: u16,
        /// 上游给的冷却信号（Retry-After / retryDelay / 日配额）。
        hint: Option<(i64, String)>,
    },
    DisableKey {
        kind: &'static str,
        message: String,
        status: u16,
    },
    Rectifiable {
        message: String,
        status: u16,
    },
    TooLong {
        message: String,
    },
    Fatal {
        error: ApiError,
    },
}

pub async fn run(state: Arc<AppState>, ctx: ForwardCtx) -> Outcome {
    let started = Instant::now();
    let (settings, candidates) = {
        let pool = state.pool.load();
        (pool.settings.clone(), ctx.plan.candidates.clone())
    };
    let breaker_cfg = settings.breaker.clone();
    let hints = Arc::new(upstream::tool_schema_hints(&ctx.body));
    let mut meta = Meta {
        degraded_from_tier: ctx.plan.degraded_from_tier,
        ..Default::default()
    };
    let mut last_error: Option<ApiError> = None;
    let mut empty_fallback: Option<(Vec<Bytes>, i64, i64, String)> = None;

    for cand in candidates.iter() {
        if meta.attempts >= settings.max_attempts {
            break;
        }
        if started.elapsed() > Duration::from_millis(settings.precommit_budget_ms) {
            last_error = Some(ApiError::overloaded("上游尝试总时长超过预算"));
            break;
        }
        if state
            .cooldowns
            .check(cand.key.id, &cand.entry.upstream_model)
            .is_some()
        {
            continue;
        }
        if !state
            .breakers
            .allow(cand.channel.id, &cand.entry.upstream_model, &breaker_cfg)
        {
            continue;
        }

        meta.attempts += 1;
        let mut input = AttemptInput {
            body: ctx.body.clone(),
            streaming: ctx.streaming,
            client_anthropic_version: ctx.client_anthropic_version.clone(),
            show_thinking: ctx.show_thinking,
            hints: Arc::clone(&hints),
            log_template: ctx.log_template.clone(),
        };
        let attempt_started = Instant::now();
        let mut tried: Vec<BuiltinRectifier> = Vec::new();
        let result = loop {
            let r = try_once(&state, cand, &input).await;
            if let AttemptResult::Rectifiable { message, .. } = &r {
                // 请求本身有问题：先在同候选上矫正重试（最多几个确定性矫正器）。
                if let Some(rectifier) =
                    apply_builtin(&mut input.body, message, &settings.rectifier, &tried)
                {
                    tracing::info!(
                        request_id = %ctx.request_id,
                        channel = %cand.channel.name,
                        rectifier = rectifier.as_str(),
                        "应用矫正器后重试同一候选：{}",
                        truncate_error(message)
                    );
                    tried.push(rectifier);
                    continue;
                }
            }
            break r;
        };
        let elapsed_ms = attempt_started.elapsed().as_millis() as i64;
        log_attempt(&state, &ctx, cand, &result, elapsed_ms, meta.attempts);

        match result {
            AttemptResult::Done {
                body,
                usage,
                stop_reason,
            } => {
                state.breakers.record(
                    cand.channel.id,
                    &cand.entry.upstream_model,
                    true,
                    &breaker_cfg,
                );
                record_session(&state, &ctx, cand);
                meta.channel_id = Some(cand.channel.id);
                meta.key_id = Some(cand.key.id);
                meta.upstream_model = cand.entry.upstream_model.clone();
                meta.usage = usage;
                meta.stop_reason = stop_reason;
                return Outcome::NonStreaming { body, meta };
            }
            AttemptResult::Committed {
                stream,
                usage,
                stop_reason,
                first_content_ms,
            } => {
                state.breakers.record(
                    cand.channel.id,
                    &cand.entry.upstream_model,
                    true,
                    &breaker_cfg,
                );
                record_session(&state, &ctx, cand);
                meta.channel_id = Some(cand.channel.id);
                meta.key_id = Some(cand.key.id);
                meta.upstream_model = cand.entry.upstream_model.clone();
                meta.usage = usage;
                meta.stop_reason = stop_reason;
                meta.first_content_ms = Some(first_content_ms);
                meta.log_written = true;
                return Outcome::Streaming { stream, meta };
            }
            AttemptResult::Empty {
                detail,
                replay,
                channel_id,
                key_id,
                upstream_model,
            } => {
                state.breakers.record(
                    cand.channel.id,
                    &cand.entry.upstream_model,
                    false,
                    &breaker_cfg,
                );
                tracing::warn!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    model = %cand.entry.upstream_model,
                    detail = %detail,
                    "上游返回 200 但没有内容，换候选"
                );
                last_error = Some(ApiError::overloaded(format!(
                    "上游 {} 未返回内容：{detail}",
                    cand.channel.name
                )));
                if !replay.is_empty() {
                    empty_fallback = Some((replay, channel_id, key_id, upstream_model));
                }
            }
            AttemptResult::Retryable {
                kind,
                message,
                status,
            } => {
                state.breakers.record(
                    cand.channel.id,
                    &cand.entry.upstream_model,
                    false,
                    &breaker_cfg,
                );
                tracing::warn!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    model = %cand.entry.upstream_model,
                    kind,
                    status = status.unwrap_or(0),
                    "上游失败，换候选：{}",
                    truncate_error(&message)
                );
                last_error = Some(translate_upstream_error(status, &message, kind));
            }
            AttemptResult::CoolKey {
                kind,
                message,
                status,
                hint,
            } => {
                let (until, reason) = match hint {
                    Some(h) => h,
                    None => (
                        state.cooldowns.set_backoff(
                            &state.db,
                            cand.key.id,
                            &cand.entry.upstream_model,
                            "额度类错误（无上游信号）",
                        ),
                        "额度类错误（指数退避）".to_string(),
                    ),
                };
                state.cooldowns.set(
                    &state.db,
                    cand.key.id,
                    &cand.entry.upstream_model,
                    until,
                    &reason,
                );
                tracing::warn!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    key = %cand.key.label,
                    kind,
                    reason = %reason,
                    "额度类错误，冷却该 key×模型"
                );
                last_error = Some(translate_upstream_error(Some(status), &message, kind));
            }
            AttemptResult::DisableKey {
                kind,
                message,
                status,
            } => {
                state.disable_upstream_key(cand.key.id, &format!("{kind}：{message}"));
                tracing::warn!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    key = %cand.key.label,
                    kind,
                    status,
                    "上游拒绝该 key，已禁用"
                );
                last_error = Some(ApiError::api(format!(
                    "上游 {} 拒绝了网关的密钥（{kind}）",
                    cand.channel.name
                )));
            }
            AttemptResult::Rectifiable { message, status } => {
                tracing::warn!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    model = %cand.entry.upstream_model,
                    status,
                    "矫正后仍被拒，换候选：{}",
                    truncate_error(&message)
                );
                last_error = Some(ApiError::invalid_request(format!(
                    "上游 {} 拒绝了该请求：{message}",
                    cand.channel.name
                )));
            }
            AttemptResult::TooLong { message } => {
                tracing::info!(
                    request_id = %ctx.request_id,
                    channel = %cand.channel.name,
                    model = %cand.entry.upstream_model,
                    "上游报上下文超长，换上下文更大的条目：{}",
                    truncate_error(&message)
                );
                last_error = Some(ApiError::prompt_too_long());
            }
            AttemptResult::Fatal { error } => {
                state
                    .breakers
                    .release(cand.channel.id, &cand.entry.upstream_model);
                return Outcome::Failed { error, meta };
            }
        }
    }

    if let Some((replay, channel_id, key_id, upstream_model)) = empty_fallback {
        // 所有候选都没内容：把最后一次的空消息原样交付（合法但空的响应），
        // 让 Claude Code 自己决定怎么处理，比回一个 529 更友好。
        meta.channel_id = Some(channel_id);
        meta.key_id = Some(key_id);
        meta.upstream_model = upstream_model;
        meta.log_written = true;
        let stream = futures::stream::iter(replay.into_iter().map(Ok)).boxed();
        return Outcome::Streaming { stream, meta };
    }

    let error = last_error.unwrap_or_else(|| ApiError::overloaded("没有可用的上游候选"));
    Outcome::Failed { error, meta }
}

fn record_session(state: &Arc<AppState>, ctx: &ForwardCtx, cand: &Candidate) {
    let Some(sid) = ctx.session_id.as_deref() else {
        return;
    };
    state.sessions.set(
        &state.db,
        sid,
        cand.channel.id,
        cand.key.id,
        cand.entry.id,
        &cand.entry.upstream_model,
    );
}

/// 真正发一次请求（不矫正）。
async fn try_once(state: &Arc<AppState>, cand: &Candidate, input: &AttemptInput) -> AttemptResult {
    let build_ctx = BuildCtx {
        client_body: &input.body,
        client_anthropic_version: input.client_anthropic_version.as_deref(),
        stream: input.streaming,
    };
    let req = match upstream::build(cand, &build_ctx) {
        Ok(r) => r,
        Err(e) => {
            return AttemptResult::Fatal {
                error: ApiError::api(format!("构造上游请求失败：{e}")),
            }
        }
    };
    let timeout = Duration::from_millis(state.pool.load().settings.first_content_timeout_ms);
    let resp = match upstream::send(&state.client, &req, timeout, input.streaming).await {
        Ok(r) => r,
        Err(e) => {
            return AttemptResult::Retryable {
                kind: e.kind(),
                message: e.to_string(),
                status: None,
            };
        }
    };
    if !resp.status().is_success() {
        // 冷却信号必须在消费响应体之前从响应头里取。
        let headers = resp.headers().clone();
        let (status, body_json, text) = upstream::error_body(resp).await;
        let hint = cooldown_from_response(status, &headers, Some(&body_json));
        return classify_error(status, body_json, text, hint);
    }
    if input.streaming {
        attempt_stream(state, cand, &req, resp, input).await
    } else {
        attempt_nonstreaming(state, cand, &req, resp, input).await
    }
}

async fn attempt_nonstreaming(
    state: &Arc<AppState>,
    cand: &Candidate,
    req: &upstream::UpstreamRequest,
    resp: reqwest::Response,
    input: &AttemptInput,
) -> AttemptResult {
    let body_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return AttemptResult::Retryable {
                kind: "body_read",
                message: e.to_string(),
                status: None,
            }
        }
    };
    let raw: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return AttemptResult::Retryable {
                kind: "bad_json",
                message: format!("上游响应不是 JSON：{e}"),
                status: None,
            }
        }
    };
    let hints = match cand.channel.protocol {
        crate::store::Protocol::Gemini => Some(input.hints.as_ref()),
        _ => None,
    };
    let mut converted = match upstream::response_to_anthropic(cand.channel.protocol, raw, hints) {
        Ok(v) => v,
        Err(e) => {
            return AttemptResult::Retryable {
                kind: "transform",
                message: format!("响应转换失败：{e}"),
                status: None,
            }
        }
    };
    let settings = state.pool.load().settings.clone();
    let echo = echo_model(&settings, cand, &req.upstream_model, input);
    if let Some(model) = converted.get_mut("model") {
        *model = json!(echo);
    }
    let usage = TokenUsage::from_claude_response(&converted)
        .map(|t| usage_from_token_usage(&t))
        .unwrap_or_default();
    let stop_reason = converted
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    AttemptResult::Done {
        body: converted,
        usage,
        stop_reason,
    }
}

async fn attempt_stream(
    state: &Arc<AppState>,
    cand: &Candidate,
    req: &upstream::UpstreamRequest,
    resp: reqwest::Response,
    input: &AttemptInput,
) -> AttemptResult {
    let settings = state.pool.load().settings.clone();
    let echo = echo_model(&settings, cand, &req.upstream_model, input);
    let mut pipeline = Pipeline::new(PipelineCfg {
        echo_model: echo,
        show_thinking: input.show_thinking,
    });
    if let Some(mut row) = input.log_template.clone() {
        row.upstream_model = cand.entry.upstream_model.clone();
        row.channel_id = Some(cand.channel.id);
        row.key_id = Some(cand.key.id);
        row.attempts = 1;
        pipeline.attach_log(Arc::clone(&state.db), row, 200);
    }
    let hints = match cand.channel.protocol {
        crate::store::Protocol::Gemini => Some(input.hints.as_ref().clone()),
        _ => None,
    };
    let mut stream = upstream::to_anthropic_stream(cand.channel.protocol, resp, hints);

    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(settings.first_content_timeout_ms);
    let started = Instant::now();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return empty_result(&mut pipeline, "等待首个内容增量超时", cand);
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Err(_) => return empty_result(&mut pipeline, "等待首个内容增量超时", cand),
            Ok(None) => return empty_result(&mut pipeline, "上游流结束但没有任何内容", cand),
            Ok(Some(Err(e))) => {
                return AttemptResult::Retryable {
                    kind: "stream_read",
                    message: e.to_string(),
                    status: None,
                }
            }
            Ok(Some(Ok(bytes))) => {
                if let Err(msg) = pipeline.feed(&bytes) {
                    return AttemptResult::Retryable {
                        kind: "stream_error",
                        message: msg,
                        status: None,
                    };
                }
                if let Some(msg) = pipeline.upstream_error() {
                    return AttemptResult::Retryable {
                        kind: "stream_error",
                        message: msg.to_string(),
                        status: None,
                    };
                }
                if pipeline.committed() {
                    let first_content_ms = started.elapsed().as_millis() as i64;
                    let usage = pipeline.usage().clone();
                    let stop_reason = pipeline.stop_reason().map(|s| s.to_string());
                    let client_stream = pipeline.into_client_stream(
                        stream,
                        Duration::from_millis(settings.ping_interval_ms),
                        Duration::from_millis(settings.idle_timeout_ms),
                    );
                    return AttemptResult::Committed {
                        stream: client_stream,
                        usage,
                        stop_reason,
                        first_content_ms,
                    };
                }
            }
        }
    }
}

/// 构造「200 但没内容」的结果，并留下可以原样交付的空消息字节。
fn empty_result(pipeline: &mut Pipeline, detail: &str, cand: &Candidate) -> AttemptResult {
    let replay = pipeline.into_empty_replay();
    pipeline.finalize_for_log(Some(("empty_stream", detail)));
    AttemptResult::Empty {
        detail: detail.to_string(),
        replay,
        channel_id: cand.channel.id,
        key_id: cand.key.id,
        upstream_model: cand.entry.upstream_model.clone(),
    }
}

/// 响应/流里的模型名（按设置）。
fn echo_model(
    settings: &Settings,
    cand: &Candidate,
    _upstream_model: &str,
    input: &AttemptInput,
) -> String {
    match settings.model_echo {
        ModelEcho::Upstream => cand.entry.upstream_model.clone(),
        ModelEcho::Request => input
            .body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(&cand.entry.upstream_model)
            .to_string(),
        ModelEcho::Fixed => settings.public_model_id.clone(),
    }
}

pub fn usage_from_token_usage(t: &TokenUsage) -> Usage {
    Usage {
        input_tokens: t.input_tokens as u64,
        output_tokens: t.output_tokens as u64,
        cache_read_tokens: t.cache_read_tokens as u64,
        cache_creation_tokens: t.cache_creation_tokens as u64,
        web_search_requests: 0,
    }
}

/// 把上游错误翻译成 Anthropic 错误（不把上游格式泄漏给客户端）。
fn translate_upstream_error(status: Option<u16>, message: &str, kind: &str) -> ApiError {
    let message = truncate_error(message);
    match status {
        Some(400) => ApiError::invalid_request(format!("上游拒绝了请求（{kind}）：{message}")),
        Some(401) | Some(403) => {
            ApiError::authentication(format!("上游鉴权失败（{kind}）：{message}"))
        }
        Some(404) => ApiError::not_found(format!("上游未找到该模型或端点（{kind}）：{message}")),
        Some(413) => ApiError::too_large(format!("请求对上游来说过大（{kind}）：{message}")),
        Some(429) => ApiError::rate_limited(format!("上游都在限流（{kind}）：{message}")),
        Some(s) if (500..600).contains(&s) => {
            ApiError::api(format!("上游 {s} 错误（{kind}）：{message}"))
        }
        _ => ApiError::overloaded(format!("所有上游候选都失败了（{kind}）：{message}")),
    }
}

/// 错误分类（对应文件头的表格）。
fn classify_error(
    status: u16,
    body: Value,
    text: String,
    hint: Option<(i64, String)>,
) -> AttemptResult {
    let message = extract_error_message(&body).unwrap_or_else(|| truncate_error(&text));
    let lower = message.to_ascii_lowercase();

    if status == 400
        && (lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("maximum context")
            || lower.contains("too many tokens")
            || lower.contains("prompt is too long")
            || lower.contains("input is too long")
            || lower.contains("token limit")
            || lower.contains("input length"))
    {
        return AttemptResult::TooLong { message };
    }

    match status {
        401 | 403 => AttemptResult::DisableKey {
            kind: "auth",
            message,
            status,
        },
        429 | 402 => AttemptResult::CoolKey {
            kind: if status == 429 { "rate_limit" } else { "quota" },
            message,
            status,
            hint,
        },
        408 | 409 | 500..=599 => AttemptResult::Retryable {
            kind: "upstream_5xx",
            message,
            status: Some(status),
        },
        400 | 413 | 422 => AttemptResult::Rectifiable { message, status },
        _ => AttemptResult::Fatal {
            error: translate_upstream_error(Some(status), &message, "upstream"),
        },
    }
}

/// 从各种错误体里抽一条可读的消息（Anthropic / OpenAI / Gemini 的字段都认）。
fn extract_error_message(body: &Value) -> Option<String> {
    if let Some(m) = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
    {
        return Some(m.to_string());
    }
    if let Some(m) = body.get("message").and_then(|m| m.as_str()) {
        return Some(m.to_string());
    }
    if let Some(m) = body
        .get("error")
        .and_then(|e| e.get("detail"))
        .and_then(|m| m.as_str())
    {
        return Some(m.to_string());
    }
    None
}

/// 记一条尝试明细（免费池排查问题的关键：能看到每次换了谁、为什么换）。
fn log_attempt(
    state: &Arc<AppState>,
    ctx: &ForwardCtx,
    cand: &Candidate,
    result: &AttemptResult,
    elapsed_ms: i64,
    attempt_no: u32,
) {
    let (status, error_type, error_message, committed) = match result {
        AttemptResult::Done { .. } | AttemptResult::Committed { .. } => {
            (Some(200), None, None, true)
        }
        AttemptResult::Empty { detail, .. } => (
            Some(200),
            Some("empty_stream".to_string()),
            Some(detail.clone()),
            false,
        ),
        AttemptResult::Retryable {
            kind,
            message,
            status,
        } => (
            *status,
            Some((*kind).to_string()),
            Some(truncate_error(message)),
            false,
        ),
        AttemptResult::CoolKey {
            kind,
            message,
            status,
            ..
        } => (
            Some(*status),
            Some((*kind).to_string()),
            Some(truncate_error(message)),
            false,
        ),
        AttemptResult::DisableKey {
            kind,
            message,
            status,
        } => (
            Some(*status),
            Some((*kind).to_string()),
            Some(truncate_error(message)),
            false,
        ),
        AttemptResult::Rectifiable { message, status } => (
            Some(*status),
            Some("rectified".to_string()),
            Some(truncate_error(message)),
            false,
        ),
        AttemptResult::TooLong { message } => (
            Some(400),
            Some("context_too_long".to_string()),
            Some(truncate_error(message)),
            false,
        ),
        AttemptResult::Fatal { error } => (
            Some(error.status.as_u16()),
            Some(error.kind.to_string()),
            Some(truncate_error(&error.message)),
            false,
        ),
    };
    state.db.write(Write::AttemptLog(Box::new(AttemptLogRow {
        request_id: ctx.request_id.clone(),
        attempt_no: attempt_no as i64,
        channel_id: Some(cand.channel.id),
        key_id: Some(cand.key.id),
        upstream_model: cand.entry.upstream_model.clone(),
        protocol: cand.channel.protocol.as_str().to_string(),
        http_status: status.map(|s| s as i64),
        error_type,
        error_message,
        first_content_ms: None,
        total_ms: elapsed_ms,
        committed,
        created_ms: chrono::Utc::now().timestamp_millis(),
    })));
}
