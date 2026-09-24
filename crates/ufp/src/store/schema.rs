//! SQLite 表结构。
//!
//! 设计要点：
//! - 渠道（channel）持有协议、base_url、附加头与一批 api key；条目（entry）是
//!   渠道下的某个上游模型，带层级与能力字段。运行期的「候选」= 条目 × 该渠道
//!   启用的 key，所以同一渠道挂多个 key 不需要重复声明模型；若某个 key 能用的
//!   模型与同渠道其他 key 不同，就单独建一个渠道。
//! - 冷却按 (key, model) 记录并落库（日配额冷却可能跨重启）；熔断只按
//!   (channel, model) 在内存里维护，重启即复位。
//! - 明细表只存元数据与截断后的错误信息，不存请求/响应正文。

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS channels (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,
    protocol      TEXT NOT NULL,              -- openai_chat | openai_responses | gemini | anthropic
    base_url      TEXT NOT NULL,
    extra_headers TEXT NOT NULL DEFAULT '{}', -- JSON 对象，逐条附加到上游请求
    enabled       INTEGER NOT NULL DEFAULT 1,
    notes         TEXT NOT NULL DEFAULT '',
    created_ms    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS upstream_keys (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id    INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    label         TEXT NOT NULL DEFAULT '',
    api_key       TEXT NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    status        TEXT NOT NULL DEFAULT 'ok', -- ok | disabled
    status_reason TEXT NOT NULL DEFAULT '',
    created_ms    INTEGER NOT NULL,
    disabled_ms   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_upstream_keys_channel ON upstream_keys(channel_id);

CREATE TABLE IF NOT EXISTS entries (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id    INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
    upstream_model TEXT NOT NULL,
    tier          INTEGER NOT NULL DEFAULT 1,   -- 数字越小越优先
    max_context   INTEGER NOT NULL DEFAULT 200000,
    vision        INTEGER NOT NULL DEFAULT 1,
    pdf           INTEGER NOT NULL DEFAULT 0,
    enabled       INTEGER NOT NULL DEFAULT 1,
    notes         TEXT NOT NULL DEFAULT '',
    created_ms    INTEGER NOT NULL,
    UNIQUE(channel_id, upstream_model)
);
CREATE INDEX IF NOT EXISTS idx_entries_tier ON entries(tier);

CREATE TABLE IF NOT EXISTS downstream_keys (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,
    key_hash     TEXT NOT NULL UNIQUE,   -- sha256(hex)，鉴权只用它
    -- 明文：只为「生成 cc-switch 一键导入链接」与「以后再抄一次」留着，
    -- 鉴权不读它。老库里建的行没有这一列（NULL），后台会提示轮换一次。
    secret       TEXT,
    key_prefix   TEXT NOT NULL,
    enabled      INTEGER NOT NULL DEFAULT 1,
    created_ms   INTEGER NOT NULL,
    last_used_ms INTEGER
);

CREATE TABLE IF NOT EXISTS cooldowns (
    key_id    INTEGER NOT NULL REFERENCES upstream_keys(id) ON DELETE CASCADE,
    model     TEXT NOT NULL,
    until_ms  INTEGER NOT NULL,
    reason    TEXT NOT NULL DEFAULT '',
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (key_id, model)
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id     TEXT PRIMARY KEY,
    channel_id     INTEGER NOT NULL,
    key_id         INTEGER NOT NULL,
    entry_id       INTEGER NOT NULL,
    upstream_model TEXT NOT NULL,
    updated_ms     INTEGER NOT NULL,
    hits           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_ms);

CREATE TABLE IF NOT EXISTS rectify_rules (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    scope             TEXT NOT NULL DEFAULT '',  -- 协议或上游模型；空表示全局
    error_fingerprint TEXT NOT NULL,             -- 归一化后的错误特征
    error_sample      TEXT NOT NULL DEFAULT '',
    patch_json        TEXT NOT NULL,             -- 受限 JSON Patch
    source            TEXT NOT NULL DEFAULT 'llm',
    hits              INTEGER NOT NULL DEFAULT 0,
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_ms        INTEGER NOT NULL,
    updated_ms        INTEGER NOT NULL,
    UNIQUE(scope, error_fingerprint)
);

CREATE TABLE IF NOT EXISTS search_backends (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL,             -- tavily | exa | firecrawl | parallel | jina
    api_key      TEXT NOT NULL DEFAULT '',
    base_url     TEXT NOT NULL DEFAULT '',
    enabled      INTEGER NOT NULL DEFAULT 1,
    cooldown_until_ms INTEGER,
    notes        TEXT NOT NULL DEFAULT '',
    created_ms   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS request_logs (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id          TEXT NOT NULL,
    downstream_key_id   INTEGER,
    session_id          TEXT,
    requested_model     TEXT NOT NULL DEFAULT '',
    upstream_model      TEXT NOT NULL DEFAULT '',
    channel_id          INTEGER,
    key_id              INTEGER,
    http_status         INTEGER NOT NULL DEFAULT 0,
    stop_reason         TEXT,
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    search_requests     INTEGER NOT NULL DEFAULT 0,
    attempts            INTEGER NOT NULL DEFAULT 0,
    streaming           INTEGER NOT NULL DEFAULT 0,
    first_content_ms    INTEGER,
    total_ms            INTEGER NOT NULL DEFAULT 0,
    error_type          TEXT,
    error_message       TEXT,
    created_ms          INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_request_logs_created ON request_logs(created_ms);
CREATE INDEX IF NOT EXISTS idx_request_logs_key ON request_logs(downstream_key_id, created_ms);

CREATE TABLE IF NOT EXISTS attempt_logs (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id      TEXT NOT NULL,
    attempt_no      INTEGER NOT NULL,
    channel_id      INTEGER,
    key_id          INTEGER,
    upstream_model  TEXT NOT NULL DEFAULT '',
    protocol        TEXT NOT NULL DEFAULT '',
    http_status     INTEGER,
    error_type      TEXT,
    error_message   TEXT,
    first_content_ms INTEGER,
    total_ms        INTEGER NOT NULL DEFAULT 0,
    committed       INTEGER NOT NULL DEFAULT 0,
    created_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_attempt_logs_request ON attempt_logs(request_id);

CREATE TABLE IF NOT EXISTS daily_rollups (
    date              TEXT NOT NULL,
    downstream_key_id INTEGER NOT NULL DEFAULT 0,
    channel_id        INTEGER NOT NULL DEFAULT 0,
    upstream_model    TEXT NOT NULL DEFAULT '',
    requests          INTEGER NOT NULL DEFAULT 0,
    errors            INTEGER NOT NULL DEFAULT 0,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    search_requests   INTEGER NOT NULL DEFAULT 0,
    first_content_ms_sum INTEGER NOT NULL DEFAULT 0,
    first_content_samples INTEGER NOT NULL DEFAULT 0,
    total_ms_sum      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (date, downstream_key_id, channel_id, upstream_model)
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS admin (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    password_hash TEXT NOT NULL,
    updated_ms    INTEGER NOT NULL
);
"#;

/// 旧数据修正，每次启动执行（幂等）。
///
/// - 旧版手动停用 key 时也把 status 写成 `disabled`，后台于是把它显示成「被上游拒绝」。
///   上游自动禁用一定会写 `disabled_ms`，据此把手动停用的那些还原成 `ok`。
pub const DATA_FIXUPS: &str = r#"
UPDATE upstream_keys SET status = 'ok', status_reason = ''
 WHERE status = 'disabled' AND disabled_ms IS NULL;
"#;

/// 老库补列（每次启动执行，幂等）。
///
/// SQLite 没有 `ADD COLUMN IF NOT EXISTS`，所以先查表结构再加。
/// 只做「加列」这种无损操作，不动已有数据。
pub fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let has_secret: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('downstream_keys') WHERE name = 'secret'")?
        .exists([])?;
    if !has_secret {
        conn.execute_batch("ALTER TABLE downstream_keys ADD COLUMN secret TEXT")?;
    }
    Ok(())
}

/// 打开连接后统一设置的 pragma。
pub const PRAGMAS: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA temp_store = MEMORY;
"#;

pub const SCHEMA_VERSION: i64 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 老库补上_secret_列_且不动已有数据() {
        // 模拟一个旧版库：downstream_keys 没有 secret 列。
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE downstream_keys (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
                 key_hash TEXT NOT NULL UNIQUE, key_prefix TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1, created_ms INTEGER NOT NULL,
                 last_used_ms INTEGER);
             INSERT INTO downstream_keys (name, key_hash, key_prefix, created_ms)
             VALUES ('老 key', 'deadbeef', 'ufp-abc', 0);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // 幂等：跑第二次不该报错
        let (name, secret): (String, Option<String>) = conn
            .query_row("SELECT name, secret FROM downstream_keys", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "老 key");
        assert_eq!(secret, None, "老行没有明文，等后台轮换时补上");
    }

    #[test]
    fn 旧版手动停用的_key_被还原_上游拒绝的保持不动() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO channels (id, name, protocol, base_url, created_ms) VALUES (1, 'c', 'openai_chat', 'x', 0);
             -- 旧版手动停用：status 也被写成 disabled，但没有 disabled_ms
             INSERT INTO upstream_keys (id, channel_id, api_key, enabled, status, created_ms) VALUES (1, 1, 'a', 0, 'disabled', 0);
             -- 真被上游拒绝：一定带 disabled_ms
             INSERT INTO upstream_keys (id, channel_id, api_key, enabled, status, status_reason, created_ms, disabled_ms)
               VALUES (2, 1, 'b', 0, 'disabled', '401', 0, 123);",
        )
        .unwrap();
        conn.execute_batch(DATA_FIXUPS).unwrap();
        conn.execute_batch(DATA_FIXUPS).unwrap(); // 幂等
        let status = |id: i64| -> (String, i64) {
            conn.query_row(
                "SELECT status, enabled FROM upstream_keys WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(
            status(1),
            ("ok".to_string(), 0),
            "手动停用：状态还原，但仍然停用"
        );
        assert_eq!(
            status(2),
            ("disabled".to_string(), 0),
            "上游拒绝的不能被洗白"
        );
    }
}
