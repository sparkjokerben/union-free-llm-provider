//! OpenAI Chat Completions 上游。
//!
//! 转换复用 cc-switch 的 `transform.rs` / `streaming.rs`：
//! - `anthropic_to_openai_with_reasoning_content(body, true)`：把 thinking 块映射成
//!   `reasoning_content`（DeepSeek/LM Studio 等兼容站认这个字段）；
//! - 流式必须补 `stream_options.include_usage`，否则上游不给 usage，token 统计全是 0。

use bytes::Bytes;

use super::{join_endpoint, with_model, BuildCtx, UpstreamRequest};
use crate::store::Candidate;
use ufp_convert::providers::transform::{
    anthropic_to_openai_with_reasoning_content, inject_openai_stream_include_usage,
};
use ufp_convert::ConvertError;

pub fn prepare(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let body = with_model(ctx.client_body, &cand.entry.upstream_model);
    let mut converted = anthropic_to_openai_with_reasoning_content(body, true)?;
    inject_openai_stream_include_usage(&mut converted);
    Ok(UpstreamRequest {
        url: join_endpoint(
            &cand.channel.base_url,
            "/v1/chat/completions",
            &["/chat/completions"],
        ),
        headers: vec![(
            "authorization".into(),
            format!("Bearer {}", cand.key.api_key),
        )],
        body: Bytes::from(serde_json::to_vec(&converted).map_err(super::anthropic::serde_err)?),
        protocol: cand.channel.protocol,
        upstream_model: cand.entry.upstream_model.clone(),
    })
}
