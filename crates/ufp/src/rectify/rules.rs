//! 矫正规则的落库、查找与错误指纹。
//!
//! 「指纹」把一条报错归一化成可复用、可展示的键：去掉具体数字与引号里的值，
//! 只留错误的结构，再挂一个短哈希。这样同一个坑再次出现时能直接套用上次的补丁，
//! 不必每次都去问分析条目（省额度、省时间）。

use std::sync::Arc;

use rusqlite::params;

use crate::api::AppState;
use crate::store::Write;

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: i64,
    pub scope: String,
    pub error_fingerprint: String,
    pub patch_json: String,
    pub source: String,
    pub enabled: bool,
}

/// 归一化错误信息并生成指纹。
pub fn fingerprint(protocol: &str, status: u16, message: &str) -> String {
    let mut normalized = String::with_capacity(message.len());
    let mut last_was_digit = false;
    for ch in message.chars().take(400) {
        if ch.is_ascii_digit() {
            if !last_was_digit {
                normalized.push('#');
            }
            last_was_digit = true;
            continue;
        }
        last_was_digit = false;
        if ch.is_whitespace() {
            if !normalized.ends_with(' ') {
                normalized.push(' ');
            }
        } else {
            normalized.push(ch.to_ascii_lowercase());
        }
    }
    let normalized = normalized.trim();
    let digest = short_hash(normalized);
    format!(
        "{protocol}|{status}|{}|{digest}",
        truncate_chars(normalized, 160)
    )
}

fn short_hash(s: &str) -> String {
    // FNV-1a：稳定、够用，不引依赖
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h >> 32) ^ (h & 0xffff_ffff))
}

fn truncate_chars(s: &str, keep: usize) -> String {
    if s.chars().count() <= keep {
        return s.to_string();
    }
    s.chars().take(keep).collect()
}

/// 找一条启用的规则：先按协议作用域，再退到全局（scope = ""）。
pub async fn find(state: &Arc<AppState>, protocol: &str, fingerprint: &str) -> Option<Rule> {
    let protocol = protocol.to_string();
    let fingerprint = fingerprint.to_string();
    state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, error_fingerprint, patch_json, source, enabled
                 FROM rectify_rules
                 WHERE error_fingerprint = ?1 AND enabled = 1 AND (scope = ?2 OR scope = '')
                 ORDER BY CASE WHEN scope = ?2 THEN 0 ELSE 1 END LIMIT 1",
            )?;
            let mut rows = stmt.query(params![fingerprint, protocol])?;
            match rows.next()? {
                Some(r) => Ok(Some(Rule {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    error_fingerprint: r.get(2)?,
                    patch_json: r.get(3)?,
                    source: r.get(4)?,
                    enabled: r.get::<_, i64>(5)? != 0,
                })),
                None => Ok(None),
            }
        })
        .await
        .ok()
        .flatten()
}

/// 记录规则命中（用于后台展示「这条规则救了多少次」）。
pub fn note_hit(state: &Arc<AppState>, rule_id: i64) {
    state.db.write(Write::RuleHit { rule_id });
}

/// 写入（或更新）一条规则。返回规则 id。
pub async fn upsert(
    state: &Arc<AppState>,
    protocol: &str,
    fingerprint: &str,
    error_sample: &str,
    patches: &[crate::rectify::patch::PatchOp],
    source: &str,
) -> Result<i64, String> {
    let patch_json = patches_to_json(patches)?;
    let (protocol, fingerprint, error_sample, source) = (
        protocol.to_string(),
        fingerprint.to_string(),
        error_sample.to_string(),
        source.to_string(),
    );
    let now = chrono::Utc::now().timestamp_millis();
    state
        .db
        .admin(move |conn| {
            conn.execute(
                "INSERT INTO rectify_rules (scope, error_fingerprint, error_sample, patch_json,
                    source, enabled, created_ms, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
                 ON CONFLICT(scope, error_fingerprint) DO UPDATE SET
                    patch_json = ?4, error_sample = ?3, updated_ms = ?6",
                params![
                    protocol,
                    fingerprint,
                    crate::pipeline::truncate_error(&error_sample),
                    patch_json,
                    source,
                    now
                ],
            )?;
            conn.query_row(
                "SELECT id FROM rectify_rules WHERE scope = ?1 AND error_fingerprint = ?2",
                params![protocol, fingerprint],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .map_err(|e| format!("写规则失败：{e}"))
}

pub fn patches_to_json(patches: &[crate::rectify::patch::PatchOp]) -> Result<String, String> {
    use crate::rectify::patch::PatchOpKind;
    let items: Vec<serde_json::Value> = patches
        .iter()
        .map(|p| {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "op".into(),
                serde_json::json!(match p.op {
                    PatchOpKind::Replace => "replace",
                    PatchOpKind::Add => "add",
                    PatchOpKind::Remove => "remove",
                }),
            );
            obj.insert("path".into(), serde_json::json!(p.path));
            if let Some(v) = &p.value {
                obj.insert("value".into(), v.clone());
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::to_string(&items).map_err(|e| format!("序列化补丁失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 指纹对数字与大小写不敏感() {
        let a = fingerprint(
            "openai_chat",
            400,
            "max_tokens: 200000 > 65536, which is the maximum",
        );
        let b = fingerprint(
            "openai_chat",
            400,
            "MAX_TOKENS: 12345 > 9999, WHICH IS THE MAXIMUM",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn 指纹区分不同错误() {
        let a = fingerprint("openai_chat", 400, "invalid tool schema");
        let b = fingerprint("openai_chat", 400, "invalid image format");
        assert_ne!(a, b);
        let c = fingerprint("gemini", 400, "invalid tool schema");
        assert_ne!(a, c, "不同协议应分开");
    }

    #[test]
    fn 补丁序列化可往返() {
        let ops = crate::rectify::patch::parse(&serde_json::json!([
            {"op": "replace", "path": "/max_tokens", "value": 4096}
        ]))
        .unwrap();
        let json = patches_to_json(&ops).unwrap();
        assert!(json.contains("/max_tokens"));
        let back = crate::rectify::patch::parse(&serde_json::from_str(&json).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
    }
}
