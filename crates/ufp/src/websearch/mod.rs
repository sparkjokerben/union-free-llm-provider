//! 网关内的 WebSearch 仿真（D13）。
//!
//! 对 Claude Code 完全透明：它看到的是标准的 `web_search` 服务端工具
//! （`server_tool_use` + `web_search_tool_result` 块），对上游模型则是一个普通的
//! 函数工具；搜索由网关自己执行，中间过程一律不外泄。
//!
//! 三个子模块：
//! - `backends`：搜索后端适配器（Tavily / Exa / Firecrawl / Parallel / Jina）与
//!   带上冷却和故障转移的后端池；
//! - `history`：请求方向的改写——把 Claude Code 历史里的服务端工具块还原成
//!   函数调用对，搜索结果正文通过 `encrypted_content` 这个「不透明信封」随历史带回来；
//! - `search_loop`：响应方向的拦截与续写——发现上游要调用搜索就把结果发给
//!   Claude Code，然后带着结果再问一次上游，直到不再调用或达到 max_uses。

pub mod backends;
pub mod history;
pub mod search_loop;

/// 给上游模型用的函数名（与 Claude Code 看到的服务端工具名保持一致）。
pub const TOOL_NAME: &str = "web_search";

/// 判断工具定义是不是 WebSearch 服务端工具（`web_search_20250305` 等各版本）。
pub fn is_web_search_tool(tool: &serde_json::Value) -> bool {
    tool.get("type")
        .and_then(|t| t.as_str())
        .map(|t| t == "web_search" || t.starts_with("web_search_"))
        .unwrap_or(false)
        || tool.get("name").and_then(|n| n.as_str()) == Some(TOOL_NAME)
}
