//! ufp 的库入口。
//!
//! 服务本身就是这些模块的组合；单独给出 lib target，是为了集成测试能直接
//! 组装 `AppState` 并起一个真实监听端口（而不是只测内部函数）。
//! `main.rs` 只负责参数解析、日志、信号与运行时。

#![cfg_attr(test, allow(non_snake_case))]
// axum 处理器习惯返回 Result<Response, Response>：Err 变体里的 Response 天生偏大，
// 这是框架用法的问题，不是错误类型设计的问题。
#![allow(clippy::result_large_err)]

pub mod alert;
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
pub mod websearch;

/// 当前版本（日志、UA 与 /healthz 用）。
///
/// 由 `build.rs` 注入：发布流水线在构建前把 git tag 写进 `crates/ufp/version.txt`，
/// 本地开发则回退到 Cargo.toml 的版本。部署流水线靠 /healthz 的这个值判断升级成功。
pub const VERSION: &str = env!("UFP_VERSION");
