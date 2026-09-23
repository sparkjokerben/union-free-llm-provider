//! 渠道 × 模型 熔断器。
//!
//! 移植自 cc-switch 的 `proxy/circuit_breaker.rs`，但键从 `provider` 扩展成
//! `渠道 × 上游模型`（D7：熔断要能只针对某个模型生效——同一渠道上 flash 正常、
//! pro 一直 503 是免费池的常态）。
//!
//! 与上游实现一致的部分：Closed/Open/HalfOpen 三态、连续失败阈值、
//! 错误率阈值（样本数达标后生效）、半开只放一个探测、没有后台定时器
//! （Open→HalfOpen 在 `allow()` 时惰性判断）。
//!
//! 与上游不同的部分（都标 `UFP:`）：
//! - 计数不再累加不清零 —— 回到 Closed 时重置，避免老数据把新故障率抬高；
//! - 探测许可在「没有结果」的路径上可以显式归还（`release()`），
//!   否则一次客户端断开就会把渠道永久卡在 HalfOpen；
//! - 长期不活跃的条目会被 `prune()` 清掉，池子换血时 map 不会无限增长。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::store::settings::BreakerSettings;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BreakerStatus {
    pub channel_id: i64,
    pub model: String,
    pub state: BreakerState,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub total: u64,
    pub failed: u64,
    pub opened_ms: Option<i64>,
}

#[derive(Debug)]
struct Entry {
    state: BreakerState,
    opened_ms: i64,
    consecutive_failures: u32,
    consecutive_successes: u32,
    total: u64,
    failed: u64,
    /// HalfOpen 下是否已有一个探测在飞（同时只允许一个）。
    probe_inflight: bool,
    last_seen_ms: i64,
}

impl Entry {
    fn new(now: i64) -> Self {
        Self {
            state: BreakerState::Closed,
            opened_ms: 0,
            consecutive_failures: 0,
            consecutive_successes: 0,
            total: 0,
            failed: 0,
            probe_inflight: false,
            last_seen_ms: now,
        }
    }
}

pub struct Breakers {
    map: Mutex<HashMap<(i64, String), Entry>>,
}

impl Default for Breakers {
    fn default() -> Self {
        Self::new()
    }
}

impl Breakers {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// 这次请求能不能用这个渠道×模型。
    ///
    /// - Closed：放行。
    /// - Open：到点则转 HalfOpen 并占用唯一的探测名额。
    /// - HalfOpen：只有在没有探测在飞时才放行。
    pub fn allow(&self, channel_id: i64, model: &str, settings: &BreakerSettings) -> bool {
        let now = now_ms();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let e = map
            .entry((channel_id, model.to_string()))
            .or_insert_with(|| Entry::new(now));
        e.last_seen_ms = now;
        match e.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if now.saturating_sub(e.opened_ms) >= settings.open_secs as i64 * 1000 {
                    e.state = BreakerState::HalfOpen;
                    e.consecutive_successes = 0;
                    e.probe_inflight = true;
                    true
                } else {
                    false
                }
            }
            BreakerState::HalfOpen => {
                if e.probe_inflight {
                    false
                } else {
                    e.probe_inflight = true;
                    true
                }
            }
        }
    }

    /// 只读地判断「现在放行的话会不会被拒」，不占用探测名额。
    ///
    /// 选择候选时用它做粗筛；真正的放行/占位由尝试循环里的 `allow()` 完成
    /// （两次判断之间可能有并发，所以 `allow()` 仍可能返回 false，那时跳过该候选即可，
    /// 不算一次失败）。
    pub fn is_available(&self, channel_id: i64, model: &str, settings: &BreakerSettings) -> bool {
        let now = now_ms();
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&(channel_id, model.to_string())) {
            None => true,
            Some(e) => match e.state {
                BreakerState::Closed => true,
                BreakerState::Open => {
                    now.saturating_sub(e.opened_ms) >= settings.open_secs as i64 * 1000
                }
                BreakerState::HalfOpen => !e.probe_inflight,
            },
        }
    }

    /// 记录一次尝试的结果。只有真正打到上游、并且拿到了明确成败的尝试才该调用它。
    pub fn record(&self, channel_id: i64, model: &str, ok: bool, settings: &BreakerSettings) {
        let now = now_ms();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let Some(e) = map.get_mut(&(channel_id, model.to_string())) else {
            return;
        };
        e.last_seen_ms = now;
        e.probe_inflight = false;
        match e.state {
            BreakerState::Closed => {
                if ok {
                    e.consecutive_failures = 0;
                } else {
                    e.total += 1;
                    e.failed += 1;
                    e.consecutive_failures += 1;
                    let rate_trip = settings.min_requests > 0
                        && e.total >= settings.min_requests as u64
                        && (e.failed as f64 / e.total as f64) >= settings.error_rate_threshold;
                    if e.consecutive_failures >= settings.failure_threshold || rate_trip {
                        e.state = BreakerState::Open;
                        e.opened_ms = now;
                    }
                }
            }
            BreakerState::HalfOpen => {
                if ok {
                    e.consecutive_successes += 1;
                    if e.consecutive_successes >= settings.success_threshold {
                        // UFP: 恢复时清零计数，避免陈旧样本继续影响错误率。
                        e.state = BreakerState::Closed;
                        e.consecutive_failures = 0;
                        e.consecutive_successes = 0;
                        e.total = 0;
                        e.failed = 0;
                    }
                } else {
                    e.state = BreakerState::Open;
                    e.opened_ms = now;
                }
            }
            BreakerState::Open => {}
        }
    }

    /// 归还探测许可：这次尝试没有产生结果（客户端提前断开、进程在写日志前出错等）。
    /// UFP: 上游实现里如果许可没还回去，渠道会永远卡在 HalfOpen。
    pub fn release(&self, channel_id: i64, model: &str) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = map.get_mut(&(channel_id, model.to_string())) {
            e.probe_inflight = false;
        }
    }

    /// 后台展示用。
    pub fn snapshot(&self) -> Vec<BreakerStatus> {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<BreakerStatus> = map
            .iter()
            .map(|((channel_id, model), e)| BreakerStatus {
                channel_id: *channel_id,
                model: model.clone(),
                state: e.state,
                consecutive_failures: e.consecutive_failures,
                consecutive_successes: e.consecutive_successes,
                total: e.total,
                failed: e.failed,
                opened_ms: if e.opened_ms > 0 {
                    Some(e.opened_ms)
                } else {
                    None
                },
            })
            .collect();
        out.sort_by(|a, b| (a.channel_id, &a.model).cmp(&(b.channel_id, &b.model)));
        out
    }

    /// 手动重置（后台「重置熔断」按钮）。返回重置掉的条目数。
    pub fn reset(&self, channel_id: Option<i64>, model: Option<&str>) -> usize {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let before = map.len();
        map.retain(|(cid, m), _| {
            let channel_match = channel_id.is_none_or(|c| c == *cid);
            let model_match = model.is_none_or(|want| want == m);
            !(channel_match && model_match)
        });
        before - map.len()
    }

    /// 清掉长期不活跃且处于 Closed 的条目。由后台的定时任务调用。
    pub fn prune(&self, idle_ms: i64) {
        let now = now_ms();
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, e| {
            e.state != BreakerState::Closed || now.saturating_sub(e.last_seen_ms) < idle_ms
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> BreakerSettings {
        BreakerSettings {
            failure_threshold: 2,
            success_threshold: 1,
            open_secs: 0, // 立即可半开，方便测试
            error_rate_threshold: 0.6,
            min_requests: 10,
        }
    }

    #[test]
    fn 连续失败两次后打开() {
        let b = Breakers::new();
        let s = settings();
        assert!(b.allow(1, "m", &s));
        b.record(1, "m", false, &s);
        assert!(b.allow(1, "m", &s));
        b.record(1, "m", false, &s);
        // open_secs = 0，下一次 allow 会直接转 HalfOpen 并占探测位
        assert!(b.allow(1, "m", &s));
        // 探测占位后，第二次 allow 被拒
        assert!(!b.allow(1, "m", &s));
        b.record(1, "m", true, &s);
        assert!(b.allow(1, "m", &s));
    }

    #[test]
    fn 探测许可可以归还() {
        let b = Breakers::new();
        let s = settings();
        b.allow(1, "m", &s);
        b.record(1, "m", false, &s);
        b.allow(1, "m", &s);
        b.record(1, "m", false, &s);
        assert!(b.allow(1, "m", &s)); // HalfOpen 探测
        assert!(!b.allow(1, "m", &s)); // 探测在飞
        b.release(1, "m"); // 这次尝试没有结果
        assert!(b.allow(1, "m", &s));
    }

    #[test]
    fn 不同模型的熔断互相独立() {
        let b = Breakers::new();
        let s = settings();
        b.record(1, "a", false, &s);
        b.record(1, "a", false, &s);
        assert!(b.allow(1, "b", &s));
    }

    #[test]
    fn 重置按渠道或模型筛选() {
        let b = Breakers::new();
        let s = settings();
        b.allow(1, "a", &s);
        b.allow(1, "b", &s);
        b.allow(2, "a", &s);
        assert_eq!(b.reset(None, Some("a")), 2);
        assert_eq!(b.snapshot().len(), 1);
    }
}
