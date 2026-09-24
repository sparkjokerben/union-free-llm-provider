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
    let mut converted = anthropic_to_gemini_with_shadow(body, None, None, None)?;
    super::shape_body(cand, ctx, &mut converted);

    let model = normalize_gemini_model_id(&cand.entry.upstream_model);
    let url = endpoint_url(&cand.channel.base_url, model, ctx.stream);

    Ok(UpstreamRequest {
        url,
        headers: vec![("x-goog-api-key".into(), cand.key.api_key.clone())],
        body: Bytes::from(serde_json::to_vec(&converted).map_err(super::anthropic::serde_err)?),
        protocol: cand.channel.protocol,
        upstream_model: cand.entry.upstream_model.clone(),
    })
}

/// 组装 Gemini 原生端点。
///
/// cc-switch 的 `resolve_gemini_native_url` 只对 **Google 自己的域名** 做版本后缀归一化
/// （`gemini_url.rs:104-121`：`/v1`、`/v1beta`、`/models` 这些后缀，只有在主机是
/// Google 的 Gemini / Vertex 端点时才改写）。别的主机一律当成「中转站自己写死的路径」，
/// 于是 `https://opencode.ai/zen/v1` 会被拼成 `/zen/v1/v1beta/models/...`，而 Zen 的
/// Gemini 端点其实是 `/zen/v1/models/<模型>:generateContent`（官方端点表逐模型列出）。
///
/// 这不是 cc-switch 的 bug——对不明来历的中转站，不动它的路径更安全。但对 Gemini 兼容的
/// 网关来说，`/v1` 与 `/v1beta` 都是 Google REST 里合法的版本段，所以：地址里已经写明了
/// 版本段或 `models` 段时，我们在这里自己拼到底；其余的（裸主机名、Google 域名、
/// 中转站自定路径）仍旧交给 cc-switch 那个归一化器。
pub(crate) fn endpoint_url(base_url: &str, model: &str, stream: bool) -> String {
    let method = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    let base = base_url.trim().trim_end_matches('/');
    // 用户直接粘完整端点时按全 URL 用（流式与否由他写的那个方法名决定）。
    if base.contains(":generateContent") || base.contains(":streamGenerateContent") {
        return base.to_string();
    }
    let path = match base.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => "",
    };
    let path = path.trim_end_matches('/');
    if path.ends_with("/models") {
        return format!("{base}/{model}:{method}");
    }
    if path.ends_with("/v1") || path.ends_with("/v1beta") {
        return format!("{base}/models/{model}:{method}");
    }
    resolve_gemini_native_url(base_url, &format!("/v1beta/models/{model}:{method}"), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_的_gemini_地址按官方端点表拼() {
        assert_eq!(
            endpoint_url("https://opencode.ai/zen/v1", "gemini-3.8-flash", false),
            "https://opencode.ai/zen/v1/models/gemini-3.8-flash:generateContent"
        );
        assert_eq!(
            endpoint_url("https://opencode.ai/zen/v1/", "gemini-3.8-flash", true),
            "https://opencode.ai/zen/v1/models/gemini-3.8-flash:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn Google_官方地址沿用_cc_switch_的归一化() {
        assert_eq!(
            endpoint_url(
                "https://generativelanguage.googleapis.com",
                "gemini-3.8-flash",
                false
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
        // Google 域名 + 版本后缀：仍旧走归一化，避免拼出 /v1beta/v1beta
        assert_eq!(
            endpoint_url(
                "https://generativelanguage.googleapis.com/v1beta",
                "gemini-3.8-flash",
                false
            ),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
    }

    #[test]
    fn 写了_models_段的地址不再重复加() {
        assert_eq!(
            endpoint_url("https://relay.example/v1/models", "gemini-x", false),
            "https://relay.example/v1/models/gemini-x:generateContent"
        );
    }

    #[test]
    fn 中转站的自定路径不被改动() {
        // 不明来历的路径前缀：保持 cc-switch 的保守行为（原样接在后面）
        assert_eq!(
            endpoint_url("https://relay.example/custom/gemini", "gemini-x", false),
            "https://relay.example/custom/gemini/v1beta/models/gemini-x:generateContent"
        );
    }

    #[test]
    fn 完整端点原样使用() {
        assert_eq!(
            endpoint_url(
                "https://relay.example/x:generateContent?key=1",
                "gemini-x",
                true
            ),
            "https://relay.example/x:generateContent?key=1"
        );
    }
}
