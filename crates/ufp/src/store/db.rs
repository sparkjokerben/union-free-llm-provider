//! SQLite 访问层。
//!
//! 分工：
//! - **写**：一个专用 std 线程独占写连接，通过有界 channel 收写入请求，凑批后
//!   在一个事务里提交。用量/尝试日志属于「尽力而为」，队列满时直接丢弃并计数，
//!   绝不让写库拖慢或拖垮请求路径。
//! - **读**：后台管理与快照重载用的少量只读连接，跑在 `spawn_blocking` 上。
//! - **管理写**：后台的增删改走独立的一条读写连接，直接在 `spawn_blocking` 里执行
//!   （WAL + busy_timeout 足以应付极低频率的后台操作）。
//! - **热路径**：完全不碰 SQLite —— 路由读的是 `ArcSwap` 里的配置快照，熔断/冷却
//!   状态在内存里（冷却另有落库用于重启恢复）。

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

use super::schema::{PRAGMAS, SCHEMA};

/// 写队列容量。满了就丢日志，只计数。
const WRITE_QUEUE: usize = 8192;
/// 单批最多提交多少条写请求。
const WRITE_BATCH: usize = 256;
/// 队列空时最长等待多久再回去看有没有新写入（毫秒）。
const WRITE_TICK_MS: u64 = 500;
/// 只读连接数。
const READERS: usize = 2;

pub type DbResult<T> = Result<T, rusqlite::Error>;

/// 一条待写记录。新增写入类型时在这里加变体，并在 `apply` 里实现。
pub enum Write {
    RequestLog(Box<RequestLogRow>),
    AttemptLog(Box<AttemptLogRow>),
    /// 冷却落库（重启后仍生效，日配额冷却会跨重启）。
    Cooldown {
        key_id: i64,
        model: String,
        until_ms: i64,
        reason: String,
    },
    /// key 被上游拒绝（401/403）：禁用并在后台标红。
    DisableKey {
        key_id: i64,
        reason: String,
    },
    /// 会话粘性映射。
    SessionUpsert {
        session_id: String,
        channel_id: i64,
        key_id: i64,
        entry_id: i64,
        upstream_model: String,
    },
    /// 矫正规则命中计数。
    RuleHit {
        rule_id: i64,
    },
    /// 下游 key 最近使用时间。
    DownstreamKeyUsed {
        key_id: i64,
    },
    /// 搜索后端冷却。
    SearchCooldown {
        backend_id: i64,
        until_ms: i64,
    },
    /// 手动清除冷却（后台）；key_id / model 为 None 表示该维度不限。
    CooldownClear {
        key_id: Option<i64>,
        model: Option<String>,
    },
    /// 清理过期会话映射。
    PruneSessions {
        cutoff_ms: i64,
    },
}

#[derive(Debug, Clone, Default)]
pub struct RequestLogRow {
    pub request_id: String,
    pub downstream_key_id: Option<i64>,
    pub session_id: Option<String>,
    pub requested_model: String,
    pub upstream_model: String,
    pub channel_id: Option<i64>,
    pub key_id: Option<i64>,
    pub http_status: i64,
    pub stop_reason: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub search_requests: i64,
    pub attempts: i64,
    pub streaming: bool,
    pub first_content_ms: Option<i64>,
    pub total_ms: i64,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AttemptLogRow {
    pub request_id: String,
    pub attempt_no: i64,
    pub channel_id: Option<i64>,
    pub key_id: Option<i64>,
    pub upstream_model: String,
    pub protocol: String,
    pub http_status: Option<i64>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub first_content_ms: Option<i64>,
    pub total_ms: i64,
    pub committed: bool,
    pub created_ms: i64,
}

pub struct Db {
    tx: SyncSender<Write>,
    readers: Vec<Mutex<Connection>>,
    admin: Mutex<Connection>,
    cursor: AtomicUsize,
    dropped: AtomicUsize,
}

impl Db {
    /// 打开（必要时创建）数据库，建表、启动写线程与独立连接。
    pub fn open(path: &Path) -> DbResult<Arc<Self>> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(PRAGMAS)?;
        conn.execute_batch(SCHEMA)?;
        drop(conn);

        let (tx, rx) = sync_channel::<Write>(WRITE_QUEUE);

        // 写线程：独占写连接，凑批提交。
        {
            let path = path.to_path_buf();
            thread::Builder::new()
                .name("ufp-db-writer".into())
                .spawn(move || {
                    let conn = match Connection::open(&path) {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::error!(error = %e, "打开写连接失败，用量统计将被丢弃");
                            return;
                        }
                    };
                    if let Err(e) = conn.execute_batch(PRAGMAS) {
                        tracing::error!(error = %e, "写连接 pragma 设置失败");
                    }
                    // 先阻塞等一条，再尽量凑批。
                    while let Ok(first) = rx.recv() {
                        let mut batch = vec![first];
                        while batch.len() < WRITE_BATCH {
                            match rx.recv_timeout(Duration::from_millis(WRITE_TICK_MS)) {
                                Ok(w) => batch.push(w),
                                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        if let Err(e) = apply_batch(&conn, batch) {
                            tracing::warn!(error = %e, "写入用量/状态失败");
                        }
                    }
                })
                .expect("启动写线程失败");
        }

        // 只读连接。
        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            let conn = Connection::open(path)?;
            conn.execute_batch(PRAGMAS)?;
            conn.execute_batch("PRAGMA query_only = ON;")?;
            readers.push(Mutex::new(conn));
        }

        let admin = Connection::open(path)?;
        admin.execute_batch(PRAGMAS)?;

        Ok(Arc::new(Self {
            tx,
            readers,
            admin: Mutex::new(admin),
            cursor: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }))
    }

    /// 异步入队一条写入；队列满时丢弃并计数（日志类写入可丢）。
    pub fn write(&self, w: Write) {
        if let Err(std::sync::mpsc::TrySendError::Full(_)) = self.tx.try_send(w) {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 100 == 1 {
                tracing::warn!(dropped = n, "写队列已满，丢弃用量记录");
            }
        }
    }

    /// 入队一条必须落地的写入（key 禁用、冷却、会话映射）；队列满时阻塞等待。
    pub fn write_blocking(&self, w: Write) {
        let _ = self.tx.send(w);
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// 在一个只读连接上跑一段查询。
    pub async fn read<T, F>(self: &Arc<Self>, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    {
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let idx = me.cursor.fetch_add(1, Ordering::Relaxed) % me.readers.len();
            let guard = me.readers[idx].lock().unwrap_or_else(|e| e.into_inner());
            f(&guard)
        })
        .await
        .expect("读线程池异常退出")
    }

    /// 在管理连接上执行一段读写的操作（后台增删改）。串行执行，不会并发写。
    pub async fn admin<T, F>(self: &Arc<Self>, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> DbResult<T> + Send + 'static,
    {
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut guard = me.admin.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut guard)
        })
        .await
        .expect("管理连接线程池异常退出")
    }
}

fn apply_batch(conn: &Connection, batch: Vec<Write>) -> DbResult<()> {
    let tx = conn.unchecked_transaction()?;
    let now = chrono::Utc::now().timestamp_millis();
    for w in batch {
        apply(conn, w, now)?;
    }
    tx.commit()
}

fn apply(conn: &Connection, w: Write, now: i64) -> DbResult<()> {
    match w {
        Write::RequestLog(r) => {
            conn.execute(
                "INSERT INTO request_logs (request_id, downstream_key_id, session_id, requested_model,
                    upstream_model, channel_id, key_id, http_status, stop_reason, input_tokens,
                    output_tokens, cache_read_tokens, cache_creation_tokens, search_requests,
                    attempts, streaming, first_content_ms, total_ms, error_type, error_message, created_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                rusqlite::params![
                    r.request_id,
                    r.downstream_key_id,
                    r.session_id,
                    r.requested_model,
                    r.upstream_model,
                    r.channel_id,
                    r.key_id,
                    r.http_status,
                    r.stop_reason,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cache_creation_tokens,
                    r.search_requests,
                    r.attempts,
                    r.streaming as i64,
                    r.first_content_ms,
                    r.total_ms,
                    r.error_type,
                    r.error_message,
                    r.created_ms,
                ],
            )?;
        }
        Write::AttemptLog(a) => {
            conn.execute(
                "INSERT INTO attempt_logs (request_id, attempt_no, channel_id, key_id, upstream_model,
                    protocol, http_status, error_type, error_message, first_content_ms, total_ms,
                    committed, created_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                rusqlite::params![
                    a.request_id,
                    a.attempt_no,
                    a.channel_id,
                    a.key_id,
                    a.upstream_model,
                    a.protocol,
                    a.http_status,
                    a.error_type,
                    a.error_message,
                    a.first_content_ms,
                    a.total_ms,
                    a.committed as i64,
                    a.created_ms,
                ],
            )?;
        }
        Write::Cooldown {
            key_id,
            model,
            until_ms,
            reason,
        } => {
            conn.execute(
                "INSERT INTO cooldowns (key_id, model, until_ms, reason, created_ms)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(key_id, model) DO UPDATE SET until_ms = ?3, reason = ?4",
                rusqlite::params![key_id, model, until_ms, reason, now],
            )?;
        }
        Write::DisableKey { key_id, reason } => {
            conn.execute(
                "UPDATE upstream_keys SET enabled = 0, status = 'disabled', status_reason = ?2, disabled_ms = ?3
                 WHERE id = ?1",
                rusqlite::params![key_id, reason, now],
            )?;
        }
        Write::SessionUpsert {
            session_id,
            channel_id,
            key_id,
            entry_id,
            upstream_model,
        } => {
            conn.execute(
                "INSERT INTO sessions (session_id, channel_id, key_id, entry_id, upstream_model, updated_ms, hits)
                 VALUES (?1,?2,?3,?4,?5,?6,1)
                 ON CONFLICT(session_id) DO UPDATE SET
                   channel_id = ?2, key_id = ?3, entry_id = ?4, upstream_model = ?5,
                   updated_ms = ?6, hits = hits + 1",
                rusqlite::params![session_id, channel_id, key_id, entry_id, upstream_model, now],
            )?;
        }
        Write::RuleHit { rule_id } => {
            conn.execute(
                "UPDATE rectify_rules SET hits = hits + 1, updated_ms = ?2 WHERE id = ?1",
                rusqlite::params![rule_id, now],
            )?;
        }
        Write::DownstreamKeyUsed { key_id } => {
            conn.execute(
                "UPDATE downstream_keys SET last_used_ms = ?2 WHERE id = ?1",
                rusqlite::params![key_id, now],
            )?;
        }
        Write::SearchCooldown {
            backend_id,
            until_ms,
        } => {
            conn.execute(
                "UPDATE search_backends SET cooldown_until_ms = ?2 WHERE id = ?1",
                rusqlite::params![backend_id, until_ms],
            )?;
        }
        Write::CooldownClear { key_id, model } => {
            // 逐条匹配，避免拼接 SQL。
            match (key_id, model) {
                (Some(k), Some(m)) => {
                    conn.execute(
                        "DELETE FROM cooldowns WHERE key_id = ?1 AND model = ?2",
                        rusqlite::params![k, m],
                    )?;
                }
                (Some(k), None) => {
                    conn.execute(
                        "DELETE FROM cooldowns WHERE key_id = ?1",
                        rusqlite::params![k],
                    )?;
                }
                (None, Some(m)) => {
                    conn.execute(
                        "DELETE FROM cooldowns WHERE model = ?1",
                        rusqlite::params![m],
                    )?;
                }
                (None, None) => {
                    conn.execute("DELETE FROM cooldowns", [])?;
                }
            }
        }
        Write::PruneSessions { cutoff_ms } => {
            conn.execute(
                "DELETE FROM sessions WHERE updated_ms < ?1",
                rusqlite::params![cutoff_ms],
            )?;
        }
    }
    Ok(())
}
