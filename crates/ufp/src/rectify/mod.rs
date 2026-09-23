//! 请求矫正（D11）。
//!
//! 遇到 400/413/422 这类「请求本身有问题」的错误时，直接换候选往往没用 ——
//! 同一个请求换个上游照样报错。这里的顺序是三级：
//!
//! 1. **确定性矫正器**（`builtin.rs`）：按错误内容挑一个改写方案，改完在同一个
//!    候选上重试一次。改写而不是删除（用户要求），比如思考签名报错时优先丢签名、
//!    保留思考正文；max_tokens 超限时往下夹而不是砍掉整段。
//! 2. **已有规则**（`rules.rs`）：同一类报错以前分析过就直接套用当时的补丁，
//!    不再浪费分析条目的额度。
//! 3. **LLM 在线分析**（`analyzer.rs`）：把「错误 + 请求骨架（无正文）」交给后台
//!    指定的分析条目，只要它给出白名单内的受限补丁（`patch.rs` 校验），
//!    应用后重试，并把补丁沉淀成规则。

pub mod analyzer;
pub mod builtin;
pub mod patch;
pub mod rules;
pub mod skeleton;

use std::sync::Arc;

use serde_json::Value;

pub use builtin::{apply_builtin, BuiltinRectifier};

use crate::api::AppState;
use crate::store::Settings;

/// 一个候选上的矫正状态（每个候选各一份：内置矫正器各用一次、分析只做一次）。
#[derive(Default)]
pub struct RectifierState {
    tried: Vec<BuiltinRectifier>,
    analyzed: bool,
}

impl RectifierState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 试着为这条报错改写请求体，返回 `Some(说明)` 表示改了、可以重试。
    pub async fn next_step(
        &mut self,
        state: &Arc<AppState>,
        settings: &Settings,
        body: &mut Value,
        status: u16,
        protocol: &str,
        error_message: &str,
    ) -> Option<String> {
        // 1) 确定性矫正器
        if let Some(rectifier) =
            apply_builtin(body, error_message, &settings.rectifier, &self.tried)
        {
            self.tried.push(rectifier);
            return Some(format!("内置矫正器 {}", rectifier.as_str()));
        }

        // 2) 已有规则
        let fingerprint = rules::fingerprint(protocol, status, error_message);
        if let Some(rule) = rules::find(state, protocol, &fingerprint).await {
            let value: Value = serde_json::from_str(&rule.patch_json).unwrap_or(Value::Null);
            match patch::parse(&value) {
                Ok(ops) => match patch::apply(body, &ops) {
                    Ok(n) if n > 0 => {
                        rules::note_hit(state, rule.id);
                        return Some(format!(
                            "已沉淀的规则 #{}（{}，{} 条改动）",
                            rule.id, rule.source, n
                        ));
                    }
                    Ok(_) => tracing::debug!(rule_id = rule.id, "规则没有产生实际改动"),
                    Err(e) => tracing::warn!(rule_id = rule.id, error = %e, "规则应用失败"),
                },
                Err(e) => tracing::warn!(rule_id = rule.id, error = %e, "规则里的补丁不合法"),
            }
        }

        // 3) LLM 在线分析（每个候选只做一次，限时 20s）
        if self.analyzed {
            return None;
        }
        self.analyzed = true;
        match analyzer::analyze(state, settings, body, status, error_message).await {
            Ok(analysis) if !analysis.patches.is_empty() => {
                let count = analysis.patches.len();
                match patch::apply(body, &analysis.patches) {
                    Ok(n) if n > 0 => {
                        let _ = rules::upsert(
                            state,
                            protocol,
                            &fingerprint,
                            error_message,
                            &analysis.patches,
                            "llm",
                        )
                        .await;
                        return Some(format!(
                            "分析条目 {} 给出 {count} 条补丁（实际改动 {n} 处）：{}",
                            analysis.used_model, analysis.why
                        ));
                    }
                    Ok(_) => tracing::info!("分析给了补丁，但应用后没有实际改动"),
                    Err(e) => tracing::warn!(error = %e, "分析给的补丁应用失败"),
                }
            }
            Ok(_) => tracing::info!("分析认为无补丁可用"),
            Err(e) => tracing::info!("在线分析未成功：{}", crate::pipeline::truncate_error(&e)),
        }
        None
    }
}
