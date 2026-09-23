//! UFP: 无状态签名信封（对应设计 D12）。
//!
//! Gemini 3 在多轮工具调用时要求把每个 `functionCall` / thought part 的
//! `thoughtSignature` 原样回传，否则会以「missing a thought_signature」拒绝。
//! 但下游是 Anthropic 协议，工具调用块和思考块上没有这样一个字段。
//!
//! cc-switch 的做法是在进程内存里存一份 shadow（按会话回放），重启即丢、
//! 多实例也不共享。这里换成无状态信封，把签名编码进两个客户端**一定会原样回传**
//! 的位置：
//!
//! - **工具调用**：塞进 `tool_use.id` 的后缀（`…_gs_<base64url>`）。
//!   客户端会把 id 原样放进历史与 `tool_result.tool_use_id`，所以它能回到网关。
//! - **思考/推理**：塞进 `thinking.signature` 或 `redacted_thinking.data`
//!   （前缀 `gemini-thought-v1:`），与 `reasoning_bridge.rs` 对 OpenAI 的处理同构。
//!
//! 这样网关重启、升级、甚至换台机器，签名都还在历史里跟着走。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

/// 工具 id 里签名的分隔符（不会出现在 Gemini 生成的 id 里）。
pub const ID_SEP: &str = "_gs_";
/// 思考签名信封的前缀。
pub const THOUGHT_PREFIX: &str = "gemini-thought-v1:";

/// 把签名编进工具 id。
pub fn encode_tool_id(base: &str, signature: &str) -> String {
    if signature.is_empty() {
        return base.to_string();
    }
    let base = bare_tool_id(base);
    format!(
        "{base}{ID_SEP}{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

/// 从工具 id 里取出（裸 id, 签名）。
pub fn decode_tool_id(id: &str) -> (String, Option<String>) {
    match id.find(ID_SEP) {
        Some(pos) => {
            let (bare, encoded) = id.split_at(pos);
            let encoded = &encoded[ID_SEP.len()..];
            match URL_SAFE_NO_PAD.decode(encoded.as_bytes()) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(sig) => (bare.to_string(), Some(sig)),
                    Err(_) => (id.to_string(), None),
                },
                Err(_) => (id.to_string(), None),
            }
        }
        None => (id.to_string(), None),
    }
}

/// 去掉信封，只留裸 id（用于判断是不是我们合成的 id）。
pub fn bare_tool_id(id: &str) -> String {
    match id.find(ID_SEP) {
        Some(pos) => id[..pos].to_string(),
        None => id.to_string(),
    }
}

/// 把签名编成思考块的信封。
pub fn encode_thought(signature: &str) -> String {
    format!(
        "{THOUGHT_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

/// 解析思考块的信封；不是我们的信封时返回 None。
pub fn decode_thought(value: &str) -> Option<String> {
    let encoded = value.strip_prefix(THOUGHT_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// 这个签名字段是不是我们放进去的（用于决定能不能当 thoughtSignature 回传）。
pub fn is_thought_envelope(value: &str) -> bool {
    value.starts_with(THOUGHT_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 工具_id_信封可往返() {
        let encoded = encode_tool_id("call_abc123", "sig-with-+/=chars");
        assert!(encoded.starts_with("call_abc123_gs_"), "{encoded}");
        assert!(!encoded.contains('+'), "用 url-safe base64：{encoded}");
        let (bare, sig) = decode_tool_id(&encoded);
        assert_eq!(bare, "call_abc123");
        assert_eq!(sig.as_deref(), Some("sig-with-+/=chars"));
    }

    #[test]
    fn 没有信封的_id_原样返回() {
        let (bare, sig) = decode_tool_id("call_plain");
        assert_eq!(bare, "call_plain");
        assert!(sig.is_none());
        assert_eq!(bare_tool_id("call_plain"), "call_plain");
    }

    #[test]
    fn 签名解码失败时退化为原_id() {
        let broken = "call_x_gs_!!!not-base64!!!";
        let (bare, sig) = decode_tool_id(broken);
        assert_eq!(bare, broken);
        assert!(sig.is_none());
    }

    #[test]
    fn 思考信封可往返() {
        let encoded = encode_thought("sig-thought-1");
        assert!(is_thought_envelope(&encoded));
        assert_eq!(decode_thought(&encoded).as_deref(), Some("sig-thought-1"));
        assert!(decode_thought("opaque-foreign-signature").is_none());
    }

    #[test]
    fn 重复编码不会叠加() {
        let once = encode_tool_id("call_1", "sig-1");
        let twice = encode_tool_id(&once, "sig-1");
        assert_eq!(once, twice, "裸 id 会被先去信封");
    }
}
