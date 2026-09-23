//! 会话 id 提取与「会话粘性」映射。
//!
//! 为什么要粘性（D6）：Claude Code 每一轮都把几万 token 的上下文原样重发，
//! 层内如果纯轮询，相邻两轮会落到不同 key、甚至不同模型上，上游的隐式前缀缓存
//! 全部落空（首 token 明显变慢），跨模型的思考签名也会失效。
//!
//! 提取顺序移植自 cc-switch 的 `proxy/session.rs`：
//! `x-claude-code-session-id` 头 → `metadata.session_id` → `metadata.user_id` 里的
//! `_session_` 后缀。映射落库（重启/升级不洗牌），内存里缓存并做节流写入。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::HeaderMap;
use rusqlite::Connection;
use serde_json::Value;

use crate::store::{Db, Write};

/// 从请求里提取会话 id；提取不到就没有粘性（每次按哈希分摊）。
pub fn extract_session_id(headers: &HeaderMap, body: &Value) -> Option<String> {
    for name in ["x-claude-code-session-id", "claude-code-session-id"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    let metadata = body.get("metadata")?;
    if let Some(user_id) = metadata.get("user_id").and_then(|v| v.as_str()) {
        if let Some(sid) = parse_session_from_user_id(user_id) {
            return Some(sid);
        }
    }
    if let Some(sid) = metadata.get("session_id").and_then(|v| v.as_str()) {
        if !sid.is_empty() {
            return Some(sid.to_string());
        }
    }
    None
}

/// 从 `user_xxx_session_yyy` 里取 `yyy`（与 cc-switch 的解析保持一致）。
pub(crate) fn parse_session_from_user_id(user_id: &str) -> Option<String> {
    let pos = user_id.find("_session_")?;
    let session_id = &user_id[pos + "_session_".len()..];
    if session_id.is_empty() {
        None
    } else {
        Some(session_id.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct StickyEntry {
    pub channel_id: i64,
    pub key_id: i64,
    pub entry_id: i64,
    pub upstream_model: String,
    pub updated_ms: i64,
    /// 上次落库时间，用于节流（测试里需要直接构造条目，所以对内可见）。
    pub(crate) persisted_ms: i64,
}

/// 落库节流间隔：映射没变且不到这个间隔就不写库。
const PERSIST_INTERVAL_MS: i64 = 10 * 60_000;

pub struct Sessions {
    /// 测试里需要直接塞一条映射，所以对内可见。
    pub(crate) map: Mutex<HashMap<String, StickyEntry>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 启动时从库里恢复。
    pub fn load(&self, conn: &Connection) -> rusqlite::Result<usize> {
        let mut stmt = conn.prepare(
            "SELECT session_id, channel_id, key_id, entry_id, upstream_model, updated_ms FROM sessions",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            let (session_id, channel_id, key_id, entry_id, upstream_model, updated_ms) = row?;
            map.insert(
                session_id,
                StickyEntry {
                    channel_id,
                    key_id,
                    entry_id,
                    upstream_model,
                    updated_ms,
                    persisted_ms: updated_ms,
                },
            );
        }
        Ok(map.len())
    }

    pub fn get(&self, session_id: &str) -> Option<StickyEntry> {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session_id).cloned()
    }

    /// 记录（或迁移）某个会话的落点。映射变了立即落库，否则按间隔节流。
    pub fn set(
        &self,
        db: &Db,
        session_id: &str,
        channel_id: i64,
        key_id: i64,
        entry_id: i64,
        upstream_model: &str,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let persist = match map.get(session_id) {
            Some(prev) => {
                prev.entry_id != entry_id
                    || prev.key_id != key_id
                    || now - prev.persisted_ms >= PERSIST_INTERVAL_MS
            }
            None => true,
        };
        map.insert(
            session_id.to_string(),
            StickyEntry {
                channel_id,
                key_id,
                entry_id,
                upstream_model: upstream_model.to_string(),
                updated_ms: now,
                persisted_ms: if persist { now } else { 0 },
            },
        );
        drop(map);
        if persist {
            db.write(Write::SessionUpsert {
                session_id: session_id.to_string(),
                channel_id,
                key_id,
                entry_id,
                upstream_model: upstream_model.to_string(),
            });
        }
    }

    /// 清理过期映射（内存 + 库）。返回内存里清掉的条数。
    pub fn prune(&self, db: &Db, ttl_days: u64) -> usize {
        let cutoff = chrono::Utc::now().timestamp_millis() - (ttl_days as i64) * 86_400_000;
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, e| e.updated_ms >= cutoff);
        let removed = before - map.len();
        drop(map);
        db.write_blocking(Write::PruneSessions { cutoff_ms: cutoff });
        removed
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 优先用会话头() {
        let mut h = HeaderMap::new();
        h.insert("x-claude-code-session-id", "abc".parse().unwrap());
        let body = json!({"metadata": {"user_id": "u_session_other"}});
        assert_eq!(extract_session_id(&h, &body).as_deref(), Some("abc"));
    }

    #[test]
    fn 从_user_id_里解析() {
        let h = HeaderMap::new();
        let body = json!({"metadata": {"user_id": "user_123_session_9f8e"}});
        assert_eq!(extract_session_id(&h, &body).as_deref(), Some("9f8e"));
    }

    #[test]
    fn 没有会话信息时返回_none() {
        let h = HeaderMap::new();
        assert!(extract_session_id(&h, &json!({"messages": []})).is_none());
    }
}
