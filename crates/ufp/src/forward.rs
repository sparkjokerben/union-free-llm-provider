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
use crate::router::select::Plan;
use crate::store::{
    AttemptLogRow, Candidate, ClientProfile, ModelEcho, RequestLogRow, Settings, Write,
};
use crate::upstream::{self, AnthropicToolSchemaHints, BuildCtx, OpencodeIds};

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
    /// 模仿 OpenCode 的候选用的会话身份（一次客户端请求里的各次尝试共用）。
    opencode: Option<OpencodeIds>,
    /// 整条客户端请求开始的时刻：流式明细的总耗时按它算。
    request_started: Instant,
    /// 本次是第几次尝试（写进明细，别再写死 1）。
    attempts: u32,
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
    // 只要候选里有模仿 OpenCode 的渠道，就先把这次请求的 OpenCode 身份算好（同一轮工具循环共用一个 request id）
    let opencode = candidates
        .iter()
        .any(|c| c.channel.client_profile == ClientProfile::OpenCode)
        .then(|| state.opencode_ids.ids(ctx.session_id.as_deref(), &ctx.body));
    let mut meta = Meta {
        degraded_from_tier: ctx.plan.degraded_from_tier,
        ..Default::default()
    };
    let mut last_error: Option<ApiError> = None;
    let mut rectifier = crate::rectify::RectifierState::new();
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
            opencode: opencode.clone(),
            request_started: started,
            attempts: meta.attempts,
        };
        // D14: 渠道勾了「思考开到最大」就打上强制标记（阶梯试出来的形式记在
        // 条目上）。网关强制开的思考一律回给客户端看。
        if apply_max_thinking(&mut input.body, cand) {
            input.show_thinking = true;
        }
        let attempt_started = Instant::now();
        rectifier.begin_candidate();
        let result = loop {
            let r = try_once(&state, cand, &input).await;
            if let AttemptResult::Rectifiable { message, status } = &r {
                // 请求本身有问题：先在同候选上矫正重试
                // （内置矫正器 → 已沉淀的规则 → LLM 在线分析）。
                let protocol = cand.channel.protocol.as_str();
                if let Some(note) = rectifier
                    .next_step(
                        &state,
                        &settings,
                        &mut input.body,
                        *status,
                        protocol,
                        message,
                    )
                    .await
                {
                    // D14: 阶梯走完 = 这个模型完全不吃思考参数。不靠关掉思考蒙混过关，
                    // 直接把它从池子里移除，换下一个候选。
                    if ufp_convert::thinking_policy::mode(&input.body)
                        == Some(ufp_convert::thinking_policy::Mode::Unsupported)
                    {
                        tracing::warn!(
                            request_id = %ctx.request_id,
                            channel = %cand.channel.name,
                            model = %cand.entry.upstream_model,
                            "上游不接受任何思考参数，已停用该条目：{}",
                            truncate_error(message)
                        );
                        state.disable_entry_for_thinking(cand.entry.id, message);
                        break r;
                    }
                    tracing::info!(
                        request_id = %ctx.request_id,
                        channel = %cand.channel.name,
                        "矫正后重试同一候选：{}（原错误：{}）",
                        note,
                        truncate_error(message)
                    );
                    continue;
                }
            }
            break r;
        };
        let succeeded = matches!(
            result,
            AttemptResult::Done { .. } | AttemptResult::Committed { .. }
        );
        rectifier.settle(&state, succeeded).await;
        // 阶梯试出来的写法这次真跑通了 → 记到条目上，下个请求直接用（不重走阶梯）。
        if succeeded {
            if let Some(mode) = ufp_convert::thinking_policy::mode(&input.body) {
                if mode != ufp_convert::thinking_policy::Mode::Max
                    && cand.entry.thinking_mode != mode.as_str()
                {
                    state.note_entry_thinking_mode(cand.entry.id, mode.as_str());
                }
            }
        }
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
                    None if kind == "forbidden" => (
                        state.cooldowns.set_backoff(
                            &state.db,
                            cand.key.id,
                            &cand.entry.upstream_model,
                            "上游不让这把 key 用这个模型",
                        ),
                        "上游不让这把 key 用这个模型".to_string(),
                    ),
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
                    "这个 key×模型暂时不能用，已冷却"
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
    // 整池不可用是要人管的事件：发一次告警（同类事件按设置限频）
    if error.status.as_u16() == 529 || error.status.is_server_error() {
        let settings = state.pool.load().settings.clone();
        state.alerter.notify(
            &settings.alerts,
            "pool_unavailable",
            "ufp：上游池暂时不可用",
            &format!(
                "请求 {} 连续尝试 {} 个候选都失败：{}。\n\n去后台「健康与冷却」看看是哪些条目在熔断/冷却。",
                ctx.request_id, meta.attempts, error.message
            ),
        );
    }
    Outcome::Failed { error, meta }
}

/// 给这个候选的请求体打上「思考开到最大」的私有标记；返回是否真的在强制思考。
///
/// 阶梯（`thinking_policy::Mode`）试出来的可用形式记在 `entries.thinking_mode`，
/// 下一个请求直接用学到的形式；记成 `unsupported` 的条目已经被网关停用，
/// 人为重新启用后按「这个模型不支持思考」处理，不发任何思考参数。
fn apply_max_thinking(body: &mut Value, cand: &Candidate) -> bool {
    use ufp_convert::thinking_policy::{set_mode, Mode};
    if !cand.channel.max_thinking {
        return false;
    }
    let mode = Mode::parse(&cand.entry.thinking_mode).unwrap_or(Mode::Max);
    set_mode(body, mode);
    mode != Mode::Unsupported
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
        opencode: input.opencode.as_ref(),
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
    // 非流式也要给「读完整响应体」一个上限，否则上游挂住会把请求一直吊着
    // （客户端自己的 600s 超时会先到，但那段时间里并发名额一直被占）。
    let body_timeout = Duration::from_millis(state.pool.load().settings.first_content_timeout_ms);
    let body_bytes = match tokio::time::timeout(body_timeout, resp.bytes()).await {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            return AttemptResult::Retryable {
                kind: "body_read",
                message: e.to_string(),
                status: None,
            }
        }
        Err(_) => {
            return AttemptResult::Retryable {
                kind: "timeout",
                message: "读取上游响应体超时".into(),
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
        pipeline.attach_log(
            Arc::clone(&state.db),
            row,
            200,
            input.request_started,
            input.attempts as i64,
        );
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
        // 模型级的 403 不是「你的 key 不行」，别把人往查 key 的路上带（见 model_scoped_forbidden）
        Some(403) if kind == "forbidden" => {
            ApiError::invalid_request(format!("上游不让用这个模型（{kind}）：{message}"))
        }
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

/// 403 的两种意思：`true` = 「这把 key 不能用这个模型」，
/// `false` = 「这把 key 本身不被接受」（吊销、封号）。
///
/// 判据是上游错误体里的类型名 / 错误码 / 话术。认不出来时返回 `false`：
/// 停用 key 会亮红灯让人看一眼，比默默把一把好 key 当成「只是这个模型不行」更安全。
fn model_scoped_forbidden(body: &Value, lower_message: &str) -> bool {
    let err = body.get("error");
    let field = |k: &str| -> String {
        err.and_then(|e| e.get(k))
            .or_else(|| body.get(k))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let hay = format!("{} {} {}", field("type"), field("code"), lower_message);
    const MARKS: &[&str] = &[
        // Zen：免费额度只给 OpenCode 客户端
        "freetier",
        "free_tier",
        "free tier",
        "from within opencode",
        // 各家对「模型级权限不足」的常见说法
        "model_not_allowed",
        "not available for this model",
        "no access to this model",
        "does not have access",
        "permission_error",
        "insufficient permission",
        "not entitled",
    ];
    MARKS.iter().any(|m| hay.contains(m))
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
        401 => AttemptResult::DisableKey {
            kind: "auth",
            message,
            status,
        },
        // 403 有两种意思，代价完全不同：
        //   「这把 key 不被接受」（吊销、封号）→ 停用 key，人工恢复；
        //   「这把 key 不能用这个模型」（免费额度只开放给某个客户端、模型没开通权限）
        //     → 只冷却这个 key×模型。当成前者处理的话，勾一个免费模型就会把整把 key
        //     停掉，连带打断同一把 key 上的付费模型。
        403 if model_scoped_forbidden(&body, &lower) => AttemptResult::CoolKey {
            kind: "forbidden",
            message,
            status,
            hint: None,
        },
        403 => AttemptResult::DisableKey {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 把分类结果压成一个字符串，测试里好读。
    fn classify(status: u16, body: Value) -> String {
        match classify_error(status, body, String::new(), None) {
            AttemptResult::DisableKey { kind, .. } => format!("停用 key（{kind}）"),
            AttemptResult::CoolKey { kind, .. } => format!("冷却 key×模型（{kind}）"),
            AttemptResult::Retryable { kind, .. } => format!("换候选（{kind}）"),
            AttemptResult::TooLong { .. } => "超长".into(),
            AttemptResult::Rectifiable { .. } => "可矫正".into(),
            AttemptResult::Fatal { .. } => "致命".into(),
            _ => "其他".into(),
        }
    }

    #[test]
    fn zen_的免费额度_403_只冷却这个模型_不停用整把_key() {
        // 原文（实测）：FreeTierError / "OpenCode's free tier can only be used from within OpenCode"
        let out = classify(
            403,
            json!({"type":"error","error":{"type":"FreeTierError",
                "message":"OpenCode's free tier can only be used from within OpenCode"}}),
        );
        assert_eq!(out, "冷却 key×模型（forbidden）");
    }

    #[test]
    fn anthropic_的权限_403_也只冷却() {
        let out = classify(
            403,
            json!({"type":"error","error":{"type":"permission_error",
                "message":"your key does not have access to this model"}}),
        );
        assert_eq!(out, "冷却 key×模型（forbidden）");
    }

    #[test]
    fn key_被吊销的_403_仍然停用_key() {
        let out = classify(
            403,
            json!({"error":{"type":"invalid_request_error","message":"API key revoked"}}),
        );
        assert_eq!(out, "停用 key（auth）");
    }

    #[test]
    fn 认不出来的_403_按停用_key_处理() {
        let out = classify(403, json!({"error":{"message":"Forbidden"}}));
        assert_eq!(out, "停用 key（auth）");
    }

    #[test]
    fn 未授权限流欠费的分类不变() {
        assert_eq!(
            classify(401, json!({"error":{"message":"invalid api key"}})),
            "停用 key（auth）"
        );
        assert_eq!(
            classify(429, json!({"error":{"message":"rate limit exceeded"}})),
            "冷却 key×模型（rate_limit）"
        );
        assert_eq!(
            classify(402, json!({"error":{"message":"Insufficient credits"}})),
            "冷却 key×模型（quota）"
        );
    }

    #[test]
    fn 上下文超长仍然是超长() {
        let out = classify(
            400,
            json!({"error":{"message":"This model's maximum context length is 128000 tokens"}}),
        );
        assert_eq!(out, "超长");
    }
}
