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
//!    应用后重试；**重试成功了**才把补丁沉淀成规则。
//!
//! 规则与分析都只能改写转换前的 Anthropic 请求体。转换层自己的 bug（字段放错层级之类）
//! 是确定性的，改输入绕不过去，那类问题要在转换层修。

pub mod analyzer;
pub mod builtin;
pub mod patch;
pub mod rules;
pub mod skeleton;

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;

pub use builtin::{apply_builtin, BuiltinRectifier};

use crate::api::AppState;
use crate::store::Settings;

/// 一次客户端请求的矫正状态。
///
/// 内置矫正器与已有规则按候选计（每个候选各用一次，`begin_candidate` 重置）；
/// 在线分析按错误指纹计（同一请求里同一类报错只分析一次，换候选也不重复花额度）。
#[derive(Default)]
pub struct RectifierState {
    tried: Vec<BuiltinRectifier>,
    rule_tried: bool,
    analyzed: HashSet<String>,
    /// 本候选上已应用、还没验证的补丁。只有重试成功（`settle(true)`）才记账：
    /// 规则记一次命中、分析给的补丁沉淀成规则。重试时又报了同一类错，说明它没用，丢掉。
    pending: Vec<Pending>,
}

enum Pending {
    Rule {
        id: i64,
        fingerprint: String,
    },
    Learned {
        protocol: String,
        fingerprint: String,
        error_sample: String,
        patches: Vec<patch::PatchOp>,
    },
}

impl Pending {
    fn fingerprint(&self) -> &str {
        match self {
            Pending::Rule { fingerprint, .. } | Pending::Learned { fingerprint, .. } => fingerprint,
        }
    }
}

impl RectifierState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 换到下一个候选：内置矫正器与规则可以重新用（请求体也会从原样重来）。
    pub fn begin_candidate(&mut self) {
        self.tried.clear();
        self.rule_tried = false;
        self.pending.clear();
    }

    /// 本候选的尝试结束了：`succeeded` 为真时，把验证过的补丁记下来。
    pub async fn settle(&mut self, state: &Arc<AppState>, succeeded: bool) {
        let pending = std::mem::take(&mut self.pending);
        if !succeeded {
            return;
        }
        for p in pending {
            match p {
                Pending::Rule { id, .. } => rules::note_hit(state, id),
                Pending::Learned {
                    protocol,
                    fingerprint,
                    error_sample,
                    patches,
                } => {
                    if let Err(e) = rules::upsert(
                        state,
                        &protocol,
                        &fingerprint,
                        &error_sample,
                        &patches,
                        "llm",
                    )
                    .await
                    {
                        tracing::warn!("补丁验证有效，但沉淀规则失败：{e}");
                    }
                }
            }
        }
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
        let fingerprint = rules::fingerprint(protocol, status, error_message);
        // 上一步的补丁改完还是同一类错：没用，不记账
        self.pending.retain(|p| {
            let useless = p.fingerprint() == fingerprint;
            if useless {
                if let Pending::Rule { id, .. } = p {
                    tracing::warn!(rule_id = id, "规则套用后仍是同一类报错，这次不计命中");
                }
            }
            !useless
        });

        // 1) 确定性矫正器
        if let Some(rectifier) =
            apply_builtin(body, error_message, &settings.rectifier, &self.tried)
        {
            self.tried.push(rectifier);
            return Some(format!("内置矫正器 {}", rectifier.as_str()));
        }

        // 2) 已有规则（每个候选只套一次：套了没用还反复套，就是对着上游死循环）
        if !self.rule_tried {
            if let Some(rule) = rules::find(state, protocol, &fingerprint).await {
                self.rule_tried = true;
                let value: Value = serde_json::from_str(&rule.patch_json).unwrap_or(Value::Null);
                match patch::parse(&value) {
                    Ok(ops) => match patch::apply(body, &ops) {
                        Ok(n) if n > 0 => {
                            self.pending.push(Pending::Rule {
                                id: rule.id,
                                fingerprint,
                            });
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
        }

        // 3) LLM 在线分析（同一请求里每类报错只做一次，限时 20s）
        if !self.analyzed.insert(fingerprint.clone()) {
            return None;
        }
        match analyzer::analyze(state, settings, body, status, protocol, error_message).await {
            Ok(analysis) if !analysis.patches.is_empty() => {
                let count = analysis.patches.len();
                match patch::apply(body, &analysis.patches) {
                    Ok(n) if n > 0 => {
                        self.pending.push(Pending::Learned {
                            protocol: protocol.to_string(),
                            fingerprint,
                            error_sample: error_message.to_string(),
                            patches: analysis.patches,
                        });
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
