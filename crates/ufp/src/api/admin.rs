//! 后台管理 API（统计、渠道/key/条目管理、熔断状态与重置、矫正规则）。
//!
//! 这一版先只有登录与状态查询，其余路由随对应的管理功能一起加。

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use serde_json::json;

use super::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/admin/api/ping", get(ping))
}

async fn ping() -> axum::Json<serde_json::Value> {
    axum::Json(json!({ "ok": true }))
}
