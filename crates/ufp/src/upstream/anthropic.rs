//! Anthropic 原生上游：请求体基本原样透传，只改写 `model`。
//!
//! 不做「扮演 Claude Code」那一套：不注入 `anthropic-beta: claude-code-20250219`，
//! 也不强制任何 beta。客户端发来的 `anthropic-version` 透传，缺省用 `2023-06-01`。
//! 需要特定 beta 的兼容渠道，在渠道配置的附加头里加 `anthropic-beta`。

use bytes::Bytes;
use serde_json::Value;

use super::{join_endpoint, with_model, BuildCtx, UpstreamRequest};
use crate::store::Candidate;
use ufp_convert::thinking_policy;
use ufp_convert::ConvertError;

pub fn prepare(cand: &Candidate, ctx: &BuildCtx<'_>) -> Result<UpstreamRequest, ConvertError> {
    let mut body = prepare_body(cand, ctx);
    super::shape_body(cand, ctx, &mut body);
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

/// 改写 `model`、按 D14 的「思考开到最大」改写 `thinking` / `output_config`，
/// 然后摘掉私有标记（这个渠道是原样透传，标记不能上线）。
fn prepare_body(cand: &Candidate, ctx: &BuildCtx<'_>) -> Value {
    let mut body = with_model(ctx.client_body, &cand.entry.upstream_model);
    if let Some(mode) = thinking_policy::mode(&body) {
        thinking_policy::apply_anthropic_max_thinking(&mut body, &cand.entry.upstream_model, mode);
    }
    thinking_policy::strip_marker(&mut body);
    body
}

pub(crate) fn serde_err(e: serde_json::Error) -> ConvertError {
    ConvertError::TransformError(format!("序列化上游请求失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChannelCfg, EntryCfg, KeyCfg, Protocol};
    use serde_json::json;

    fn candidate(model: &str) -> Candidate {
        Candidate {
            channel: ChannelCfg {
                id: 1,
                name: "ch".into(),
                protocol: Protocol::Anthropic,
                base_url: "https://example.test".into(),
                extra_headers: Vec::new(),
                enabled: true,
                client_profile: Default::default(),
                max_thinking: true,
            },
            key: KeyCfg {
                id: 1,
                channel_id: 1,
                label: "k".into(),
                api_key: "sk".into(),
                enabled: true,
            },
            entry: EntryCfg {
                id: 1,
                channel_id: 1,
                upstream_model: model.into(),
                tier: 1,
                max_context: 200_000,
                vision: true,
                pdf: false,
                enabled: true,
                thinking_mode: String::new(),
            },
        }
    }

    fn prepare_with(model: &str, client: Value) -> Value {
        let cand = candidate(model);
        let ctx = BuildCtx {
            client_body: &client,
            client_anthropic_version: None,
            stream: false,
            opencode: None,
        };
        prepare_body(&cand, &ctx)
    }

    #[test]
    fn 新模型走_adaptive_加_max_effort() {
        let client = json!({
            "max_tokens": 32000,
            "thinking": {"type": "disabled"},
            "_ufp_reasoning": "max",
        });
        let body = prepare_with("claude-sonnet-5", client);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "max");
        assert!(body.get("_ufp_reasoning").is_none(), "私有标记不上线");
    }

    #[test]
    fn 旧模型走预算_不动_max_tokens() {
        let client = json!({"max_tokens": 32000, "_ufp_reasoning": "max"});
        let body = prepare_with("claude-sonnet-4-5-20250929", client);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 31_999);
        assert_eq!(body["max_tokens"], 32000);
    }

    #[test]
    fn 没有标记时原样透传() {
        let client = json!({"max_tokens": 1024, "thinking": {"type": "disabled"}});
        let body = prepare_with("claude-sonnet-5", client);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn 阶梯退到_alt_时_effort_降一档() {
        let client = json!({"max_tokens": 32000, "_ufp_reasoning": "alt"});
        let body = prepare_with("claude-opus-4-6", client);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn 不支持时什么都不加() {
        let client = json!({"max_tokens": 32000, "_ufp_reasoning": "unsupported"});
        let body = prepare_with("claude-sonnet-5", client);
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    }
}
