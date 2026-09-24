//! LLM 在线分析：把「错误 + 请求骨架」交给后台指定的分析条目，换回一个受限补丁。
//!
//! 为什么只发骨架而不是整个请求（D11）：
//! 1. Claude Code 的单个请求动辄几万 token，整包发过去要吃掉分析条目的一次额度；
//! 2. 让 LLM 重写几百 KB 的 JSON 极易写坏；
//! 3. 对话正文不该发给另一家上游。
//!
//! 分析结果只是一段建议 —— 真正决定改不改的是 `patch.rs` 的白名单校验。

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::api::AppState;
use crate::store::Settings;
use crate::upstream::{self, BuildCtx};

use super::patch::{self, PatchOp};
use super::skeleton;

/// 分析请求的超时（超过就放弃，换候选去）。
const ANALYZE_TIMEOUT: Duration = Duration::from_secs(20);
/// 分析条目用来思考与写补丁的输出预算。
const ANALYZE_MAX_TOKENS: u32 = 1500;

const SYSTEM_PROMPT: &str = "你是 LLM 网关的请求修复助手。\
用户会给你：某次上游请求失败时上游返回的错误信息，以及那次请求的「骨架」\
（正文已被替换成占位符，但结构、字段名、JSON Schema 都是真的）。\
请判断请求为什么被拒，并给出最小的改写方案。\
只输出一个 JSON 对象，不要任何解释文字或代码块标记，格式：\
{\"why\": \"一句话原因\", \"patch\": [{\"op\":\"replace\",\"path\":\"/tools/0/input_schema/type\",\"value\":\"object\"}]}。\
约束：op 只能是 replace / add / remove；path 是 JSON 指针；\
允许改动的路径只有 /tools/...（工具定义与 schema）、/tool_choice、/max_tokens、\
/thinking/...、/system、/output_config/...、以及 /messages/<n>/content/<m> 里的 \
type/name/tool_use_id/is_error/cache_control；\
能「改写」就不要「删除」（例如改 schema 而不是删掉整个工具）；最多 20 条；\
如果无法在不违反上述约束的前提下修好，就输出 {\"why\":\"...\",\"patch\":[]}。";

/// 分析结果。
pub struct Analysis {
    pub patches: Vec<PatchOp>,
    pub why: String,
    /// 分析用的候选（写日志用）。
    pub used_model: String,
}

/// 让分析条目给出补丁。
pub async fn analyze(
    state: &Arc<AppState>,
    settings: &Settings,
    body: &Value,
    status: u16,
    error_message: &str,
) -> Result<Analysis, String> {
    let Some(entry_id) = settings.analysis_entry_id else {
        return Err("未配置分析条目".into());
    };
    let cand = {
        let pool = state.pool.load();
        crate::router::select::candidate_for_entry(&pool, entry_id).ok_or_else(|| {
            "分析条目不可用（条目被禁用、渠道没有启用的 key，或条目已被删除）".to_string()
        })?
    };

    let skeleton = skeleton::build(body);
    let protocol = cand.channel.protocol.as_str().to_string();
    let prompt = format!(
        "上游协议：{protocol}\n上游返回的 HTTP 状态：{status}\n上游返回的错误：\n{}\n\n请求骨架（JSON）：\n{}",
        crate::pipeline::truncate_error(error_message),
        serde_json::to_string_pretty(&skeleton).unwrap_or_else(|_| "{}".into()),
    );
    let analyzer_body = json!({
        "model": cand.entry.upstream_model,
        "max_tokens": ANALYZE_MAX_TOKENS,
        "stream": false,
        "system": SYSTEM_PROMPT,
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
    });

    let build_ctx = BuildCtx {
        client_body: &analyzer_body,
        client_anthropic_version: None,
        stream: false,
        opencode: None,
    };
    let req = upstream::build(&cand, &build_ctx).map_err(|e| format!("构造分析请求失败：{e}"))?;
    let resp = upstream::send(&state.client, &req, ANALYZE_TIMEOUT, false)
        .await
        .map_err(|e| format!("分析请求失败：{e}"))?;
    if !resp.status().is_success() {
        let (status, _body, text) = upstream::error_body(resp).await;
        return Err(format!(
            "分析条目返回 {status}：{}",
            crate::pipeline::truncate_error(&text)
        ));
    }
    let raw: Value = resp
        .json()
        .await
        .map_err(|e| format!("分析响应不是 JSON：{e}"))?;
    let converted = upstream::response_to_anthropic(cand.channel.protocol, raw, None)
        .map_err(|e| format!("分析响应转换失败：{e}"))?;
    let text = extract_text(&converted);
    let parsed = extract_json(&text)?;
    let why = parsed
        .get("why")
        .and_then(|w| w.as_str())
        .unwrap_or("")
        .to_string();
    let patches = patch::parse(parsed.get("patch").unwrap_or(&Value::Null)).map_err(|e| {
        format!(
            "分析给出的补丁不合法（{e}），原文：{}",
            crate::pipeline::truncate_error(&text)
        )
    })?;
    Ok(Analysis {
        patches,
        why,
        used_model: cand.entry.upstream_model.clone(),
    })
}

fn extract_text(message: &Value) -> String {
    let Some(content) = message.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    content
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                b.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从可能夹着解释文字的回复里抠出第一个 JSON 对象。
fn extract_json(text: &str) -> Result<Value, String> {
    let start = text.find('{').ok_or_else(|| {
        format!(
            "分析回复里没有 JSON：{}",
            crate::pipeline::truncate_error(text)
        )
    })?;
    // 从后往前找最后一个能解析成功的 '}'
    let bytes = text.as_bytes();
    let mut end = text.len();
    while end > start {
        if bytes[end - 1] == b'}' {
            if let Ok(v) = serde_json::from_str::<Value>(&text[start..end]) {
                return Ok(v);
            }
        }
        end -= 1;
    }
    Err(format!(
        "分析回复里的 JSON 解析不了：{}",
        crate::pipeline::truncate_error(text)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 能从夹着说明的回复里抠出_json() {
        let text = "我看了下，问题在 schema。\n{\"why\": \"缺少 type\", \"patch\": [{\"op\":\"replace\",\"path\":\"/tools/0/input_schema/type\",\"value\":\"object\"}]}\n希望有帮助。";
        let v = extract_json(text).unwrap();
        assert_eq!(v["why"], "缺少 type");
    }

    #[test]
    fn 没有_json_时报错() {
        assert!(extract_json("这个请求看起来没问题").is_err());
    }

    #[test]
    fn 提取文本块() {
        let msg = json!({"content": [
            {"type": "text", "text": "第一段"},
            {"type": "tool_use", "id": "t", "name": "x", "input": {}},
            {"type": "text", "text": "第二段"}
        ]});
        assert_eq!(extract_text(&msg), "第一段\n第二段");
    }
}
