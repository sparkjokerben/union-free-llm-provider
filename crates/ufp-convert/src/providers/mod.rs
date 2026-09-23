//! 各上游协议的转换器。
//!
//! 与 cc-switch 的 `proxy/providers/` 相比，这里只保留纯粹的转换模块：
//! 上游的 `claude.rs`（分发 + URL + 鉴权）、`auth.rs`、`adapter.rs` 以及
//! Codex / Copilot / xAI 的 OAuth 与命名空间修正都只服务于 cc-switch 自己的
//! 客户端形态，不在这条移植线上；对应的职责由服务 crate 的 `upstream/` 承担。

pub mod gemini_schema;
pub mod gemini_shadow;
// UFP: 无状态签名信封（替代 shadow store）。
pub mod gemini_signature;
pub mod reasoning_bridge;
pub mod streaming;
pub mod streaming_gemini;
pub mod streaming_responses;
pub mod transform;
pub mod transform_gemini;
pub mod transform_responses;
