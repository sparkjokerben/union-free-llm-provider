//! 确定性矫正器。
//!
//! 每个矫正器都回答一个问题：「这条错误信息，我知道该怎么改写请求吗？」
//! 知道就改写请求体并返回矫正器名字，转发层据此在同候选上重试一次。
//!
//! 复用 cc-switch 的两个整流器（思考签名、思考预算），另外补两个：
//! 图片降级、max_tokens 越界下夹。
//!
//! Gemini 工具 schema 走 `parameters` 还是 `parametersJsonSchema` 由转换层
//! （`ufp_convert::providers::gemini_schema`）在发请求前决定，这里不参与。

use serde_json::{json, Value};

use ufp_convert::thinking_budget_rectifier::{
    rectify_thinking_budget, should_rectify_thinking_budget,
};
use ufp_convert::thinking_rectifier::{
    rectify_anthropic_request, should_rectify_thinking_signature,
};
use ufp_convert::types::RectifierConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinRectifier {
    ThinkingSignature,
    ThinkingBudget,
    /// 上游不认当前思考参数写法：换一个等价的高配写法（不靠关掉思考通过）。
    ThinkingForm,
    /// 阶梯走完：这个模型完全不支持思考（转发层据此禁用条目）。
    ThinkingUnsupported,
    MediaFallback,
    MaxTokensClamp,
}

impl BuiltinRectifier {
    pub fn as_str(self) -> &'static str {
        match self {
            BuiltinRectifier::ThinkingSignature => "thinking_signature",
            BuiltinRectifier::ThinkingBudget => "thinking_budget",
            BuiltinRectifier::ThinkingForm => "thinking_form",
            BuiltinRectifier::ThinkingUnsupported => "thinking_unsupported",
            BuiltinRectifier::MediaFallback => "media_fallback",
            BuiltinRectifier::MaxTokensClamp => "max_tokens_clamp",
        }
    }
}

/// 按错误信息挑一个矫正器改写请求体。返回 `Some` 表示改了、可以在同候选上重试。
///
/// `tried` 是本候选上已经用过的矫正器（每个只用一次，避免反复改写）。
// edition 2021 用不了 let-chains，这里的嵌套 if 是刻意的。
#[allow(clippy::collapsible_if)]
pub fn apply_builtin(
    body: &mut Value,
    error_message: &str,
    cfg: &RectifierConfig,
    tried: &[BuiltinRectifier],
) -> Option<BuiltinRectifier> {
    let has = |r: BuiltinRectifier| tried.contains(&r);

    if !has(BuiltinRectifier::ThinkingSignature)
        && should_rectify_thinking_signature(Some(error_message), cfg)
    {
        let result = rectify_anthropic_request(body);
        if result.applied {
            return Some(BuiltinRectifier::ThinkingSignature);
        }
    }

    if !has(BuiltinRectifier::ThinkingBudget)
        && should_rectify_thinking_budget(Some(error_message), cfg)
    {
        let result = rectify_thinking_budget(body);
        if result.applied {
            return Some(BuiltinRectifier::ThinkingBudget);
        }
    }

    // UFP: 思考参数阶梯（D14）。只在渠道勾了「思考开到最大」（请求体带私有标记）
    // 且上游明说不吃思考参数时动：先降一档写法，都不行就标记「不支持」。
    if cfg.enabled && ufp_convert::thinking_policy::is_thinking_rejection(error_message) {
        if let Some(mode) = ufp_convert::thinking_policy::mode(body) {
            match mode.downgrade() {
                // 阶梯要连降两档（max→alt→legacy），不用 `tried` 限次数：
                // 降不到底就是 None，天然有界。
                Some(next) => {
                    ufp_convert::thinking_policy::set_mode(body, next);
                    return Some(BuiltinRectifier::ThinkingForm);
                }
                None if mode != ufp_convert::thinking_policy::Mode::Unsupported
                    && !has(BuiltinRectifier::ThinkingUnsupported) =>
                {
                    ufp_convert::thinking_policy::set_mode(
                        body,
                        ufp_convert::thinking_policy::Mode::Unsupported,
                    );
                    return Some(BuiltinRectifier::ThinkingUnsupported);
                }
                _ => {}
            }
        }
    }

    if !has(BuiltinRectifier::MaxTokensClamp) {
        if let Some(limit) = max_tokens_limit(error_message) {
            if clamp_max_tokens(body, limit) {
                return Some(BuiltinRectifier::MaxTokensClamp);
            }
        }
    }

    if !has(BuiltinRectifier::MediaFallback)
        && cfg.request_media_fallback
        && looks_like_media_rejection(error_message)
    {
        if replace_images_with_placeholder(body) > 0 {
            return Some(BuiltinRectifier::MediaFallback);
        }
    }

    None
}

/// 从错误信息里找 `max_tokens` 的上限。
///
/// 典型措辞：
/// - `max_tokens: 200000 > 65536, which is the maximum allowed number of output tokens`
/// - `max_completion_tokens must be less than or equal to 16384`
/// - `Invalid max_tokens: must be <= 8192`
fn max_tokens_limit(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("max_tokens")
        || lower.contains("max_completion_tokens")
        || lower.contains("output tokens")
        || lower.contains("max_output_tokens"))
    {
        return None;
    }
    // 取错误信息里出现的数字，挑「像是上限」的那个：优先取 > 与 <=/> 后面的数。
    let bytes = message.as_bytes();
    let mut best: Option<u64> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if let Ok(n) = message[start..i].parse::<u64>() {
                // 只认 1000 以上的数（token 上限不会是个位数）。
                if n >= 1000 {
                    best = Some(best.map_or(n, |b: u64| b.min(n)));
                }
            }
        } else {
            i += 1;
        }
    }
    best
}

/// 把 `max_tokens` 夹到上限以内（只往下夹，不往上抬）。
fn clamp_max_tokens(body: &mut Value, limit: u64) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    let field = if obj.contains_key("max_tokens") {
        "max_tokens"
    } else if obj.contains_key("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        return false;
    };
    let current = obj.get(field).and_then(|v| v.as_u64()).unwrap_or(0);
    if current <= limit {
        return false;
    }
    obj.insert(field.into(), json!(limit));
    true
}

/// 判断错误是否像「上游不接受图片/文档」。
fn looks_like_media_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let media = [
        "image",
        "vision",
        "multimodal",
        "media",
        "图片",
        "document",
        "pdf",
    ];
    let reject = [
        "not support",
        "unsupported",
        "does not support",
        "can't process",
        "cannot process",
        "invalid content",
        "only text",
        "text-only",
        "不支持",
    ];
    media.iter().any(|m| lower.contains(m)) && reject.iter().any(|r| lower.contains(r))
}

/// 把所有图片块换成占位文本（只在没有视觉能力的条目上兜底；优先走路由迁移）。
fn replace_images_with_placeholder(body: &mut Value) -> usize {
    let mut replaced = 0;
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for block in content.iter_mut() {
                let is_media = block
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "image" || t == "document")
                    .unwrap_or(false);
                if is_media {
                    let label = if block.get("type").and_then(|t| t.as_str()) == Some("document") {
                        "[Unsupported Document]"
                    } else {
                        "[Unsupported Image]"
                    };
                    *block = json!({"type": "text", "text": label});
                    replaced += 1;
                }
            }
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 识别并下夹_max_tokens() {
        let mut body = json!({"max_tokens": 200000, "messages": []});
        let applied = apply_builtin(
            &mut body,
            "max_tokens: 200000 > 65536, which is the maximum allowed number of output tokens",
            &RectifierConfig::default(),
            &[],
        );
        assert_eq!(applied, Some(BuiltinRectifier::MaxTokensClamp));
        assert_eq!(body["max_tokens"], 65536);
    }

    #[test]
    fn 上限更高时不动() {
        let mut body = json!({"max_tokens": 4096, "messages": []});
        assert!(apply_builtin(
            &mut body,
            "max_tokens must be less than or equal to 65536",
            &RectifierConfig::default(),
            &[]
        )
        .is_none());
    }

    #[test]
    fn 图片被拒时替换为占位文本() {
        let mut body = json!({
            "max_tokens": 100,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "看看"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}
            ]}]
        });
        let applied = apply_builtin(
            &mut body,
            "This model does not support image input",
            &RectifierConfig::default(),
            &[],
        );
        assert_eq!(applied, Some(BuiltinRectifier::MediaFallback));
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            "[Unsupported Image]"
        );
    }

    #[test]
    fn 上游不认思考参数时逐级换写法_不关掉思考() {
        use ufp_convert::thinking_policy::{mode, set_mode, Mode};
        let mut body = json!({"max_tokens": 100});
        set_mode(&mut body, Mode::Max);
        let applied = apply_builtin(
            &mut body,
            "Unsupported parameter: 'reasoning' is not supported with this model.",
            &RectifierConfig::default(),
            &[],
        );
        assert_eq!(applied, Some(BuiltinRectifier::ThinkingForm));
        assert_eq!(mode(&body), Some(Mode::Alt), "换写法，不关思考");

        let applied = apply_builtin(
            &mut body,
            "Unknown name \"reasoning\"",
            &RectifierConfig::default(),
            &[BuiltinRectifier::ThinkingForm],
        );
        assert_eq!(applied, Some(BuiltinRectifier::ThinkingForm));
        assert_eq!(mode(&body), Some(Mode::Legacy));

        let applied = apply_builtin(
            &mut body,
            "thinking is not allowed here",
            &RectifierConfig::default(),
            &[BuiltinRectifier::ThinkingForm],
        );
        assert_eq!(applied, Some(BuiltinRectifier::ThinkingUnsupported));
        assert_eq!(mode(&body), Some(Mode::Unsupported));
    }

    #[test]
    fn 没有强制标记的渠道不碰阶梯() {
        let mut body = json!({"max_tokens": 100});
        assert_eq!(
            apply_builtin(
                &mut body,
                "Unsupported parameter: 'reasoning'",
                &RectifierConfig::default(),
                &[]
            ),
            None
        );
    }

    #[test]
    fn 每个矫正器只用一次() {
        let mut body = json!({"max_tokens": 200000, "messages": []});
        let tried = [BuiltinRectifier::MaxTokensClamp];
        assert!(apply_builtin(
            &mut body,
            "max_tokens: 200000 > 65536",
            &RectifierConfig::default(),
            &tried
        )
        .is_none());
    }

    #[test]
    fn 思考签名错误走上游整流器() {
        let mut body = json!({
            "max_tokens": 100,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "推理", "signature": "bad-sig"}
            ]}]
        });
        let applied = apply_builtin(
            &mut body,
            "Invalid `signature` in `thinking` block",
            &RectifierConfig::default(),
            &[],
        );
        assert_eq!(applied, Some(BuiltinRectifier::ThinkingSignature));
    }
}
