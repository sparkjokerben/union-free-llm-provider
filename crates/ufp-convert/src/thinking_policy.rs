//! UFP: 「思考开到最大」的统一口径（D14）。
//!
//! 渠道勾了这个开关（`channels.max_thinking`，预设建的渠道默认开）后，网关在每个候选
//! 的请求体副本上插一个私有标记 `_ufp_reasoning`，各协议的转换器只认这个标记，把
//! 思考按该上游的「最大」形态发出去——不管下游客户端发的是 `thinking: disabled`、
//! 不发，还是一个很小的 `budget_tokens`。
//!
//! 几条设计上的取舍：
//! - **标记是私有的，白名单补丁碰不到它**（`rectify/patch.rs` 只放行 `/tools`、
//!   `/thinking`、`/max_tokens` 这些路径），所以在线分析与沉淀规则无法把强制关掉；
//!   思考签名整流器清历史也清不掉它。转换器都从头重建 body，标记不会上线，只有
//!   Anthropic 原生透传需要显式删掉。
//! - **认不出的模型名乐观处理**（Anthropic 走 adaptive、Gemini 走 level），靠
//!   [`is_thinking_rejection`] + 阶梯退让兜底：上游不吃就换参数形式，全不吃就禁用条目。
//! - **不通过「关掉思考」来让请求通过**：阶梯的最后一级是「这个模型不支持」，
//!   不是「不发思考参数」。
//!
//! 各协议「最大」的事实（官方文档核实过的部分写在下面的函数注释里）：
//! - Anthropic：4.6+ 是 `thinking:{type:"adaptive"}` + `output_config.effort`，最高
//!   `max`；`budget_tokens` 在 4.7 及更新（含 Opus 5 / Sonnet 5 / Fable 5）上直接 400，
//!   4.5 及更早只认 `enabled` + `budget_tokens`（1024 ≤ budget < max_tokens）。
//!   4.7+ 的 `display` 默认 `omitted`（思考正文空），要正文得显式 `summarized`。
//! - Gemini：3.x 用 `thinkingLevel`（`high` 最高档）；2.5/2.0 用 `thinkingBudget`。
//! - OpenAI：Chat 用 `reasoning_effort` / OpenRouter 风格的 `reasoning` 对象，
//!   Responses 用 `reasoning` 对象。

use serde_json::{json, Value};

/// 请求体上的私有标记键（下划线开头，客户端不会发这个字段）。
pub const MARKER_KEY: &str = "_ufp_reasoning";

/// 思考参数的形式阶梯，从上到下依次退让。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    /// 各协议的「最大」形态。
    Max,
    /// 上游不认第一种写法时的等价高配形态。
    Alt,
    /// 最后兜底：老式预算形态。
    Legacy,
    /// 阶梯走完：这个模型不支持思考（由网关禁用条目，不发任何思考参数）。
    Unsupported,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Max => "max",
            Mode::Alt => "alt",
            Mode::Legacy => "legacy",
            Mode::Unsupported => "unsupported",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "max" => Some(Mode::Max),
            "alt" => Some(Mode::Alt),
            "legacy" => Some(Mode::Legacy),
            "unsupported" => Some(Mode::Unsupported),
            _ => None,
        }
    }

    /// 降一档；已经最低则返回 None（表示「不支持」，由调用方禁用条目）。
    pub fn downgrade(self) -> Option<Mode> {
        match self {
            Mode::Max => Some(Mode::Alt),
            Mode::Alt => Some(Mode::Legacy),
            Mode::Legacy => None,
            Mode::Unsupported => None,
        }
    }
}

/// 往请求体上打/换标记。
pub fn set_mode(body: &mut Value, mode: Mode) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert(MARKER_KEY.into(), json!(mode.as_str()));
    }
}

/// 读标记；没有标记 = 这个渠道没开强制，返回 None（转换器按客户端发的原样处理）。
pub fn mode(body: &Value) -> Option<Mode> {
    body.get(MARKER_KEY)
        .and_then(Value::as_str)
        .and_then(Mode::parse)
}

/// 从请求体上摘掉标记（Anthropic 原生透传在序列化前调用）。
pub fn strip_marker(body: &mut Value) {
    if let Some(obj) = body.as_object_mut() {
        obj.remove(MARKER_KEY);
    }
}

// ── Anthropic 模型版本 ────────────────────────────────────────────────────

/// Claude 模型的 `(major, minor)`；日期段（20250929）不算版本。认不出来返回 None。
///
/// 写法沿用 `transform_gemini.rs` 里 `is_gemini_3_series` 的 `rsplit('/')`：
/// `anthropic/claude-opus-4-6`、`us.anthropic.claude-…-v1:0` 这类带厂商前缀的都要能认。
pub fn claude_version(model: &str) -> Option<(u16, u16)> {
    let lower = model.trim().to_ascii_lowercase();
    let tail = lower.rsplit('/').next().unwrap_or(&lower);
    // 有的渠道写成 `claude-sonnet-4-5…`，有的把版本直接跟在厂商段后面；都取含
    // `claude` 的最后一段。
    let tail = if tail.starts_with("claude") {
        tail.to_string()
    } else {
        format!("claude{}", tail.rsplit("claude").next()?)
    };
    let rest = tail.strip_prefix("claude")?;
    let rest = rest.trim_start_matches(['-', '.']);
    let mut nums = rest
        .split(['-', '.', ':'])
        .filter(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|seg| seg.parse::<u16>().ok())
        // 日期段（20250929）与 Bedrock 的 `-v1` 里的数字靠这个过滤掉。
        .filter(|n| *n < 1000);
    let major = nums.next()?;
    Some((major, nums.next().unwrap_or(0)))
}

/// 4.6 起是 adaptive + effort 的世界；4.5 及更早只认 budget_tokens。
/// 认不出的名字（`None`）乐观按新世界处理，靠阶梯退让回老式预算。
pub fn claude_supports_adaptive(version: Option<(u16, u16)>) -> bool {
    version.is_none_or(|v| v >= (4, 6))
}

/// 4.7 起 `display` 默认 `omitted`，思考正文要显式要 `summarized`。
/// 认不出的名字一律带上（真被拒会走阶梯的 Legacy 分支，那里不发 display）。
pub fn claude_needs_display(version: Option<(u16, u16)>) -> bool {
    version.is_none_or(|v| v >= (4, 7))
}

// ── Gemini 分档 ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiSeries {
    /// 1.x：不支持思考。
    Legacy,
    /// 2.0 / 2.5：用 `thinkingBudget`。
    Budget,
    /// 3.x 及更新：用 `thinkingLevel`。
    Level,
    /// 认不出来的名字：乐观按 3.x 处理，靠阶梯退让。
    Unknown,
}

impl GeminiSeries {
    pub fn supports_thinking(self) -> bool {
        !matches!(self, GeminiSeries::Legacy)
    }
    pub fn uses_level(self) -> bool {
        matches!(self, GeminiSeries::Level | GeminiSeries::Unknown)
    }
}

/// Gemini 模型分档：`gemini-3.5-flash` → Level，`gemini-2.5-flash` → Budget，
/// `gemini-flash-1.5` → Legacy，`models/gemini-3-pro` → Level。
pub fn gemini_series(model: &str) -> GeminiSeries {
    let lower = model.trim().to_ascii_lowercase();
    let tail = lower.rsplit('/').next().unwrap_or(&lower);
    let Some(pos) = tail.find("gemini-") else {
        return GeminiSeries::Unknown;
    };
    let rest = &tail[pos + "gemini-".len()..];
    // `gemini-flash-1.5` 的版本号在族名后面，`gemini-2.5-flash` 在前面：取第一段
    // 纯数字（或数字打头）即可，两种写法都覆盖。
    let major = rest.split(['-', '.', ':']).find_map(|seg| {
        let digits: String = seg.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty())
            .then(|| digits.parse::<u32>().ok())
            .flatten()
    });
    match major {
        Some(1) => GeminiSeries::Legacy,
        Some(2) => GeminiSeries::Budget,
        Some(major) if major >= 3 => GeminiSeries::Level,
        // 版本号在族名后面（`gemini-flash-1.5`）时上面已经拿到 1；这里兜住
        // `gemini-flash` 这类完全没有版本号的。
        Some(_) => GeminiSeries::Unknown,
        None => GeminiSeries::Unknown,
    }
}

// ── 各协议「最大」形态 ────────────────────────────────────────────────────

/// Gemini 的 `thinkingConfig`。`max_output_tokens` 是转换后的 `maxOutputTokens`。
///
/// 2.x 的动态预算（-1）会把整个 `maxOutputTokens` 吃掉导致空回话，所以用推导预算：
/// 留 1024 给回答，上限 24576。
pub fn gemini_thinking_config(
    model: &str,
    mode: Mode,
    max_output_tokens: Option<i64>,
) -> Option<Value> {
    let series = gemini_series(model);
    if !series.supports_thinking() || mode == Mode::Unsupported {
        return None;
    }
    let mut cfg = json!({ "includeThoughts": true });
    let budget = |cap: i64| -> i64 {
        let room = max_output_tokens.unwrap_or(32_000).saturating_sub(1);
        room.clamp(1024, cap)
    };
    match (series, mode) {
        // 3.x 首选 level（high 是最高档），退让时改用预算。
        (GeminiSeries::Level, Mode::Max) | (GeminiSeries::Unknown, Mode::Max) => {
            cfg["thinkingLevel"] = json!("high");
        }
        (_, Mode::Max) => {
            cfg["thinkingBudget"] = json!(budget(24_576));
        }
        (_, Mode::Alt) => {
            cfg["thinkingBudget"] = json!(budget(24_576));
        }
        (_, Mode::Legacy) => {
            cfg["thinkingBudget"] = json!(budget(1024));
        }
        _ => {}
    }
    Some(cfg)
}

/// Anthropic 原生上游：`thinking` / `output_config` 按版本与 mode 改写。
///
/// 只动这两个字段，其余原样透传。`budget_tokens` 必须 `1024 ≤ budget < max_tokens`，
/// 所以不动客户端的 `max_tokens`、预算从它推导；退化值（`max_tokens ≤ 1024`）抬到 1025。
pub fn apply_anthropic_max_thinking(body: &mut Value, model: &str, mode: Mode) {
    if mode == Mode::Unsupported {
        return;
    }
    let version = claude_version(model);
    let adaptive = claude_supports_adaptive(version);
    let max_tokens = body.get("max_tokens").and_then(Value::as_i64).unwrap_or(0);
    let budget = max_tokens.saturating_sub(1).clamp(1024, 32_000);
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if adaptive {
        let mut thinking = json!({ "type": "adaptive" });
        if claude_needs_display(version) {
            // 4.7+ 默认 omitted → 思考正文是空的；用户要看到思考，带 summarized。
            thinking["display"] = json!("summarized");
        }
        obj.insert("thinking".into(), thinking);
        let effort = match mode {
            Mode::Alt => "high",
            _ => "max",
        };
        let output_config = obj
            .entry("output_config".to_string())
            .or_insert_with(|| json!({}));
        if let Some(oc) = output_config.as_object_mut() {
            oc.insert("effort".into(), json!(effort));
        }
        return;
    }
    if max_tokens > 0 && max_tokens <= 1024 {
        obj.insert("max_tokens".into(), json!(1025));
    }
    obj.insert(
        "thinking".into(),
        json!({ "type": "enabled", "budget_tokens": budget }),
    );
}

/// OpenAI Chat：返回 `(字段名, 值)`，由转换器写进 body。认不出形式时返回 None。
///
/// OpenAI 原生 id（`gpt-5+` / o 系列 / `grok-4.5+`，判定已去厂商前缀）用
/// `reasoning_effort`；其它走 OpenRouter 风格的 `reasoning` 对象。
pub fn openai_chat_reasoning(
    model: &str,
    mode: Mode,
    openai_native: bool,
) -> Option<(&'static str, Value)> {
    if mode == Mode::Unsupported {
        return None;
    }
    if openai_native {
        let effort = match mode {
            Mode::Max => crate::providers::transform::max_reasoning_effort(model),
            Mode::Alt => "high",
            _ => "high",
        };
        return Some(("reasoning_effort", json!(effort)));
    }
    match mode {
        Mode::Max => Some(("reasoning", json!({ "effort": "max" }))),
        Mode::Alt => Some(("reasoning", json!({ "max_tokens": 24_576 }))),
        _ => Some(("reasoning_effort", json!("high"))),
    }
}

/// OpenAI Responses 的 `reasoning` 对象。
///
/// 强制思考时除了 `effort`，还要 `summary`（网关把思考回给客户端看）与
/// `include: reasoning.encrypted_content`——信封靠它往返（`reasoning_bridge.rs`），
/// 少了它多轮工具调用会断链。`store:false` 一并要：拿不到上一轮的 reasoning item。
pub fn openai_responses_reasoning(model: &str, mode: Mode) -> Option<Value> {
    if mode == Mode::Unsupported {
        return None;
    }
    let effort = match mode {
        Mode::Alt => "high",
        _ => crate::providers::transform::max_reasoning_effort(model),
    };
    Some(json!({ "effort": effort, "summary": "auto" }))
}

// ── 阶梯触发词 ────────────────────────────────────────────────────────────

/// 错误信息是不是「上游不吃我们发的思考参数」。
///
/// 必须同时含一个思考类词与一个拒绝类词：像 Gemini 的
/// `parameters.properties[batch].items: missing field` 不含思考词，不会误触发。
pub fn is_thinking_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    const THINKING: &[&str] = &[
        "reasoning",
        "thinking",
        "thought",
        "effort",
        "budget_tokens",
        "thinkingconfig",
        "thinking_level",
        "thinking_budget",
    ];
    const REJECT: &[&str] = &[
        "unsupported",
        "not supported",
        "does not support",
        "unrecognized",
        "unknown",
        "invalid",
        "extra inputs",
        "not allowed",
        "unexpected",
        "not permitted",
    ];
    THINKING.iter().any(|t| lower.contains(t)) && REJECT.iter().any(|r| lower.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_版本表() {
        assert_eq!(claude_version("claude-sonnet-4-5-20250929"), Some((4, 5)));
        assert_eq!(claude_version("claude-opus-4-1-20250805"), Some((4, 1)));
        assert_eq!(claude_version("claude-3-5-sonnet-20241022"), Some((3, 5)));
        assert_eq!(claude_version("claude-3-7-sonnet-20250219"), Some((3, 7)));
        assert_eq!(claude_version("claude-opus-4-6"), Some((4, 6)));
        assert_eq!(claude_version("anthropic/claude-opus-4-6"), Some((4, 6)));
        assert_eq!(claude_version("claude-sonnet-5"), Some((5, 0)));
        assert_eq!(claude_version("claude-fable-5-1"), Some((5, 1)));
        assert_eq!(claude_version("claude-haiku-4-5-20251001"), Some((4, 5)));
        assert_eq!(claude_version("gpt-4o"), None);
        assert_eq!(claude_version(""), None);
    }

    #[test]
    fn adaptive_与_display_的分界() {
        assert!(!claude_supports_adaptive(claude_version(
            "claude-sonnet-4-5-20250929"
        )));
        assert!(claude_supports_adaptive(claude_version("claude-opus-4-6")));
        assert!(claude_supports_adaptive(claude_version("claude-sonnet-5")));
        // 认不出来的名字乐观按 adaptive
        assert!(claude_supports_adaptive(None));
        assert!(!claude_needs_display(claude_version("claude-opus-4-6")));
        assert!(claude_needs_display(claude_version("claude-sonnet-5")));
    }

    #[test]
    fn gemini_分档() {
        assert_eq!(gemini_series("gemini-3.5-flash"), GeminiSeries::Level);
        assert_eq!(gemini_series("gemini-3-flash-preview"), GeminiSeries::Level);
        assert_eq!(gemini_series("models/gemini-3-pro"), GeminiSeries::Level);
        assert_eq!(gemini_series("gemini-2.5-flash"), GeminiSeries::Budget);
        assert_eq!(gemini_series("gemini-2.0-flash"), GeminiSeries::Budget);
        assert_eq!(gemini_series("gemini-flash-1.5"), GeminiSeries::Legacy);
        assert_eq!(gemini_series("gemini-20"), GeminiSeries::Level);
        assert_eq!(gemini_series("gpt-4o"), GeminiSeries::Unknown);
    }

    #[test]
    fn gemini_3_用_level_最高档() {
        let cfg = gemini_thinking_config("gemini-3.5-flash", Mode::Max, Some(32_000)).unwrap();
        assert_eq!(cfg["includeThoughts"], true);
        assert_eq!(cfg["thinkingLevel"], "high");
        assert!(cfg.get("thinkingBudget").is_none());
    }

    #[test]
    fn gemini_25_用推导预算_不吃掉整个输出() {
        // maxOutputTokens 很小也不能低于预算下限 1024
        let cfg = gemini_thinking_config("gemini-2.5-flash", Mode::Max, Some(256)).unwrap();
        assert_eq!(cfg["thinkingBudget"], 1024);
        let cfg = gemini_thinking_config("gemini-2.5-flash", Mode::Max, Some(32_000)).unwrap();
        assert_eq!(cfg["thinkingBudget"], 24_576);
    }

    #[test]
    fn gemini_1_与不支持_都不发思考() {
        assert!(gemini_thinking_config("gemini-flash-1.5", Mode::Max, Some(1024)).is_none());
        assert!(
            gemini_thinking_config("gemini-3.5-flash", Mode::Unsupported, Some(1024)).is_none()
        );
    }

    #[test]
    fn gemini_阶梯_从_level_退到预算() {
        let alt = gemini_thinking_config("gemini-3.5-flash", Mode::Alt, Some(32_000)).unwrap();
        assert_eq!(alt["thinkingBudget"], 24_576);
        assert!(alt.get("thinkingLevel").is_none());
    }

    #[test]
    fn anthropic_46_用_adaptive_加_max_effort() {
        let mut body = json!({"max_tokens": 32000});
        apply_anthropic_max_thinking(&mut body, "claude-sonnet-5", Mode::Max);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert_eq!(body["output_config"]["effort"], "max");
        assert_eq!(body["max_tokens"], 32000);
    }

    #[test]
    fn anthropic_45_用预算_不动_max_tokens() {
        let mut body = json!({"max_tokens": 32000});
        apply_anthropic_max_thinking(&mut body, "claude-sonnet-4-5-20250929", Mode::Max);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 31_999);
        assert_eq!(body["max_tokens"], 32000);
    }

    #[test]
    fn anthropic_退化_max_tokens_被抬到_预算之上() {
        let mut body = json!({"max_tokens": 128});
        apply_anthropic_max_thinking(&mut body, "claude-sonnet-4-5-20250929", Mode::Max);
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["max_tokens"], 1025);
    }

    #[test]
    fn anthropic_保留_output_config_其它键() {
        let mut body = json!({"max_tokens": 32000, "output_config": {"format": {"type": "json"}}});
        apply_anthropic_max_thinking(&mut body, "claude-opus-4-6", Mode::Max);
        assert_eq!(body["output_config"]["format"]["type"], "json");
        assert_eq!(body["output_config"]["effort"], "max");
    }

    #[test]
    fn openai_chat_两种字段名() {
        assert_eq!(
            openai_chat_reasoning("openai/gpt-5.6", Mode::Max, true),
            Some(("reasoning_effort", json!("max")))
        );
        assert_eq!(
            openai_chat_reasoning("qwen/qwen3.8-27b:free", Mode::Max, false),
            Some(("reasoning", json!({ "effort": "max" })))
        );
        assert_eq!(
            openai_chat_reasoning("qwen/qwen3.8-27b:free", Mode::Alt, false),
            Some(("reasoning", json!({ "max_tokens": 24_576 })))
        );
        assert!(openai_chat_reasoning("qwen/qwen3.8-27b:free", Mode::Unsupported, false).is_none());
    }

    #[test]
    fn responses_强制思考带_summary_与_include() {
        let r = openai_responses_reasoning("openai/gpt-5.6", Mode::Max).unwrap();
        assert_eq!(r["effort"], "max");
        assert_eq!(r["summary"], "auto");
        assert_eq!(
            openai_responses_reasoning("gpt-4o", Mode::Alt).unwrap()["effort"],
            "high"
        );
        assert!(openai_responses_reasoning("gpt-5", Mode::Unsupported).is_none());
    }

    #[test]
    fn 阶梯_认得出思考参数被拒() {
        assert!(is_thinking_rejection("Unsupported parameter: 'reasoning'"));
        assert!(is_thinking_rejection(
            "thinking.type: Extra inputs are not permitted"
        ));
        assert!(is_thinking_rejection("Unknown name \"reasoning_effort\""));
        assert!(!is_thinking_rejection(
            "GenerateContentRequest.tools[0].function_declarations[31].parameters.properties[batch].items: missing field."
        ));
        assert!(!is_thinking_rejection("Provider returned error"));
    }

    #[test]
    fn 模式_降级到头返回_none() {
        assert_eq!(Mode::Max.downgrade(), Some(Mode::Alt));
        assert_eq!(Mode::Alt.downgrade(), Some(Mode::Legacy));
        assert_eq!(Mode::Legacy.downgrade(), None);
    }
}
