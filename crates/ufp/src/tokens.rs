//! 本地 token 估算。
//!
//! 用途有二：`/v1/messages/count_tokens`（cc-switch 直接返回 404，这里给个可用值），
//! 以及路由时判断请求是否放得进某个条目的上下文窗口。
//!
//! 刻意不调上游计数接口：不消耗免费额度、毫秒级返回，而且各家口径本来就不一致。
//! 估算法偏保守（宁可略高），CJK 按 1 字 ≈ 1 token，其余按 4 字节 ≈ 1 token，
//! 图片按 base64 体积折算，文档（PDF）按字节数折算。

use serde_json::Value;

/// 非 CJK 文本的字节/token 比。
const BYTES_PER_TOKEN: f64 = 4.0;
/// 图片 token 估算上限（Anthropic 对大图约 1600 token 封顶）。
const IMAGE_MAX_TOKENS: f64 = 1600.0;
/// 每多少 base64 字节折算 1 token（经验值：一张 1MB 的 PNG 约 1300 token）。
const IMAGE_BYTES_PER_TOKEN: f64 = 800.0;
/// 文档（PDF）每多少字节折算 1 token。
const DOC_BYTES_PER_TOKEN: f64 = 1200.0;

/// 估算一段文本的 token 数。
pub fn estimate_text(s: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other_bytes = 0u64;
    for ch in s.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other_bytes += ch.len_utf8() as u64;
        }
    }
    cjk + ((other_bytes as f64) / BYTES_PER_TOKEN).ceil() as u64
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF      // 谚文字母
        | 0x2E80..=0x2EFF    // 部首
        | 0x3000..=0x303F    // CJK 标点
        | 0x3040..=0x30FF    // 假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本区
        | 0xAC00..=0xD7AF    // 谚文音节
        | 0xF900..=0xFAFF    // 兼容表意
        | 0x20000..=0x2FA1F  // 扩展 B 及以后
    )
}

/// 估算一个 Anthropic 内容块（或任意 JSON 结构）的 token 数。
pub fn estimate_value(v: &Value) -> u64 {
    match v {
        Value::String(s) => estimate_text(s),
        Value::Array(items) => items.iter().map(estimate_value).sum(),
        Value::Object(map) => estimate_object(map),
        Value::Number(_) | Value::Bool(_) | Value::Null => 1,
    }
}

fn estimate_object(map: &serde_json::Map<String, Value>) -> u64 {
    let block_type = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match block_type {
        "image" => estimate_media(map, IMAGE_BYTES_PER_TOKEN, IMAGE_MAX_TOKENS),
        "document" => estimate_media(map, DOC_BYTES_PER_TOKEN, IMAGE_MAX_TOKENS * 4.0),
        "thinking" | "redacted_thinking" => {
            // 思考块的 signature 是编码后的状态，不是真实文本量，按正文估算即可。
            map.get("thinking")
                .map(estimate_value)
                .unwrap_or(64)
                .max(64)
        }
        "tool_use" | "server_tool_use" => {
            let name = map.get("name").map(estimate_value).unwrap_or(0);
            let input = map.get("input").map(estimate_value).unwrap_or(0);
            name + input + 8
        }
        "tool_result" | "web_search_tool_result" => {
            map.get("content").map(estimate_value).unwrap_or(0) + 8
        }
        _ => {
            // 通用回退：把所有字段值加起来，再补一点结构开销。
            let body: u64 = map.values().map(estimate_value).sum();
            body + map.len() as u64
        }
    }
}

fn estimate_media(map: &serde_json::Map<String, Value>, bytes_per_token: f64, cap: f64) -> u64 {
    // base64 图片/文档：按 data 字段的体积折算。
    let bytes = map
        .get("source")
        .and_then(|s| s.get("data"))
        .and_then(|d| d.as_str())
        .map(|d| d.len() as f64)
        .unwrap_or(0.0);
    let tokens = (bytes / bytes_per_token).ceil();
    tokens.clamp(32.0, cap) as u64
}

/// 估算一次 Messages 请求的总输入量（messages + system + tools）。
pub fn estimate_request(body: &Value) -> u64 {
    let mut total = 0u64;
    if let Some(system) = body.get("system") {
        total += estimate_value(system);
    }
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for m in messages {
            total += estimate_value(m);
            // 每条消息的角色、分隔等固定开销。
            total += 4;
        }
    }
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        for t in tools {
            total += estimate_value(t);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 估算中文按字计() {
        assert_eq!(estimate_text("你好世界"), 4);
    }

    #[test]
    fn 估算英文按字节折算() {
        // "hello world" 11 字节 → ceil(11/4) = 3
        assert_eq!(estimate_text("hello world"), 3);
    }

    #[test]
    fn 请求总量包含系统提示与工具() {
        let body = json!({
            "system": "你是助手",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "Read", "description": "读取文件", "input_schema": {"type": "object"}}]
        });
        let n = estimate_request(&body);
        assert!(n >= 10, "估算结果应覆盖各部分，实际 {n}");
    }

    #[test]
    fn 图片有下限与上限() {
        let small = json!({"type": "image", "source": {"type": "base64", "data": "aaaa"}});
        assert!(estimate_value(&small) >= 32);
        let big =
            json!({"type": "image", "source": {"type": "base64", "data": "a".repeat(10_000_000)}});
        assert_eq!(estimate_value(&big), IMAGE_MAX_TOKENS as u64);
    }
}
