//! 每日维护：明细汇总、过期清理、数据库备份。
//!
//! 明细表（request_logs / attempt_logs）只保留一段时间，之后按
//! (日期, 下游 key, 渠道, 上游模型) 汇总进 `daily_rollups` 永久保留 ——
//! 这样 10G 磁盘上用几个月也不会撑爆，而长期趋势还在。
//!
//! 备份用 `VACUUM INTO`：在线的、一致的 SQLite 快照，不用停机。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use crate::store::Db;

/// 保留多少份备份。
const BACKUP_KEEP: usize = 7;

#[derive(Debug, Default, Clone)]
pub struct MaintenanceReport {
    pub rolled_up: i64,
    pub pruned_details: i64,
    pub pruned_attempts: i64,
    pub pruned_cooldowns: i64,
    pub pruned_sessions: i64,
    pub backup: Option<PathBuf>,
    pub vacuums_removed: usize,
}

/// 跑一轮每日维护。`cutoff_ms` 之前（含）的明细会被汇总并删除。
pub async fn run_daily(
    db: &Arc<Db>,
    cutoff_ms: i64,
    session_ttl_days: u64,
    backup_dir: &Path,
) -> rusqlite::Result<MaintenanceReport> {
    let backup_dir = backup_dir.to_path_buf();
    let session_cutoff =
        chrono::Utc::now().timestamp_millis() - (session_ttl_days as i64) * 86_400_000;
    db.admin(move |conn| {
        let mut report = MaintenanceReport::default();
        let tx = conn.unchecked_transaction()?;

        report.rolled_up = tx.execute(
            "INSERT INTO daily_rollups (
                 date, downstream_key_id, channel_id, upstream_model,
                 requests, errors, input_tokens, output_tokens,
                 cache_read_tokens, cache_creation_tokens, search_requests,
                 first_content_ms_sum, first_content_samples, total_ms_sum)
             SELECT date(created_ms/1000,'unixepoch'),
                    COALESCE(downstream_key_id, 0), COALESCE(channel_id, 0), upstream_model,
                    COUNT(*),
                    SUM(CASE WHEN http_status >= 400 THEN 1 ELSE 0 END),
                    SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_read_tokens), SUM(cache_creation_tokens), SUM(search_requests),
                    SUM(COALESCE(first_content_ms, 0)),
                    SUM(CASE WHEN first_content_ms IS NOT NULL THEN 1 ELSE 0 END),
                    SUM(total_ms)
             FROM request_logs WHERE created_ms < ?1
             GROUP BY 1, 2, 3, 4
             ON CONFLICT(date, downstream_key_id, channel_id, upstream_model) DO UPDATE SET
                 requests = requests + excluded.requests,
                 errors = errors + excluded.errors,
                 input_tokens = input_tokens + excluded.input_tokens,
                 output_tokens = output_tokens + excluded.output_tokens,
                 cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens,
                 cache_creation_tokens = cache_creation_tokens + excluded.cache_creation_tokens,
                 search_requests = search_requests + excluded.search_requests,
                 first_content_ms_sum = first_content_ms_sum + excluded.first_content_ms_sum,
                 first_content_samples = first_content_samples + excluded.first_content_samples,
                 total_ms_sum = total_ms_sum + excluded.total_ms_sum",
            rusqlite::params![cutoff_ms],
        )? as i64;

        report.pruned_details = tx.execute(
            "DELETE FROM request_logs WHERE created_ms < ?1",
            rusqlite::params![cutoff_ms],
        )? as i64;
        report.pruned_attempts = tx.execute(
            "DELETE FROM attempt_logs WHERE created_ms < ?1",
            rusqlite::params![cutoff_ms],
        )? as i64;
        report.pruned_cooldowns = tx.execute(
            "DELETE FROM cooldowns WHERE until_ms < ?1",
            rusqlite::params![chrono::Utc::now().timestamp_millis()],
        )? as i64;
        report.pruned_sessions = tx.execute(
            "DELETE FROM sessions WHERE updated_ms < ?1",
            rusqlite::params![session_cutoff],
        )? as i64;

        tx.commit()?;

        // 备份：VACUUM INTO 出来的是一致快照
        report.backup = backup(conn, &backup_dir).ok().flatten();
        report.vacuums_removed = prune_backups(&backup_dir).unwrap_or(0);
        Ok(report)
    })
    .await
}

fn backup(conn: &Connection, dir: &Path) -> rusqlite::Result<Option<PathBuf>> {
    if !dir.exists() && std::fs::create_dir_all(dir).is_err() {
        return Ok(None);
    }
    let name = format!("ufp-{}.db", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(name);
    // VACUUM INTO 不接受参数绑定的路径，这里只拼我们自己生成的文件名
    conn.execute(&format!("VACUUM INTO '{}'", path.display()), [])?;
    Ok(Some(path))
}

fn prune_backups(dir: &Path) -> std::io::Result<usize> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("ufp-") && n.ends_with(".db"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    let mut removed = 0;
    while files.len() > BACKUP_KEEP {
        let old = files.remove(0);
        if std::fs::remove_file(&old).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 汇总并清理过期明细() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        // 造两条「很旧」的明细
        db.admin(|conn| {
            for i in 0..2 {
                conn.execute(
                    "INSERT INTO request_logs (request_id, requested_model, upstream_model, http_status,
                        input_tokens, output_tokens, search_requests, attempts, streaming, total_ms, created_ms)
                     VALUES (?1, 'm', 'up', 200, 100, 10, 0, 1, 1, 500, ?2)",
                    rusqlite::params![format!("r{i}"), 1_000_000i64],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

        let cutoff = chrono::Utc::now().timestamp_millis() - 86_400_000;
        let report = run_daily(&db, cutoff, 30, &dir.path().join("backup"))
            .await
            .unwrap();
        assert_eq!(report.pruned_details, 2);
        assert!(report.rolled_up >= 1);
        assert!(report.backup.is_some(), "应当生成一份备份");

        let (requests, input): (i64, i64) = db
            .read(|conn| {
                conn.query_row(
                    "SELECT SUM(requests), SUM(input_tokens) FROM daily_rollups",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(requests, 2);
        assert_eq!(input, 200);
    }
}
