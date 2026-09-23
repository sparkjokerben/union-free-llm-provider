//! 上游适配层：把 Anthropic 请求翻译成各协议的实际请求，并把上游响应交回转换器。
//!
//! 与 cc-switch 的差别（都在 `UFP:` 注释里）：
//! - 不再按环境变量名猜鉴权方式（cc-switch 的 `claude.rs` 会因为 key 是从
//!   `ANTHROPIC_API_KEY` 还是 `ANTHROPIC_AUTH_TOKEN` 读来的而改变请求头），
//!   这里按协议固定：OpenAI 兼容 → `Authorization: Bearer`；Gemini → `x-goog-api-key`；
//!   Anthropic → `x-api-key`。
//! - 不再注入 `anthropic-beta: claude-code-20250219`（那是 cc-switch 在扮演
//!   Claude Code 客户端）；需要特殊 beta 的渠道在渠道配置的附加头里自己加。
//! - 请求体里的 `model` 一律先改写为条目的上游模型名，再交转换器
//!   （Gemini 走 URL，其余协议由转换器带上）。

pub mod anthropic;
pub mod gemini;
pub mod openai_chat;
pub mod openai_responses;

use std::io;

use bytes::Bytes;
use futures::stream::BoxStream;
use serde_json::{json, Value};

use ufp_convert::ConvertError;

use crate::store::{Candidate, Protocol};

pub use ufp_convert::providers::transform_gemini::AnthropicToolSchemaHints;

/// 一次尝试要发出去的请求。
#[derive(Debug, Clone)]
pub struct UpstreamRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub protocol: Protocol,
    pub upstream_model: String,
}

/// 构造请求需要的上下文。
pub struct BuildCtx<'a> {
    /// 客户端原始请求体（Anthropic 形态）。
    pub client_body: &'a Value,
    /// 客户端发来的 `anthropic-version`（Anthropic 原生上游透传用）。
    pub client_anthropic_version: Option<&'a str>,
    /// 客户端是否要求流式。
    pub stream: bool,
}

pub fn build(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let mut req = match cand.channel.protocol {
        Protocol::Anthropic => anthropic::prepare(cand, ctx)?,
        Protocol::OpenAiChat => openai_chat::prepare(cand, ctx)?,
        Protocol::OpenAiResponses => openai_responses::prepare(cand, ctx)?,
        Protocol::Gemini => gemini::prepare(cand, ctx)?,
    };
    // 渠道配置的附加头放在最后，同名覆盖（大小写不敏感）。
    for (k, v) in &cand.channel.extra_headers {
        req.headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
        req.headers.push((k.clone(), v.clone()));
    }
    Ok(req)
}

/// 把请求体里的 `model` 换成条目的上游模型名（转换器会带上它）。
pub(crate) fn with_model(body: &Value, model: &str) -> Value {
    let mut out = body.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
    out
}

/// 拼接上游 URL。
///
/// 容忍用户把 base_url 填成各种形态：
/// - `https://host`、`https://host/v1`、`https://host/openai/v1` → 追加默认路径；
/// - 直接粘完整端点（含 `full_url_markers` 里的标记）→ 原样使用。
pub fn join_endpoint(base_url: &str, default_path: &str, full_url_markers: &[&str]) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return default_path.to_string();
    }
    for marker in full_url_markers {
        if base.ends_with(marker) || base.contains(&format!("{marker}?")) {
            return base.to_string();
        }
    }
    if base.ends_with("/v1") {
        // 已经是 …/v1，补默认路径里去掉 /v1 的部分。
        let tail = default_path.strip_prefix("/v1").unwrap_or(default_path);
        format!("{base}{tail}")
    } else {
        format!("{base}{default_path}")
    }
}

/// 发送请求时的错误（区分「等响应头超时」与「传输层错误」）。
#[derive(Debug)]
pub enum SendError {
    /// 建连或等响应头超过了首内容超时。
    Timeout,
    /// 传输层错误（连接失败、TLS、协议错误等）。
    Transport(reqwest::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Timeout => write!(f, "等待上游响应头超时"),
            SendError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl SendError {
    /// 归错类别（写进尝试明细的 error_type）。
    pub fn kind(&self) -> &'static str {
        match self {
            SendError::Timeout => "timeout",
            SendError::Transport(e) => {
                if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connect"
                } else {
                    "transport"
                }
            }
        }
    }
}

/// 发送请求。`timeout` 覆盖「建连 + 等到响应头」，流式响应本身的时长不受它限制。
pub async fn send(
    client: &reqwest::Client,
    req: &UpstreamRequest,
    timeout: std::time::Duration,
    accept_sse: bool,
) -> Result<reqwest::Response, SendError> {
    let mut builder = client
        .post(&req.url)
        .header("content-type", "application/json")
        // 强制 identity：网关自己按块处理字节流，压缩只会白耗 CPU。
        .header("accept-encoding", "identity");
    if accept_sse {
        builder = builder.header("accept", "text/event-stream");
    }
    for (k, v) in &req.headers {
        builder = builder.header(k, v);
    }
    let fut = builder.body(req.body.clone()).send();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(e)) => Err(SendError::Transport(e)),
        Err(_) => Err(SendError::Timeout),
    }
}

/// 读取错误响应体（截断到 64KB），供错误分类与日志使用。
pub async fn error_body(resp: reqwest::Response) -> (u16, serde_json::Value, String) {
    const CAP: usize = 64 * 1024;
    let status = resp.status().as_u16();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return (status, Value::Null, format!("读取错误响应体失败：{e}")),
    };
    let truncated = bytes.len().min(CAP);
    let text = String::from_utf8_lossy(&bytes[..truncated]).to_string();
    let json = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    (status, json, text)
}

/// 流式：把上游字节流交给对应的转换器，产出 Anthropic SSE 字节流。
pub fn to_anthropic_stream(
    protocol: Protocol,
    resp: reqwest::Response,
    hints: Option<AnthropicToolSchemaHints>,
) -> BoxStream<'static, io::Result<Bytes>> {
    let stream = resp.bytes_stream();
    match protocol {
        Protocol::OpenAiChat => {
            Box::pin(ufp_convert::providers::streaming::create_anthropic_sse_stream(stream))
        }
        Protocol::OpenAiResponses => Box::pin(
            ufp_convert::providers::streaming_responses::create_anthropic_sse_stream_from_responses(
                stream,
            ),
        ),
        Protocol::Gemini => Box::pin(
            ufp_convert::providers::streaming_gemini::create_anthropic_sse_stream_from_gemini(
                stream, None, None, None, hints,
            ),
        ),
        // Anthropic 原生：已经是 Anthropic SSE，原样透传（字节流的错误类型换一下）。
        Protocol::Anthropic => Box::pin(futures::StreamExt::map(stream, |r| {
            r.map_err(io::Error::other)
        })),
    }
}

/// 非流式：把上游响应体转成 Anthropic 形态。
pub fn response_to_anthropic(
    protocol: Protocol,
    body: Value,
    hints: Option<&AnthropicToolSchemaHints>,
) -> Result<Value, ConvertError> {
    match protocol {
        Protocol::OpenAiChat => ufp_convert::providers::transform::openai_to_anthropic(body),
        Protocol::OpenAiResponses => {
            ufp_convert::providers::transform_responses::responses_to_anthropic(body)
        }
        Protocol::Gemini => {
            ufp_convert::providers::transform_gemini::gemini_to_anthropic_with_shadow_and_hints(
                body, None, None, None, hints,
            )
        }
        Protocol::Anthropic => Ok(body),
    }
}

/// 从原始请求里抽出工具 schema 提示（Gemini 侧修正工具入参用）。
pub fn tool_schema_hints(body: &Value) -> AnthropicToolSchemaHints {
    ufp_convert::providers::transform_gemini::extract_anthropic_tool_schema_hints(body)
}
