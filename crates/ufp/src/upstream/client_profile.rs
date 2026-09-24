//! 按客户端的样子发请求：目前只有 OpenCode（访问 OpenCode Zen 时用）。
//!
//! 请求头里静态的部分（User-Agent、`x-opencode-client`、`x-opencode-project` 等）写在渠道的
//! 附加头里，后台可见可改；这里只管两件附加头做不到的事：
//!
//! 1. **会话级 id**：`x-opencode-session` / `x-opencode-request` 按会话、按轮次变化。
//!    附加头里写占位 `{opencode_session}` / `{opencode_request}`，发请求时换成这里生成的值。
//! 2. **请求体**：OpenCode 按模型族给请求体加的那些字段（`store:false`、`prompt_cache_key`、
//!    思考参数、`tool_choice` 等）。
//!
//! 依据（`sst/opencode` 1.18.32，本机抓包核对过）：
//! - 头：`packages/opencode/src/session/llm/request.ts` 的 `headers`（providerID 以 `opencode` 开头的分支）；
//! - id 格式：`packages/opencode/src/id/id.ts`（会话 id 降序、消息 id 升序）；
//! - 请求体：`packages/opencode/src/provider/transform.ts` 的 `options()` 与 `applyCaching()`。
//!   `applyCaching` 只对 Claude 系列生效，而 Claude Code 自己已经打好了 `cache_control`，所以不用再做。
//!
//! 原则：**不覆盖客户端明确给的值**。Claude Code 已经给了的思考强度、tool_choice 之类原样保留，
//! 只补 OpenCode 会带、而这次请求里缺着的字段。

use std::collections::HashMap;
use std::sync::Mutex;

use rand::Rng;
use serde_json::{json, Value};

use crate::store::Protocol;

/// 附加头里的占位：换成会话 id。
pub const SESSION_PLACEHOLDER: &str = "{opencode_session}";
/// 附加头里的占位：换成本轮的请求 id。
pub const REQUEST_PLACEHOLDER: &str = "{opencode_request}";

/// 一次请求用的 OpenCode 身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpencodeIds {
    /// `ses_…`：同一个 Claude Code 会话不变。
    pub session: String,
    /// `msg_…`：同一轮用户输入不变（工具循环的每一步共用），新一轮换新的。
    pub request: String,
}

impl OpencodeIds {
    /// 没有会话可以挂靠时现生成一组（OpenCode 每次 `run` 也是新会话）。
    pub fn fresh() -> Self {
        let now = now_ms();
        Self {
            session: create_id("ses", true, now, 1),
            request: create_id("msg", false, now, 2),
        }
    }
}

/// 会话 → OpenCode 身份的映射。只在内存里：重启后相当于 OpenCode 开了个新会话，不影响什么。
pub struct OpencodeBook {
    cap: usize,
    inner: Mutex<BookInner>,
}

struct BookInner {
    map: HashMap<String, Slot>,
    tick: u64,
    last_ms: i64,
    counter: u64,
}

struct Slot {
    session: String,
    turn: usize,
    request: String,
    used: u64,
}

impl BookInner {
    /// 照 `id.ts` 的做法：同一毫秒内的 id 用计数器区分。
    fn next_counter(&mut self, now: i64) -> u64 {
        if now != self.last_ms {
            self.last_ms = now;
            self.counter = 0;
        }
        self.counter += 1;
        self.counter
    }
}

impl Default for OpencodeBook {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl OpencodeBook {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            inner: Mutex::new(BookInner {
                map: HashMap::new(),
                tick: 0,
                last_ms: 0,
                counter: 0,
            }),
        }
    }

    /// 这次请求该用的身份。`session_id` 是网关从 Claude Code 请求里认出的会话 id。
    pub fn ids(&self, session_id: Option<&str>, body: &Value) -> OpencodeIds {
        let Some(sid) = session_id.filter(|s| !s.is_empty()) else {
            return OpencodeIds::fresh();
        };
        let turn = user_turns(body);
        let now = now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.tick += 1;
        let tick = g.tick;
        if let Some(slot) = g.map.get(sid) {
            if slot.turn == turn {
                let ids = OpencodeIds {
                    session: slot.session.clone(),
                    request: slot.request.clone(),
                };
                if let Some(slot) = g.map.get_mut(sid) {
                    slot.used = tick;
                }
                return ids;
            }
        }
        let c = g.next_counter(now);
        let request = create_id("msg", false, now, c);
        if let Some(slot) = g.map.get_mut(sid) {
            slot.turn = turn;
            slot.request = request.clone();
            slot.used = tick;
            return OpencodeIds {
                session: slot.session.clone(),
                request,
            };
        }
        if g.map.len() >= self.cap {
            // 满了淘汰最久没用的一条（O(n)，只在新会话进来时发生）
            if let Some(old) = g
                .map
                .iter()
                .min_by_key(|(_, s)| s.used)
                .map(|(k, _)| k.clone())
            {
                g.map.remove(&old);
            }
        }
        let c = g.next_counter(now);
        let session = create_id("ses", true, now, c);
        g.map.insert(
            sid.to_string(),
            Slot {
                session: session.clone(),
                turn,
                request: request.clone(),
                used: tick,
            },
        );
        OpencodeIds { session, request }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// `id.ts` 的 `create`：前缀 + 6 字节时间（毫秒 × 0x1000 + 计数，降序时按位取反）的十六进制
/// + 14 位 base62 随机串，一共 26 位。
fn create_id(prefix: &str, descending: bool, now_ms: i64, counter: u64) -> String {
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut v = (now_ms as u64).wrapping_mul(0x1000).wrapping_add(counter);
    if descending {
        v = !v;
    }
    let mut out = String::with_capacity(prefix.len() + 27);
    out.push_str(prefix);
    out.push('_');
    for i in 0..6 {
        let byte = (v >> (40 - 8 * i)) & 0xff;
        out.push_str(&format!("{byte:02x}"));
    }
    let mut rng = rand::thread_rng();
    for _ in 0..14 {
        // 与 OpenCode 一样取 `随机字节 % 62`（有一点偏差，照抄就是为了一样）
        let b: u8 = rng.gen();
        out.push(B62[(b % 62) as usize] as char);
    }
    out
}

/// 第几轮用户输入：数「不含 tool_result 的 user 消息」。
/// 工具循环里续写的那几步只追加 tool_result，轮次不变，于是 request id 也不变——和 OpenCode 一样。
pub fn user_turns(body: &Value) -> usize {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return 0;
    };
    messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .filter(|m| match m.get("content") {
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Array(blocks)) => {
                !blocks.is_empty()
                    && !blocks
                        .iter()
                        .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            }
            _ => false,
        })
        .count()
}

/// 把附加头值里的占位换掉。
pub fn fill_placeholders(value: &str, ids: Option<&OpencodeIds>) -> String {
    let Some(ids) = ids else {
        return value.to_string();
    };
    if !value.contains('{') {
        return value.to_string();
    }
    value
        .replace(SESSION_PLACEHOLDER, &ids.session)
        .replace(REQUEST_PLACEHOLDER, &ids.request)
}

/// 值里有没有 OpenCode 的占位（拉模型列表这类没有会话的场合要去掉它们）。
pub fn has_placeholder(value: &str) -> bool {
    value.contains(SESSION_PLACEHOLDER) || value.contains(REQUEST_PLACEHOLDER)
}

/// 按 OpenCode 的样子调整转换后的请求体。
pub fn shape_opencode(protocol: Protocol, model: &str, body: &mut Value, ids: &OpencodeIds) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let has_tools = obj
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    let id = model.to_ascii_lowercase();
    match protocol {
        Protocol::OpenAiResponses => {
            // options()：@ai-sdk/openai 一律 store:false
            obj.insert("store".into(), json!(false));
            // AI SDK 的 Responses 实现把 system 放成 input 里的 developer 消息，而不是 instructions
            if let Some(Value::String(instr)) = obj.remove("instructions") {
                if !instr.is_empty() {
                    let input = obj.entry("input").or_insert_with(|| json!([]));
                    if let Some(arr) = input.as_array_mut() {
                        arr.insert(0, json!({"role": "developer", "content": instr}));
                    }
                }
            }
            // request.ts：Responses 家族的函数工具一律 strict:false
            if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) {
                for t in tools {
                    if t.get("type").and_then(Value::as_str) == Some("function") {
                        t["strict"] = json!(false);
                    }
                }
            }
            if id.contains("gpt-5") && !id.contains("gpt-5-chat") {
                let reasoning = obj.entry("reasoning").or_insert_with(|| json!({}));
                if let Some(r) = reasoning.as_object_mut() {
                    if !id.contains("gpt-5-pro") {
                        r.entry("effort").or_insert(json!("medium"));
                    }
                    r.entry("summary").or_insert(json!("auto"));
                }
                if id.contains("gpt-5.") && !id.contains("codex") && !id.contains("-chat") {
                    let text = obj.entry("text").or_insert_with(|| json!({}));
                    if let Some(t) = text.as_object_mut() {
                        t.entry("verbosity").or_insert(json!("low"));
                    }
                }
                // providerID 以 opencode 开头：缓存键用会话 id，并要回加密推理（store:false 下多轮要用）
                obj.insert("prompt_cache_key".into(), json!(ids.session));
                let include = obj.entry("include").or_insert_with(|| json!([]));
                if let Some(arr) = include.as_array_mut() {
                    if !arr
                        .iter()
                        .any(|v| v.as_str() == Some("reasoning.encrypted_content"))
                    {
                        arr.push(json!("reasoning.encrypted_content"));
                    }
                }
            }
            if has_tools {
                obj.entry("tool_choice").or_insert(json!("auto"));
            }
        }
        Protocol::OpenAiChat => {
            if ["kimi-k2-thinking", "glm-4.6"].contains(&id.as_str()) {
                obj.entry("chat_template_args")
                    .or_insert(json!({"enable_thinking": true}));
            }
            if has_tools {
                obj.entry("tool_choice").or_insert(json!("auto"));
            }
        }
        Protocol::Anthropic => {
            // Claude Code 的 metadata.user_id 不该以 OpenCode 的身份发出去；OpenCode 也不发 metadata
            obj.remove("metadata");
            // @ai-sdk/anthropic 默认开细粒度工具流式：自定义工具上带 eager_input_streaming
            if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) {
                for t in tools {
                    let custom =
                        matches!(t.get("type").and_then(Value::as_str), None | Some("custom"));
                    if custom && t.get("input_schema").is_some() {
                        if let Some(o) = t.as_object_mut() {
                            o.entry("eager_input_streaming").or_insert(json!(true));
                        }
                    }
                }
            }
            if has_tools {
                obj.entry("tool_choice").or_insert(json!({"type": "auto"}));
            }
        }
        Protocol::Gemini => {
            // options()：会推理的 Gemini 带 includeThoughts，非老款（1.x/2.x）再加 thinkingLevel:high
            let legacy = is_legacy_gemini(&id);
            let gen = obj.entry("generationConfig").or_insert_with(|| json!({}));
            if let Some(g) = gen.as_object_mut() {
                if !g.contains_key("thinkingConfig") {
                    let mut tc = json!({"includeThoughts": true});
                    if !legacy {
                        tc["thinkingLevel"] = json!("high");
                    }
                    g.insert("thinkingConfig".into(), tc);
                }
            }
            if has_tools && !obj.contains_key("toolConfig") {
                obj.insert(
                    "toolConfig".into(),
                    json!({"functionCallingConfig": {"mode": "AUTO"}}),
                );
            }
        }
    }
}

/// transform.ts 的 `GEMINI_LEGACY_RE = /gemini-(?:(?:flash|pro)-)?[12](?:[.-]|$)/i`。
fn is_legacy_gemini(id: &str) -> bool {
    let Some(pos) = id.find("gemini-") else {
        return false;
    };
    let mut rest = &id[pos + "gemini-".len()..];
    for p in ["flash-", "pro-"] {
        if let Some(r) = rest.strip_prefix(p) {
            rest = r;
            break;
        }
    }
    let mut chars = rest.chars();
    matches!(chars.next(), Some('1' | '2')) && matches!(chars.next(), None | Some('.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn looks_like_opencode_id(id: &str, prefix: &str) -> bool {
        let Some(rest) = id.strip_prefix(prefix).and_then(|r| r.strip_prefix('_')) else {
            return false;
        };
        rest.len() == 26
            && rest[..12]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            && rest[12..].chars().all(|c| c.is_ascii_alphanumeric())
    }

    #[test]
    fn id_格式与_opencode_一致() {
        // 抓包里的真实值：ses_f2b0a2cacffeR75IvpnPf8uVH9 / msg_0d4f5d384001EY134OLz40JVTK
        assert!(looks_like_opencode_id(
            "ses_f2b0a2cacffeR75IvpnPf8uVH9",
            "ses"
        ));
        assert!(looks_like_opencode_id(
            "msg_0d4f5d384001EY134OLz40JVTK",
            "msg"
        ));
        let ids = OpencodeIds::fresh();
        assert!(
            looks_like_opencode_id(&ids.session, "ses"),
            "{}",
            ids.session
        );
        assert!(
            looks_like_opencode_id(&ids.request, "msg"),
            "{}",
            ids.request
        );
        // 同一时刻：会话 id 是时间取反（降序），消息 id 是时间本身（升序）
        let t = 1_790_000_000_000i64;
        let s = create_id("ses", true, t, 1);
        let m = create_id("msg", false, t, 1);
        let hs = u64::from_str_radix(&s[4..16], 16).unwrap();
        let hm = u64::from_str_radix(&m[4..16], 16).unwrap();
        assert_eq!(hs, !hm & 0xffff_ffff_ffff);
        // 与 id.ts 的算法逐位一致：(t * 0x1000 + 1) 的低 48 位
        assert_eq!(hm, ((t as u64) * 0x1000 + 1) & 0xffff_ffff_ffff);
    }

    #[test]
    fn 同一轮工具循环共用_request_新一轮换新的_会话不变() {
        let book = OpencodeBook::new(8);
        let turn1 = json!({"messages": [{"role": "user", "content": "读一下 README"}]});
        let turn1_tool = json!({"messages": [
            {"role": "user", "content": "读一下 README"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "# demo"},
                                         {"type": "text", "text": "<system-reminder>x</system-reminder>"}]}
        ]});
        let turn2 = json!({"messages": [
            {"role": "user", "content": "读一下 README"},
            {"role": "assistant", "content": "好了"},
            {"role": "user", "content": [{"type": "text", "text": "再总结一下"}]}
        ]});
        let a = book.ids(Some("cc-1"), &turn1);
        let b = book.ids(Some("cc-1"), &turn1_tool);
        let c = book.ids(Some("cc-1"), &turn2);
        assert_eq!(a, b, "工具结果续写还是同一轮");
        assert_eq!(a.session, c.session);
        assert_ne!(a.request, c.request, "新一轮用户输入换新的 request id");
        let other = book.ids(Some("cc-2"), &turn1);
        assert_ne!(other.session, a.session);
        // 没有会话 id：每次现生成
        assert_ne!(book.ids(None, &turn1), book.ids(None, &turn1));
    }

    #[test]
    fn 表满了淘汰最久没用的() {
        let book = OpencodeBook::new(2);
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let a = book.ids(Some("a"), &body);
        book.ids(Some("b"), &body);
        book.ids(Some("a"), &body); // a 刚用过
        book.ids(Some("c"), &body); // 挤掉 b
        assert_eq!(book.len(), 2);
        assert_eq!(book.ids(Some("a"), &body).session, a.session, "a 还在");
    }

    #[test]
    fn 占位替换() {
        let ids = OpencodeIds {
            session: "ses_x".into(),
            request: "msg_y".into(),
        };
        assert_eq!(fill_placeholders("{opencode_session}", Some(&ids)), "ses_x");
        assert_eq!(fill_placeholders("{opencode_request}", Some(&ids)), "msg_y");
        assert_eq!(fill_placeholders("cli", Some(&ids)), "cli");
        assert!(has_placeholder("{opencode_request}"));
        assert!(!has_placeholder("opencode/1.18.32"));
    }

    fn ids() -> OpencodeIds {
        OpencodeIds {
            session: "ses_f2b0a2411ffecEm25Ktn3C6j6F".into(),
            request: "msg_0d4f5dc15001oqOS7EiS2qqWcg".into(),
        }
    }

    #[test]
    fn responses_照抓包补字段_不覆盖客户端给的() {
        // 抓包（gpt-5.5）：store:false、developer 消息、prompt_cache_key=会话、
        // reasoning{effort:medium,summary:auto}、text.verbosity=low、include 加密推理、工具 strict:false
        let mut body = json!({
            "model": "gpt-5.5", "instructions": "SYS",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [{"type": "function", "name": "bash", "parameters": {}}],
            "stream": true
        });
        shape_opencode(Protocol::OpenAiResponses, "gpt-5.5", &mut body, &ids());
        assert_eq!(body["store"], false);
        assert!(body.get("instructions").is_none());
        assert_eq!(
            body["input"][0],
            json!({"role": "developer", "content": "SYS"})
        );
        assert_eq!(body["prompt_cache_key"], "ses_f2b0a2411ffecEm25Ktn3C6j6F");
        assert_eq!(
            body["reasoning"],
            json!({"effort": "medium", "summary": "auto"})
        );
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tool_choice"], "auto");

        // 客户端给了思考强度：保留
        let mut body = json!({"model": "gpt-5.5", "input": [], "reasoning": {"effort": "high"}});
        shape_opencode(Protocol::OpenAiResponses, "gpt-5.5", &mut body, &ids());
        assert_eq!(body["reasoning"]["effort"], "high");
        // codex 不加 verbosity；非 gpt-5 系列不加缓存键
        let mut body = json!({"model": "gpt-5.3-codex", "input": []});
        shape_opencode(
            Protocol::OpenAiResponses,
            "gpt-5.3-codex",
            &mut body,
            &ids(),
        );
        assert!(body.get("text").is_none());
        let mut body = json!({"model": "grok-4.7", "input": []});
        shape_opencode(Protocol::OpenAiResponses, "grok-4.7", &mut body, &ids());
        assert_eq!(body["store"], false);
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn anthropic_去掉_metadata_自定义工具开细粒度流式() {
        let mut body = json!({
            "model": "claude-sonnet-5", "metadata": {"user_id": "user_abc_session_x"},
            "tools": [
                {"name": "Bash", "input_schema": {"type": "object"}},
                {"type": "web_search_20250305", "name": "web_search"}
            ],
            "messages": []
        });
        shape_opencode(Protocol::Anthropic, "claude-sonnet-5", &mut body, &ids());
        assert!(body.get("metadata").is_none());
        assert_eq!(body["tools"][0]["eager_input_streaming"], true);
        assert!(body["tools"][1].get("eager_input_streaming").is_none());
        assert_eq!(body["tool_choice"], json!({"type": "auto"}));
    }

    #[test]
    fn gemini_补思考配置_老款不加_thinking_level() {
        let mut body = json!({"contents": [], "tools": [{"functionDeclarations": []}]});
        shape_opencode(Protocol::Gemini, "gemini-3.8-flash", &mut body, &ids());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": true, "thinkingLevel": "high"})
        );
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
        // 客户端（转换器）已经给了思考配置：不动
        let mut body = json!({"generationConfig": {"thinkingConfig": {"includeThoughts": false}}});
        shape_opencode(Protocol::Gemini, "gemini-3.8-flash", &mut body, &ids());
        assert_eq!(
            body["generationConfig"]["thinkingConfig"],
            json!({"includeThoughts": false})
        );
        assert!(is_legacy_gemini("gemini-2.5-flash"));
        assert!(is_legacy_gemini("gemini-flash-1.5"));
        assert!(!is_legacy_gemini("gemini-3.8-flash"));
        assert!(!is_legacy_gemini("gemini-20"));
    }

    #[test]
    fn chat_只补_tool_choice() {
        let mut body =
            json!({"model": "big-pickle", "messages": [], "tools": [{"type": "function"}]});
        shape_opencode(Protocol::OpenAiChat, "big-pickle", &mut body, &ids());
        assert_eq!(body["tool_choice"], "auto");
        assert!(body.get("chat_template_args").is_none());
        let mut body = json!({"model": "x", "messages": []});
        shape_opencode(Protocol::OpenAiChat, "x", &mut body, &ids());
        assert!(
            body.get("tool_choice").is_none(),
            "没有工具就不带 tool_choice"
        );
    }
}
