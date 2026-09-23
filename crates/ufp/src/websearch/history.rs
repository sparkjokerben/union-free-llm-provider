//! 请求方向的 WebSearch 改写。
//!
//! 客户端（Claude Code）发来的历史里，带着它上一轮拿到的服务端工具块：
//! `server_tool_use` 与 `web_search_tool_result`。上游模型并不认识这些块，
//! 所以这里把它们还原成「普通函数调用 + 工具结果」的一对，并且：
//!
//! 1. 工具定义从 `web_search_20250305` 换成普通函数 `web_search`；
//! 2. 搜索结果正文通过 `encrypted_content` 这个不透明信封随历史带回来
//!    （Claude Code 只做原样回传，不解析它）——这样多轮之后模型仍然看得到当初的
//!    搜索结果，而不是只剩几个链接；
//! 3. 顺手识别 Claude Code 的「固定形态搜索请求」（快速路径）：那种请求里第一次
//!    调用上游只是为了把查询词复述一遍，网关可以直接省掉这次调用。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};

use super::backends::{results_as_text, SearchItem};

/// 解析出来的搜索计划。
#[derive(Debug, Clone, Default)]
pub struct SearchPlan {
    /// 请求里是否带服务端 WebSearch 工具。
    pub has_tool: bool,
    /// 单次请求最多执行多少次搜索（取客户端 max_uses 与网关上限的较小值）。
    pub max_uses: u32,
    /// 命中快速路径时的查询词（可以直接去搜，不必先问上游）。
    pub fast_path_query: Option<String>,
}

/// 改写请求体，并给出搜索计划。
///
/// 返回的 body 可以交给转换器发给上游；原 body 不动（调用方自己决定怎么用）。
pub fn prepare_request(body: &Value, max_uses_cap: u32) -> (Value, SearchPlan) {
    let tools = body.get("tools").and_then(|t| t.as_array());
    let Some(tools) = tools else {
        return (body.clone(), SearchPlan::default());
    };
    let mut has_tool = false;
    let mut max_uses = max_uses_cap;
    for tool in tools {
        if super::is_web_search_tool(tool) {
            has_tool = true;
            if let Some(n) = tool.get("max_uses").and_then(|v| v.as_u64()) {
                max_uses = max_uses.min(n as u32);
            }
        }
    }
    if !has_tool {
        return (body.clone(), SearchPlan::default());
    }

    let mut out = body.clone();
    // 1) 工具定义：服务端工具 → 普通函数
    if let Some(tools) = out.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools.iter_mut() {
            if super::is_web_search_tool(tool) {
                *tool = json!({
                    "name": super::TOOL_NAME,
                    "description": "Search the web and return the most relevant results for a query. \
                                    Use it whenever the answer depends on current information.",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "The search query"}
                        },
                        "required": ["query"]
                    }
                });
            }
        }
    }
    // 2) 历史里的服务端工具块 → 函数调用对
    rewrite_history(&mut out);

    let plan = SearchPlan {
        has_tool: true,
        max_uses: max_uses.max(1),
        fast_path_query: detect_fast_path(&out),
    };
    (out, plan)
}

/// 把 assistant 消息里的 `server_tool_use` + `web_search_tool_result`
/// 还原成函数调用与工具结果。
fn rewrite_history(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    // 先收集所有结果块的内容（按 tool_use_id 索引）
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for msg in messages.iter() {
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("web_search_tool_result") {
                if let Some(id) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                    results.insert(id.to_string(), result_text_from_block(block));
                }
            }
        }
    }
    if results.is_empty() {
        return;
    }

    // 逐条消息改写：服务端工具块换掉，工具结果挪到紧随其后的 user 消息里
    let mut pending_results: Vec<Value> = Vec::new();
    let mut insert_after: Vec<usize> = Vec::new();
    for (idx, msg) in messages.iter_mut().enumerate() {
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let mut new_content: Vec<Value> = Vec::with_capacity(content.len());
        let mut local_results: Vec<Value> = Vec::new();
        for block in content.iter() {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("server_tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("srvtoolu_unknown")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or(json!({}));
                    new_content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": super::TOOL_NAME,
                        "input": input,
                    }));
                    if let Some(text) = results.get(&id) {
                        local_results.push(json!({
                            "type": "tool_result",
                            "tool_use_id": id,
                            "content": [{"type": "text", "text": text}],
                        }));
                    }
                }
                // 结果块本身不再作为内容留下（已经变成 tool_result）
                Some("web_search_tool_result") => {}
                _ => new_content.push(block.clone()),
            }
        }
        *content = new_content;
        if !local_results.is_empty() {
            pending_results.extend(local_results);
            insert_after.push(idx);
        }
    }

    // 把工具结果插到对应 assistant 消息之后的 user 消息里
    let mut offset = 0usize;
    for idx in insert_after {
        let target = idx + 1 + offset;
        // 结果必须跟在 user 消息里；如果下一条已是 user 消息就并进去，否则新建一条
        if let Some(next) = messages.get_mut(target) {
            if next.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(content) = next.get_mut("content").and_then(|c| c.as_array_mut()) {
                    let mut merged = std::mem::take(&mut pending_results);
                    merged.append(content);
                    *content = merged;
                }
                continue;
            }
        }
        let blocks = std::mem::take(&mut pending_results);
        messages.insert(
            target.min(messages.len()),
            json!({"role": "user", "content": blocks}),
        );
        offset += 1;
    }
}

/// 从结果块里还原出当初喂给模型的文本。
fn result_text_from_block(block: &Value) -> String {
    let query = block
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .to_string();
    let mut items: Vec<SearchItem> = Vec::new();
    if let Some(list) = block.get("content").and_then(|c| c.as_array()) {
        for item in list {
            if item.get("type").and_then(|t| t.as_str()) != Some("web_search_result") {
                continue;
            }
            let url = item
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            // 优先用我们自己塞进 encrypted_content 的完整载荷
            let decoded = item
                .get("encrypted_content")
                .and_then(|e| e.as_str())
                .and_then(decode_payload);
            match decoded {
                Some(item) => items.push(item),
                None => items.push(SearchItem {
                    url,
                    title,
                    snippet: String::new(),
                    published: item
                        .get("page_age")
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string()),
                }),
            }
        }
    }
    results_as_text(&query, &items)
}

/// 把一条搜索结果编码成可以塞进 `encrypted_content` 的载荷。
pub fn encode_payload(item: &SearchItem) -> String {
    let value = json!({
        "u": item.url,
        "t": item.title,
        "s": item.snippet,
        "p": item.published,
    });
    B64.encode(serde_json::to_vec(&value).unwrap_or_default())
}

pub fn decode_payload(s: &str) -> Option<SearchItem> {
    let bytes = B64.decode(s.as_bytes()).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    Some(SearchItem {
        url: value.get("u")?.as_str()?.to_string(),
        title: value
            .get("t")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        snippet: value
            .get("s")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        published: value
            .get("p")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string()),
    })
}

/// 识别 Claude Code 的固定形态搜索请求。
///
/// 形态（本机 2.1.280 实测）：系统提示固定为
/// `You are an assistant for performing a web search tool use`，
/// 只有一条 user 消息，文本为 `Perform a web search for the query: <query>`，
/// 并且强制调用 web_search。命中就直接拿 query 去搜，省掉一次上游调用。
pub fn detect_fast_path(body: &Value) -> Option<String> {
    const SYSTEM_HINT: &str = "You are an assistant for performing a web search tool use";
    const USER_PREFIX: &str = "Perform a web search for the query:";

    let system_text = match body.get("system") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    if !system_text.contains(SYSTEM_HINT) {
        return None;
    }
    let messages = body.get("messages")?.as_array()?;
    if messages.len() != 1 {
        return None;
    }
    let content = messages[0].get("content")?;
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let query = text.trim().strip_prefix(USER_PREFIX)?.trim();
    if query.is_empty() {
        return None;
    }
    Some(query.to_string())
}

/// 快速路径的请求改写：把 Claude Code 那个「只为了复述查询词」的请求，
/// 换成「带着搜索结果直接回答」的请求。
pub fn rewrite_for_fast_path(body: &mut Value, query: &str, results_text: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    obj.insert(
        "system".into(),
        json!(
            "You are a helpful assistant with access to web search results. \
             Answer the user's query using the provided results, and cite the sources as markdown links."
        ),
    );
    // 续写/回答阶段不能再强制调用工具，否则模型会一直循环搜索
    obj.insert("tool_choice".into(), json!({"type": "auto"}));
    obj.insert(
        "messages".into(),
        json!([{
            "role": "user",
            "content": [{
                "type": "text",
                "text": format!(
                    "Perform a web search for the query: {query}\n\nHere are the search results:\n\n{results_text}\n\nAnswer the query now using only these results."
                )
            }]
        }]),
    );
}

/// 构造「带着搜索结果继续」的请求体：把函数调用与结果接到历史后面。
pub fn append_search_round(
    body: &mut Value,
    call_id: &str,
    query: &str,
    results_text: &str,
    relax_tool_choice: bool,
) {
    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return;
    };
    messages.push(json!({
        "role": "assistant",
        "content": [{
            "type": "tool_use",
            "id": call_id,
            "name": super::TOOL_NAME,
            "input": {"query": query},
        }],
    }));
    messages.push(json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": call_id,
            "content": [{"type": "text", "text": results_text}],
        }],
    }));
    if relax_tool_choice {
        if let Some(obj) = body.as_object_mut() {
            // 续写时不能再强制调用工具，否则模型会一直循环搜索。
            obj.insert("tool_choice".into(), json!({"type": "auto"}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_with_history() -> Value {
        json!({
            "model": "x",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "今天天气如何"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "我查一下"},
                    {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "北京天气"}},
                    {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": [
                        {"type": "web_search_result", "url": "https://weather.example/1",
                         "title": "北京天气", "encrypted_content": encode_payload(&SearchItem{
                             url: "https://weather.example/1".into(), title: "北京天气".into(),
                             snippet: "今天晴，20 度".into(), published: None }), "page_age": null}
                    ]}
                ]},
            ],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
        })
    }

    #[test]
    fn 工具定义换成普通函数() {
        let (out, plan) = prepare_request(&body_with_history(), 8);
        assert!(plan.has_tool);
        assert_eq!(plan.max_uses, 8);
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "web_search");
        assert!(tools[0].get("type").is_none(), "不该再带服务端工具的 type");
        assert_eq!(tools[0]["input_schema"]["required"][0], "query");
    }

    #[test]
    fn 历史里的服务端工具块还原成函数调用对() {
        let (out, _) = prepare_request(&body_with_history(), 8);
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3, "应新增一条 user 消息携带 tool_result");
        let assistant = &messages[1]["content"];
        let has_tool_use = assistant
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "tool_use" && b["id"] == "srvtoolu_1");
        assert!(has_tool_use, "{assistant}");
        let has_server_block = assistant
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["type"] == "server_tool_use" || b["type"] == "web_search_tool_result");
        assert!(!has_server_block, "服务端工具块不该留给上游：{assistant}");
        let tool_result = &messages[2]["content"][0];
        assert_eq!(tool_result["type"], "tool_result");
        let text = tool_result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("北京天气"), "{text}");
        assert!(text.contains("今天晴，20 度"), "摘要应随信封带回来：{text}");
    }

    #[test]
    fn 没有搜索工具时原样返回() {
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let (out, plan) = prepare_request(&body, 8);
        assert!(!plan.has_tool);
        assert_eq!(out, body);
    }

    #[test]
    fn 识别快速路径() {
        let body = json!({
            "system": "You are an assistant for performing a web search tool use\n\nYou are a helpful assistant.",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "Perform a web search for the query: 北京 天气"}]}],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
            "tool_choice": {"type": "tool", "name": "web_search"},
        });
        let (_, plan) = prepare_request(&body, 8);
        assert_eq!(plan.fast_path_query.as_deref(), Some("北京 天气"));
    }

    #[test]
    fn 普通请求不算快速路径() {
        let body = json!({
            "system": "You are Claude Code",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "帮我查下天气"}]}],
            "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        });
        let (_, plan) = prepare_request(&body, 8);
        assert!(plan.fast_path_query.is_none());
    }

    #[test]
    fn 信封编解码可往返() {
        let item = SearchItem {
            url: "https://a.example/1".into(),
            title: "标题".into(),
            snippet: "正文".into(),
            published: Some("2025-01-01".into()),
        };
        let encoded = encode_payload(&item);
        let decoded = decode_payload(&encoded).unwrap();
        assert_eq!(decoded.url, item.url);
        assert_eq!(decoded.snippet, item.snippet);
        assert_eq!(decoded.published, item.published);
    }

    #[test]
    fn 追加搜索轮次会放宽工具选择() {
        let mut body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tool_choice": {"type": "tool", "name": "web_search"},
        });
        append_search_round(&mut body, "call_1", "查询", "结果文本", true);
        assert_eq!(body["tool_choice"]["type"], "auto");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    }
}
