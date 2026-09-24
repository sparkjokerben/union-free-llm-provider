//! `/v1/models` 与 `/v1/messages/count_tokens`。
//!
//! 对外暴露**池子里所有能用的模型**：请求里写哪个上游模型名，路由就优先走它
//! （`router/select.rs` 规则 5），写不出来或者那个模型正好不可用时，照样回落到
//! 整个池子——故障转移与自动路由都在。请求里的名字不做校验（D4），这里列出的
//! 只是「写哪些名字有意义」。
//!
//! `settings.public_model_id` 是「不点名」的那个名字（默认 `ufp`），排在第一位。
//! 计数走本地估算（cc-switch 是直接 404，Claude Code 能否容忍这个失败没实测过，
//! 不如给个值）。

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use super::auth::authenticate;
use super::error::ApiError;
use super::AppState;
use crate::store::Pool;
use crate::tokens;

/// 池子里当前真正可用的上游模型名（去重、排序）。
///
/// 「可用」= 条目启用 + 渠道启用 + 该渠道至少有一把启用的 key。冷却与熔断不算：
/// 它们是暂时的，而且点名之后路由本来就会绕开不可用的条目。
pub fn usable_models(pool: &Pool) -> Vec<String> {
    let mut set = BTreeSet::new();
    for entry in pool.entries.iter().filter(|e| e.enabled) {
        let Some(channel) = pool.channels.get(&entry.channel_id) else {
            continue;
        };
        if !channel.enabled {
            continue;
        }
        let has_key = pool
            .keys
            .get(&entry.channel_id)
            .map(|ks| ks.iter().any(|k| k.enabled))
            .unwrap_or(false);
        if !has_key {
            continue;
        }
        set.insert(entry.upstream_model.clone());
    }
    set.into_iter().collect()
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers).await?;
    let pool = state.pool.load();
    let created = chrono::Utc::now().timestamp();
    let public = pool.settings.public_model_id.clone();

    let mut data: Vec<Value> = vec![json!({
        "type": "model",
        "id": public,
        "display_name": format!("{public}（网关自动路由）"),
        "created_at": created,
    })];
    for id in usable_models(&pool) {
        if id == public {
            continue;
        }
        data.push(json!({
            "type": "model",
            "id": id,
            "display_name": id,
            "created_at": created,
        }));
    }
    let first = data
        .first()
        .and_then(|d| d["id"].as_str())
        .unwrap_or("")
        .to_string();
    let last = data
        .last()
        .and_then(|d| d["id"].as_str())
        .unwrap_or("")
        .to_string();
    Ok(Json(json!({
        "type": "list",
        "data": data,
        "has_more": false,
        "first_id": first,
        "last_id": last,
    })))
}

pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    authenticate(&state, &headers).await?;
    if body.get("messages").and_then(|m| m.as_array()).is_none() {
        return Err(ApiError::invalid_request("messages: field required"));
    }
    // 与 Anthropic 一致：只返回输入侧的计量。
    Ok(Json(
        json!({ "input_tokens": tokens::estimate_request(&body) }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChannelCfg, EntryCfg, KeyCfg, Protocol, Settings};

    fn pool() -> Pool {
        let channel = |id: i64, enabled: bool| ChannelCfg {
            id,
            name: format!("ch{id}"),
            protocol: Protocol::OpenAiChat,
            base_url: "http://localhost".into(),
            extra_headers: Vec::new(),
            enabled,
        };
        let key = |id: i64, channel_id: i64, enabled: bool| KeyCfg {
            id,
            channel_id,
            label: format!("k{id}"),
            api_key: "sk-test".into(),
            enabled,
        };
        let entry = |id: i64, channel_id: i64, model: &str, enabled: bool| EntryCfg {
            id,
            channel_id,
            upstream_model: model.into(),
            tier: 1,
            max_context: 200_000,
            vision: true,
            pdf: false,
            enabled,
        };
        Pool {
            channels: [(1, channel(1, true)), (2, channel(2, false))]
                .into_iter()
                .collect(),
            keys: [
                (1, vec![key(11, 1, true), key(12, 1, false)]),
                (2, vec![key(21, 2, true)]),
                (3, vec![key(31, 3, false)]),
            ]
            .into_iter()
            .collect(),
            entries: vec![
                entry(101, 1, "model-b", true),
                entry(102, 1, "model-a", true),
                entry(103, 1, "model-b", true), // 同渠道重复：去重
                entry(104, 1, "model-off", false),
                entry(105, 2, "model-dead-channel", true), // 渠道停用
                entry(106, 3, "model-no-key", true),       // 渠道没有启用的 key
            ],
            downstream: Default::default(),
            settings: Settings::default(),
            loaded_ms: 0,
        }
    }

    #[test]
    fn 只列真正能用的模型_去重且排序() {
        let got = usable_models(&pool());
        assert_eq!(got, vec!["model-a".to_string(), "model-b".to_string()]);
    }

    #[test]
    fn 停用条目_停用渠道_没有启用的_key_都不算可用() {
        let got = usable_models(&pool());
        for gone in ["model-off", "model-dead-channel", "model-no-key"] {
            assert!(!got.iter().any(|m| m == gone), "{gone} 不该出现");
        }
    }
}
