//! 请求骨架：把请求体压成「结构 + 字段名 + 尺寸」，去掉全部对话正文。
//!
//! 这是把报错交给另一个上游去分析时发出去的东西（D11）。目标：
//! - 保留模型排查问题需要的一切：消息角色、内容块类型、工具名与 input_schema、
//!   各种结构字段（tool_choice、thinking、max_tokens）；
//! - 不发送对话正文、图片数据、以及任何可能敏感的内容；
//! - 控制在十几 KB 以内，别把分析条目的上下文吃满。

use serde_json::{json, Map, Value};

/// 骨架的目标上限（字节）。超了就把工具 schema 继续削。
const TARGET_BYTES: usize = 16 * 1024;
/// 单个字符串字段（描述等）保留多少字符。
const STRING_KEEP: usize = 200;
/// schema 里数组（enum / anyOf 等）最多保留多少项。
const ARRAY_KEEP: usize = 20;
/// schema 最大递归深度。
const DEPTH_LIMIT: usize = 6;

/// 生成请求骨架。
pub fn build(body: &Value) -> Value {
    let mut out = Map::new();
    if let Some(model) = body.get("model") {
        out.insert("model".into(), model.clone());
    }
    // 顶层的标量/结构字段原样保留（它们正是排查问题的关键）
    for key in [
        "max_tokens",
        "stream",
        "tool_choice",
        "thinking",
        "output_config",
        "temperature",
        "top_p",
        "stop_sequences",
        "metadata_present",
    ] {
        if let Some(v) = body.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if body.get("metadata").is_some() {
        out.insert("metadata_present".into(), json!(true));
    }
    if let Some(system) = body.get("system") {
        out.insert(
            "system".into(),
            match system {
                Value::String(s) => json!(format!("<system {} 字符>", s.chars().count())),
                Value::Array(blocks) => json!(format!("<system {} 块>", blocks.len())),
                other => other.clone(),
            },
        );
    }

    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        let msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let role = m.get("role").cloned().unwrap_or(json!("user"));
                let content = match m.get("content") {
                    Some(Value::String(s)) => json!(format!("<text {} 字符>", s.chars().count())),
                    Some(Value::Array(blocks)) => {
                        json!(blocks.iter().map(block_skeleton).collect::<Vec<_>>())
                    }
                    other => other.cloned().unwrap_or(Value::Null),
                };
                json!({"role": role, "content": content})
            })
            .collect();
        out.insert("messages".into(), json!(msgs));
    }

    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        let tools: Vec<Value> = tools.iter().map(tool_skeleton).collect();
        out.insert("tools".into(), json!(tools));
    }

    let mut value = Value::Object(out);
    // 超预算就先砍工具描述，再砍 schema 里的字符串
    if serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) > TARGET_BYTES {
        strip_descriptions(&mut value);
    }
    if serde_json::to_vec(&value).map(|v| v.len()).unwrap_or(0) > TARGET_BYTES {
        strip_schema_strings(&mut value);
    }
    value
}

fn block_skeleton(block: &Value) -> Value {
    let ty = block.get("type").and_then(|t| t.as_str()).unwrap_or("?");
    match ty {
        "text" => json!({"type": "text", "text": placeholder_text(block.get("text"))}),
        "thinking" => {
            json!({"type": "thinking", "thinking": placeholder_text(block.get("thinking"))})
        }
        "redacted_thinking" => json!({"type": "redacted_thinking", "data": "<签名>"}),
        "image" | "document" => {
            let media = block
                .get("source")
                .and_then(|s| s.get("media_type"))
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let bytes = block
                .get("source")
                .and_then(|s| s.get("data"))
                .and_then(|d| d.as_str())
                .map(|d| d.len())
                .unwrap_or(0);
            json!({"type": ty, "media_type": media, "data_bytes": bytes})
        }
        "tool_use" | "server_tool_use" => json!({
            "type": ty,
            "id": block.get("id").cloned().unwrap_or(json!("")),
            "name": block.get("name").cloned().unwrap_or(json!("")),
            "input": value_skeleton(block.get("input"), 0),
        }),
        "tool_result" => json!({
            "type": "tool_result",
            "tool_use_id": block.get("tool_use_id").cloned().unwrap_or(json!("")),
            "is_error": block.get("is_error").cloned().unwrap_or(json!(false)),
            "content": match block.get("content") {
                Some(Value::Array(items)) => json!(items.iter().map(block_skeleton).collect::<Vec<_>>()),
                Some(Value::String(s)) => json!(format!("<text {} 字符>", s.chars().count())),
                other => other.cloned().unwrap_or(Value::Null),
            },
        }),
        _ => block.clone(),
    }
}

fn tool_skeleton(tool: &Value) -> Value {
    let mut out = Map::new();
    for key in ["type", "name"] {
        if let Some(v) = tool.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(d) = tool.get("description").and_then(|d| d.as_str()) {
        out.insert("description".into(), json!(truncate(d, STRING_KEEP)));
    }
    if let Some(schema) = tool.get("input_schema") {
        out.insert("input_schema".into(), schema_skeleton(schema, 0));
    }
    Value::Object(out)
}

/// schema 骨架：保留关键字与结构，压缩数组与长字符串。
fn schema_skeleton(schema: &Value, depth: usize) -> Value {
    if depth > DEPTH_LIMIT {
        return json!("<过深省略>");
    }
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                match k.as_str() {
                    "properties" => {
                        if let Some(props) = v.as_object() {
                            let mut p = Map::new();
                            for (name, sub) in props.iter().take(60) {
                                p.insert(name.clone(), schema_skeleton(sub, depth + 1));
                            }
                            out.insert("properties".into(), Value::Object(p));
                        }
                    }
                    "enum" | "anyOf" | "oneOf" | "allOf" => {
                        if let Some(items) = v.as_array() {
                            let kept: Vec<Value> = items
                                .iter()
                                .take(ARRAY_KEEP)
                                .map(|i| schema_skeleton(i, depth + 1))
                                .collect();
                            out.insert(k.clone(), json!(kept));
                            if items.len() > ARRAY_KEEP {
                                out.insert(format!("{k}_total"), json!(items.len()));
                            }
                        }
                    }
                    "items" | "additionalProperties" | "not" => {
                        out.insert(k.clone(), schema_skeleton(v, depth + 1));
                    }
                    _ => {
                        out.insert(
                            k.clone(),
                            match v {
                                Value::String(s) => json!(truncate(s, STRING_KEEP)),
                                other => other.clone(),
                            },
                        );
                    }
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => json!(items
            .iter()
            .take(ARRAY_KEEP)
            .map(|i| schema_skeleton(i, depth + 1))
            .collect::<Vec<_>>()),
        Value::String(s) => json!(truncate(s, STRING_KEEP)),
        other => other.clone(),
    }
}

/// tool_use 的 input：保留结构与字段名，值换成占位符。
fn value_skeleton(value: Option<&Value>, depth: usize) -> Value {
    if depth > 4 {
        return json!("<过深省略>");
    }
    match value {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Object(map)) => {
            let mut out = Map::new();
            for (k, v) in map.iter().take(40) {
                out.insert(k.clone(), value_skeleton(Some(v), depth + 1));
            }
            Value::Object(out)
        }
        Some(Value::Array(items)) => json!(items
            .iter()
            .take(ARRAY_KEEP)
            .map(|i| value_skeleton(Some(i), depth + 1))
            .collect::<Vec<_>>()),
        Some(Value::String(s)) => json!(format!("<字符串 {} 字符>", s.chars().count())),
        Some(other) => other.clone(),
    }
}

fn placeholder_text(value: Option<&Value>) -> Value {
    match value.and_then(|v| v.as_str()) {
        Some(s) => json!(format!("<text {} 字符>", s.chars().count())),
        None => json!("<text>"),
    }
}

fn truncate(s: &str, keep: usize) -> String {
    if s.chars().count() <= keep {
        return s.to_string();
    }
    let head: String = s.chars().take(keep).collect();
    format!("{head}…（共 {} 字符）", s.chars().count())
}

fn strip_descriptions(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        Value::Object(map) => {
            map.remove("description");
            map.values_mut().for_each(strip_descriptions);
        }
        _ => {}
    }
}

fn strip_schema_strings(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_schema_strings),
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k == "enum" {
                    if let Some(items) = v.as_array() {
                        *v = json!(format!("<{} 个取值>", items.len()));
                    }
                } else {
                    strip_schema_strings(v);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 骨架里没有正文只有结构() {
        let body = json!({
            "model": "gpt-4o-mini",
            "max_tokens": 1024,
            "system": "你是一个很长的系统提示很长的系统提示",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "这是秘密对话内容"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"path": "/secret/path"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "文件内容很机密"}]}
                ]}
            ],
            "tools": [{"name": "Read", "description": "读取文件",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}],
            "metadata": {"user_id": "u_session_abc"}
        });
        let skeleton = build(&body);
        let text = serde_json::to_string(&skeleton).unwrap();
        assert!(!text.contains("这是秘密对话内容"), "{text}");
        assert!(!text.contains("/secret/path"), "{text}");
        assert!(!text.contains("文件内容很机密"), "{text}");
        assert!(!text.contains("u_session_abc"), "{text}");
        // 结构与字段名必须留下来
        assert!(text.contains("\"required\":[\"path\"]"), "{text}");
        assert!(text.contains("\"name\":\"Read\""), "{text}");
        assert!(text.contains("\"tool_result\""), "{text}");
        assert!(text.contains("metadata_present"), "{text}");
    }

    #[test]
    fn 巨型_schema_会被压到预算内() {
        let mut props = Map::new();
        for i in 0..400 {
            props.insert(
                format!("field_{i}"),
                json!({"type": "string", "enum": (0..200).map(|n| format!("v{n}")).collect::<Vec<_>>()}),
            );
        }
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "Big", "input_schema": {"type": "object", "properties": props}}]
        });
        let skeleton = build(&body);
        let size = serde_json::to_vec(&skeleton).unwrap().len();
        assert!(
            size < TARGET_BYTES * 2,
            "骨架应被压到 32KB 以内，实际 {size}"
        );
    }
}
