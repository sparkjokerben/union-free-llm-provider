//! 响应方向的 WebSearch 拦截与续写。
//!
//! 这个模块坐在 `forward::run` 的客户端字节流之上，做三件事：
//!
//! 1. **重排内容块索引**：因为下面要往流里插入块，所有索引由这里统一分配
//!    （上游每段响应的索引都是从 0 开始的，跨段必须重新编号）。
//! 2. **拦截工具调用**：上游模型调用 `web_search` 时，把这次调用从流里拿掉，
//!    换成 Claude Code 认识的 `server_tool_use` + `web_search_tool_result` 两块，
//!    然后带着搜索结果再问一次上游（续写），如此循环直到不再调用或达到 max_uses。
//! 3. **收尾**：各段自带的 `message_delta`/`message_stop` 一律压掉，最后合并
//!    usage 与 stop_reason，只发一份。

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use ufp_convert::sse::{append_utf8_safe, strip_sse_field, take_sse_block};

use crate::api::AppState;
use crate::forward::{ForwardCtx, Meta, Outcome};
use crate::pipeline::{truncate_error, Usage};
use crate::store::{Settings, Write};
use crate::websearch::backends::{self, SearchBackend, SearchItem};
use crate::websearch::history::{self, SearchPlan};

/// 搜索循环需要的上下文。
pub struct SearchState {
    pub plan: SearchPlan,
    pub backends: Vec<SearchBackend>,
    pub settings: Settings,
    pub request_id: String,
}

/// 把「首段响应流」接上搜索循环，返回给客户端的流。
pub fn wrap(
    state: Arc<AppState>,
    ctx: ForwardCtx,
    search: SearchState,
    first: BoxStream<'static, std::io::Result<Bytes>>,
    first_meta: Meta,
    pre: Option<(String, Vec<SearchItem>, String)>,
) -> BoxStream<'static, std::io::Result<Bytes>> {
    Box::pin(async_stream::stream! {
        let mut next_index: u64 = 0;
        let mut usage = merge_usage(&Usage::default(), &first_meta.usage, false);
        let mut stop_reason = first_meta.stop_reason.clone();
        // 快速路径已经在进循环前搜过一次了，从 1 起算。
        let mut searches_done: u32 = if pre.is_some() { 1 } else { 0 };
        let mut body = ctx.body.clone();

        // 快速路径：先把两个搜索块备好，等首段的 message_start 发出后接上。
        let mut pre_blocks: Option<Vec<Bytes>> = pre
            .as_ref()
            .map(|(q, items, _)| search_blocks(0, q, items));
        if pre_blocks.is_some() {
            next_index = 2;
            usage.web_search_requests = 1;
        }

        let mut segment = first;
        let mut first_segment = true;
        loop {
            // ---- 处理一段上游响应 ----
            let mut buffer = String::new();
            let mut utf8_remainder: Vec<u8> = Vec::new();
            let mut index_map: HashMap<u64, u64> = HashMap::new();
            let mut intercepted: Option<Intercepted> = None;
            let mut need_continuation: Option<(String, String)> = None; // (query, call_id)

            while let Some(chunk) = segment.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Ok(error_event(&format!("读取上游流失败：{e}")));
                        return;
                    }
                };
                append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                while let Some(block) = take_sse_block(&mut buffer) {
                    let (event, value) = parse_event(&block);
                    let kind = value
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or(event.as_str())
                        .to_string();
                    match kind.as_str() {
                        "message_start" => {
                            capture_usage(&mut usage, &value, true);
                            if first_segment {
                                yield Ok(rewrite_event(&block, &index_map, None));
                                if let Some(blocks) = pre_blocks.take() {
                                    // 快速路径：首段的 message_start 之后立刻给出搜索块
                                    for b in blocks { yield Ok(b); }
                                }
                            }
                            // 续写段的 message_start 压掉：客户端已经收到过一份了
                        }
                        "content_block_start" => {
                            let raw_index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            let block_type = value
                                .get("content_block")
                                .and_then(|b| b.get("type"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            let tool_name = value
                                .get("content_block")
                                .and_then(|b| b.get("name"))
                                .and_then(|n| n.as_str())
                                .unwrap_or("");
                            if block_type == "tool_use" && tool_name == crate::websearch::TOOL_NAME {
                                // 开始拦截：这块不发给客户端
                                intercepted = Some(Intercepted {
                                    raw_index,
                                    assigned_index: next_index,
                                    call_id: value
                                        .get("content_block")
                                        .and_then(|b| b.get("id"))
                                        .and_then(|i| i.as_str())
                                        .unwrap_or("call_unknown")
                                        .to_string(),
                                    query: String::new(),
                                });
                                continue;
                            }
                            assign_index(&mut index_map, raw_index, &mut next_index);
                            yield Ok(rewrite_event(&block, &index_map, None));
                        }
                        "content_block_delta" => {
                            let raw_index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            if let Some(it) = intercepted.as_mut() {
                                if it.raw_index == raw_index {
                                    // 累积 input_json_delta 里的查询词
                                    if let Some(partial) = value
                                        .get("delta")
                                        .and_then(|d| d.get("partial_json"))
                                        .and_then(|p| p.as_str())
                                    {
                                        it.query.push_str(partial);
                                    }
                                    continue;
                                }
                            }
                            yield Ok(rewrite_event(&block, &index_map, None));
                        }
                        "content_block_stop" => {
                            let raw_index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            if let Some(it) = intercepted.take() {
                                if it.raw_index == raw_index {
                                    let query = extract_query(&it.query);
                                    let call_id = it.call_id.clone();
                                    let assigned = it.assigned_index;
                                    // 被拦下的是一个块，换成了两个块，后续块的编号要顺延。
                                    next_index = assigned + 2;
                                    if searches_done >= search.plan.max_uses {
                                        // 超过 max_uses：回一个错误结果，收尾
                                        let text = format!(
                                            "Web search limit reached ({}) for this request.",
                                            search.plan.max_uses
                                        );
                                        for b in search_error_blocks(assigned, &query, "max_uses_exceeded") {
                                            yield Ok(b);
                                        }
                                        searches_done += 1;
                                        // 用错误结果续写一次，让模型收尾（不再放宽工具选择以外的事）
                                        history::append_search_round(&mut body, &call_id, &query, &text, true);
                                        need_continuation = Some((query, call_id));
                                        break;
                                    }
                                    // 真正执行搜索
                                    let outcome = run_search(
                                        Arc::clone(&state),
                                        &search,
                                        &query,
                                    )
                                    .await;
                                    match outcome {
                                        Ok((items, text)) => {
                                            for b in search_blocks(assigned, &query, &items) { yield Ok(b); }
                                            searches_done += 1;
                                            usage.web_search_requests = searches_done as u64;
                                            history::append_search_round(
                                                &mut body,
                                                &call_id,
                                                &query,
                                                &text,
                                                true,
                                            );
                                            tracing::info!(
                                                request_id = %search.request_id,
                                                query = %query,
                                                results = items.len(),
                                                round = searches_done,
                                                "已执行搜索并续写"
                                            );
                                        }
                                        Err(message) => {
                                            let text = format!(
                                                "Web search is unavailable: {}. Answer without live search results and say so.",
                                                truncate_error(&message)
                                            );
                                            for b in search_error_blocks(assigned, &query, "unavailable") {
                                                yield Ok(b);
                                            }
                                            history::append_search_round(&mut body, &call_id, &query, &text, true);
                                        }
                                    }
                                    need_continuation = Some((query, call_id));
                                    break;
                                }
                            }
                            yield Ok(rewrite_event(&block, &index_map, None));
                        }
                        "message_delta" => {
                            capture_usage(&mut usage, &value, false);
                            if let Some(reason) = value
                                .get("delta")
                                .and_then(|d| d.get("stop_reason"))
                                .and_then(|r| r.as_str())
                            {
                                stop_reason = Some(reason.to_string());
                            }
                            // 压掉：收尾时统一发
                        }
                        "message_stop" | "ping" => {
                            if kind == "ping" {
                                yield Ok(Bytes::from(format!("{block}\n\n")));
                            }
                        }
                        "error" => {
                            yield Ok(Bytes::from(format!("{block}\n\n")));
                            return;
                        }
                        _ => {
                            yield Ok(rewrite_event(&block, &index_map, None));
                        }
                    }
                    if need_continuation.is_some() {
                        break;
                    }
                }
                if need_continuation.is_some() {
                    break;
                }
            }

            let Some((_query, _call_id)) = need_continuation else {
                break; // 这一段正常结束（或出错退出）
            };

            // ---- 续写：带着搜索结果再问一次上游 ----
            let next_ctx = ForwardCtx {
                body: body.clone(),
                ..clone_ctx(&ctx)
            };
            match crate::forward::run(Arc::clone(&state), next_ctx).await {
                Outcome::Streaming { stream, meta } => {
                    usage = merge_usage(&usage, &meta.usage, true);
                    if meta.stop_reason.is_some() {
                        stop_reason = meta.stop_reason.clone();
                    }
                    segment = stream;
                    first_segment = false;
                }
                Outcome::NonStreaming { body: value, meta } => {
                    // 上游忽略 stream:true 时给了整包 JSON：转成事件流继续处理
                    usage = merge_usage(&usage, &meta.usage, true);
                    if meta.stop_reason.is_some() {
                        stop_reason = meta.stop_reason.clone();
                    }
                    segment = json_message_to_stream(&value);
                    first_segment = false;
                }
                Outcome::Failed { error, .. } => {
                    yield Ok(error_event(&format!(
                        "续写失败：{}",
                        truncate_error(&error.message)
                    )));
                    return;
                }
            }
        }

        // ---- 收尾：合并后的 usage 与 stop_reason ----
        let reason = stop_reason.clone().unwrap_or_else(|| "end_turn".into());
        yield Ok(final_delta(&reason, &usage));
        yield Ok(Bytes::from_static(
            b"event: message_stop\ndata: {\"type\": \"message_stop\"}\n\n",
        ));
    })
}

fn clone_ctx(ctx: &ForwardCtx) -> ForwardCtx {
    ForwardCtx {
        request_id: ctx.request_id.clone(),
        session_id: ctx.session_id.clone(),
        downstream_key_id: ctx.downstream_key_id,
        body: ctx.body.clone(),
        streaming: ctx.streaming,
        plan: ctx.plan.clone(),
        client_anthropic_version: ctx.client_anthropic_version.clone(),
        requested_model: ctx.requested_model.clone(),
        show_thinking: ctx.show_thinking,
        log_template: ctx.log_template.clone(),
    }
}

/// 被拦截的工具调用。
struct Intercepted {
    raw_index: u64,
    assigned_index: u64,
    call_id: String,
    /// `input_json_delta` 拼起来的原始 JSON 片段。
    query: String,
}

fn extract_query(partial_json: &str) -> String {
    serde_json::from_str::<Value>(partial_json)
        .ok()
        .and_then(|v| {
            v.get("query")
                .and_then(|q| q.as_str())
                .map(|q| q.to_string())
        })
        .unwrap_or_else(|| partial_json.trim().to_string())
}

fn assign_index(map: &mut HashMap<u64, u64>, raw: u64, next: &mut u64) -> u64 {
    if let Some(existing) = map.get(&raw) {
        return *existing;
    }
    let assigned = *next;
    *next += 1;
    map.insert(raw, assigned);
    assigned
}

/// 事件直通，但把 `index` 换成网关分配的编号。
fn rewrite_event(block: &str, map: &HashMap<u64, u64>, force_index: Option<u64>) -> Bytes {
    let (event, mut value) = parse_event(block);
    let raw_index = value.get("index").and_then(|v| v.as_u64());
    if let (Some(raw), Some(obj)) = (raw_index, value.as_object_mut()) {
        let assigned = force_index.unwrap_or_else(|| map.get(&raw).copied().unwrap_or(raw));
        obj.insert("index".into(), json!(assigned));
    }
    let data = serde_json::to_string(&value).unwrap_or_else(|_| "{}".into());
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

fn parse_event(block: &str) -> (String, Value) {
    let mut event = String::new();
    let mut data = String::new();
    for line in block.lines() {
        if let Some(v) = strip_sse_field(line, "event") {
            event = v.trim().to_string();
        } else if let Some(v) = strip_sse_field(line, "data") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v);
        }
    }
    let value = serde_json::from_str::<Value>(data.trim()).unwrap_or(Value::Null);
    if event.is_empty() {
        event = value
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("message")
            .to_string();
    }
    (event, value)
}

fn capture_usage(usage: &mut Usage, value: &Value, is_start: bool) {
    let source = if is_start {
        value.get("message").and_then(|m| m.get("usage"))
    } else {
        value.get("usage")
    };
    let Some(u) = source else {
        return;
    };
    let get = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    if is_start {
        // 输入侧取各段的最大值（续写会把整段历史重发）
        usage.input_tokens = usage.input_tokens.max(get("input_tokens"));
        usage.cache_read_tokens = usage.cache_read_tokens.max(get("cache_read_input_tokens"));
        usage.cache_creation_tokens = usage
            .cache_creation_tokens
            .max(get("cache_creation_input_tokens"));
    } else {
        // 输出侧累加（每段各产生一部分回答）
        // 续写是把整段历史重发，输出侧是新增的部分，所以累加。
        usage.output_tokens += get("output_tokens");
    }
    if let Some(n) = u
        .get("server_tool_use")
        .and_then(|s| s.get("web_search_requests"))
        .and_then(|v| v.as_u64())
    {
        usage.web_search_requests = usage.web_search_requests.max(n);
    }
}

fn merge_usage(base: &Usage, extra: &Usage, add_output: bool) -> Usage {
    Usage {
        input_tokens: base.input_tokens.max(extra.input_tokens),
        output_tokens: if add_output {
            base.output_tokens + extra.output_tokens
        } else {
            base.output_tokens.max(extra.output_tokens)
        },
        cache_read_tokens: base.cache_read_tokens.max(extra.cache_read_tokens),
        cache_creation_tokens: base.cache_creation_tokens.max(extra.cache_creation_tokens),
        web_search_requests: base.web_search_requests.max(extra.web_search_requests),
    }
}

fn final_delta(stop_reason: &str, usage: &Usage) -> Bytes {
    let value = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_read_input_tokens": usage.cache_read_tokens,
            "cache_creation_input_tokens": usage.cache_creation_tokens,
            "server_tool_use": {"web_search_requests": usage.web_search_requests},
        }
    });
    Bytes::from(format!("event: message_delta\ndata: {value}\n\n"))
}

fn error_event(message: &str) -> Bytes {
    crate::pipeline::Pipeline::error_event(message, "overloaded_error")
}

/// 生成 `server_tool_use` + `web_search_tool_result` 两个块。
pub fn search_blocks(index: u64, query: &str, items: &[SearchItem]) -> Vec<Bytes> {
    let id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
    let mut out = Vec::with_capacity(4 + items.len());
    out.push(sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {
                "type": "server_tool_use",
                "id": id,
                "name": crate::websearch::TOOL_NAME,
                "input": {},
            }
        }),
    ));
    out.push(sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": serde_json::to_string(&json!({"query": query})).unwrap_or_default()},
        }),
    ));
    out.push(sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    ));
    // 结果块整块放在 content_block_start 里（Claude Code 只从 start 事件里读结果）
    let content: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "type": "web_search_result",
                "url": item.url,
                "title": item.title,
                // 信封：把正文塞进 encrypted_content，历史回来时还能还原
                "encrypted_content": history::encode_payload(item),
                "page_age": item.published,
            })
        })
        .collect();
    out.push(sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index + 1,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": id,
                "content": content,
            }
        }),
    ));
    out.push(sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index + 1}),
    ));
    out
}

/// 生成失败结果块（`content` 是对象而不是数组）。
pub fn search_error_blocks(index: u64, query: &str, error_code: &str) -> Vec<Bytes> {
    let id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
    vec![
        sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": "server_tool_use", "id": id, "name": crate::websearch::TOOL_NAME, "input": {}},
            }),
        ),
        sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": serde_json::to_string(&json!({"query": query})).unwrap_or_default()},
            }),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index}),
        ),
        sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index + 1,
                "content_block": {
                    "type": "web_search_tool_result",
                    "tool_use_id": id,
                    "content": {"type": "web_search_tool_result_error", "error_code": error_code},
                },
            }),
        ),
        sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": index + 1}),
        ),
    ]
}

fn sse(event: &str, value: &Value) -> Bytes {
    let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

/// 把「上游其实返回了整包 JSON」的情况变成事件流，交给同一套循环处理。
fn json_message_to_stream(value: &Value) -> BoxStream<'static, std::io::Result<Bytes>> {
    let mut events: Vec<Bytes> = Vec::new();
    let message = value.clone();
    events.push(sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": message.get("id").cloned().unwrap_or(json!("msg_synth")),
                "type": "message",
                "role": "assistant",
                "model": message.get("model").cloned().unwrap_or(json!("")),
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": message.get("usage").cloned().unwrap_or(json!({})),
            }
        }),
    ));
    if let Some(blocks) = value.get("content").and_then(|c| c.as_array()) {
        for (i, block) in blocks.iter().enumerate() {
            let i = i as u64;
            events.push(sse(
                "content_block_start",
                &json!({"type": "content_block_start", "index": i, "content_block": block}),
            ));
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                events.push(sse(
                    "content_block_delta",
                    &json!({"type": "content_block_delta", "index": i,
                            "delta": {"type": "text_delta", "text": text}}),
                ));
            }
            events.push(sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": i}),
            ));
        }
    }
    events.push(sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": value.get("stop_reason").cloned().unwrap_or(json!("end_turn")), "stop_sequence": null},
            "usage": value.get("usage").cloned().unwrap_or(json!({})),
        }),
    ));
    events.push(sse("message_stop", &json!({"type": "message_stop"})));
    futures::stream::iter(events.into_iter().map(Ok)).boxed()
}

/// 执行一次搜索：在后端之间做故障转移，失败的写入冷却。
pub async fn run_search(
    state: Arc<AppState>,
    search: &SearchState,
    query: &str,
) -> Result<(Vec<SearchItem>, String), String> {
    if search.backends.is_empty() {
        return Err("网关还没有配置搜索后端".into());
    }
    if !search.settings.search.enabled {
        return Err("网关已关闭网页搜索".into());
    }
    let now = chrono::Utc::now().timestamp_millis();
    let mut last_error = String::from("所有搜索后端都不可用");
    for backend in search.backends.iter() {
        if backend.cooldown_until_ms.unwrap_or(0) > now {
            continue;
        }
        match backends::search(
            &state.client,
            backend,
            query,
            search.settings.search.top_results,
            search.settings.search.snippet_bytes,
        )
        .await
        {
            Ok(items) => {
                let text = backends::results_as_text(query, &items);
                return Ok((items, text));
            }
            Err(failure) => {
                tracing::warn!(
                    request_id = %search.request_id,
                    backend = %backend.name,
                    "搜索后端失败：{}",
                    failure.message
                );
                if let Some(until) = failure.cooldown_until_ms {
                    state.db.write(Write::SearchCooldown {
                        backend_id: backend.id,
                        until_ms: until,
                    });
                }
                last_error = failure.message;
            }
        }
    }
    Err(last_error)
}

/// 非流式响应里的 WebSearch 处理：把模型对 `web_search` 的工具调用换成
/// 服务端工具块。返回是否替换过（非流式路径不续写，只是让响应形态合法）。
pub fn replace_tool_use_in_message(value: &mut Value, query: &str, items: &[SearchItem]) -> bool {
    let Some(content) = value.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return false;
    };
    let mut replaced = false;
    let mut extra: Vec<Value> = Vec::new();
    for block in content.iter_mut() {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && block.get("name").and_then(|n| n.as_str()) == Some(crate::websearch::TOOL_NAME)
        {
            let query = block
                .get("input")
                .and_then(|i| i.get("query"))
                .and_then(|q| q.as_str())
                .unwrap_or(query)
                .to_string();
            let id = block
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("srvtoolu_unknown")
                .to_string();
            *block = json!({
                "type": "server_tool_use",
                "id": id,
                "name": crate::websearch::TOOL_NAME,
                "input": {"query": query},
            });
            let content_items: Vec<Value> = items
                .iter()
                .map(|item| {
                    json!({
                        "type": "web_search_result",
                        "url": item.url,
                        "title": item.title,
                        "encrypted_content": history::encode_payload(item),
                        "page_age": item.published,
                    })
                })
                .collect();
            extra.push(json!({
                "type": "web_search_tool_result",
                "tool_use_id": id,
                "content": content_items,
            }));
            replaced = true;
        }
    }
    content.extend(extra);
    replaced
}
