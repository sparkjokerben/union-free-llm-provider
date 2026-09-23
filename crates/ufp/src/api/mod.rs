//! HTTP 入口：Anthropic 协议的下游面 + 后台管理面。

pub mod admin;
pub mod auth;
pub mod error;
pub mod messages;
pub mod models;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::health::{Breakers, Cooldowns};
use crate::router::Sessions;
use crate::store::{Db, Snapshot};

pub struct AppState {
    pub cfg: Config,
    pub db: Arc<Db>,
    /// 配置快照（后台改动后整份替换）。
    pub pool: Arc<Snapshot>,
    /// 出站 HTTP 客户端。连接池是共享的，不设整体超时（流式请求可能很长），
    /// 超时由转发层按「连接 / 首内容 / chunk 空闲」分别控制。
    pub client: reqwest::Client,
    /// 全局在途请求闸门。
    pub inflight: Arc<Semaphore>,
    /// 熔断（渠道 × 模型，内存）。
    pub breakers: Arc<Breakers>,
    /// 冷却（key × 模型，落库）。
    pub cooldowns: Arc<Cooldowns>,
    /// 会话粘性映射（落库）。
    pub sessions: Arc<Sessions>,
    /// 后台登录会话（内存，重启失效）。
    pub admin_sessions: Arc<admin::session::Sessions>,
    /// 邮件告警（限频）。
    pub alerter: Arc<crate::alert::Alerter>,
    /// 备份目录（每日维护写 SQLite 快照）。
    pub backup_dir: std::path::PathBuf,
    pub started_ms: i64,
}

impl AppState {
    pub fn new(
        cfg: Config,
        db: Arc<Db>,
        pool: Arc<Snapshot>,
        cooldowns: Arc<Cooldowns>,
        sessions: Arc<Sessions>,
    ) -> Arc<Self> {
        let cfg_backup_dir = cfg
            .db_path
            .parent()
            .map(|p| p.join("backup"))
            .unwrap_or_else(|| std::path::PathBuf::from("./backup"));
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(
                pool.load().settings.connect_timeout_ms,
            ))
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(60))
            .user_agent(format!("ufp/{}", crate::VERSION))
            .build()
            .expect("构造 HTTP 客户端失败");
        let inflight = Arc::new(Semaphore::new(cfg.max_inflight));
        Arc::new(Self {
            cfg,
            db,
            pool,
            client,
            inflight,
            breakers: Arc::new(Breakers::new()),
            cooldowns,
            sessions,
            admin_sessions: Arc::new(admin::session::Sessions::new()),
            alerter: Arc::new(crate::alert::Alerter::new()),
            backup_dir: cfg_backup_dir,
            started_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// 上游明确拒绝这个 key（401/403）：落库禁用并立刻刷新快照，
    /// 后续请求不会再选它；后台会把它标红，等人工恢复。
    pub fn disable_upstream_key(&self, key_id: i64, reason: &str) {
        let reason = crate::pipeline::truncate_error(reason);
        self.db.write_blocking(crate::store::Write::DisableKey {
            key_id,
            reason: reason.clone(),
        });
        let settings = self.pool.load().settings.clone();
        self.alerter.notify(
            &settings.alerts,
            "key_disabled",
            "ufp：上游拒绝了网关的 key",
            &format!("key id {key_id} 已被自动禁用，请在后台「渠道与条目」确认后手动恢复。\n\n原因：{reason}"),
        );
        let pool = Arc::clone(&self.pool);
        let db = Arc::clone(&self.db);
        tokio::spawn(async move {
            if let Err(e) = pool.reload(&db).await {
                tracing::warn!(error = %e, "禁用 key 后刷新配置快照失败");
            }
        });
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let body_limit = state.cfg.max_body_bytes;
    Router::new()
        // 根路径给一个极简落地页（别人打开域名时不会看到 nginx 的 404）
        .route("/", get(landing))
        .route("/favicon.ico", get(favicon))
        // 下游面（Anthropic 协议）
        .route("/v1/messages", post(messages::messages))
        .route("/v1/messages/count_tokens", post(models::count_tokens))
        .route("/v1/models", get(models::list_models))
        // 给 nginx / OpenRC 探活
        .route("/healthz", get(healthz))
        // 后台管理面
        .merge(admin::routes())
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// 极简落地页：告诉来者这是什么、怎么用，不暴露任何内部信息。
async fn landing() -> impl axum::response::IntoResponse {
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>ufp</title>
<style>
 :root {{ color-scheme: dark }}
 body {{ margin:0; min-height:100vh; display:grid; place-items:center;
        font:15px/1.7 -apple-system,"PingFang SC","Microsoft YaHei",sans-serif;
        background:#0f1115; color:#e6e8ee }}
 main {{ max-width:34rem; padding:2rem }}
 h1 {{ font-size:1.25rem; margin:0 0 .25rem }}
 p {{ color:#98a0b0; margin:.35rem 0 }}
 code {{ background:#1f232c; padding:.1rem .35rem; border-radius:.25rem; font-size:.9em }}
 a {{ color:#6ea8fe }}
 .row {{ margin-top:1.25rem; display:flex; gap:1.25rem; flex-wrap:wrap }}
</style></head>
<body><main>
<h1>ufp · 统一 LLM 中转网关</h1>
<p>本服务只提供 Anthropic Messages 协议（<code>/v1/messages</code>），供 Claude Code 之类的客户端使用。</p>
<div class="row">
  <a href="/admin">管理后台</a>
  <a href="/healthz">健康状态</a>
</div>
<p style="margin-top:1.5rem;font-size:.8rem;opacity:.6">v{}</p>
</main></body></html>"#,
        crate::VERSION
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
}

async fn favicon() -> axum::http::StatusCode {
    axum::http::StatusCode::NO_CONTENT
}

async fn healthz(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    let pool = state.pool.load();
    axum::Json(json!({
        "ok": true,
        "version": crate::VERSION,
        "uptime_ms": chrono::Utc::now().timestamp_millis() - state.started_ms,
        "channels": pool.channels.len(),
        "entries": pool.entries.len(),
        "inflight": state.cfg.max_inflight.saturating_sub(state.inflight.available_permits()),
        "dropped_writes": state.db.dropped(),
    }))
}
