//! Anthropic 事件后处理管线（D10/D12）。
//!
//! 转换器产出的是 Anthropic SSE 字节流；这一层在它之后、客户端之前，负责四件事：
//!
//! 1. **提交闸门**：在收到「首个内容增量」之前，所有事件都先攒着不下发。这样上游
//!    返回 200 之后才报错、给空流、或者首 token 卡住时，转发层还能悄悄换下一个候选
//!    （cc-switch 是收到 200 就直接往下发，之后出事只能让客户端看到断流）。
//! 2. **model 回显**：把 `message_start` 里的模型名改成配置要求的值（默认回显真实
//!    上游模型名）。
//! 3. **思考显示开关**：客户端没开 thinking 时，把 `thinking` 块改写成
//!    `redacted_thinking`（保留签名，丢掉正文），这样多轮工具调用仍然合法，
//!    客户端界面上也不会出现思考内容。
//! 4. **收尾与保活**：上游流缺 `message_stop` 时补上（cc-switch 的已知缺口，会让
//!    块一直悬着）；空闲时补 `ping`；超时或流内错误转成标准 Anthropic `error` 事件。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use ufp_convert::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use ufp_convert::usage::parser::TokenUsage;

use crate::store::{Db, RequestLogRow, Write};

/// 提交前最多缓存多少字节（防御性上限：正常情况只有几百字节的 message_start）。
const PENDING_CAP: usize = 256 * 1024;
/// 单个 thinking 块改写时最多缓存多少字节。
const THINKING_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub web_search_requests: u64,
}

impl Usage {
    pub fn apply_token_usage(&mut self, t: &TokenUsage) {
        if t.input_tokens > 0 {
            self.input_tokens = t.input_tokens as u64;
        }
        if t.output_tokens > 0 {
            self.output_tokens = t.output_tokens as u64;
        }
        if t.cache_read_tokens > 0 {
            self.cache_read_tokens = t.cache_read_tokens as u64;
        }
        if t.cache_creation_tokens > 0 {
            self.cache_creation_tokens = t.cache_creation_tokens as u64;
        }
    }
}

#[derive(Debug, Clone)]
pub struct PipelineCfg {
    /// 要写给客户端的模型名（`message_start.message.model`）。
    pub echo_model: String,
    /// 客户端是否要求显示思考。
    pub show_thinking: bool,
}

/// 正在被改写的 thinking 块。
struct ThinkingRewrite {
    index: u64,
    signature: Option<String>,
    buffered: usize,
}

pub struct Pipeline {
    cfg: PipelineCfg,
    buffer: String,
    utf8_remainder: Vec<u8>,
    /// 待发给客户端的字节（提交后才非空）。
    out: VecDeque<Bytes>,
    /// 提交前攒下的事件。
    pending: Vec<Bytes>,
    committed: bool,
    has_content: bool,
    saw_message_stop: bool,
    finished: bool,
    usage: Usage,
    stop_reason: Option<String>,
    thinking: Option<ThinkingRewrite>,
    /// 上游流内错误（提交前出现时可换候选）。
    upstream_error: Option<String>,
    /// 流式请求的落库上下文：管线结束时（或客户端断开被 drop 时）写请求明细。
    log: Option<PipelineLog>,
}

struct PipelineLog {
    db: Arc<Db>,
    row: RequestLogRow,
    status: i64,
    written: bool,
}

impl Pipeline {
    pub fn new(cfg: PipelineCfg) -> Self {
        Self {
            cfg,
            buffer: String::new(),
            utf8_remainder: Vec::new(),
            out: VecDeque::new(),
            pending: Vec::new(),
            committed: false,
            has_content: false,
            saw_message_stop: false,
            finished: false,
            usage: Usage::default(),
            stop_reason: None,
            thinking: None,
            upstream_error: None,
            log: None,
        }
    }

    /// 挂上请求明细的落库上下文（流式请求用）。
    ///
    /// 管线在流结束、或客户端提前断开导致管线被 drop 时写这一行（带 Drop 兜底），
    /// 所以「客户端看到一半就断了」也有记录。
    pub fn attach_log(&mut self, db: Arc<Db>, mut row: RequestLogRow, status: i64) {
        row.streaming = true;
        row.created_ms = chrono::Utc::now().timestamp_millis();
        self.log = Some(PipelineLog {
            db,
            row,
            status,
            written: false,
        });
    }

    /// 写请求明细；`error` 非空时记错误类型与消息。
    fn write_log(&mut self, error: Option<(String, String)>) {
        let Some(log) = self.log.as_mut() else {
            return;
        };
        if log.written {
            return;
        }
        log.written = true;
        let mut row = log.row.clone();
        row.input_tokens = self.usage.input_tokens as i64;
        row.output_tokens = self.usage.output_tokens as i64;
        row.cache_read_tokens = self.usage.cache_read_tokens as i64;
        row.cache_creation_tokens = self.usage.cache_creation_tokens as i64;
        row.search_requests = self.usage.web_search_requests as i64;
        row.stop_reason = self.stop_reason.clone();
        row.http_status = log.status;
        if let Some((kind, message)) = error {
            row.error_type = Some(kind);
            row.error_message = Some(truncate_error(&message));
        }
        log.db.write(Write::RequestLog(Box::new(row)));
    }

    pub fn committed(&self) -> bool {
        self.committed
    }

    pub fn has_content(&self) -> bool {
        self.has_content
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    pub fn upstream_error(&self) -> Option<&str> {
        self.upstream_error.as_deref()
    }

    /// 取走本次产生的、可以发给客户端的字节。
    pub fn take_out(&mut self) -> Vec<Bytes> {
        self.out.drain(..).collect()
    }

    /// 取走提交前攒下的字节（提交时调用，或最后一次候选失败时原样交付）。
    pub fn take_pending(&mut self) -> Vec<Bytes> {
        std::mem::take(&mut self.pending)
    }

    /// 喂入一块转换后的 Anthropic SSE 字节。
    ///
    /// 返回 `Err` 表示上游在流里报了错且尚未提交 —— 转发层可以据此换候选。
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        append_utf8_safe(&mut self.buffer, &mut self.utf8_remainder, bytes);
        while let Some(block) = take_sse_block(&mut self.buffer) {
            self.handle_block(&block)?;
        }
        Ok(())
    }

    fn handle_block(&mut self, block: &str) -> Result<(), String> {
        // 取 data 字段（可能有多行 data:，按 SSE 规范用 \n 拼接）。
        let mut data = String::new();
        let mut saw_event = false;
        for line in block.lines() {
            if let Some(v) = strip_sse_field(line, "data") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v);
                saw_event = true;
            } else if strip_sse_field(line, "event").is_some() {
                saw_event = true;
            }
        }
        if !saw_event {
            self.emit_block(block);
            return Ok(());
        }

        let parsed: Option<Value> = serde_json::from_str(data.trim()).ok();
        let Some(value) = parsed else {
            // 解析不了的块原样透传（保持与上游一致的宽容度）。
            self.emit_block(block);
            return Ok(());
        };
        let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match kind {
            "message_start" => {
                let mut value = value;
                if let Some(model) = value
                    .get_mut("message")
                    .and_then(|m| m.as_object_mut())
                    .and_then(|m| m.get_mut("model"))
                {
                    *model = json!(self.cfg.echo_model);
                }
                // UFP: cc-switch 的 message_start 不带 content/stop_reason，
                // 官方 SDK 的流累加器会对着 message.content 追加，这里补齐。
                if let Some(msg) = value.get_mut("message").and_then(|m| m.as_object_mut()) {
                    msg.entry("content").or_insert_with(|| json!([]));
                    msg.entry("stop_reason").or_insert(Value::Null);
                    msg.entry("stop_sequence").or_insert(Value::Null);
                }
                self.capture_message_start_usage(&value);
                self.emit_value("message_start", &value);
            }
            "content_block_start" => {
                let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let block_type = value
                    .get("content_block")
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if block_type == "thinking" && !self.cfg.show_thinking {
                    // 开始缓存这个块，稍后改写成 redacted_thinking。
                    self.thinking = Some(ThinkingRewrite {
                        index,
                        signature: None,
                        buffered: 0,
                    });
                    return Ok(()); // 不 emit，等 stop 时一次性改写
                }
                self.mark_content(block_type != "ping");
                self.emit_value("content_block_start", &value);
            }
            "content_block_delta" => {
                let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let delta_type = value
                    .get("delta")
                    .and_then(|d| d.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if let Some(t) = self.thinking.as_mut() {
                    if t.index == index {
                        if delta_type == "signature_delta" {
                            t.signature = value
                                .get("delta")
                                .and_then(|d| d.get("signature"))
                                .and_then(|s| s.as_str())
                                .map(|s| s.to_string());
                        }
                        t.buffered += block.len();
                        if t.buffered > THINKING_CAP {
                            // 异常大的思考块：放弃改写，原样放行（宁可显示思考，也不能卡住）。
                            let buffered = self.thinking.take().expect("刚判断过");
                            self.mark_content(true);
                            self.emit_value(
                                "content_block_start",
                                &json!({
                                    "type": "content_block_start",
                                    "index": buffered.index,
                                    "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                                }),
                            );
                            self.emit_value("content_block_delta", &value);
                        }
                        return Ok(());
                    }
                }
                self.mark_content(delta_type != "ping");
                self.emit_value("content_block_delta", &value);
            }
            "content_block_stop" => {
                let index = value.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(t) = self.thinking.as_ref() {
                    if t.index == index {
                        let t = self.thinking.take().expect("刚判断过");
                        // 改写成 redacted_thinking：签名照带，正文丢掉。
                        let data = t.signature.clone().unwrap_or_default();
                        self.mark_content(true);
                        self.emit_value(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": t.index,
                                "content_block": {"type": "redacted_thinking", "data": data},
                            }),
                        );
                        self.emit_value(
                            "content_block_stop",
                            &json!({"type": "content_block_stop", "index": t.index}),
                        );
                        return Ok(());
                    }
                }
                self.emit_value("content_block_stop", &value);
            }
            "message_delta" => {
                self.capture_message_delta(&value);
                self.emit_value("message_delta", &value);
            }
            "message_stop" => {
                self.saw_message_stop = true;
                self.emit_value("message_stop", &value);
            }
            "error" | "overloaded_error" => {
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("上游返回了错误事件")
                    .to_string();
                if self.committed {
                    // 已经发出去了，只能把错误原样给客户端。
                    self.emit_value(kind, &value);
                } else {
                    // 还没提交：记下来，让转发层换候选。
                    self.upstream_error = Some(message);
                }
            }
            _ => {
                self.emit_value(kind, &value);
            }
        }
        Ok(())
    }

    /// 记下「已经有真实内容」，并在第一次时提交闸门。
    fn mark_content(&mut self, is_content: bool) {
        if !is_content {
            return;
        }
        self.has_content = true;
        if !self.committed {
            self.committed = true;
            // 把提交前攒下的事件搬到 out，随后（或已经）一起下发。
            for b in self.pending.drain(..) {
                self.out.push_back(b);
            }
        }
    }

    fn emit_value(&mut self, event: &str, value: &Value) {
        let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
        // emit_block 负责补事件之间的空行。
        let block = format!("event: {event}\ndata: {data}");
        self.emit_block(&block);
    }

    fn emit_block(&mut self, block: &str) {
        let bytes = Bytes::from(format!("{block}\n\n"));
        if self.committed {
            self.out.push_back(bytes);
        } else {
            // 提交前：超出防御上限就丢最旧的（正常情况远达不到）。
            if self.pending.iter().map(|b| b.len()).sum::<usize>() < PENDING_CAP {
                self.pending.push(bytes);
            }
        }
    }

    fn capture_message_start_usage(&mut self, value: &Value) {
        if let Some(usage) = value.get("message").and_then(|m| m.get("usage")) {
            let t = TokenUsage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                cache_read_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                cache_creation_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                ..Default::default()
            };
            self.usage.apply_token_usage(&t);
            if let Some(n) = usage
                .get("server_tool_use")
                .and_then(|s| s.get("web_search_requests"))
                .and_then(|v| v.as_u64())
            {
                self.usage.web_search_requests = self.usage.web_search_requests.max(n);
            }
        }
    }

    fn capture_message_delta(&mut self, value: &Value) {
        if let Some(reason) = value
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .and_then(|r| r.as_str())
        {
            self.stop_reason = Some(reason.to_string());
        }
        if let Some(usage) = value.get("usage") {
            let t = TokenUsage {
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                cache_read_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                cache_creation_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
                ..Default::default()
            };
            self.usage.apply_token_usage(&t);
            if let Some(n) = usage
                .get("server_tool_use")
                .and_then(|s| s.get("web_search_requests"))
                .and_then(|v| v.as_u64())
            {
                self.usage.web_search_requests = self.usage.web_search_requests.max(n);
            }
        }
    }

    /// 上游流结束时的收尾。缺 `message_stop` 就补上（含一条 `message_delta`，
    /// 否则客户端拿不到 stop_reason）。
    pub fn finalize(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        if self.committed && !self.saw_message_stop {
            if self.stop_reason.is_none() {
                self.emit_value(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                        "usage": {"output_tokens": self.usage.output_tokens},
                    }),
                );
            } else {
                self.emit_value(
                    "message_delta",
                    &json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": self.stop_reason, "stop_sequence": null},
                        "usage": {"output_tokens": self.usage.output_tokens},
                    }),
                );
            }
            self.emit_value("message_stop", &json!({"type": "message_stop"}));
        }
        self.take_out()
    }

    /// 构造一个 Anthropic `error` 事件（提交后出问题时的收尾方式）。
    pub fn error_event(message: &str, kind: &str) -> Bytes {
        let body = json!({
            "type": "error",
            "error": {"type": kind, "message": message},
        });
        Bytes::from(format!("event: error\ndata: {body}\n\n"))
    }

    fn sse_event(event: &str, value: &Value) -> Bytes {
        let data = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// 整理成一条**完整但为空**的消息：所有候选都只给了空流时，把它原样交付给
    /// 客户端（合法的空响应），比回一个 529 更友好。
    pub fn into_empty_replay(&mut self) -> Vec<Bytes> {
        if self.committed {
            let mut out = self.take_pending();
            out.extend(self.finalize());
            return out;
        }
        let mut out = self.take_pending();
        if out.is_empty() {
            self.finished = true;
            return out;
        }
        out.push(Self::sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 0},
            }),
        ));
        out.push(Self::sse_event(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
        self.stop_reason = Some("end_turn".into());
        self.finished = true;
        out
    }

    /// 明确地为明细标注一个错误（并标记管线已收尾，避免 Drop 再写一遍）。
    pub fn finalize_for_log(&mut self, error: Option<(&str, &str)>) {
        match error {
            Some((kind, message)) => {
                self.write_log(Some((kind.to_string(), truncate_error(message))))
            }
            None => self.write_log(None),
        }
        self.finished = true;
    }

    /// 把「剩余的上游流」包装成发给客户端的流：先冲刷提交前缓存，随后逐块处理、
    /// 补 ping、控空闲超时、收尾补 message_stop。
    pub fn into_client_stream(
        mut self,
        mut upstream: BoxStream<'static, std::io::Result<Bytes>>,
        ping_interval: Duration,
        idle_timeout: Duration,
    ) -> BoxStream<'static, std::io::Result<Bytes>> {
        Box::pin(async_stream::stream! {
            // 提交前攒下的事件先发出去（此时 out 里应该只有提交那一刻搬过来的内容）。
            for b in self.take_pending() {
                yield Ok(b);
            }
            for b in self.take_out() {
                yield Ok(b);
            }
            let mut last_activity = tokio::time::Instant::now();
            let mut last_emit = tokio::time::Instant::now();
            loop {
                let ping_deadline = last_emit + ping_interval;
                let idle_deadline = last_activity + idle_timeout;
                tokio::select! {
                    item = upstream.next() => {
                        match item {
                            Some(Ok(bytes)) => {
                                last_activity = tokio::time::Instant::now();
                                match self.feed(&bytes) {
                                    Ok(()) => {
                                        let out = self.take_out();
                                        if !out.is_empty() {
                                            last_emit = tokio::time::Instant::now();
                                        }
                                        for b in out { yield Ok(b); }
                                    }
                                    Err(msg) => {
                                        self.write_log(Some((
                                            "stream_error".into(),
                                            msg.clone(),
                                        )));
                                        yield Ok(Self::error_event(
                                            &format!("上游流内错误：{msg}"),
                                            "overloaded_error",
                                        ));
                                        return;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                let msg = format!("读取上游流失败：{e}");
                                self.write_log(Some(("stream_error".into(), msg.clone())));
                                yield Ok(Self::error_event(&msg, "api_error"));
                                return;
                            }
                            None => {
                                for b in self.finalize() { yield Ok(b); }
                                self.write_log(None);
                                return;
                            }
                        }
                    }
                    _ = tokio::time::sleep_until(idle_deadline) => {
                        self.write_log(Some((
                            "idle_timeout".into(),
                            "上游长时间没有返回数据（流空闲超时）".into(),
                        )));
                        yield Ok(Self::error_event(
                            "上游长时间没有返回数据（流空闲超时）",
                            "overloaded_error",
                        ));
                        return;
                    }
                    _ = tokio::time::sleep_until(ping_deadline) => {
                        last_emit = tokio::time::Instant::now();
                        yield Ok(Bytes::from_static(b"event: ping\ndata: {\"type\": \"ping\"}\n\n"));
                    }
                }
            }
        })
    }
}

/// 客户端是否要求显示思考（`thinking.type` 为 enabled/adaptive）。
pub fn wants_thinking(body: &Value) -> bool {
    body.get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "enabled" || t == "adaptive")
        .unwrap_or(false)
}

/// 错误信息落库前截断（明细表不该被一个巨大的错误体撑爆）。
pub fn truncate_error(message: &str) -> String {
    const CAP: usize = 2048;
    if message.len() <= CAP {
        return message.to_string();
    }
    let mut end = CAP;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…（已截断）", &message[..end])
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // 客户端中途断开时也把明细写掉（此时流没有正常收尾）。
        let error = if self.finished || self.committed {
            None
        } else {
            Some((
                "upstream_error".to_string(),
                self.upstream_error
                    .clone()
                    .unwrap_or_else(|| "流未正常结束".into()),
            ))
        };
        self.write_log(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(show_thinking: bool) -> PipelineCfg {
        PipelineCfg {
            echo_model: "upstream-model-x".into(),
            show_thinking,
        }
    }

    fn sse(event: &str, value: Value) -> String {
        format!("event: {event}\ndata: {value}\n\n")
    }

    fn collect(mut p: Pipeline, chunks: &[String]) -> (String, bool) {
        for c in chunks {
            p.feed(c.as_bytes()).expect("不该有流内错误");
        }
        let mut out: Vec<u8> = p.take_pending().iter().flat_map(|b| b.to_vec()).collect();
        out.extend(p.finalize().iter().flat_map(|b| b.to_vec()));
        (String::from_utf8_lossy(&out).to_string(), p.committed())
    }

    #[test]
    fn 提交前不下发直到出现内容增量() {
        let mut p = Pipeline::new(cfg(true));
        p.feed(sse("message_start", json!({"type":"message_start","message":{"model":"real-model","usage":{"input_tokens":10}}})).as_bytes()).unwrap();
        assert!(!p.committed());
        assert!(p.take_out().is_empty(), "提交前不得下发");
        p.feed(sse("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()).unwrap();
        assert!(p.committed(), "内容块开始即视为有内容");
        let out = String::from_utf8_lossy(
            &p.take_out()
                .iter()
                .flat_map(|b| b.to_vec())
                .collect::<Vec<_>>(),
        )
        .to_string();
        assert!(
            out.contains("message_start"),
            "提交时要把攒下的 message_start 一起放出去：{out}"
        );
        assert!(out.contains("content_block_start"), "{out}");
    }

    #[test]
    fn 模型名被改写() {
        let p = Pipeline::new(cfg(true));
        let chunks = vec![
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"real-model","usage":{"input_tokens":3}}}),
            ),
            sse(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}),
            ),
            sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            sse("message_stop", json!({"type":"message_stop"})),
        ];
        let (out, committed) = collect(p, &chunks);
        assert!(committed);
        assert!(out.contains("\"model\":\"upstream-model-x\""), "{out}");
        assert!(!out.contains("real-model"), "{out}");
    }

    #[test]
    fn 空流不算内容() {
        let p = Pipeline::new(cfg(true));
        let chunks = vec![
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            ),
            sse("message_stop", json!({"type":"message_stop"})),
        ];
        let (_, committed) = collect(p, &chunks);
        assert!(!committed, "只有 message_stop 不算内容");
    }

    #[test]
    fn 提交后补发缺失的_message_stop() {
        let mut p = Pipeline::new(cfg(true));
        p.feed(
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            )
            .as_bytes(),
        )
        .unwrap();
        p.feed(sse("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})).as_bytes()).unwrap();
        p.feed(sse("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}})).as_bytes()).unwrap();
        p.take_out();
        // 上游直接断了，没有 message_delta / message_stop
        let out = String::from_utf8_lossy(
            &p.finalize()
                .iter()
                .flat_map(|b| b.to_vec())
                .collect::<Vec<_>>(),
        )
        .to_string();
        assert!(out.contains("message_delta"), "{out}");
        assert!(out.contains("message_stop"), "{out}");
        assert!(out.contains("end_turn"), "{out}");
    }

    #[test]
    fn 不显示思考时改写成_redacted_thinking() {
        let p = Pipeline::new(cfg(false));
        let chunks = vec![
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            ),
            sse(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"秘密推理过程"}}),
            ),
            sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG-123"}}),
            ),
            sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            sse(
                "content_block_start",
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            ),
            sse("message_stop", json!({"type":"message_stop"})),
        ];
        let (out, _) = collect(p, &chunks);
        assert!(out.contains("redacted_thinking"), "{out}");
        assert!(out.contains("SIG-123"), "签名必须带上：{out}");
        assert!(!out.contains("秘密推理过程"), "思考正文不该下发：{out}");
    }

    #[test]
    fn 显示思考时原样保留() {
        let p = Pipeline::new(cfg(true));
        let chunks = vec![
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            ),
            sse(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
            ),
            sse(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"推理"}}),
            ),
            sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
        ];
        let (out, _) = collect(p, &chunks);
        assert!(out.contains("thinking_delta"), "{out}");
        assert!(out.contains("推理"), "{out}");
        assert!(!out.contains("redacted_thinking"), "{out}");
    }

    #[test]
    fn 提交前的流内错误可被识别() {
        let mut p = Pipeline::new(cfg(true));
        p.feed(
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            )
            .as_bytes(),
        )
        .unwrap();
        p.feed(
            sse(
                "error",
                json!({"type":"error","error":{"type":"overloaded_error","message":"上游过载"}}),
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(p.upstream_error().is_some());
        assert!(!p.committed());
    }

    #[test]
    fn 提取_usage_与_stop_reason() {
        let mut p = Pipeline::new(cfg(true));
        let chunks = [
            sse(
                "message_start",
                json!({"type":"message_start","message":{"model":"m","usage":{"input_tokens":123,"cache_read_input_tokens":45}}}),
            ),
            sse(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ),
            sse(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":67,"server_tool_use":{"web_search_requests":2}}}),
            ),
            sse("message_stop", json!({"type":"message_stop"})),
        ];
        for c in &chunks {
            p.feed(c.as_bytes()).unwrap();
        }
        assert_eq!(p.usage().input_tokens, 123);
        assert_eq!(p.usage().cache_read_tokens, 45);
        assert_eq!(p.usage().output_tokens, 67);
        assert_eq!(p.usage().web_search_requests, 2);
        assert_eq!(p.stop_reason(), Some("tool_use"));
    }

    #[test]
    fn 思考开关解析() {
        assert!(wants_thinking(
            &json!({"thinking": {"type": "enabled", "budget_tokens": 1000}})
        ));
        assert!(wants_thinking(&json!({"thinking": {"type": "adaptive"}})));
        assert!(!wants_thinking(&json!({"thinking": {"type": "disabled"}})));
        assert!(!wants_thinking(&json!({})));
    }
}
