//! Anthropic 原生上游：请求体基本原样透传，只改写 `model`。
//!
//! 不做「扮演 Claude Code」那一套：不注入 `anthropic-beta: claude-code-20250219`，
//! 也不强制任何 beta。客户端发来的 `anthropic-version` 透传，缺省用 `2023-06-01`。
//! 需要特定 beta 的兼容渠道，在渠道配置的附加头里加 `anthropic-beta`。

use bytes::Bytes;

use super::{join_endpoint, with_model, BuildCtx, UpstreamRequest};
use crate::store::Candidate;
use ufp_convert::ConvertError;

pub fn prepare(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let body = with_model(ctx.client_body, &cand.entry.upstream_model);
    let version = ctx.client_anthropic_version.unwrap_or("2023-06-01");
    Ok(UpstreamRequest {
        url: join_endpoint(&cand.channel.base_url, "/v1/messages", &["/v1/messages"]),
        headers: vec![
            ("x-api-key".into(), cand.key.api_key.clone()),
            ("anthropic-version".into(), version.to_string()),
        ],
        body: Bytes::from(serde_json::to_vec(&body).map_err(serde_err)?),
        protocol: cand.channel.protocol,
        upstream_model: cand.entry.upstream_model.clone(),
    })
}

pub(crate) fn serde_err(e: serde_json::Error) -> ConvertError {
    ConvertError::TransformError(format!("序列化上游请求失败：{e}"))
}
