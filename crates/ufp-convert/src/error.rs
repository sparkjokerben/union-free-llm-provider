//! UFP: 取代 cc-switch 的 `proxy/error.rs`。
//!
//! 移植过来的转换代码只会产生 `TransformError`（转换失败）与 `InvalidRequest`
//! （请求本身不合法）两种错误；上游 cc-switch 那个版本还带着 axum 的
//! `IntoResponse`、`reqwest::Error` 分类等传输层职责，那些属于服务 crate，
//! 不在这里。变体名保持与上游一致，只是把类型名从 `ProxyError` 改为
//! `ConvertError`，以便和服务层的错误类型区分。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConvertError {
    /// 转换过程中遇到无法处理的结构（请求或响应）。
    #[error("{0}")]
    TransformError(String),

    /// 请求不符合当前上游协议的约束（例如缺失可解析的工具名）。
    #[error("{0}")]
    InvalidRequest(String),
}

pub type Result<T> = std::result::Result<T, ConvertError>;

impl ConvertError {
    pub fn transform(message: impl std::fmt::Display) -> Self {
        Self::TransformError(message.to_string())
    }

    pub fn invalid(message: impl std::fmt::Display) -> Self {
        Self::InvalidRequest(message.to_string())
    }
}
