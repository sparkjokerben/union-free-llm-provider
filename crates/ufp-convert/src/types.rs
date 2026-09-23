//! UFP: 从 cc-switch 的 `proxy/types.rs` 里裁剪出来的最小集合。
//!
//! 上游那个文件混合了代理配置、优化器配置、媒体降级开关等一大堆只服务于
//! cc-switch UI 的类型；移植过来的模块只读取 `RectifierConfig` 的三个开关，
//! 其余类型留在服务 crate 里。

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// 请求整流器开关。对应 cc-switch 的 `RectifierConfig`；字段名与
/// `rename_all = "camelCase"` 都保持一致，方便直接复用同一份配置 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RectifierConfig {
    /// 总开关：是否启用整流器
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 请求整流：thinking 签名整流器
    ///
    /// 处理错误：Invalid 'signature' in 'thinking' block
    #[serde(default = "default_true")]
    pub request_thinking_signature: bool,

    /// 请求整流：thinking budget 整流器
    ///
    /// 处理错误：budget_tokens + thinking 相关约束
    #[serde(default = "default_true")]
    pub request_thinking_budget: bool,

    /// 请求整流：不支持的图片降级
    ///
    /// 上游拒绝图片输入时，把图片块替换为 `[Unsupported Image]` 标记，让对话不中断。
    /// UFP：服务层目前靠路由（迁移到支持 vision 的条目）解决多模态能力不匹配，
    /// 这个开关先原样保留 —— 它同时也是上游兜底路径的开关。
    #[serde(default = "default_true")]
    pub request_media_fallback: bool,

    /// 请求整流：确认纯文本注册表的发送前降级
    ///
    /// 在模型未声明能力时，按内置的确认纯文本注册表预先剥离图片。
    #[serde(default = "default_true")]
    pub request_media_heuristic: bool,
}

impl Default for RectifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            request_thinking_signature: true,
            request_thinking_budget: true,
            request_media_fallback: true,
            request_media_heuristic: true,
        }
    }
}
