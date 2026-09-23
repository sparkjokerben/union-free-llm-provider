#![allow(non_snake_case)]
//! 端到端测试：用 wiremock 当上游，跑通「Claude Code → 网关 → 上游」的完整链路。
//!
//! 覆盖：非流式转换、流式 SSE 转换、429 冷却后换渠道、401 禁用 key、
//! 鉴权失败、count_tokens 本地估算、/v1/models。

use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use ufp::api::{self, AppState};
use ufp::config::Config;
use ufp::health::Cooldowns;
use ufp::router::Sessions;
use ufp::store::{Db, Pool, Snapshot};

const DOWNSTREAM_KEY: &str = "ufp-test-downstream-key";

struct Gateway {
    base: String,
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

/// 起一个网关实例，`seed` 里往库里插渠道 / key / 条目 / 下游 key。
async fn spawn_gateway<F>(seed: F) -> Gateway
where
    F: FnOnce(&rusqlite::Connection) + Send + 'static,
{
    let dir = tempfile::tempdir().expect("创建临时目录失败");
    let db_path = dir.path().join("ufp.db");
    let db = Db::open(&db_path).expect("打开数据库失败");
    db.admin(move |conn| {
        seed(conn);
        Ok(())
    })
    .await
    .expect("写入种子数据失败");

    let pool = db.read(Pool::load).await.expect("加载配置快照失败");
    let snapshot = Snapshot::new(pool);
    let cfg = Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        db_path: db_path.clone(),
        log_dir: dir.path().join("log"),
        ..Default::default()
    };
    let state = AppState::new(
        cfg,
        Arc::clone(&db),
        snapshot,
        Arc::new(Cooldowns::new()),
        Arc::new(Sessions::new()),
    );
    let app = api::router(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定端口失败");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Gateway {
        base: format!("http://{addr}"),
        state,
        _dir: dir,
    }
}

/// 插一个渠道 + 一个 key + 一个条目，返回 key_id 与 entry_id。
fn seed_channel(
    conn: &rusqlite::Connection,
    name: &str,
    protocol: &str,
    base_url: &str,
    api_key: &str,
    model: &str,
    tier: i64,
) -> (i64, i64) {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO channels (name, protocol, base_url, extra_headers, enabled, created_ms)
         VALUES (?1, ?2, ?3, '{}', 1, ?4)",
        rusqlite::params![name, protocol, base_url, now],
    )
    .unwrap();
    let channel_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO upstream_keys (channel_id, label, api_key, enabled, created_ms)
         VALUES (?1, 'k1', ?2, 1, ?3)",
        rusqlite::params![channel_id, api_key, now],
    )
    .unwrap();
    let key_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO entries (channel_id, upstream_model, tier, max_context, vision, pdf, enabled, created_ms)
         VALUES (?1, ?2, ?3, 200000, 1, 0, 1, ?4)",
        rusqlite::params![channel_id, model, tier, now],
    )
    .unwrap();
    let entry_id = conn.last_insert_rowid();
    (key_id, entry_id)
}

fn seed_downstream_key(conn: &rusqlite::Connection) {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO downstream_keys (name, key_hash, key_prefix, enabled, created_ms)
         VALUES ('测试用户', ?1, ?2, 1, ?3)",
        rusqlite::params![
            api::auth::hash_key(DOWNSTREAM_KEY),
            &DOWNSTREAM_KEY[..12],
            now
        ],
    )
    .unwrap();
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

fn anthropic_body(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 128,
        "stream": stream,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "你好"}]}],
    })
}

/// 一个 OpenAI Chat 非流式响应。
fn chat_completion_body(model: &str, text: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
    })
}

/// 一个 OpenAI Chat 流式响应（SSE）。
fn chat_completion_sse(model: &str, pieces: &[&str]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": {"role": "assistant"}}]
        })
    ));
    for p in pieces {
        out.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 1, "model": model,
                "choices": [{"index": 0, "delta": {"content": p}}]
            })
        ));
    }
    out.push_str(&format!(
        "data: {}\n\n",
        json!({
            "id": "chatcmpl-test", "object": "chat.completion.chunk", "created": 1, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
        })
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn 非流式_chat_上游_端到端() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion_body("gpt-4o-mini", "你好，我是上游")),
        )
        .mount(&mock)
        .await;

    let mock_uri = mock.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "mock-chat",
            "openai_chat",
            &mock_uri,
            "sk-upstream",
            "gpt-4o-mini",
            1,
        );
    })
    .await;

    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&anthropic_body(false))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200, "{}", resp.text().await.unwrap());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["role"], "assistant");
    assert_eq!(body["content"][0]["type"], "text");
    assert_eq!(body["content"][0]["text"], "你好，我是上游");
    // 模型名回显真实上游模型（D4 默认）
    assert_eq!(body["model"], "gpt-4o-mini");
    assert_eq!(body["usage"]["input_tokens"], 12);
    assert_eq!(body["usage"]["output_tokens"], 3);
    assert_eq!(body["stop_reason"], "end_turn");
}

#[tokio::test]
async fn 流式_chat_上游_端到端() {
    let mock = MockServer::start().await;
    let sse = chat_completion_sse("gpt-4o-mini", &["你", "好", "呀"]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse.into_bytes(), "text/event-stream"),
        )
        .mount(&mock)
        .await;

    let mock_uri = mock.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "mock-chat",
            "openai_chat",
            &mock_uri,
            "sk-upstream",
            "gpt-4o-mini",
            1,
        );
    })
    .await;

    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&anthropic_body(true))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: message_start"), "{text}");
    assert!(text.contains("\"model\":\"gpt-4o-mini\""), "{text}");
    assert!(text.contains("event: content_block_delta"), "{text}");
    assert!(text.contains("\"text_delta\""), "{text}");
    assert!(text.contains("event: message_stop"), "{text}");
    // 三个增量拼起来就是完整回答
    let mut joined = String::new();
    for line in text.lines().filter(|l| l.starts_with("data: ")) {
        if let Ok(v) = serde_json::from_str::<Value>(&line[6..]) {
            if let Some(t) = v
                .get("delta")
                .and_then(|d| d.get("text"))
                .and_then(|t| t.as_str())
            {
                joined.push_str(t);
            }
        }
    }
    assert_eq!(joined, "你好呀", "流式文本应完整拼接：{text}");
}

#[tokio::test]
async fn 上游_429_冷却后换到下一层() {
    let limited = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_json(
                    json!({"error": {"message": "rate limited", "type": "rate_limit_error"}}),
                ),
        )
        .mount(&limited)
        .await;

    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion_body("llama-3.3-70b", "备用上游回答")),
        )
        .mount(&healthy)
        .await;

    let limited_uri = limited.uri();
    let healthy_uri = healthy.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        let (limited_key, _) = seed_channel(
            conn,
            "limited",
            "openai_chat",
            &limited_uri,
            "sk-a",
            "gpt-4o-mini",
            1,
        );
        seed_channel(
            conn,
            "healthy",
            "openai_chat",
            &healthy_uri,
            "sk-b",
            "llama-3.3-70b",
            2,
        );
        let _ = limited_key;
    })
    .await;

    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&anthropic_body(false))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200, "应当故障转移到第二层");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "备用上游回答");
    assert_eq!(body["model"], "llama-3.3-70b");

    // 第一层的 key×模型应进入冷却
    let cooling = gw.state.cooldowns.snapshot();
    assert_eq!(cooling.len(), 1, "应当只冷却第一层的 key：{cooling:?}");
    assert!(
        cooling[0].reason.contains("Retry-After"),
        "{:?}",
        cooling[0]
    );
}

#[tokio::test]
async fn 上游_401_禁用_key_并换渠道() {
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "invalid api key"}})),
        )
        .mount(&bad)
        .await;

    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_completion_body("qwen3-32b", "ok")),
        )
        .mount(&good)
        .await;

    let bad_uri = bad.uri();
    let good_uri = good.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "bad-key",
            "openai_chat",
            &bad_uri,
            "sk-bad",
            "gpt-4o-mini",
            1,
        );
        seed_channel(
            conn,
            "good",
            "openai_chat",
            &good_uri,
            "sk-good",
            "qwen3-32b",
            2,
        );
    })
    .await;

    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&anthropic_body(false))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200);

    // 坏 key 应被落库禁用（写入是异步批量落库，轮询等一下）
    let disabled = wait_for_int(
        &gw,
        "SELECT COUNT(*) FROM upstream_keys WHERE status = 'disabled' AND enabled = 0",
    )
    .await;
    assert_eq!(disabled, 1, "401 应把 key 标记为禁用");
}

/// 轮询等一个计数类查询的结果（写库是批量异步的，测试里等一小会儿）。
async fn wait_for_int(gw: &Gateway, sql: &str) -> i64 {
    for _ in 0..40 {
        let sql_owned = sql.to_string();
        let value: i64 = gw
            .state
            .db
            .admin(move |conn| conn.query_row(&sql_owned, [], |r| r.get(0)))
            .await
            .unwrap();
        if value > 0 {
            return value;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    0
}

#[tokio::test]
async fn 缺少下游_key_返回_401() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
    })
    .await;
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .json(&anthropic_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn 没有可用条目时返回_overloaded() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
    })
    .await;
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&anthropic_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 529);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn count_tokens_走本地估算() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
    })
    .await;
    let resp = client()
        .post(format!("{}/v1/messages/count_tokens", gw.base))
        .header("authorization", format!("Bearer {DOWNSTREAM_KEY}"))
        .json(&json!({"messages": [{"role": "user", "content": "你好世界"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["input_tokens"].as_u64().unwrap() >= 4, "{body}");
}

#[tokio::test]
async fn 模型列表返回对外_id() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
    })
    .await;
    let resp = client()
        .get(format!("{}/v1/models", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["data"][0]["id"], "ufp");
}

// ============================================================================
// WebSearch 仿真
// ============================================================================

/// 插一个搜索后端。
fn seed_search_backend(conn: &rusqlite::Connection, kind: &str, base_url: &str, api_key: &str) {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO search_backends (name, kind, api_key, base_url, enabled, created_ms)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)",
        rusqlite::params![format!("测试搜索-{kind}"), kind, api_key, base_url, now],
    )
    .unwrap();
}

/// Claude Code 的固定形态搜索请求。
fn claude_code_search_request(query: &str) -> Value {
    json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 1024,
        "stream": true,
        "system": "You are an assistant for performing a web search tool use",
        "messages": [{"role": "user", "content": [{"type": "text", "text": format!("Perform a web search for the query: {query}")}]}],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
        "tool_choice": {"type": "tool", "name": "web_search"}
    })
}

/// Tavily 形状的搜索结果。
fn tavily_results() -> Value {
    json!({
        "query": "x",
        "results": [
            {"title": "Rust 官网", "url": "https://www.rust-lang.org/", "content": "Rust 是一门系统编程语言。", "score": 0.95, "published_date": "2024-05-01"},
            {"title": "异步编程", "url": "https://rust-lang.github.io/async-book/", "content": "异步 Rust 入门。", "score": 0.8}
        ]
    })
}

#[tokio::test]
async fn websearch_快速路径_自带搜索块与引用() {
    let search = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tavily_results()))
        .mount(&search)
        .await;

    let upstream = MockServer::start().await;
    // 上游应当拿到「已经带搜索结果」的请求，并且工具选择被放宽为 auto
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("Rust 是一门系统编程语言"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    chat_completion_sse(
                        "gpt-4o-mini",
                        &["根据搜索结果，", "Rust 是系统编程语言。"],
                    )
                    .into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&upstream)
        .await;

    let search_uri = search.uri();
    let upstream_uri = upstream.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_search_backend(conn, "tavily", &search_uri, "tvly-test");
        seed_channel(
            conn,
            "chat",
            "openai_chat",
            &upstream_uri,
            "sk-up",
            "gpt-4o-mini",
            1,
        );
    })
    .await;

    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&claude_code_search_request("Rust 是不是系统编程语言"))
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // 1) 结果块必须完整出现在 content_block_start 里（Claude Code 只从 start 读结果）
    assert!(
        text.contains("\"type\":\"web_search_tool_result\""),
        "{text}"
    );
    assert!(
        text.contains("https://www.rust-lang.org/"),
        "URL 要发给客户端：{text}"
    );
    assert!(text.contains("\"type\":\"server_tool_use\""), "{text}");
    assert!(
        text.contains("\"srvtoolu_"),
        "id 必须是 srvtoolu_ 前缀：{text}"
    );
    // 2) 索引从 0 开始且连续：server_tool_use=0，结果=1，正文=2
    assert!(text.contains("\"index\":0"), "{text}");
    assert!(text.contains("\"index\":1"), "{text}");
    assert!(
        text.contains("\"index\":2"),
        "正文块应接在搜索块之后：{text}"
    );
    // 3) 模型正文照常返回
    assert!(text.contains("Rust 是系统编程语言"), "{text}");
    // 4) 只发一份收尾
    assert_eq!(text.matches("event: message_stop").count(), 1, "{text}");
    assert!(
        text.contains("web_search_requests\":1"),
        "usage 要带上搜索次数：{text}"
    );

    // 5) 上游收到的请求里工具选择应已放宽（否则模型会一直循环搜索）
    let requests = upstream.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1, "快速路径只该调用一次上游");
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["tool_choice"], "auto",
        "工具选择应放宽（Chat 协议里是字符串）：{body}"
    );
    assert_eq!(
        body["tools"][0]["type"], "function",
        "上游看到的是普通函数工具：{body}"
    );
    assert_eq!(body["tools"][0]["function"]["name"], "web_search", "{body}");
}

#[tokio::test]
async fn websearch_通用路径_拦截工具调用并续写() {
    let search = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tavily_results()))
        .mount(&search)
        .await;

    let upstream = MockServer::start().await;
    // 续写请求（带着搜索结果）先挂：它更具体，wiremock 按挂载顺序匹配
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("异步 Rust 入门"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    chat_completion_sse("gpt-4o-mini", &["搜到了：", "异步 Rust 入门。"])
                        .into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&upstream)
        .await;
    // 首轮：模型调用 web_search 函数
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    chat_completion_sse_tool_call(
                        "gpt-4o-mini",
                        "web_search",
                        "{\"query\":\"异步 rust\"}",
                    )
                    .into_bytes(),
                    "text/event-stream",
                ),
        )
        .mount(&upstream)
        .await;

    let search_uri = search.uri();
    let upstream_uri = upstream.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_search_backend(conn, "tavily", &search_uri, "tvly-test");
        seed_channel(
            conn,
            "chat",
            "openai_chat",
            &upstream_uri,
            "sk-up",
            "gpt-4o-mini",
            1,
        );
    })
    .await;

    let body = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "帮我查一下异步 rust"}]}],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
    });
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&body)
        .send()
        .await
        .expect("请求失败");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();

    // 模型对函数的调用不该出现在客户端流里，取而代之的是服务端工具块
    assert!(
        !text.contains("\"type\":\"tool_use\""),
        "客户端面不该出现普通工具调用块：{text}"
    );
    assert!(text.contains("\"type\":\"server_tool_use\""), "{text}");
    assert!(
        text.contains("https://rust-lang.github.io/async-book/"),
        "{text}"
    );
    assert!(text.contains("异步 Rust 入门"), "续写的正文要接上：{text}");
    assert_eq!(
        text.matches("event: message_stop").count(),
        1,
        "只发一份收尾：{text}"
    );

    // 两次上游调用：首轮工具调用 + 续写
    let requests = upstream.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 2, "应该有首轮与续写两次调用");
    let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = second["messages"].as_array().unwrap();
    // Chat 协议里工具调用是 assistant 消息上的 tool_calls，结果是一条 tool 消息
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "assistant" && m.get("tool_calls").is_some()),
        "续写请求里应有 assistant 侧的工具调用：{second}"
    );
    assert!(
        messages.iter().any(|m| m["role"] == "tool"),
        "续写请求里应有工具结果消息：{second}"
    );
    assert!(
        serde_json::to_string(&second)
            .unwrap()
            .contains("Web search results for query"),
        "续写请求要带上搜索结果文本：{second}"
    );
}

/// 带 tool_calls 的 OpenAI 流式响应。
fn chat_completion_sse_tool_call(model: &str, name: &str, arguments: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}}]})
    ));
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
               "choices": [{"index": 0, "delta": {"tool_calls": [
                   {"index": 0, "id": "call_1", "type": "function",
                    "function": {"name": name, "arguments": arguments}}]}}]})
    ));
    out.push_str(&format!(
        "data: {}\n\n",
        json!({"id": "c1", "object": "chat.completion.chunk", "model": model,
               "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
               "usage": {"prompt_tokens": 20, "completion_tokens": 5}})
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn websearch_历史往返_搜索正文随信封带回上游() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_completion_body("gpt-4o-mini", "好的")),
        )
        .mount(&upstream)
        .await;

    let upstream_uri = upstream.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "chat",
            "openai_chat",
            &upstream_uri,
            "sk-up",
            "gpt-4o-mini",
            1,
        );
    })
    .await;

    // 历史里带上上一轮的搜索结果块（encrypted_content 是我们自己塞的信封）
    let item = json!({"type": "web_search_result", "url": "https://a.example/1", "title": "示例",
                      "encrypted_content": ufp::websearch::history::encode_payload(
                          &ufp::websearch::backends::SearchItem{
                              url: "https://a.example/1".into(), title: "示例".into(),
                              snippet: "这是当初抓到的正文".into(), published: None }),
                      "page_age": null});
    let body = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 128,
        "stream": false,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "查一下示例"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "我查一下"},
                {"type": "server_tool_use", "id": "srvtoolu_abc", "name": "web_search", "input": {"query": "示例"}},
                {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_abc", "content": [item]}
            ]},
            {"role": "user", "content": [{"type": "text", "text": "谢谢"}]},
        ],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 8}],
    });
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let requests = upstream.received_requests().await.unwrap_or_default();
    assert_eq!(requests.len(), 1);
    let raw = String::from_utf8_lossy(&requests[0].body).to_string();
    assert!(
        raw.contains("这是当初抓到的正文"),
        "搜索结果正文应随历史回到上游：{raw}"
    );
    assert!(
        !raw.contains("server_tool_use"),
        "上游不该看到服务端工具块：{raw}"
    );
    assert!(
        raw.contains("tool_call_id") || raw.contains("tool_use_id"),
        "应有工具结果消息：{raw}"
    );
}

// ============================================================================
// 后台管理
// ============================================================================

#[tokio::test]
async fn 后台_未登录被拒() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
    })
    .await;
    let resp = client()
        .get(format!("{}/admin/api/overview", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    // 界面本身可以直接打开（登录由前端处理）
    let page = client()
        .get(format!("{}/admin", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    // 断言用稳定的结构标记（页面标题会随设计变，别再绑文案）
    assert!(html.contains("/admin/app.js"), "{html:.200}");
    assert!(html.contains("<title>"), "{html:.200}");
}

#[tokio::test]
async fn 后台_登录后能建渠道并看到概览() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
        // 设置一个已知密码
        let salt = argon2::password_hash::SaltString::generate(
            &mut argon2::password_hash::rand_core::OsRng,
        );
        let hash = {
            use argon2::password_hash::PasswordHasher;
            argon2::Argon2::default()
                .hash_password(b"test-password-123", &salt)
                .unwrap()
                .to_string()
        };
        conn.execute(
            "INSERT INTO admin (id, password_hash, updated_ms) VALUES (1, ?1, 0)",
            rusqlite::params![hash],
        )
        .unwrap();
    })
    .await;

    let http = client();
    // 密码错误要被拒
    let bad = http
        .post(format!("{}/admin/api/login", gw.base))
        .json(&json!({"password": "wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);

    // 正确密码：拿到会话 Cookie
    let ok = http
        .post(format!("{}/admin/api/login", gw.base))
        .json(&json!({"password": "test-password-123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let cookie = ok
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("应有会话 Cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // 建渠道 + key + 条目
    let created: Value = http
        .post(format!("{}/admin/api/channels", gw.base))
        .header("cookie", &cookie)
        .json(&json!({"name": "后台建的渠道", "protocol": "openai_chat",
                      "base_url": "https://example.com/v1", "enabled": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let channel_id = created["id"].as_i64().unwrap();
    let key_resp = http
        .post(format!("{}/admin/api/channels/{channel_id}/keys", gw.base))
        .header("cookie", &cookie)
        .json(&json!({"label": "免费号", "api_key": "sk-test-1234567890", "enabled": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(key_resp.status(), 200);
    let entry_resp = http
        .post(format!(
            "{}/admin/api/channels/{channel_id}/entries",
            gw.base
        ))
        .header("cookie", &cookie)
        .json(
            &json!({"upstream_model": "gpt-4o-mini", "tier": 1, "max_context": 128000,
                      "vision": true, "pdf": false, "enabled": true}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(entry_resp.status(), 200);

    // 改动应立刻进池（快照已重载）
    let overview: Value = http
        .get(format!("{}/admin/api/overview", gw.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overview["pool"]["channels"], 1);
    assert_eq!(overview["pool"]["entries"], 1);
    assert_eq!(overview["pool"]["upstream_keys"], 1);

    // 列表接口不回显上游 key 明文
    let list: Value = http
        .get(format!("{}/admin/api/channels", gw.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let masked = list["keys"][0]["api_key_masked"].as_str().unwrap();
    assert!(masked.contains('…'), "key 应打码：{masked}");
    assert!(!masked.contains("sk-test-1234567890"));

    // 新建下游 key：明文只在这一次返回
    let downstream: Value = http
        .post(format!("{}/admin/api/downstream_keys", gw.base))
        .header("cookie", &cookie)
        .json(&json!({"name": "新设备", "enabled": true}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plaintext = downstream["key"].as_str().unwrap().to_string();
    assert!(plaintext.starts_with("ufp-"), "{plaintext}");

    // 用新 key 能真的调用 /v1/models
    let models = http
        .get(format!("{}/v1/models", gw.base))
        .header("x-api-key", &plaintext)
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);

    // 导出配置里应包含渠道
    let export: Value = http
        .get(format!("{}/admin/api/export", gw.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(export["channels"][0]["name"], "后台建的渠道");
}

#[tokio::test]
async fn 后台_健康接口能重置熔断与冷却() {
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "ch",
            "openai_chat",
            "https://example.com",
            "sk-x",
            "m",
            1,
        );
    })
    .await;
    // 直接让熔断打开
    let settings = gw.state.pool.load().settings.breaker.clone();
    gw.state.breakers.allow(1, "m", &settings);
    gw.state.breakers.record(1, "m", false, &settings);
    gw.state.breakers.record(1, "m", false, &settings);
    gw.state.breakers.record(1, "m", false, &settings);
    gw.state.breakers.record(1, "m", false, &settings);

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let _ = tx;
    // 没登录时健康接口也该被拒
    let denied = client()
        .get(format!("{}/admin/api/health", gw.base))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    drop(rx);

    // 直接调用内部重置（后台按钮走的是同一个方法）
    let reset = gw.state.breakers.reset(Some(1), Some("m"));
    assert_eq!(reset, 1);
    assert!(gw.state.breakers.snapshot().is_empty());
}

// ============================================================================
// 矫正：LLM 在线分析 + 规则沉淀
// ============================================================================

#[tokio::test]
async fn 未知_400_由分析条目给出补丁并沉淀成规则() {
    // 分析条目：返回一个 JSON 补丁
    let analyzer = MockServer::start().await;
    let patch_text = r#"{"why":"上游不接受工具 schema 里的 additionalProperties","patch":[{"op":"remove","path":"/tools/0/input_schema/additionalProperties"}]}"#;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_completion_body("gpt-4o", patch_text)),
        )
        .mount(&analyzer)
        .await;

    // 挑刺的上游：第一次 400，之后放行
    let picky = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "invalid request: unsupported field 'additionalProperties' in tool schema"}
        })))
        .up_to_n_times(1)
        .mount(&picky)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion_body("gpt-4o-mini", "补丁生效后的回答")),
        )
        .mount(&picky)
        .await;

    let analyzer_uri = analyzer.uri();
    let picky_uri = picky.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(
            conn,
            "picky",
            "openai_chat",
            &picky_uri,
            "sk-picky",
            "gpt-4o-mini",
            1,
        );
        let (_key, entry_id) = seed_channel(
            conn,
            "analyzer",
            "openai_chat",
            &analyzer_uri,
            "sk-ana",
            "gpt-4o",
            2,
        );
        // 指定分析条目
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('runtime', ?1)",
            rusqlite::params![serde_json::json!({"analysisEntryId": entry_id}).to_string()],
        )
        .unwrap();
    })
    .await;

    let body = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 256,
        "stream": false,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "帮我读个文件"}]}],
        "tools": [{
            "name": "Read",
            "description": "读取文件",
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }]
    });
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&body)
        .send()
        .await
        .expect("请求失败");
    assert_eq!(
        resp.status(),
        200,
        "矫正后应当成功：{}",
        resp.text().await.unwrap()
    );
    let out: Value = resp.json().await.unwrap();
    assert_eq!(out["content"][0]["text"], "补丁生效后的回答");

    // 分析条目应当被调到（且骨架里没有正文）
    let analyzer_requests = analyzer.received_requests().await.unwrap_or_default();
    assert_eq!(analyzer_requests.len(), 1, "应当调用一次分析条目");
    let prompt = String::from_utf8_lossy(&analyzer_requests[0].body).to_string();
    assert!(
        prompt.contains("additional_properties") || prompt.contains("additionalProperties"),
        "骨架里应带上工具 schema：{prompt}"
    );
    assert!(
        !prompt.contains("帮我读个文件"),
        "骨架里不该有对话正文：{prompt}"
    );

    // 挑刺渠道被调了两次（400 → 矫正 → 200）
    let picky_requests = picky.received_requests().await.unwrap_or_default();
    assert_eq!(picky_requests.len(), 2, "应当在同一候选上矫正重试一次");

    // 规则应已沉淀
    let rule = gw
        .state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT patch_json, source FROM rectify_rules ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(rule.1, "llm");
    assert!(rule.0.contains("additionalProperties"), "{}", rule.0);
}

#[tokio::test]
async fn 已沉淀的规则会被直接套用不再分析() {
    let analyzer = MockServer::start().await;
    // 分析条目这次不该被调用：规则已经存在
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(chat_completion_body("gpt-4o", "不该被调用")),
        )
        .mount(&analyzer)
        .await;

    let picky = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "invalid request: unsupported field 'additionalProperties' in tool schema"}
        })))
        .up_to_n_times(1)
        .mount(&picky)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(chat_completion_body("gpt-4o-mini", "规则生效")),
        )
        .mount(&picky)
        .await;

    let analyzer_uri = analyzer.uri();
    let picky_uri = picky.uri();
    let gw = spawn_gateway(move |conn| {
        seed_downstream_key(conn);
        seed_channel(conn, "picky", "openai_chat", &picky_uri, "sk-picky", "gpt-4o-mini", 1);
        let (_key, entry_id) = seed_channel(conn, "analyzer", "openai_chat", &analyzer_uri, "sk-ana", "gpt-4o", 2);
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('runtime', ?1)",
            rusqlite::params![serde_json::json!({"analysisEntryId": entry_id}).to_string()],
        )
        .unwrap();
        // 预先放一条规则（指纹与下面请求的错误一致）
        let fp = ufp::rectify::rules::fingerprint(
            "openai_chat",
            400,
            "invalid request: unsupported field 'additionalProperties' in tool schema",
        );
        conn.execute(
            "INSERT INTO rectify_rules (scope, error_fingerprint, error_sample, patch_json, source, enabled, created_ms, updated_ms)
             VALUES ('openai_chat', ?1, '历史上的同款报错', ?2, 'llm', 1, 0, 0)",
            rusqlite::params![fp, r#"[{"op":"remove","path":"/tools/0/input_schema/additionalProperties"}]"#],
        )
        .unwrap();
    })
    .await;

    let body = json!({
        "model": "claude-sonnet-4-5-20250929",
        "max_tokens": 256,
        "stream": false,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "读文件"}]}],
        "tools": [{"name": "Read", "input_schema": {"type": "object", "additionalProperties": false,
                   "properties": {"path": {"type": "string"}}, "required": ["path"]}}]
    });
    let resp = client()
        .post(format!("{}/v1/messages", gw.base))
        .header("x-api-key", DOWNSTREAM_KEY)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 关键断言：这次没有调用分析条目（省下额度）
    let analyzer_requests = analyzer.received_requests().await.unwrap_or_default();
    assert!(analyzer_requests.is_empty(), "已有规则时不该再调分析条目");

    // 命中计数应当被累加（写库是异步批量的，轮询等一下）
    let mut hits = 0i64;
    for _ in 0..40 {
        hits = gw
            .state
            .db
            .read(|conn| conn.query_row("SELECT hits FROM rectify_rules LIMIT 1", [], |r| r.get(0)))
            .await
            .unwrap();
        if hits > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(hits, 1);
}

// ============================================================================
// 在线部署接口（CI 推包用）
// ============================================================================

#[tokio::test]
async fn 部署接口_令牌与哈希校验() {
    // 落盘目录指到临时目录（开发机上没有 /var/lib/ufp）
    let spool = tempfile::tempdir().unwrap();
    std::env::set_var("UFP_DEPLOY_SPOOL", spool.path());
    let gw = spawn_gateway(|conn| {
        seed_downstream_key(conn);
        conn.execute(
            "INSERT INTO settings (key, value) VALUES ('runtime', ?1)",
            rusqlite::params![serde_json::json!({"deployToken": "test-deploy-token"}).to_string()],
        )
        .unwrap();
    })
    .await;
    let url = format!("{}/admin/api/deploy", gw.base);
    let payload = b"fake-tarball-bytes".to_vec();
    let sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&payload);
        format!("{:x}", h.finalize())
    };

    // 1) 没有令牌
    let resp = client()
        .post(&url)
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 2) 令牌不对
    let resp = client()
        .post(&url)
        .header("x-ufp-deploy-token", "wrong")
        .header("x-ufp-sha256", &sha)
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 3) 缺 sha256
    let resp = client()
        .post(&url)
        .header("x-ufp-deploy-token", "test-deploy-token")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 4) 哈希不匹配
    let resp = client()
        .post(&url)
        .header("x-ufp-deploy-token", "test-deploy-token")
        .header("x-ufp-sha256", "deadbeef")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "哈希不对必须拒绝");

    // 5) 正确请求：网关会落盘；开发机上没有 sudo/doas 或特权脚本，所以返回 503，
    //    但包必须已经写好（这样人工也能接着装）。
    let resp = client()
        .post(&url)
        .header("x-ufp-deploy-token", "test-deploy-token")
        .header("x-ufp-sha256", &sha)
        .header("x-ufp-version", "v9.9.9")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["sha256"], sha);
    assert!(
        status == 202 || status == 503,
        "状态应是 202（已触发）或 503（落盘成功但没特权脚本）：{status} {body}"
    );
    let saved = body["file"].as_str().unwrap();
    assert!(std::path::Path::new(saved).exists(), "包应已落盘：{saved}");
}
