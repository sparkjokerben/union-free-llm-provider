//! 协议转换层：Anthropic Messages ⇄ OpenAI Chat / OpenAI Responses / Gemini 原生。
//!
//! 本 crate 的代码移植自 cc-switch（见 `UPSTREAM.md`），保持上游的文件划分，
//! 所有改动都用 `// UFP:` 注释标出，便于之后 diff 合并上游修复。
//!
//! 这里只做纯粹的格式转换：输入 `serde_json::Value` 或上游字节流，输出 Anthropic
//! 形态的 JSON / SSE 字节流。路由、熔断、用量统计、WebSearch 仿真都在服务 crate 里。

// UFP: 移植过来的条目里，有一批是给服务 crate 调用的公开入口（转换器、SSE 工具、
// 用量解析等）。在服务 crate 接入之前它们在本 crate 内无人引用，会刷一堆
// dead_code 警告；先整体静音，等服务 crate 用起来之后再逐条把可见性收窄到实际需要的范围。
#![allow(dead_code)]

pub mod body_filter;
pub mod content_encoding;
pub mod error;
pub mod gemini_url;
pub mod json_canonical;
pub mod providers;
pub mod sse;
pub mod thinking_budget_rectifier;
pub mod thinking_rectifier;
pub mod tool_media;
pub mod types;
pub mod usage;

pub use error::ConvertError;
