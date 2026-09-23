//! 熔断与冷却。
//!
//! 分工（D7）：
//! - **熔断**（`breaker.rs`）：按「渠道 × 上游模型」，管真故障（5xx、超时、连接失败、
//!   提交前的空流/断流）。状态只在内存，重启即复位。
//! - **冷却**（`cooldown.rs`）：按「key × 上游模型」，管额度类问题（429、402、
//!   日配额用尽），到期时间来自上游的 Retry-After / retryDelay，落库以便跨重启生效。
//! - **key 禁用**：401/403 直接把 key 置为 disabled（落库，后台标红手动恢复）。

pub mod breaker;
pub mod cooldown;

pub use breaker::{BreakerState, BreakerStatus, Breakers};
pub use cooldown::Cooldowns;
