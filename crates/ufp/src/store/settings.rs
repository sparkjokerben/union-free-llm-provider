//! 运行期设置（存在 `settings` 表的单份 JSON 里，后台可改、即时生效）。
//!
//! 与进程级 `config.rs` 的分工：这里的每一项改完都不需要重启。

use serde::{Deserialize, Serialize};

use ufp_convert::types::RectifierConfig;

/// 响应/流里 `model` 字段写什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelEcho {
    /// 回显真实上游模型名（默认；后台与统计里能看到实际用了谁）。
    #[default]
    Upstream,
    /// 回显客户端请求里的模型名。
    Request,
    /// 回显固定的对外 id。
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BreakerSettings {
    /// 连续失败多少次打开熔断。
    pub failure_threshold: u32,
    /// 半开状态下连续成功多少次恢复。
    pub success_threshold: u32,
    /// 打开后多久允许一次半开探测（秒）。
    pub open_secs: u64,
    /// 错误率阈值（样本数达到 min_requests 后生效）。
    pub error_rate_threshold: f64,
    /// 错误率生效所需的最小样本数。
    pub min_requests: u32,
}

impl Default for BreakerSettings {
    fn default() -> Self {
        Self {
            failure_threshold: 4,
            success_threshold: 2,
            open_secs: 60,
            error_rate_threshold: 0.6,
            min_requests: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SearchSettings {
    pub enabled: bool,
    /// 给上游模型的搜索结果条数。
    pub top_results: u32,
    /// 单次请求里最多执行多少次搜索（对应 web_search 的 max_uses 上限）。
    pub max_uses: u32,
    /// 单条结果正文截断长度。
    pub snippet_bytes: usize,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            top_results: 5,
            max_uses: 8,
            snippet_bytes: 8000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SmtpSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    /// 收件人列表。
    pub to: Vec<String>,
    /// starttls | tls | none
    pub security: String,
}

impl Default for SmtpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            username: String::new(),
            password: String::new(),
            from: String::new(),
            to: Vec::new(),
            security: "starttls".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AlertSettings {
    pub smtp: SmtpSettings,
    /// 同类事件的最小推送间隔（秒），避免一次故障刷屏。
    pub min_interval_secs: u64,
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self {
            smtp: SmtpSettings::default(),
            min_interval_secs: 1800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// 对外展示的模型 id（`/v1/models` 用它；请求里的 model 不校验）。
    pub public_model_id: String,
    pub model_echo: ModelEcho,
    /// 单次请求最多尝试多少个候选（不含矫正后的重试）。
    pub max_attempts: u32,
    pub connect_timeout_ms: u64,
    /// 单次尝试「等到首个内容增量」的超时；超时视为该次尝试失败，可以换候选。
    pub first_content_timeout_ms: u64,
    /// 已提交给客户端后，chunk 之间的空闲超时。
    pub idle_timeout_ms: u64,
    /// 从开始到提交的总预算（Claude Code 默认 600s 超时之内）。
    pub precommit_budget_ms: u64,
    /// 流内空闲时每隔多久补一个 ping 事件。
    pub ping_interval_ms: u64,
    pub breaker: BreakerSettings,
    pub rectifier: RectifierConfig,
    /// 用于「让另一个上游分析报错并给出改写方案」的条目 id；None 表示停用该机制。
    pub analysis_entry_id: Option<i64>,
    pub search: SearchSettings,
    pub alerts: AlertSettings,
    /// 后台会话有效期（小时）。
    pub admin_session_hours: u64,
    /// 会话粘性映射保留天数。
    pub session_ttl_days: u64,
    /// 请求明细保留天数，之后汇总进 daily_rollups。
    pub detail_retention_days: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            public_model_id: "ufp".into(),
            model_echo: ModelEcho::default(),
            max_attempts: 5,
            connect_timeout_ms: 10_000,
            first_content_timeout_ms: 90_000,
            idle_timeout_ms: 120_000,
            precommit_budget_ms: 300_000,
            ping_interval_ms: 15_000,
            breaker: BreakerSettings::default(),
            rectifier: RectifierConfig::default(),
            analysis_entry_id: None,
            search: SearchSettings::default(),
            alerts: AlertSettings::default(),
            admin_session_hours: 24,
            session_ttl_days: 30,
            detail_retention_days: 30,
        }
    }
}
