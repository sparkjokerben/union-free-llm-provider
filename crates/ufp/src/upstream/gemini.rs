//! Gemini 原生上游（AI Studio / 兼容网关的 `/v1beta/models/...`）。
//!
//! - 模型名进 URL，不在请求体里（转换器产出的 body 没有 `model` 字段）。
//! - 鉴权用 `x-goog-api-key`。
//! - URL 归一化复用 cc-switch 的 `gemini_url.rs`：能把
//!   `https://host/v1beta`、`https://host/openai`、`https://host/models/xxx` 这类
//!   历史写法统一成标准端点。
//!
//! 这一版不传 shadow store：thoughtSignature 走「无状态签名信封」，
//! 由转换层把签名塞进 thinking/redacted_thinking 块，不依赖进程内存。

use bytes::Bytes;

use super::{with_model, BuildCtx, UpstreamRequest};
use crate::store::Candidate;
use ufp_convert::gemini_url::{normalize_gemini_model_id, resolve_gemini_native_url};
use ufp_convert::providers::transform_gemini::anthropic_to_gemini_with_shadow;
use ufp_convert::ConvertError;

pub fn prepare(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let body = with_model(ctx.client_body, &cand.entry.upstream_model);
    let converted = anthropic_to_gemini_with_shadow(body, None, None, None)?;

    let model = normalize_gemini_model_id(&cand.entry.upstream_model);
    let endpoint = if ctx.stream {
        format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("/v1beta/models/{model}:generateContent")
    };
    let base_url = cand.channel.base_url.trim();
    // 用户直接粘完整端点（带 :generateContent）时按全 URL 处理。
    let is_full_url =
        base_url.contains(":generateContent") || base_url.contains(":streamGenerateContent");
    let url = resolve_gemini_native_url(base_url, &endpoint, is_full_url);

    Ok(UpstreamRequest {
        url,
        headers: vec![("x-goog-api-key".into(), cand.key.api_key.clone())],
        body: Bytes::from(serde_json::to_vec(&converted).map_err(super::anthropic::serde_err)?),
        protocol: cand.channel.protocol,
        upstream_model: cand.entry.upstream_model.clone(),
    })
}
