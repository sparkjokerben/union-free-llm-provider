//! ufp 的库入口。
//!
//! 服务本身就是这些模块的组合；单独给出 lib target，是为了集成测试能直接
//! 组装 `AppState` 并起一个真实监听端口（而不是只测内部函数）。
//! `main.rs` 只负责参数解析、日志、信号与运行时。

#![cfg_attr(test, allow(non_snake_case))]

pub mod api;
pub mod config;
pub mod forward;
pub mod health;
pub mod pipeline;
pub mod rectify;
pub mod router;
pub mod store;
pub mod tokens;
pub mod upstream;

/// 当前版本（日志与 UA 用）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
