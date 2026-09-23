//! 搜索后端适配器与后端池。
//!
//! 免费搜索服务各有各的请求形状，但返回的核心都是「标题 + 链接 + 摘要」，所以
//! 请求按 `kind` 分别构造，响应用一个宽松的提取器统一解析——这样上游 API 换代
//! 时不容易整条链路挂掉。
//!
//! 池化与故障转移复用 LLM 条目那套思路：后端进入冷却（按 429/5xx 的错误信号或
//! 指数退避）后跳过，全部冷却时返回错误，由调用方决定是给 Claude Code 一个
//! `web_search_tool_result_error` 还是继续别的方式。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 一条搜索结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchItem {
    pub url: String,
    pub title: String,
    /// 正文摘要（给模型看的主要内容）。
    #[serde(default)]
    pub snippet: String,
    /// 发布时间（有就带上，Anthropic 的 `page_age` 用它）。
    #[serde(default)]
    pub published: Option<String>,
}

/// 一个搜索后端。
#[derive(Debug, Clone)]
pub struct SearchBackend {
    pub id: i64,
    pub name: String,
    pub kind: SearchKind,
    pub api_key: String,
    pub base_url: String,
    pub enabled: bool,
    pub cooldown_until_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Tavily,
    Exa,
    Firecrawl,
    Parallel,
    Jina,
}

impl SearchKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "tavily" => Some(SearchKind::Tavily),
            "exa" => Some(SearchKind::Exa),
            "firecrawl" => Some(SearchKind::Firecrawl),
            "parallel" => Some(SearchKind::Parallel),
            "jina" => Some(SearchKind::Jina),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SearchKind::Tavily => "tavily",
            SearchKind::Exa => "exa",
            SearchKind::Firecrawl => "firecrawl",
            SearchKind::Parallel => "parallel",
            SearchKind::Jina => "jina",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            SearchKind::Tavily => "https://api.tavily.com",
            SearchKind::Exa => "https://api.exa.ai",
            SearchKind::Firecrawl => "https://api.firecrawl.dev",
            SearchKind::Parallel => "https://api.parallel.ai",
            SearchKind::Jina => "https://s.jina.ai",
        }
    }
}

/// 从库里读出所有启用的搜索后端。
pub fn load_backends(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<SearchBackend>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, api_key, base_url, enabled, cooldown_until_ms
         FROM search_backends WHERE enabled = 1 ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, kind, api_key, base_url, enabled, cooldown_until_ms) = row?;
        let Some(kind) = SearchKind::parse(&kind) else {
            tracing::warn!(backend = %name, kind, "未知的搜索后端类型，已跳过");
            continue;
        };
        out.push(SearchBackend {
            id,
            name,
            kind,
            api_key,
            base_url: if base_url.trim().is_empty() {
                kind.default_base_url().to_string()
            } else {
                base_url
            },
            enabled: enabled != 0,
            cooldown_until_ms,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct SearchFailure {
    pub message: String,
    /// 该后端应冷却到什么时候（None 表示不算它的错）。
    pub cooldown_until_ms: Option<i64>,
}

/// 调用一个后端执行搜索。
pub async fn search(
    client: &reqwest::Client,
    backend: &SearchBackend,
    query: &str,
    limit: u32,
    snippet_bytes: usize,
) -> Result<Vec<SearchItem>, SearchFailure> {
    let (url, req) = build_request(client, backend, query, limit)?;
    let resp = match tokio::time::timeout(Duration::from_secs(20), req.send()).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(SearchFailure {
                message: format!("请求 {} 失败：{e}", backend.name),
                cooldown_until_ms: Some(backoff(300)),
            })
        }
        Err(_) => {
            return Err(SearchFailure {
                message: format!("{} 超时", backend.name),
                cooldown_until_ms: Some(backoff(120)),
            })
        }
    };

    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok());
    let body_text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let cooldown = match status {
            429 | 402 | 403 => Some(
                retry_after
                    .map(|s| now_ms() + s * 1000)
                    .unwrap_or_else(|| backoff(900)),
            ),
            500..=599 => Some(backoff(300)),
            _ => None,
        };
        return Err(SearchFailure {
            message: format!(
                "{} 返回 {status}：{}",
                backend.name,
                truncate(&body_text, 300)
            ),
            cooldown_until_ms: cooldown,
        });
    }
    let value: Value = serde_json::from_str(&body_text).map_err(|e| SearchFailure {
        message: format!("{} 的响应不是 JSON：{e}", backend.name),
        cooldown_until_ms: None,
    })?;
    let items = extract_results(&value, limit as usize, snippet_bytes);
    if items.is_empty() {
        tracing::warn!(
            backend = %backend.name,
            url = %url,
            "搜索后端返回了空结果集"
        );
    }
    Ok(items)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn backoff(secs: i64) -> i64 {
    now_ms() + secs * 1000
}

fn truncate(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// 构造各后端的请求（复用网关的 HTTP 客户端，连接池共享）。
fn build_request(
    client: &reqwest::Client,
    backend: &SearchBackend,
    query: &str,
    limit: u32,
) -> Result<(String, reqwest::RequestBuilder), SearchFailure> {
    let base = backend.base_url.trim_end_matches('/');
    let (url, req) = match backend.kind {
        SearchKind::Tavily => (
            format!("{base}/search"),
            client
                .post(format!("{base}/search"))
                .header("authorization", format!("Bearer {}", backend.api_key))
                .json(&json!({
                    "query": query,
                    "max_results": limit,
                    "search_depth": "basic",
                    "include_answer": false,
                    "include_raw_content": false,
                })),
        ),
        SearchKind::Exa => (
            format!("{base}/search"),
            client
                .post(format!("{base}/search"))
                .header("x-api-key", &backend.api_key)
                .json(&json!({
                    "query": query,
                    "numResults": limit,
                    "type": "auto",
                    "contents": {"text": {"maxCharacters": 4000}},
                })),
        ),
        SearchKind::Firecrawl => (
            format!("{base}/v2/search"),
            client
                .post(format!("{base}/v2/search"))
                .header("authorization", format!("Bearer {}", backend.api_key))
                .json(&json!({"query": query, "limit": limit, "sources": ["web"]})),
        ),
        SearchKind::Parallel => (
            format!("{base}/v1beta/search"),
            client
                .post(format!("{base}/v1beta/search"))
                .header("x-api-key", &backend.api_key)
                .json(&json!({
                    "objective": query,
                    "search_queries": [query],
                    "max_results": limit,
                    "max_chars_per_result": 4000,
                })),
        ),
        SearchKind::Jina => {
            let url = format!("{base}/?q={}", urlencode(query));
            let mut req = client.get(&url).header("accept", "application/json");
            if !backend.api_key.trim().is_empty() {
                req = req.header("authorization", format!("Bearer {}", backend.api_key));
            }
            (url, req)
        }
    };
    Ok((url, req))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 宽松的结果提取：在响应里找所有「像搜索结果」的对象。
///
/// 认的字段名覆盖各家叫法（url/link、title/name、content/snippet/description/text、
/// published_date/publishedDate/page_age），所以上游改字段名时通常还能活。
fn extract_results(value: &Value, limit: usize, snippet_bytes: usize) -> Vec<SearchItem> {
    let mut items = Vec::new();
    collect_items(value, &mut items, snippet_bytes, 0);
    items.truncate(limit);
    items
}

fn collect_items(value: &Value, out: &mut Vec<SearchItem>, snippet_bytes: usize, depth: usize) {
    if depth > 8 || out.len() >= 64 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_items(item, out, snippet_bytes, depth + 1);
            }
        }
        Value::Object(map) => {
            if let Some(url) = first_str(map, &["url", "link", "href", "source_url"]) {
                if url.starts_with("http") {
                    let title = first_str(map, &["title", "name", "headline"]).unwrap_or_default();
                    let snippet = first_str(
                        map,
                        &[
                            "content",
                            "snippet",
                            "description",
                            "text",
                            "summary",
                            "markdown",
                        ],
                    )
                    .unwrap_or_default();
                    let published = first_str(
                        map,
                        &[
                            "published_date",
                            "publishedDate",
                            "page_age",
                            "date",
                            "published",
                        ],
                    );
                    let snippet = truncate(&snippet, snippet_bytes);
                    // 同一 URL 只留第一条（不同后端会在嵌套结构里重复出现）。
                    if !out.iter().any(|i| i.url == url) {
                        out.push(SearchItem {
                            url,
                            title,
                            snippet,
                            published,
                        });
                    }
                    return;
                }
            }
            for v in map.values() {
                collect_items(v, out, snippet_bytes, depth + 1);
            }
        }
        _ => {}
    }
}

fn first_str(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = map.get(*k).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
        // 有些后端把正文放在 {"content": {"text": ...}} 里
        if let Some(inner) = map.get(*k).and_then(|v| v.as_object()) {
            if let Some(s) = inner.get("text").and_then(|v| v.as_str()) {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// 把搜索结果整理成给上游模型看的文本（对应 Claude Code 最终拿到的 tool_result 内容）。
pub fn results_as_text(query: &str, items: &[SearchItem]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Web search results for query: \"{query}\"\n\n"));
    if items.is_empty() {
        out.push_str("No results were returned for this query.\n");
        return out;
    }
    let links: Vec<Value> = items
        .iter()
        .map(|i| json!({"title": i.title, "url": i.url}))
        .collect();
    out.push_str(&format!(
        "Links: {}\n\n",
        serde_json::to_string(&links).unwrap()
    ));
    for (i, item) in items.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, item.title, item.url));
        if !item.snippet.is_empty() {
            out.push_str("   ");
            out.push_str(&item.snippet.replace('\n', " "));
            out.push('\n');
        }
        if let Some(p) = &item.published {
            out.push_str(&format!("   （发布：{p}）\n"));
        }
    }
    out.push_str(
        "\nREMINDER: You MUST include the sources above in your response to the user using markdown hyperlinks.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 宽松提取_tavily_形状() {
        let v = json!({"results": [
            {"title": "Rust 官网", "url": "https://www.rust-lang.org/", "content": "Rust 语言主页", "score": 0.9, "published_date": "2024-01-01"},
            {"title": "无链接", "content": "x"}
        ]});
        let items = extract_results(&v, 5, 1000);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://www.rust-lang.org/");
        assert_eq!(items[0].published.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn 宽松提取_嵌套形状() {
        let v = json!({"success": true, "data": {"web": [
            {"url": "https://a.example/1", "title": "A", "description": "desc"}
        ]}});
        let items = extract_results(&v, 5, 1000);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].snippet, "desc");
    }

    #[test]
    fn 同一个_url_只留一条() {
        let v = json!([
            {"url": "https://a.example/1", "title": "A"},
            {"url": "https://a.example/1", "title": "A 重复"}
        ]);
        assert_eq!(extract_results(&v, 5, 1000).len(), 1);
    }

    #[test]
    fn 结果文本包含链接与正文() {
        let items = vec![SearchItem {
            url: "https://a.example/1".into(),
            title: "标题".into(),
            snippet: "摘要内容".into(),
            published: None,
        }];
        let text = results_as_text("测试", &items);
        assert!(text.contains("https://a.example/1"));
        assert!(text.contains("摘要内容"));
        assert!(text.contains("REMINDER"));
    }
}
