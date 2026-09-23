//! key × 模型 冷却。
//!
//! 额度类错误（429、402、日配额用尽）不该熔断整个渠道 —— 换个 key 就能继续。
//! 冷却时长优先取上游给的信号：
//! 1. `Retry-After` 响应头（秒或 HTTP 日期）；
//! 2. Gemini 错误体里的 `details[].retryDelay`（形如 `"31s"`）；
//! 3. Gemini 的 `QuotaFailure` 里带 `PerDay` 的配额 id → 冷却到配额重置时刻。
//!
//! 都没有就按次数指数退避。
//!
//! 与熔断不同，冷却会落库：日配额冷却动辄几小时，重启后不该重新去撞。

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::Connection;

use crate::store::Db;

#[derive(Debug, Clone)]
struct Entry {
    until_ms: i64,
    reason: String,
    /// 连续命中次数，用于指数退避。
    hits: u32,
}

#[derive(Debug, Clone)]
pub struct CooldownInfo {
    pub key_id: i64,
    pub model: String,
    pub until_ms: i64,
    pub reason: String,
}

pub struct Cooldowns {
    map: Mutex<HashMap<(i64, String), Entry>>,
}

impl Default for Cooldowns {
    fn default() -> Self {
        Self::new()
    }
}

/// 指数退避的基数与上限（无上游信号时使用）。
const BACKOFF_BASE_MS: i64 = 30_000;
const BACKOFF_MAX_MS: i64 = 30 * 60_000;

impl Cooldowns {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 从库里恢复未到期的冷却（启动时调用）。
    pub fn load(&self, conn: &Connection) -> rusqlite::Result<usize> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut stmt = conn
            .prepare("SELECT key_id, model, until_ms, reason FROM cooldowns WHERE until_ms > ?1")?;
        let rows = stmt.query_map([now], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            let (key_id, model, until_ms, reason) = row?;
            map.insert(
                (key_id, model),
                Entry {
                    until_ms,
                    reason,
                    hits: 1,
                },
            );
        }
        Ok(map.len())
    }

    /// 是否在冷却中。
    pub fn check(&self, key_id: i64, model: &str) -> Option<CooldownInfo> {
        let now = chrono::Utc::now().timestamp_millis();
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let e = map.get(&(key_id, model.to_string()))?;
        if e.until_ms > now {
            Some(CooldownInfo {
                key_id,
                model: model.to_string(),
                until_ms: e.until_ms,
                reason: e.reason.clone(),
            })
        } else {
            None
        }
    }

    /// 设置冷却（同时落库，跨重启生效）。
    pub fn set(&self, db: &Db, key_id: i64, model: &str, until_ms: i64, reason: &str) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let hits = map
            .get(&(key_id, model.to_string()))
            .map(|e| e.hits + 1)
            .unwrap_or(1);
        map.insert(
            (key_id, model.to_string()),
            Entry {
                until_ms,
                reason: reason.to_string(),
                hits,
            },
        );
        db.write_blocking(crate::store::Write::Cooldown {
            key_id,
            model: model.to_string(),
            until_ms,
            reason: reason.to_string(),
        });
    }

    /// 按「上游没给信号」的指数退避设置冷却。
    pub fn set_backoff(&self, db: &Db, key_id: i64, model: &str, reason: &str) -> i64 {
        let hits = {
            let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
            map.get(&(key_id, model.to_string()))
                .map(|e| e.hits + 1)
                .unwrap_or(1)
        };
        let backoff = BACKOFF_BASE_MS << hits.min(6);
        let backoff = backoff.min(BACKOFF_MAX_MS);
        let until = chrono::Utc::now().timestamp_millis() + backoff;
        self.set(db, key_id, model, until, reason);
        until
    }

    /// 手动清除（后台）。返回清除条数。
    pub fn clear(&self, db: &Db, key_id: Option<i64>, model: Option<&str>) -> usize {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|(kid, m), _| {
            let key_match = key_id.is_none_or(|k| k == *kid);
            let model_match = model.is_none_or(|want| want == m);
            !(key_match && model_match)
        });
        let removed = before - map.len();
        drop(map);
        db.write_blocking(crate::store::Write::CooldownClear {
            key_id,
            model: model.map(|m| m.to_string()),
        });
        removed
    }

    /// 清掉已过期条目（后台定时调用，顺手清理库里的过期行）。
    pub fn prune(&self, now_ms: i64) -> usize {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|_, e| e.until_ms > now_ms);
        before - map.len()
    }

    /// 后台展示用。
    pub fn snapshot(&self) -> Vec<CooldownInfo> {
        let now = chrono::Utc::now().timestamp_millis();
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<CooldownInfo> = map
            .iter()
            .filter(|(_, e)| e.until_ms > now)
            .map(|((key_id, model), e)| CooldownInfo {
                key_id: *key_id,
                model: model.clone(),
                until_ms: e.until_ms,
                reason: e.reason.clone(),
            })
            .collect();
        out.sort_by(|a, b| (a.key_id, &a.model).cmp(&(b.key_id, &b.model)));
        out
    }
}

/// 从响应头与错误体里解析冷却到期时刻。
///
/// 返回 `(until_ms, reason)`；都没有信号时返回 `None`，由调用方决定退回退避策略。
pub fn cooldown_from_response(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: Option<&serde_json::Value>,
) -> Option<(i64, String)> {
    let now = chrono::Utc::now().timestamp_millis();

    // 1) Retry-After（秒数或 HTTP 日期）
    if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Ok(secs) = v.parse::<i64>() {
            if secs > 0 {
                return Some((now + secs * 1000, format!("上游 Retry-After {secs}s")));
            }
        } else if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(v) {
            let until = dt.timestamp_millis();
            if until > now {
                return Some((until, "上游 Retry-After（HTTP 日期）".into()));
            }
        }
    }

    // 2) 各家错误体里的额度信息
    if let Some(body) = body {
        if let Some((until, reason)) = quota_signal(now, body) {
            return Some((until, reason));
        }
    }

    if status == 402 {
        // 余额/额度用尽：没有明确信号时按较长冷却处理，别一直撞。
        return Some((
            now + BACKOFF_MAX_MS,
            "上游返回 402（额度或余额不足）".into(),
        ));
    }
    None
}

/// 从错误体里找额度相关信号（Gemini 的 RetryInfo / QuotaFailure，以及常见字段）。
fn quota_signal(now: i64, body: &serde_json::Value) -> Option<(i64, String)> {
    // 通用：错误消息里带 "retryDelay": "31s"
    let text = body.to_string();
    if let Some(pos) = text.find("retryDelay") {
        let tail = &text[pos..];
        if let Some(s) = extract_delay_seconds(tail) {
            return Some((now + s * 1000, format!("上游建议 {s}s 后重试")));
        }
    }
    // Gemini 的 QuotaFailure：quotaId 带 PerDay 表示日配额
    if text.contains("PerDay") {
        let reset = next_utc_midnight_ms();
        return Some((reset, "日配额已用尽（冷却到配额重置）".into()));
    }
    if text.contains("RESOURCE_EXHAUSTED") || text.contains("quota") || text.contains("rate limit")
    {
        return None; // 交给退避策略
    }
    None
}

/// 从类似 `"retryDelay":"31s"` 的片段里取秒数。
fn extract_delay_seconds(text: &str) -> Option<i64> {
    let start = text.find(':')?;
    let rest = &text[start + 1..];
    let start = rest.find('"')?;
    let rest = &rest[start + 1..];
    let end = rest.find('"')?;
    let value = rest[..end].trim();
    if let Some(secs) = value.strip_suffix('s') {
        return secs.trim().parse::<f64>().ok().map(|f| f.ceil() as i64);
    }
    value.parse::<f64>().ok().map(|f| f.ceil() as i64)
}

/// 下一个 UTC 零点（日配额的常见重置时刻）。
fn next_utc_midnight_ms() -> i64 {
    use chrono::{Duration, TimeZone, Utc};
    let now = Utc::now();
    let tomorrow = (now + Duration::days(1)).date_naive();
    Utc.from_utc_datetime(&tomorrow.and_hms_opt(0, 0, 0).expect("合法的零点"))
        .timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 解析秒数形式的_retry_after() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "42".parse().unwrap());
        let (until, reason) = cooldown_from_response(429, &h, None).unwrap();
        let delta = until - chrono::Utc::now().timestamp_millis();
        assert!((41_000..=42_000).contains(&delta), "delta={delta} {reason}");
    }

    #[test]
    fn 解析_gemini_的_retry_delay() {
        let body = json!({
            "error": {
                "code": 429,
                "message": "Quota exceeded",
                "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "31s"}]
            }
        });
        let h = reqwest::header::HeaderMap::new();
        let (until, _) = cooldown_from_response(429, &h, Some(&body)).unwrap();
        let delta = until - chrono::Utc::now().timestamp_millis();
        assert!((30_000..=31_000).contains(&delta), "delta={delta}");
    }

    #[test]
    fn 日配额冷却到零点() {
        let body = json!({"error": {"details": [{"quotaId": "GenerateRequestsPerDayPerProjectPerModel-FreeTier"}]}});
        let h = reqwest::header::HeaderMap::new();
        let (until, reason) = cooldown_from_response(429, &h, Some(&body)).unwrap();
        assert!(reason.contains("日配额"), "{reason}");
        assert!(until > chrono::Utc::now().timestamp_millis());
    }

    #[test]
    fn 无信号时返回_none() {
        let h = reqwest::header::HeaderMap::new();
        assert!(cooldown_from_response(429, &h, None).is_none());
    }
}
