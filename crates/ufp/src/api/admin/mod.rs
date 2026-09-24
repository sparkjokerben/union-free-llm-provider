//! 后台管理 API。
//!
//! - `/admin` 返回内嵌的单页界面（无前端构建步骤，`include_str!` 进二进制）；
//! - `/admin/api/*` 是需要登录的 JSON 接口：统计、渠道/key/条目、下游 key、
//!   熔断与冷却状态与重置、矫正规则、搜索后端、运行期设置、导入导出。
//!
//! 所有写操作都走 `mutate()`：写完立刻重载配置快照，改动即时生效，不需要重启。

pub mod presets;
pub mod session;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;

use crate::api::error::ApiError;
use crate::api::AppState;
use crate::store::{save_settings, Settings, Write};
use session::{client_ip, require_auth, session_cookie};

const INDEX_HTML: &str = include_str!("ui/index.html");
const APP_JS: &str = include_str!("ui/app.js");

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/admin", get(index))
        .route("/admin/app.js", get(app_js))
        .route("/admin/api/login", post(login))
        .route("/admin/api/logout", post(logout))
        .route("/admin/api/password", post(change_password))
        .route("/admin/api/overview", get(overview))
        .route("/admin/api/stats", get(stats))
        .route("/admin/api/requests", get(requests))
        .route("/admin/api/attempts", get(attempts))
        .route("/admin/api/pulse", get(pulse))
        .route(
            "/admin/api/channels",
            get(list_channels).post(create_channel),
        )
        .route(
            "/admin/api/channels/{id}",
            patch(update_channel).delete(delete_channel),
        )
        .route("/admin/api/channels/{id}/keys", post(create_key))
        .route("/admin/api/keys/{id}", patch(update_key).delete(delete_key))
        .route("/admin/api/keys/{id}/enable", post(enable_key))
        .route("/admin/api/channels/{id}/entries", post(create_entry))
        .route(
            "/admin/api/entries/{id}",
            patch(update_entry).delete(delete_entry),
        )
        .route(
            "/admin/api/downstream_keys",
            get(list_downstream_keys).post(create_downstream_key),
        )
        .route(
            "/admin/api/downstream_keys/{id}",
            patch(update_downstream_key).delete(delete_downstream_key),
        )
        .route("/admin/api/health", get(health))
        .route("/admin/api/health/reset", post(reset_health))
        .route("/admin/api/rules", get(list_rules))
        .route(
            "/admin/api/rules/{id}",
            patch(update_rule).delete(delete_rule),
        )
        .route(
            "/admin/api/search_backends",
            get(list_search_backends).post(create_search_backend),
        )
        .route(
            "/admin/api/search_backends/{id}",
            patch(update_search_backend).delete(delete_search_backend),
        )
        .route("/admin/api/search_backends/test", post(test_search_backend))
        .route("/admin/api/presets", get(presets::list))
        .route("/admin/api/presets/{id}/models", post(presets::models))
        .route("/admin/api/presets/{id}/apply", post(presets::apply))
        .route("/admin/api/settings", get(get_settings).put(put_settings))
        .route("/admin/api/test_connection", post(test_connection))
        .route("/admin/api/deploy", post(deploy))
        .route("/admin/api/deploy-token", post(rotate_deploy_token))
        .route("/admin/api/export", get(export_config))
        .route("/admin/api/import", post(import_config))
}

// ============================================================================
// 静态界面
// ============================================================================

/// 后台页面：注入当前版本号（脚本 URL 带版本，换版本必然换 URL，不会被旧缓存粘住），
/// 并且一律 no-store —— 后台资源必须跟着二进制走，缓存过期策略在这里只会帮倒忙。
async fn index() -> impl IntoResponse {
    let html = INDEX_HTML.replace("__UFP_VERSION__", crate::VERSION);
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
}

async fn app_js() -> impl IntoResponse {
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        APP_JS,
    )
}

// ============================================================================
// 登录
// ============================================================================

#[derive(Deserialize)]
struct LoginPayload {
    password: String,
}

async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<LoginPayload>,
) -> Result<Response, Response> {
    let ip = client_ip(&headers);
    if state.admin_sessions.too_many(&ip) {
        return Err(session::unauthorized("登录失败次数过多，请稍后再试"));
    }
    let stored: Option<String> = state
        .db
        .admin(|conn| {
            conn.query_row("SELECT password_hash FROM admin WHERE id = 1", [], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await
        .map_err(internal)?;
    let Some(stored) = stored else {
        return Err(session::unauthorized(
            "还没有设置后台密码，先在服务器上运行：ufp set-admin-password",
        ));
    };
    let parsed = PasswordHash::new(&stored)
        .map_err(|e| session::unauthorized(&format!("密码哈希损坏：{e}")))?;
    if Argon2::default()
        .verify_password(payload.password.as_bytes(), &parsed)
        .is_err()
    {
        state.admin_sessions.note_failure(&ip);
        return Err(session::unauthorized("密码不正确"));
    }
    state.admin_sessions.clear_failures(&ip);
    let ttl_hours = state.pool.load().settings.admin_session_hours.max(1);
    let token = state
        .admin_sessions
        .issue(Duration::from_secs(ttl_hours * 3600));
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::SET_COOKIE,
            session_cookie(&token, ttl_hours),
        )],
        Json(json!({"ok": true})),
    )
        .into_response())
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = session::token_from_headers(&headers) {
        state.admin_sessions.revoke(&token);
    }
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, session::clear_cookie())],
        Json(json!({"ok": true})),
    )
        .into_response()
}

#[derive(Deserialize)]
struct PasswordPayload {
    old_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<PasswordPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if payload.new_password.len() < 8 {
        return Err(session::unauthorized("新密码至少 8 位"));
    }
    let stored: Option<String> = state
        .db
        .admin(|conn| {
            conn.query_row("SELECT password_hash FROM admin WHERE id = 1", [], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await
        .map_err(internal)?;
    if let Some(stored) = stored {
        let parsed = PasswordHash::new(&stored).map_err(internal)?;
        if Argon2::default()
            .verify_password(payload.old_password.as_bytes(), &parsed)
            .is_err()
        {
            return Err(session::unauthorized("原密码不正确"));
        }
    }
    let hash = hash_password(&payload.new_password).map_err(internal)?;
    mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO admin (id, password_hash, updated_ms) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET password_hash = ?1, updated_ms = ?2",
            params![hash, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("哈希失败：{e}"))
}

// ============================================================================
// 概览与统计
// ============================================================================

async fn overview(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let pool = state.pool.load();
    let (channels, entries, keys, downstream) = (
        pool.channels.len(),
        pool.entries.len(),
        pool.keys.values().map(|v| v.len()).sum::<usize>(),
        pool.downstream.len(),
    );
    let search_backends = pool_search_backends(&state).await;
    drop(pool);

    let today_start = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().timestamp_millis())
        .unwrap_or(0);
    let one_hour_ago = chrono::Utc::now().timestamp_millis() - 3_600_000;
    let summary = state
        .db
        .read(move |conn| {
            let today = conn.query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0),
                        COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(search_requests),0),
                        COALESCE(SUM(CASE WHEN http_status >= 400 THEN 1 ELSE 0 END),0),
                        COALESCE(CAST(AVG(first_content_ms) AS INTEGER),0)
                 FROM request_logs WHERE created_ms >= ?1",
                params![today_start],
                |r| {
                    Ok(json!({
                        "requests": r.get::<_, i64>(0)?,
                        "input_tokens": r.get::<_, i64>(1)?,
                        "output_tokens": r.get::<_, i64>(2)?,
                        "cache_read_tokens": r.get::<_, i64>(3)?,
                        "search_requests": r.get::<_, i64>(4)?,
                        "errors": r.get::<_, i64>(5)?,
                        "avg_first_content_ms": r.get::<_, i64>(6)?,
                    }))
                },
            )?;
            let recent_errors: i64 = conn.query_row(
                "SELECT COUNT(*) FROM request_logs WHERE created_ms >= ?1 AND http_status >= 400",
                params![one_hour_ago],
                |r| r.get(0),
            )?;
            Ok(json!({ "today": today, "recent_errors": recent_errors }))
        })
        .await
        .map_err(internal)?;

    let breakers = state.breakers.snapshot();
    let cooldowns = state.cooldowns.snapshot();
    Ok(Json(json!({
        "pool": {
            "channels": channels,
            "entries": entries,
            "upstream_keys": keys,
            "downstream_keys": downstream,
            "search_backends": search_backends,
        },
        "stats": summary,
        "health": {
            "breakers_open": breakers.iter().filter(|b| b.state != crate::health::BreakerState::Closed).count(),
            "breakers": breakers.len(),
            "cooldowns": cooldowns.len(),
        },
        "deploy": {
            "token_set": !current_deploy_token(&state).await.is_empty(),
            "apply_script": APPLY_SCRIPT,
            "spool": DEPLOY_SPOOL,
        },
        "runtime": {
            "uptime_ms": chrono::Utc::now().timestamp_millis() - state.started_ms,
            "inflight": state.cfg.max_inflight.saturating_sub(state.inflight.available_permits()),
            "max_inflight": state.cfg.max_inflight,
            "dropped_writes": state.db.dropped(),
            "version": crate::VERSION,
        }
    }))
    .into_response())
}

async fn pool_search_backends(state: &Arc<AppState>) -> usize {
    state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM search_backends WHERE enabled = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .unwrap_or(0) as usize
}

#[derive(Deserialize)]
struct StatsQuery {
    #[serde(default = "default_days")]
    days: i64,
}

fn default_days() -> i64 {
    7
}

async fn stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<StatsQuery>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let days = q.days.clamp(1, 90);
    let since = chrono::Utc::now().timestamp_millis() - days * 86_400_000;
    let value = state
        .db
        .read(move |conn| {
            // 明细 + 已汇总的按天数据合并（明细只留 30 天）
            let mut daily = conn.prepare(
                "SELECT day, SUM(requests), SUM(errors), SUM(input_tokens), SUM(output_tokens), SUM(search_requests) FROM (
                    SELECT date(created_ms/1000,'unixepoch') AS day, COUNT(*) AS requests,
                           SUM(CASE WHEN http_status >= 400 THEN 1 ELSE 0 END) AS errors,
                           SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens,
                           SUM(search_requests) AS search_requests
                    FROM request_logs WHERE created_ms >= ?1 GROUP BY day
                    UNION ALL
                    SELECT date, requests, errors, input_tokens, output_tokens, search_requests
                    FROM daily_rollups WHERE date >= date(?1/1000,'unixepoch')
                 ) GROUP BY day ORDER BY day",
            )?;
            let daily: Vec<Value> = daily
                .query_map(params![since], |r| {
                    Ok(json!({
                        "date": r.get::<_, String>(0)?,
                        "requests": r.get::<_, i64>(1)?,
                        "errors": r.get::<_, i64>(2)?,
                        "input_tokens": r.get::<_, i64>(3)?,
                        "output_tokens": r.get::<_, i64>(4)?,
                        "search_requests": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut by_key = conn.prepare(
                "SELECT COALESCE(d.name, '（已删除）') , l.downstream_key_id, COUNT(*),
                        SUM(l.input_tokens), SUM(l.output_tokens),
                        SUM(CASE WHEN l.http_status >= 400 THEN 1 ELSE 0 END)
                 FROM request_logs l LEFT JOIN downstream_keys d ON d.id = l.downstream_key_id
                 WHERE l.created_ms >= ?1 GROUP BY l.downstream_key_id ORDER BY COUNT(*) DESC",
            )?;
            let by_key: Vec<Value> = by_key
                .query_map(params![since], |r| {
                    Ok(json!({
                        "name": r.get::<_, String>(0)?,
                        "id": r.get::<_, Option<i64>>(1)?,
                        "requests": r.get::<_, i64>(2)?,
                        "input_tokens": r.get::<_, i64>(3)?,
                        "output_tokens": r.get::<_, i64>(4)?,
                        "errors": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut by_channel = conn.prepare(
                "SELECT COALESCE(c.name, '（已删除）'), l.channel_id, COUNT(*),
                        SUM(l.input_tokens), SUM(l.output_tokens),
                        SUM(CASE WHEN l.http_status >= 400 THEN 1 ELSE 0 END)
                 FROM request_logs l LEFT JOIN channels c ON c.id = l.channel_id
                 WHERE l.created_ms >= ?1 GROUP BY l.channel_id ORDER BY COUNT(*) DESC",
            )?;
            let by_channel: Vec<Value> = by_channel
                .query_map(params![since], |r| {
                    Ok(json!({
                        "name": r.get::<_, String>(0)?,
                        "id": r.get::<_, Option<i64>>(1)?,
                        "requests": r.get::<_, i64>(2)?,
                        "input_tokens": r.get::<_, i64>(3)?,
                        "output_tokens": r.get::<_, i64>(4)?,
                        "errors": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut by_model = conn.prepare(
                "SELECT upstream_model, COUNT(*), SUM(input_tokens), SUM(output_tokens),
                        SUM(CASE WHEN http_status >= 400 THEN 1 ELSE 0 END),
                        COALESCE(CAST(AVG(first_content_ms) AS INTEGER),0)
                 FROM request_logs WHERE created_ms >= ?1
                 GROUP BY upstream_model ORDER BY COUNT(*) DESC",
            )?;
            let by_model: Vec<Value> = by_model
                .query_map(params![since], |r| {
                    Ok(json!({
                        "model": r.get::<_, String>(0)?,
                        "requests": r.get::<_, i64>(1)?,
                        "input_tokens": r.get::<_, i64>(2)?,
                        "output_tokens": r.get::<_, i64>(3)?,
                        "errors": r.get::<_, i64>(4)?,
                        "avg_first_content_ms": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(json!({
                "days": days,
                "daily": daily,
                "by_key": by_key,
                "by_channel": by_channel,
                "by_model": by_model,
            }))
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

#[derive(Deserialize)]
struct RequestsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    before_id: Option<i64>,
    only_errors: Option<bool>,
}

fn default_limit() -> i64 {
    100
}

async fn requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RequestsQuery>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let limit = q.limit.clamp(1, 500);
    let before = q.before_id.unwrap_or(i64::MAX);
    let only_errors = q.only_errors.unwrap_or(false);
    let value = state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.request_id, COALESCE(d.name,''), l.requested_model, l.upstream_model,
                        COALESCE(c.name,''), COALESCE(k.label,''), l.http_status, l.stop_reason,
                        l.input_tokens, l.output_tokens, l.cache_read_tokens, l.cache_creation_tokens,
                        l.search_requests, l.attempts, l.streaming, l.first_content_ms, l.total_ms,
                        l.error_type, l.error_message, l.created_ms, l.session_id
                 FROM request_logs l
                 LEFT JOIN downstream_keys d ON d.id = l.downstream_key_id
                 LEFT JOIN channels c ON c.id = l.channel_id
                 LEFT JOIN upstream_keys k ON k.id = l.key_id
                 WHERE l.id < ?1 AND (?2 = 0 OR l.http_status >= 400)
                 ORDER BY l.id DESC LIMIT ?3",
            )?;
            let rows: Vec<Value> = stmt
                .query_map(params![before, only_errors as i64, limit], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "request_id": r.get::<_, String>(1)?,
                        "key_name": r.get::<_, String>(2)?,
                        "requested_model": r.get::<_, String>(3)?,
                        "upstream_model": r.get::<_, String>(4)?,
                        "channel": r.get::<_, String>(5)?,
                        "upstream_key": r.get::<_, String>(6)?,
                        "status": r.get::<_, i64>(7)?,
                        "stop_reason": r.get::<_, Option<String>>(8)?,
                        "input_tokens": r.get::<_, i64>(9)?,
                        "output_tokens": r.get::<_, i64>(10)?,
                        "cache_read_tokens": r.get::<_, i64>(11)?,
                        "cache_creation_tokens": r.get::<_, i64>(12)?,
                        "search_requests": r.get::<_, i64>(13)?,
                        "attempts": r.get::<_, i64>(14)?,
                        "streaming": r.get::<_, i64>(15)? != 0,
                        "first_content_ms": r.get::<_, Option<i64>>(16)?,
                        "total_ms": r.get::<_, i64>(17)?,
                        "error_type": r.get::<_, Option<String>>(18)?,
                        "error_message": r.get::<_, Option<String>>(19)?,
                        "created_ms": r.get::<_, i64>(20)?,
                        "session_id": r.get::<_, Option<String>>(21)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

#[derive(Deserialize)]
struct PulseQuery {
    #[serde(default = "default_pulse_limit")]
    limit: i64,
}

fn default_pulse_limit() -> i64 {
    80
}

/// 最近若干次上游尝试，给「尝试色带」用（池子的心电图）。
async fn pulse(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<PulseQuery>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let limit = q.limit.clamp(10, 300);
    let value = state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT a.created_ms, a.upstream_model, COALESCE(c.name,''), COALESCE(k.label,''),
                        a.http_status, a.error_type, a.total_ms, a.committed
                 FROM attempt_logs a
                 LEFT JOIN channels c ON c.id = a.channel_id
                 LEFT JOIN upstream_keys k ON k.id = a.key_id
                 ORDER BY a.id DESC LIMIT ?1",
            )?;
            let rows: Vec<Value> = stmt
                .query_map(params![limit], |r| {
                    Ok(json!({
                        "at": r.get::<_, i64>(0)?,
                        "model": r.get::<_, String>(1)?,
                        "channel": r.get::<_, String>(2)?,
                        "key": r.get::<_, String>(3)?,
                        "status": r.get::<_, Option<i64>>(4)?,
                        "error_type": r.get::<_, Option<String>>(5)?,
                        "ms": r.get::<_, i64>(6)?,
                        "committed": r.get::<_, i64>(7)? != 0,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

#[derive(Deserialize)]
struct AttemptsQuery {
    request_id: String,
}

async fn attempts(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AttemptsQuery>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let request_id = q.request_id.clone();
    let value = state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT a.attempt_no, COALESCE(c.name,''), COALESCE(k.label,''), a.upstream_model,
                        a.protocol, a.http_status, a.error_type, a.error_message, a.total_ms,
                        a.committed, a.created_ms
                 FROM attempt_logs a
                 LEFT JOIN channels c ON c.id = a.channel_id
                 LEFT JOIN upstream_keys k ON k.id = a.key_id
                 WHERE a.request_id = ?1 ORDER BY a.attempt_no",
            )?;
            let rows: Vec<Value> = stmt
                .query_map(params![request_id], |r| {
                    Ok(json!({
                        "attempt_no": r.get::<_, i64>(0)?,
                        "channel": r.get::<_, String>(1)?,
                        "upstream_key": r.get::<_, String>(2)?,
                        "upstream_model": r.get::<_, String>(3)?,
                        "protocol": r.get::<_, String>(4)?,
                        "status": r.get::<_, Option<i64>>(5)?,
                        "error_type": r.get::<_, Option<String>>(6)?,
                        "error_message": r.get::<_, Option<String>>(7)?,
                        "total_ms": r.get::<_, i64>(8)?,
                        "committed": r.get::<_, i64>(9)? != 0,
                        "created_ms": r.get::<_, i64>(10)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

// ============================================================================
// 渠道 / key / 条目
// ============================================================================

#[derive(Deserialize)]
struct ChannelPayload {
    name: String,
    protocol: String,
    base_url: String,
    #[serde(default)]
    extra_headers: HashMap<String, String>,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    notes: String,
}

#[derive(Deserialize)]
struct KeyPayload {
    #[serde(default)]
    label: String,
    api_key: String,
    #[serde(default = "yes")]
    enabled: bool,
}

#[derive(Deserialize)]
struct EntryPayload {
    upstream_model: String,
    #[serde(default = "tier_default")]
    tier: i32,
    #[serde(default = "context_default")]
    max_context: u32,
    #[serde(default = "yes")]
    vision: bool,
    #[serde(default)]
    pdf: bool,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    notes: String,
}

fn yes() -> bool {
    true
}
fn tier_default() -> i32 {
    1
}
fn context_default() -> u32 {
    200_000
}

async fn list_channels(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let value = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, protocol, base_url, extra_headers, enabled, notes, created_ms
                 FROM channels ORDER BY id",
            )?;
            let channels: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "protocol": r.get::<_, String>(2)?,
                        "base_url": r.get::<_, String>(3)?,
                        "extra_headers": serde_json::from_str::<Value>(&r.get::<_, String>(4)?)
                            .unwrap_or(json!({})),
                        "enabled": r.get::<_, i64>(5)? != 0,
                        "notes": r.get::<_, String>(6)?,
                        "created_ms": r.get::<_, i64>(7)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut stmt = conn.prepare(
                "SELECT id, channel_id, label, api_key, enabled, status, status_reason, disabled_ms
                 FROM upstream_keys ORDER BY id",
            )?;
            let keys: Vec<Value> = stmt
                .query_map([], |r| {
                    let api_key: String = r.get(3)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "channel_id": r.get::<_, i64>(1)?,
                        "label": r.get::<_, String>(2)?,
                        "api_key_masked": mask_key(&api_key),
                        "enabled": r.get::<_, i64>(4)? != 0,
                        "status": r.get::<_, String>(5)?,
                        "status_reason": r.get::<_, String>(6)?,
                        "disabled_ms": r.get::<_, Option<i64>>(7)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut stmt = conn.prepare(
                "SELECT id, channel_id, upstream_model, tier, max_context, vision, pdf, enabled, notes
                 FROM entries ORDER BY tier, channel_id, id",
            )?;
            let entries: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "channel_id": r.get::<_, i64>(1)?,
                        "upstream_model": r.get::<_, String>(2)?,
                        "tier": r.get::<_, i64>(3)?,
                        "max_context": r.get::<_, i64>(4)?,
                        "vision": r.get::<_, i64>(5)? != 0,
                        "pdf": r.get::<_, i64>(6)? != 0,
                        "enabled": r.get::<_, i64>(7)? != 0,
                        "notes": r.get::<_, String>(8)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            Ok(json!({"channels": channels, "keys": keys, "entries": entries}))
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

/// 只显示首尾，中间打码（上游 key 明文存库，但界面不回显）。
fn mask_key(key: &str) -> String {
    let n = key.chars().count();
    if n <= 10 {
        return "•".repeat(n.max(4));
    }
    let head: String = key.chars().take(6).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

async fn create_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<ChannelPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if crate::store::Protocol::parse(&p.protocol).is_none() {
        return Err(bad_request(
            "协议只能是 openai_chat / openai_responses / gemini / anthropic",
        ));
    }
    let headers_json = serde_json::to_string(&p.extra_headers).unwrap_or_else(|_| "{}".into());
    let id = mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO channels (name, protocol, base_url, extra_headers, enabled, notes, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.name,
                p.protocol,
                p.base_url.trim_end_matches('/'),
                headers_json,
                p.enabled as i64,
                p.notes,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await?;
    Ok(Json(json!({"id": id})).into_response())
}

async fn update_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<ChannelPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let headers_json = serde_json::to_string(&p.extra_headers).unwrap_or_else(|_| "{}".into());
    mutate(&state, move |conn| {
        conn.execute(
            "UPDATE channels SET name = ?2, protocol = ?3, base_url = ?4,
                    extra_headers = ?5, enabled = ?6, notes = ?7 WHERE id = ?1",
            params![
                id,
                p.name,
                p.protocol,
                p.base_url.trim_end_matches('/'),
                headers_json,
                p.enabled as i64,
                p.notes
            ],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_channel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM channels WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn create_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(channel_id): Path<i64>,
    Json(p): Json<KeyPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if p.api_key.trim().is_empty() {
        return Err(bad_request("api_key 不能为空"));
    }
    let id = mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO upstream_keys (channel_id, label, api_key, enabled, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                channel_id,
                p.label,
                p.api_key.trim(),
                p.enabled as i64,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await?;
    Ok(Json(json!({"id": id})).into_response())
}

#[derive(Deserialize)]
struct KeyUpdate {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn update_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<KeyUpdate>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        if let Some(label) = p.label {
            conn.execute(
                "UPDATE upstream_keys SET label = ?2 WHERE id = ?1",
                params![id, label],
            )?;
        }
        if let Some(key) = p.api_key {
            conn.execute(
                "UPDATE upstream_keys SET api_key = ?2 WHERE id = ?1",
                params![id, key.trim()],
            )?;
        }
        match p.enabled {
            // 手动启用：连带清掉「被上游拒绝」的标记
            Some(true) => {
                conn.execute(
                    "UPDATE upstream_keys SET enabled = 1, status = 'ok', status_reason = '',
                            disabled_ms = NULL WHERE id = ?1",
                    params![id],
                )?;
            }
            // 手动停用只动 enabled。status 专指「上游拒绝过这把 key」（401/403 自动禁用）；
            // 以前这里也写成 disabled，手动停用的 key 于是在后台被标成「被上游拒绝」。
            Some(false) => {
                conn.execute(
                    "UPDATE upstream_keys SET enabled = 0 WHERE id = ?1",
                    params![id],
                )?;
            }
            None => {}
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM upstream_keys WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

/// 重新启用被自动禁用的 key（后台「恢复」按钮）。
async fn enable_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute(
            "UPDATE upstream_keys SET enabled = 1, status = 'ok', status_reason = '', disabled_ms = NULL
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn create_entry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(channel_id): Path<i64>,
    Json(p): Json<EntryPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if p.upstream_model.trim().is_empty() {
        return Err(bad_request("upstream_model 不能为空"));
    }
    let id = mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO entries (channel_id, upstream_model, tier, max_context, vision, pdf, enabled, notes, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                channel_id,
                p.upstream_model.trim(),
                p.tier,
                p.max_context,
                p.vision as i64,
                p.pdf as i64,
                p.enabled as i64,
                p.notes,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await?;
    Ok(Json(json!({"id": id})).into_response())
}

async fn update_entry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<EntryPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute(
            "UPDATE entries SET upstream_model = ?2, tier = ?3, max_context = ?4,
                    vision = ?5, pdf = ?6, enabled = ?7, notes = ?8 WHERE id = ?1",
            params![
                id,
                p.upstream_model.trim(),
                p.tier,
                p.max_context,
                p.vision as i64,
                p.pdf as i64,
                p.enabled as i64,
                p.notes
            ],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_entry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ============================================================================
// 下游 key
// ============================================================================

#[derive(Deserialize)]
struct DownstreamPayload {
    name: String,
    #[serde(default = "yes")]
    enabled: bool,
}

async fn list_downstream_keys(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let value = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, key_prefix, enabled, created_ms, last_used_ms
                 FROM downstream_keys ORDER BY id",
            )?;
            let rows: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "key_prefix": r.get::<_, String>(2)?,
                        "enabled": r.get::<_, i64>(3)? != 0,
                        "created_ms": r.get::<_, i64>(4)?,
                        "last_used_ms": r.get::<_, Option<i64>>(5)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

async fn create_downstream_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<DownstreamPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    // 生成明文：只在这次响应里返回一次，库里只存哈希
    let plaintext = format!("ufp-{}", random_token(32));
    let hash = crate::api::auth::hash_key(&plaintext);
    let prefix: String = plaintext.chars().take(12).collect();
    let name = p.name.clone();
    mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO downstream_keys (name, key_hash, key_prefix, enabled, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                name,
                hash,
                prefix,
                p.enabled as i64,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"key": plaintext})).into_response())
}

fn random_token(len: usize) -> String {
    use rand::Rng;
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

async fn update_downstream_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<DownstreamPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute(
            "UPDATE downstream_keys SET name = ?2, enabled = ?3 WHERE id = ?1",
            params![id, p.name, p.enabled as i64],
        )?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_downstream_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM downstream_keys WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ============================================================================
// 健康：熔断 / 冷却
// ============================================================================

async fn health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let breakers = state
        .breakers
        .snapshot()
        .into_iter()
        .map(|b| {
            let name = state
                .pool
                .load()
                .channels
                .get(&b.channel_id)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            json!({
                "channel_id": b.channel_id,
                "channel": name,
                "model": b.model,
                "state": b.state.as_str(),
                "consecutive_failures": b.consecutive_failures,
                "total": b.total,
                "failed": b.failed,
                "opened_ms": b.opened_ms,
            })
        })
        .collect::<Vec<_>>();
    let cooldowns = state
        .cooldowns
        .snapshot()
        .into_iter()
        .map(|c| {
            json!({
                "key_id": c.key_id,
                "model": c.model,
                "until_ms": c.until_ms,
                "reason": c.reason,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"breakers": breakers, "cooldowns": cooldowns})).into_response())
}

#[derive(Deserialize)]
struct ResetPayload {
    #[serde(default)]
    channel_id: Option<i64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    key_id: Option<i64>,
    /// true 表示连冷却一起清。
    #[serde(default)]
    clear_cooldowns: bool,
}

async fn reset_health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<ResetPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let breakers = state.breakers.reset(p.channel_id, p.model.as_deref());
    let cooldowns = if p.clear_cooldowns || p.model.is_some() || p.key_id.is_some() {
        state
            .cooldowns
            .clear(&state.db, p.key_id, p.model.as_deref())
    } else {
        0
    };
    Ok(Json(json!({"breakers_reset": breakers, "cooldowns_cleared": cooldowns})).into_response())
}

// ============================================================================
// 矫正规则
// ============================================================================

async fn list_rules(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let value = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, error_fingerprint, error_sample, patch_json, source, hits,
                        enabled, created_ms, updated_ms
                 FROM rectify_rules ORDER BY id DESC",
            )?;
            let rows: Vec<Value> = stmt
                .query_map([], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "scope": r.get::<_, String>(1)?,
                        "error_fingerprint": r.get::<_, String>(2)?,
                        "error_sample": r.get::<_, String>(3)?,
                        "patch_json": r.get::<_, String>(4)?,
                        "source": r.get::<_, String>(5)?,
                        "hits": r.get::<_, i64>(6)?,
                        "enabled": r.get::<_, i64>(7)? != 0,
                        "created_ms": r.get::<_, i64>(8)?,
                        "updated_ms": r.get::<_, i64>(9)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

#[derive(Deserialize)]
struct RuleUpdate {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    patch_json: Option<String>,
}

async fn update_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<RuleUpdate>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if let Some(patch) = &p.patch_json {
        if serde_json::from_str::<Value>(patch).is_err() {
            return Err(bad_request("补丁必须是合法 JSON"));
        }
    }
    mutate(&state, move |conn| {
        if let Some(enabled) = p.enabled {
            conn.execute(
                "UPDATE rectify_rules SET enabled = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id, enabled as i64, chrono::Utc::now().timestamp_millis()],
            )?;
        }
        if let Some(patch) = p.patch_json {
            conn.execute(
                "UPDATE rectify_rules SET patch_json = ?2, updated_ms = ?3 WHERE id = ?1",
                params![id, patch, chrono::Utc::now().timestamp_millis()],
            )?;
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_rule(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM rectify_rules WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ============================================================================
// 搜索后端
// ============================================================================

#[derive(Deserialize)]
struct SearchBackendPayload {
    name: String,
    kind: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    notes: String,
}

async fn list_search_backends(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let value = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, kind, api_key, base_url, enabled, cooldown_until_ms, notes
                 FROM search_backends ORDER BY id",
            )?;
            let rows: Vec<Value> = stmt
                .query_map([], |r| {
                    let key: String = r.get(3)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "name": r.get::<_, String>(1)?,
                        "kind": r.get::<_, String>(2)?,
                        // 空 key（jina 可以不带）就回空串，界面显示「无需密钥」而不是一串圆点
                        "api_key_masked": if key.is_empty() { String::new() } else { mask_key(&key) },
                        "base_url": r.get::<_, String>(4)?,
                        "enabled": r.get::<_, i64>(5)? != 0,
                        "cooldown_until_ms": r.get::<_, Option<i64>>(6)?,
                        "notes": r.get::<_, String>(7)?,
                    }))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(internal)?;
    Ok(Json(value).into_response())
}

async fn create_search_backend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<SearchBackendPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    if crate::websearch::backends::SearchKind::parse(&p.kind).is_none() {
        return Err(bad_request(
            "搜索后端类型只能是 tavily / exa / firecrawl / parallel / jina",
        ));
    }
    let id = mutate(&state, move |conn| {
        conn.execute(
            "INSERT INTO search_backends (name, kind, api_key, base_url, enabled, notes, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                p.name,
                p.kind,
                p.api_key.trim(),
                p.base_url.trim().trim_end_matches('/'),
                p.enabled as i64,
                p.notes,
                chrono::Utc::now().timestamp_millis()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    })
    .await?;
    Ok(Json(json!({"id": id})).into_response())
}

async fn update_search_backend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(p): Json<SearchBackendPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        // api_key 为空表示不改（界面上不回显明文）
        if p.api_key.trim().is_empty() {
            conn.execute(
                "UPDATE search_backends SET name = ?2, kind = ?3, base_url = ?4,
                        enabled = ?5, notes = ?6 WHERE id = ?1",
                params![
                    id,
                    p.name,
                    p.kind,
                    p.base_url.trim().trim_end_matches('/'),
                    p.enabled as i64,
                    p.notes
                ],
            )?;
        } else {
            conn.execute(
                "UPDATE search_backends SET name = ?2, kind = ?3, api_key = ?4, base_url = ?5,
                        enabled = ?6, notes = ?7, cooldown_until_ms = NULL WHERE id = ?1",
                params![
                    id,
                    p.name,
                    p.kind,
                    p.api_key.trim(),
                    p.base_url.trim().trim_end_matches('/'),
                    p.enabled as i64,
                    p.notes
                ],
            )?;
        }
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

async fn delete_search_backend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    mutate(&state, move |conn| {
        conn.execute("DELETE FROM search_backends WHERE id = ?1", params![id])?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct SearchTestPayload {
    /// 已保存的后端；和下面的字段一起给时，非空字段覆盖库里的值（表单里「先测再存」）。
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

/// 后台「测试搜索后端」：拿真实配置搜一次，报耗时、条数和前几条标题。
///
/// 和条目的连通性测试一样，只读不写：失败不进冷却，成功也不清冷却——
/// 测试是人在看，不该替流量做决定（换了 key 保存时会自动清冷却）。
async fn test_search_backend(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<SearchTestPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    use crate::websearch::backends::{self, SearchBackend, SearchKind};

    let saved = match p.id {
        Some(id) => state
            .db
            .read(move |conn| backends::load_backend(conn, id))
            .await
            .map_err(internal)?,
        None => None,
    };
    if p.id.is_some() && saved.is_none() {
        return Err(bad_request("这个搜索后端不存在（可能刚被删掉）"));
    }
    let nonempty = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let kind = match nonempty(&p.kind) {
        Some(k) => SearchKind::parse(&k).ok_or_else(|| bad_request("不认识的搜索后端类型"))?,
        None => match &saved {
            Some(b) => b.kind,
            None => return Err(bad_request("缺少搜索后端类型")),
        },
    };
    let api_key = nonempty(&p.api_key)
        .or_else(|| saved.as_ref().map(|b| b.api_key.clone()))
        .unwrap_or_default();
    // 换了类型就别沿用旧类型的默认地址
    let base_url = nonempty(&p.base_url)
        .or_else(|| {
            saved
                .as_ref()
                .filter(|b| b.kind == kind)
                .map(|b| b.base_url.clone())
        })
        .unwrap_or_else(|| kind.default_base_url().to_string());
    let backend = SearchBackend {
        id: saved.as_ref().map(|b| b.id).unwrap_or(0),
        name: saved
            .as_ref()
            .map(|b| b.name.clone())
            .unwrap_or_else(|| kind.as_str().to_string()),
        kind,
        api_key,
        base_url,
        enabled: true,
        cooldown_until_ms: None,
    };
    let query = nonempty(&p.query).unwrap_or_else(|| "rust programming language".to_string());

    let started = std::time::Instant::now();
    let out = backends::search(&state.client, &backend, &query, 5, 300).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let body = match out {
        Ok(items) => json!({
            "ok": true,
            "query": query,
            "latency_ms": latency_ms,
            "count": items.len(),
            "items": items.iter().take(3).map(|i| json!({"title": i.title, "url": i.url})).collect::<Vec<_>>(),
            "cooling_until_ms": saved.and_then(|b| b.cooldown_until_ms)
                .filter(|&t| t > chrono::Utc::now().timestamp_millis()),
        }),
        Err(f) => json!({
            "ok": false,
            "query": query,
            "latency_ms": latency_ms,
            "error": f.message,
            "hint": connect_hint(&backend.base_url).await,
        }),
    };
    Ok(Json(body).into_response())
}

// ============================================================================
// 运行期设置
// ============================================================================

async fn get_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let row = state
        .db
        .read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM settings WHERE key = 'runtime'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok())
        })
        .await
        .map_err(internal)?;
    let mut value: Value = row
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::to_value(Settings::default()).unwrap_or(json!({})));
    // 部署令牌不进设置编辑框：它只在生成时显示一次，是否已配置看概览的 deploy.token_set。
    // 放进来的话明文就躺在编辑框里；而且在别处轮换过令牌后，这边把旧内容保存一次，
    // 令牌就被悄悄改回去，CI 部署从此 401。
    if let Some(obj) = value.as_object_mut() {
        obj.remove("deployToken");
    }
    Ok(Json(value).into_response())
}

async fn put_settings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let mut settings: Settings =
        serde_json::from_value(payload).map_err(|e| bad_request(&format!("设置格式不对：{e}")))?;
    mutate(&state, move |conn| {
        // 部署令牌不经过设置编辑框（见 get_settings），保存时沿用库里现有的值。
        settings.deploy_token = crate::store::load_settings(conn)?.deploy_token;
        save_settings(conn, &settings)?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({"ok": true})).into_response())
}

// ============================================================================
// 测试上游连通性（后台「测试」按钮）
// ============================================================================

#[derive(Deserialize)]
struct TestPayload {
    /// 要测的条目 id。
    entry_id: i64,
    /// 自定义提问（不传就用一句 ping）。
    #[serde(default)]
    prompt: Option<String>,
}

/// `POST /admin/api/test_connection`：拿条目的真实配置发一次最小请求。
///
/// 纯诊断，**不影响熔断与冷却状态**（不记账、不写库、不进路由），
/// 所以你可以放心用它试那些已经熔断/冷却的条目。
async fn test_connection(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(p): Json<TestPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;

    let cand = {
        let pool = state.pool.load();
        crate::router::select::candidate_for_entry(&pool, p.entry_id)
    };
    let Some(cand) = cand else {
        return Err(bad_request(
            "这个条目现在不在池里（被禁用、渠道没 key，或已被删除）",
        ));
    };

    let prompt = p
        .prompt
        .unwrap_or_else(|| "连通性测试：请只回复 OK".to_string());
    let body = json!({
        "model": cand.entry.upstream_model,
        // 别给太小：Gemini 3.x 这类默认带思考的模型会把预算先花在思考上，
        // 32 个 token 常常一个字的回话都剩不下，后台就只显示「空回话」——
        // 连接明明是通的，看起来却像没通。
        "max_tokens": 256,
        "stream": false,
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
    });
    let base = json!({
        "ok": false,
        "channel": cand.channel.name,
        "key": cand.key.label,
        "upstream_model": cand.entry.upstream_model,
        "protocol": cand.channel.protocol.as_str(),
    });

    let started = std::time::Instant::now();
    let build = crate::upstream::build(
        &cand,
        &crate::upstream::BuildCtx {
            client_body: &body,
            client_anthropic_version: None,
            stream: false,
        },
    );
    let req = match build {
        Ok(r) => r,
        Err(e) => {
            let mut out = base.clone();
            out["error"] = json!(format!("构造上游请求失败：{e}"));
            return Ok(Json(out).into_response());
        }
    };

    let timeout = Duration::from_millis(
        state
            .pool
            .load()
            .settings
            .first_content_timeout_ms
            .min(30_000),
    );
    let resp = match crate::upstream::send(&state.client, &req, timeout, false).await {
        Ok(r) => r,
        Err(e) => {
            let mut out = base.clone();
            out["latency_ms"] = json!(started.elapsed().as_millis() as u64);
            out["error_type"] = json!(e.kind());
            out["error"] = json!(e.to_string());
            out["hint"] = json!(connect_hint(&req.url).await);
            return Ok(Json(out).into_response());
        }
    };

    let status = resp.status().as_u16();
    let latency_ms = started.elapsed().as_millis() as u64;
    if !resp.status().is_success() {
        let (status, _body, text) = crate::upstream::error_body(resp).await;
        let mut out = base.clone();
        out["status"] = json!(status);
        out["latency_ms"] = json!(latency_ms);
        out["error_type"] = json!("http");
        out["error"] = json!(crate::pipeline::truncate_error(&text));
        return Ok(Json(out).into_response());
    }

    // 成功：转成 Anthropic 形态，取模型回话与 usage
    let raw: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            let mut out = base.clone();
            out["status"] = json!(status);
            out["latency_ms"] = json!(latency_ms);
            out["error"] = json!(format!("响应不是 JSON：{e}"));
            return Ok(Json(out).into_response());
        }
    };
    let hints = crate::upstream::tool_schema_hints(&body);
    let converted =
        crate::upstream::response_to_anthropic(cand.channel.protocol, raw, Some(&hints))
            .unwrap_or(Value::Null);
    let reply = converted
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let usage = converted.get("usage").cloned().unwrap_or(json!({}));

    Ok(Json(json!({
        "ok": true,
        "channel": cand.channel.name,
        "key": cand.key.label,
        "upstream_model": cand.entry.upstream_model,
        "protocol": cand.channel.protocol.as_str(),
        "status": status,
        "latency_ms": latency_ms,
        "reply": crate::pipeline::truncate_error(&reply),
        "usage": usage,
    }))
    .into_response())
}

// ============================================================================
// 在线部署接口（给 CI 用）
// ============================================================================

/// 上传的归档落在哪里（网关以 ufp 用户身份运行，只能写自己的目录）。
pub const DEPLOY_SPOOL: &str = "/var/lib/ufp/incoming";

/// 直读库里的部署令牌。
///
/// 不能用内存快照：`ufp set-deploy-token` 是离线命令，直接写库，运行中的网关不会
/// 知道快照已经过期（自测时就踩到过这个坑：接口报「没有配置部署令牌」）。
/// 令牌是安全敏感配置，每次直读也更利于轮换即时生效。
async fn current_deploy_token(state: &Arc<AppState>) -> String {
    state
        .db
        .read(|conn| Ok(crate::store::load_settings(conn)?.deploy_token))
        .await
        .unwrap_or_default()
}

/// 实际用的落盘目录：可用环境变量覆盖（测试与非常规部署用）。
fn deploy_spool_dir() -> std::path::PathBuf {
    std::env::var_os("UFP_DEPLOY_SPOOL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(DEPLOY_SPOOL))
}
/// 特权应用脚本：root 所有，网关通过 sudo / doas 调用它。
pub const APPLY_SCRIPT: &str = "/usr/local/bin/ufp-apply-deploy";
/// 单个归档的大小上限（二进制约 10MB，压缩后 4MB 上下）。
const DEPLOY_MAX_BYTES: usize = 32 * 1024 * 1024;

/// `POST /admin/api/deploy`：CI 把「新版本 tar.gz」推上来。
///
/// 认证用 `x-ufp-deploy-token`（和后台登录是两套），并且必须带
/// `x-ufp-sha256`；网关只负责「验签 + 落盘 + 触发特权脚本」，
/// 真正的安装/升级/回滚由 `ufp-apply-deploy`（root）执行。
async fn deploy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Response> {
    // 直读库里的令牌（不依赖内存快照，见 current_deploy_token 的说明）
    let expected_token = current_deploy_token(&state).await;
    if expected_token.is_empty() {
        return Err(bad_request(
            "没有配置部署令牌：先在服务器上运行 `ufp set-deploy-token`",
        ));
    }
    let Some(token) = headers
        .get("x-ufp-deploy-token")
        .and_then(|v| v.to_str().ok())
    else {
        return Err(session::unauthorized("缺少 x-ufp-deploy-token"));
    };
    let ok = token
        .as_bytes()
        .ct_eq(expected_token.as_bytes())
        .unwrap_u8()
        == 1;
    if !ok {
        // 部署接口被扫到过就值得看一眼日志
        tracing::warn!(ip = %session::client_ip(&headers), "部署接口令牌不匹配");
        return Err(session::unauthorized("部署令牌不正确"));
    }

    let Some(expected) = headers
        .get("x-ufp-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
    else {
        return Err(bad_request("缺少 x-ufp-sha256（上传内容的 sha256）"));
    };
    if body.len() > DEPLOY_MAX_BYTES {
        return Err(
            ApiError::too_large(format!("归档超过上限（{DEPLOY_MAX_BYTES} 字节）")).into_response(),
        );
    }
    let actual = sha256_hex(&body);
    if actual != expected {
        return Err(bad_request(&format!(
            "sha256 不匹配：期望 {expected}，实际 {actual}"
        )));
    }

    // 落盘
    let dir = deploy_spool_dir();
    let dir = dir.as_path();
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        return Err(internal(format!("创建 {} 失败：{e}", dir.display())));
    }
    let version = headers
        .get("x-ufp-version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let path = dir.join(format!(
        "deploy-{}.tgz",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    if let Err(e) = tokio::fs::write(&path, &body).await {
        return Err(internal(format!("写入 {} 失败：{e}", path.display())));
    }
    tracing::info!(
        version = %version,
        bytes = body.len(),
        file = %path.display(),
        "收到部署包，准备触发升级"
    );

    // 触发特权脚本（不等待它跑完：升级过程中网关自己会被替换掉）
    let (triggered, note) = trigger_apply(&path, &actual, &version);
    let status = if triggered {
        StatusCode::ACCEPTED
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok((
        status,
        Json(json!({
            "ok": triggered,
            "version": version,
            "file": path.display().to_string(),
            "sha256": actual,
            "triggered": triggered,
            "note": note,
            "next": "升级脚本在后台跑；轮询 /healthz 看 version 是否变成新版本",
        })),
    )
        .into_response())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 调特权脚本。优先 sudo -n（配了 NOPASSWD 才不会卡住），再试 doas。
fn trigger_apply(tarball: &std::path::Path, sha: &str, version: &str) -> (bool, String) {
    let log_path = deploy_spool_dir().join("last-deploy.log");
    for (launcher, args) in [
        ("sudo", vec!["-n", APPLY_SCRIPT]),
        ("doas", vec![APPLY_SCRIPT]),
    ] {
        if which(launcher).is_none() {
            continue;
        }
        let mut cmd = std::process::Command::new(launcher);
        cmd.args(args)
            .arg(tarball)
            .arg(sha)
            // 版本号只用于日志（部署脚本会打印它）
            .env("UFP_DEPLOY_VERSION", version)
            .stdin(std::process::Stdio::null());
        // 升级日志追加到 spool 里的固定文件，方便事后排查（每次尝试单独打开）
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(f) => {
                let err = f.try_clone().ok();
                cmd.stdout(std::process::Stdio::from(f));
                cmd.stderr(match err {
                    Some(f) => std::process::Stdio::from(f),
                    None => std::process::Stdio::null(),
                });
            }
            Err(_) => {
                cmd.stdout(std::process::Stdio::null());
                cmd.stderr(std::process::Stdio::null());
            }
        }
        match cmd.spawn() {
            Ok(child) => {
                // 不 wait：升级过程中网关会被新进程替换，脚本要在后台跑完
                tracing::info!(pid = child.id(), launcher, "已触发升级脚本");
                return (true, format!("已通过 {launcher} 触发 {APPLY_SCRIPT}"));
            }
            Err(e) => {
                tracing::warn!(launcher, error = %e, "触发升级脚本失败");
            }
        }
    }
    (
        false,
        format!(
            "没有可用的 sudo/doas，或 {APPLY_SCRIPT} 未安装；归档已保存在 {}",
            tarball.display()
        ),
    )
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// 生成（或轮换）部署令牌。
async fn rotate_deploy_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let token = random_token(48);
    let token_for_db = token.clone();
    mutate(&state, move |conn| {
        let mut settings = crate::store::load_settings(conn)?;
        settings.deploy_token = token_for_db;
        crate::store::save_settings(conn, &settings)?;
        Ok(())
    })
    .await?;
    Ok(Json(json!({
        "token": token,
        "hint": "只显示这一次；填到仓库 Secret DEPLOY_TOKEN",
    }))
    .into_response())
}

// ============================================================================
// 导入 / 导出
// ============================================================================

async fn export_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let value = state.db.read(export_all).await.map_err(internal)?;
    Ok(Json(value).into_response())
}

fn export_all(conn: &Connection) -> rusqlite::Result<Value> {
    let dump = |sql: &str, cols: &[&str]| -> rusqlite::Result<Vec<Value>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            let mut obj = serde_json::Map::new();
            for (i, col) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = r.get(i)?;
                obj.insert(col.to_string(), sql_value_to_json(v));
            }
            Ok(Value::Object(obj))
        })?;
        rows.collect()
    };
    Ok(json!({
        "version": 1,
        "channels": dump("SELECT id, name, protocol, base_url, extra_headers, enabled, notes FROM channels", &["id","name","protocol","base_url","extra_headers","enabled","notes"])?,
        "upstream_keys": dump("SELECT id, channel_id, label, api_key, enabled, status, status_reason FROM upstream_keys", &["id","channel_id","label","api_key","enabled","status","status_reason"])?,
        "entries": dump("SELECT id, channel_id, upstream_model, tier, max_context, vision, pdf, enabled, notes FROM entries", &["id","channel_id","upstream_model","tier","max_context","vision","pdf","enabled","notes"])?,
        "downstream_keys": dump("SELECT id, name, key_prefix, enabled FROM downstream_keys", &["id","name","key_prefix","enabled"])?,
        "rectify_rules": dump("SELECT id, scope, error_fingerprint, error_sample, patch_json, source, enabled FROM rectify_rules", &["id","scope","error_fingerprint","error_sample","patch_json","source","enabled"])?,
        "search_backends": dump("SELECT id, name, kind, api_key, base_url, enabled, notes FROM search_backends", &["id","name","kind","api_key","base_url","enabled","notes"])?,
        "settings": conn.query_row("SELECT value FROM settings WHERE key = 'runtime'", [], |r| r.get::<_, String>(0)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(json!({})),
    }))
}

fn sql_value_to_json(v: rusqlite::types::Value) -> Value {
    match v {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(i) => json!(i),
        rusqlite::types::Value::Real(f) => json!(f),
        rusqlite::types::Value::Text(s) => {
            // extra_headers / patch_json 之类的字段是 JSON 字符串，直接展开更好用
            serde_json::from_str(&s).unwrap_or(Value::String(s))
        }
        rusqlite::types::Value::Blob(_) => Value::Null,
    }
}

/// 导入：按名字/模型做 upsert，不删现有数据（安全第一）。
/// 只支持本网关自己导出的格式；下游 key 的明文不在导出里（只存哈希），
/// 所以导入时保留原哈希，密钥继续可用。
async fn import_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let imported = mutate(&state, move |conn| import_all(conn, &payload)).await?;
    Ok(Json(json!({"imported": imported})).into_response())
}

fn import_all(conn: &Connection, payload: &Value) -> rusqlite::Result<Value> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut counts = json!({"channels": 0, "upstream_keys": 0, "entries": 0, "downstream_keys": 0, "search_backends": 0, "rectify_rules": 0});
    let tx = conn.unchecked_transaction()?;

    // 渠道：按 name 唯一
    if let Some(channels) = payload.get("channels").and_then(|c| c.as_array()) {
        for ch in channels {
            let name = ch.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let headers = ch.get("extra_headers").cloned().unwrap_or(json!({}));
            tx.execute(
                "INSERT INTO channels (name, protocol, base_url, extra_headers, enabled, notes, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(name) DO UPDATE SET protocol = ?2, base_url = ?3, extra_headers = ?4, enabled = ?5, notes = ?6",
                params![
                    name,
                    ch.get("protocol").and_then(|v| v.as_str()).unwrap_or("openai_chat"),
                    ch.get("base_url").and_then(|v| v.as_str()).unwrap_or(""),
                    serde_json::to_string(&headers).unwrap_or_else(|_| "{}".into()),
                    ch.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                    ch.get("notes").and_then(|v| v.as_str()).unwrap_or(""),
                    now
                ],
            )?;
            counts["channels"] = json!(counts["channels"].as_i64().unwrap_or(0) + 1);
        }
    }

    let channel_id_by_name = |tx: &Connection, name: &str| -> rusqlite::Result<Option<i64>> {
        tx.query_row(
            "SELECT id FROM channels WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    };
    let channel_name_by_id = |tx: &Connection, id: i64| -> rusqlite::Result<Option<String>> {
        tx.query_row(
            "SELECT name FROM channels WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    };

    if let Some(rows) = payload.get("upstream_keys").and_then(|c| c.as_array()) {
        for row in rows {
            let channel_id = row.get("channel_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let Some(channel_name) = channel_name_by_id(&tx, channel_id)? else {
                continue;
            };
            let Some(target) = channel_id_by_name(&tx, &channel_name)? else {
                continue;
            };
            let api_key = row.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
            if api_key.is_empty() {
                continue;
            }
            // 同一个渠道里同一把 key 只留一条
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM upstream_keys WHERE channel_id = ?1 AND api_key = ?2",
                params![target, api_key],
                |r| r.get(0),
            )?;
            if exists == 0 {
                tx.execute(
                    "INSERT INTO upstream_keys (channel_id, label, api_key, enabled, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        target,
                        row.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                        api_key,
                        row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                        now
                    ],
                )?;
                counts["upstream_keys"] = json!(counts["upstream_keys"].as_i64().unwrap_or(0) + 1);
            }
        }
    }

    if let Some(rows) = payload.get("entries").and_then(|c| c.as_array()) {
        for row in rows {
            let channel_id = row.get("channel_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let Some(channel_name) = channel_name_by_id(&tx, channel_id)? else {
                continue;
            };
            let Some(target) = channel_id_by_name(&tx, &channel_name)? else {
                continue;
            };
            tx.execute(
                "INSERT INTO entries (channel_id, upstream_model, tier, max_context, vision, pdf, enabled, notes, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(channel_id, upstream_model) DO UPDATE SET tier = ?3, max_context = ?4, vision = ?5, pdf = ?6, enabled = ?7, notes = ?8",
                params![
                    target,
                    row.get("upstream_model").and_then(|v| v.as_str()).unwrap_or(""),
                    row.get("tier").and_then(|v| v.as_i64()).unwrap_or(1),
                    row.get("max_context").and_then(|v| v.as_i64()).unwrap_or(200_000),
                    row.get("vision").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                    row.get("pdf").and_then(|v| v.as_bool()).unwrap_or(false) as i64,
                    row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                    row.get("notes").and_then(|v| v.as_str()).unwrap_or(""),
                    now
                ],
            )?;
            counts["entries"] = json!(counts["entries"].as_i64().unwrap_or(0) + 1);
        }
    }

    if let Some(rows) = payload.get("search_backends").and_then(|c| c.as_array()) {
        for row in rows {
            let kind = row.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if crate::websearch::backends::SearchKind::parse(kind).is_none() {
                continue;
            }
            let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let exists: i64 = tx.query_row(
                "SELECT COUNT(*) FROM search_backends WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )?;
            if exists == 0 {
                tx.execute(
                    "INSERT INTO search_backends (name, kind, api_key, base_url, enabled, notes, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        name,
                        kind,
                        row.get("api_key").and_then(|v| v.as_str()).unwrap_or(""),
                        row.get("base_url").and_then(|v| v.as_str()).unwrap_or(""),
                        row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                        row.get("notes").and_then(|v| v.as_str()).unwrap_or(""),
                        now
                    ],
                )?;
                counts["search_backends"] =
                    json!(counts["search_backends"].as_i64().unwrap_or(0) + 1);
            }
        }
    }

    if let Some(rows) = payload.get("rectify_rules").and_then(|c| c.as_array()) {
        for row in rows {
            tx.execute(
                "INSERT INTO rectify_rules (scope, error_fingerprint, error_sample, patch_json, source, enabled, created_ms, updated_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(scope, error_fingerprint) DO UPDATE SET patch_json = ?4, enabled = ?6, updated_ms = ?7",
                params![
                    row.get("scope").and_then(|v| v.as_str()).unwrap_or(""),
                    row.get("error_fingerprint").and_then(|v| v.as_str()).unwrap_or(""),
                    row.get("error_sample").and_then(|v| v.as_str()).unwrap_or(""),
                    row.get("patch_json").and_then(|v| v.as_str()).unwrap_or("[]"),
                    row.get("source").and_then(|v| v.as_str()).unwrap_or("import"),
                    row.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) as i64,
                    now
                ],
            )?;
            counts["rectify_rules"] = json!(counts["rectify_rules"].as_i64().unwrap_or(0) + 1);
        }
    }

    if let Some(settings) = payload.get("settings") {
        if let Ok(s) = serde_json::from_value::<Settings>(settings.clone()) {
            save_settings(&tx, &s)?;
        }
    }

    tx.commit()?;
    let _ = channel_id_by_name;
    Ok(counts)
}

// ============================================================================
// 小工具
// ============================================================================

fn internal<E: std::fmt::Display>(e: E) -> Response {
    tracing::warn!(error = %e, "后台操作失败");
    ApiError::api(format!("内部错误：{e}")).into_response()
}

fn bad_request(message: &str) -> Response {
    ApiError::invalid_request(message.to_string()).into_response()
}

/// 测试失败时补一句人话：目标只有 IPv4 地址、而本机没有 IPv4 出口时，原始报错只是一句
/// 「error sending request」，看不出是网络层面根本到不了（这台 VPS 就是纯 IPv6）。
async fn connect_hint(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host((host.as_str(), port)).await {
            Ok(it) => it.collect(),
            Err(_) => return Some(format!("{host} 解析不出地址（DNS 失败）")),
        };
    if addrs.is_empty() || addrs.iter().any(|a| !a.is_ipv4() || a.ip().is_loopback()) {
        return None;
    }
    // UDP connect 不发包，只问路由表：连「文档保留地址」都没路由，就是没有 IPv4 出口。
    // 有 IPv4 出口时不猜——原始报错（拒绝连接、超时……）比猜测准。
    let v4_route = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9"))
        .is_ok();
    (!v4_route).then(|| {
        format!(
            "{host} 只有 IPv4 地址，而这台服务器没有 IPv4 出口，根本连不上它——换一个有 IPv6 的服务"
        )
    })
}

/// 写完库立刻重载配置快照（改动即时生效，不用重启）。
async fn mutate<T, F>(state: &Arc<AppState>, f: F) -> Result<T, Response>
where
    T: Send + 'static,
    F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let out = state.db.admin(f).await.map_err(internal)?;
    if let Err(e) = state.pool.reload(&state.db).await {
        tracing::warn!(error = %e, "重载配置快照失败");
    }
    Ok(out)
}

/// 供 `/healthz`（无鉴权）与后台共用的健康计数。
pub fn health_summary(state: &AppState) -> Value {
    let pool = state.pool.load();
    json!({
        "entries": pool.entries.len(),
        "channels": pool.channels.len(),
        "breakers": state.breakers.snapshot().len(),
        "cooldowns": state.cooldowns.snapshot().len(),
        "dropped_writes": state.db.dropped(),
    })
}

/// 供搜索冷却等内部写入复用（避免各处直接依赖 store::Write）。
pub fn note_search_cooldown(state: &AppState, backend_id: i64, until_ms: i64) {
    state.db.write(Write::SearchCooldown {
        backend_id,
        until_ms,
    });
}
