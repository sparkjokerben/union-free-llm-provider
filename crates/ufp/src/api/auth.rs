//! 下游 key 鉴权。
//!
//! 同时接受 `x-api-key`（Anthropic SDK 默认）与 `Authorization: Bearer …`
//! （`ANTHROPIC_AUTH_TOKEN` 走这条）。库里只存 sha256 十六进制，明文只在
//! 创建时给用户看一次。

use std::sync::Arc;

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::error::ApiError;
use super::AppState;
use crate::store::{DownstreamKeyCfg, Write};

pub fn hash_key(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 从请求头里取下游 key 的明文。
pub fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    for name in ["authorization", "Authorization"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if let Some(rest) = v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
            {
                let rest = rest.trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

/// 校验下游 key，返回它的配置。
pub async fn authenticate(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<Arc<DownstreamKeyCfg>, ApiError> {
    let Some(token) = extract_token(headers) else {
        return Err(ApiError::authentication(
            "missing API key: pass it in the x-api-key header or as a Bearer token",
        ));
    };
    let presented = hash_key(&token);
    let pool = state.pool.load();
    let Some(found) = pool.downstream.get(&presented) else {
        return Err(ApiError::authentication("invalid API key"));
    };
    // 命中与否本身就来自哈希表查找；这里再对命中的条目做一次恒定时间比较，
    // 避免任何前缀级的信息泄漏。
    if found
        .key_hash
        .as_bytes()
        .ct_eq(presented.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(ApiError::authentication("invalid API key"));
    }
    if !found.enabled {
        return Err(ApiError::permission("this API key has been disabled"));
    }
    let cfg = Arc::new(found.clone());
    state.db.write(Write::DownstreamKeyUsed { key_id: cfg.id });
    Ok(cfg)
}
