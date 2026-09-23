//! OpenAI Responses API 上游。
//!
//! 转换复用 cc-switch 的 `transform_responses.rs` / `streaming_responses.rs`。
//! 这里传 `is_codex_oauth = false`：那是给 ChatGPT 订阅后端用的分支
//! （`store: false` + 加密推理往返），普通 Responses 上游不需要。

use bytes::Bytes;

use super::{join_endpoint, with_model, BuildCtx, UpstreamRequest};
use crate::store::Candidate;
use ufp_convert::providers::transform_responses::anthropic_to_responses;
use ufp_convert::ConvertError;

pub fn prepare(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let body = with_model(ctx.client_body, &cand.entry.upstream_model);
    let converted = anthropic_to_responses(body, None, false, false)?;
    Ok(UpstreamRequest {
        url: join_endpoint(&cand.channel.base_url, "/v1/responses", &["/responses"]),
        headers: vec![(
            "authorization".into(),
            format!("Bearer {}", cand.key.api_key),
        )],
        body: Bytes::from(serde_json::to_vec(&converted).map_err(super::anthropic::serde_err)?),
        protocol: cand.channel.protocol,
        upstream_model: cand.entry.upstream_model.clone(),
    })
}
