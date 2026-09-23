#![allow(non_snake_case)]
//! 端到端测试：用 wiremock 当上游，跑通「Claude Code → 网关 → 上游」的完整链路。
//!
//! 覆盖：非流式转换、流式 SSE 转换、429 冷却后换渠道、401 禁用 key、
//! 鉴权失败、count_tokens 本地估算、/v1/models。

use std::sync::Arc;

use serde_json::{json, Value};
use wiremock::matchers::{method, path};
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
