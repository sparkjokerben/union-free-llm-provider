//! 后台管理会话：密码登录、Cookie 会话、登录限速。
//!
//! 密码用 argon2 哈希存在库里（`ufp set-admin-password` 或后台里改），
//! 会话 token 只放内存（重启即失效，配合 Cookie 过期时间足够用）。
//! 登录失败按来源 IP 限速，防止被暴力猜。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rand::Rng;

use crate::api::AppState;

/// 登录失败限速：每个 IP 每分钟最多几次。
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);

pub struct Sessions {
    /// token → 过期时刻
    tokens: Mutex<HashMap<String, Instant>>,
    /// ip → (失败次数, 窗口起点)
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            tokens: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    pub fn issue(&self, ttl: Duration) -> String {
        let token: String = rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(48)
            .map(char::from)
            .collect();
        let mut tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        // 顺手清掉过期的，避免长期运行积累
        let now = Instant::now();
        tokens.retain(|_, exp| *exp > now);
        tokens.insert(token.clone(), now + ttl);
        token
    }

    pub fn valid(&self, token: &str) -> bool {
        let now = Instant::now();
        let tokens = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        tokens.get(token).map(|exp| *exp > now).unwrap_or(false)
    }

    pub fn revoke(&self, token: &str) {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token);
    }

    /// 记录一次登录失败，返回是否已经超过限速。
    pub fn note_failure(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let entry = failures.entry(ip.to_string()).or_insert((0, now));
        if now.duration_since(entry.1) > LOGIN_WINDOW {
            *entry = (0, now);
        }
        entry.0 += 1;
        entry.0 > LOGIN_MAX_FAILURES
    }

    pub fn clear_failures(&self, ip: &str) {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ip);
    }

    pub fn too_many(&self, ip: &str) -> bool {
        let now = Instant::now();
        let failures = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        failures
            .get(ip)
            .map(|(n, start)| *n > LOGIN_MAX_FAILURES && now.duration_since(*start) <= LOGIN_WINDOW)
            .unwrap_or(false)
    }
}

pub const COOKIE_NAME: &str = "ufp_admin";

/// 从 Cookie 头里取会话 token。
pub fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub fn client_ip(headers: &HeaderMap) -> String {
    // 走 Cloudflare 代理时，这两个头才是真实客户端（x-real-ip 会变成 CF 边缘地址）
    for name in ["cf-connecting-ip", "x-real-ip", "x-forwarded-for"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let first = v.split(',').next().unwrap_or("").trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    "unknown".to_string()
}

/// 校验后台会话；失败返回 401 响应。
pub fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(token) = token_from_headers(headers) else {
        return Err(unauthorized("请先登录"));
    };
    if !state.admin_sessions.valid(&token) {
        return Err(unauthorized("登录已过期，请重新登录"));
    }
    Ok(())
}

pub fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(serde_json::json!({"error": message})),
    )
        .into_response()
}

/// 设置会话 Cookie（SameSite=Lax + HttpOnly；有反代时再加 Secure）。
pub fn session_cookie(token: &str, ttl_hours: u64) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        ttl_hours * 3600
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}
