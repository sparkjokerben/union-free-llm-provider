//! 渠道预设：常见免费额度来源一键接入。
//!
//! 一个预设 = 协议与地址 + 该上游「原生客户端」的请求头 + 怎么拉模型列表。
//! 后台的流程是：选预设 → 填 key → 拉模型列表（标出免费 / 不支持工具的）→ 勾选 → 应用。
//! 应用是幂等的：同一预设再应用一次只补新 key 和新条目，不会重复建渠道。
//!
//! 模仿客户端请求头的做法参照 cc-switch `src/config/userAgentPresets.ts`（把转发请求伪装成
//! 上游认得的客户端，由用户显式选择）。头都写进渠道的「附加请求头」，建好后在后台可见可改。
//! 版本号取自各客户端 2026-09 的最新发布，过时了改渠道里的值即可。
//!
//! 边界：OpenCode Zen 的免费额度只允许在 OpenCode 内使用（服务端会回 403
//! `FreeTierError`）。这是对方明确的访问限制，这里不伪装 OpenCode 去绕过它——
//! Zen 预设只接用户自己的 Zen key，并且不列免费模型（403 还会触发网关自动停用 key，
//! 连带把同一把 key 上的付费条目一起停掉）。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use super::session::require_auth;
use super::{bad_request, internal, mutate};
use crate::api::AppState;
use crate::store::Protocol;

/// 一个预设。
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub summary: &'static str,
    /// 去哪申请 key。
    pub key_url: &'static str,
    /// 请求头模仿的是谁（界面上说明用）；None 表示不模仿任何客户端。
    pub client: Option<&'static str>,
    /// 按协议分的入口地址（Zen 按模型族分走不同协议）。
    pub routes: &'static [(Protocol, &'static str)],
    pub models: ModelSource,
    /// 拉模型列表是否需要 key。
    pub list_needs_key: bool,
    /// 界面上额外提醒的几句话。
    pub notes: &'static [&'static str],
}

#[derive(Clone, Copy)]
pub enum ModelSource {
    OpenRouter,
    Gemini,
    Zen,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        summary: "聚合几百个模型，带 :free 的免费（按账号限速，充值过 10 美元的账号日额度更高）。",
        key_url: "https://openrouter.ai/keys",
        client: Some("OpenCode 1.18.32"),
        routes: &[(Protocol::OpenAiChat, "https://openrouter.ai/api/v1")],
        models: ModelSource::OpenRouter,
        list_needs_key: false,
        notes: &[
            "免费模型的额度按账号算，多把 key 挂在同一个账号下不会变多。",
            "不支持工具调用的模型 Claude Code 用不了，列表里不让勾。",
        ],
    },
    Preset {
        id: "google-ai-studio",
        name: "Google AI Studio",
        summary: "Gemini 原生接口。免费额度由 key 所在项目决定，接口本身看不出哪些免费。",
        key_url: "https://aistudio.google.com/apikey",
        client: Some("Gemini CLI 0.61.0"),
        routes: &[(
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
        )],
        models: ModelSource::Gemini,
        list_needs_key: true,
        notes: &["User-Agent 里带着模型名，发请求时按条目自动替换（附加请求头里的 {model}）。"],
    },
    Preset {
        id: "opencode-zen",
        name: "OpenCode Zen",
        summary: "OpenCode 的精选模型网关，按量付费；不同模型族走不同协议，会按需建多个渠道。",
        key_url: "https://opencode.ai/auth",
        client: None,
        routes: &[
            (Protocol::OpenAiChat, "https://opencode.ai/zen/v1"),
            (Protocol::OpenAiResponses, "https://opencode.ai/zen/v1"),
            (Protocol::Anthropic, "https://opencode.ai/zen/v1"),
        ],
        models: ModelSource::Zen,
        list_needs_key: false,
        notes: &[
            "Zen 的免费模型只允许在 OpenCode 里用（其他客户端会被 403），这里不列，也不伪装 OpenCode 去绕。",
            "Gemini 系列在 Zen 上走的是 Google 形态的路径，请直接用 Google AI Studio 预设。",
        ],
    },
];

pub fn find(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// 该预设在建渠道时写入的附加请求头（每次建渠道现生成：安装 id 之类要像一台真实的机器）。
pub fn client_headers(preset: &Preset) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    match preset.id {
        // OpenCode 访问 OpenRouter 时的头：provider.ts 的 HTTP-Referer / X-Title，
        // User-Agent 是 opencode/<版本> 加上 AI SDK 自己追加的两段（openrouter 走 provider-utils 4.0.23）。
        "openrouter" => {
            h.insert("HTTP-Referer".into(), "https://opencode.ai/".into());
            h.insert("X-Title".into(), "opencode".into());
            h.insert(
                "User-Agent".into(),
                "opencode/1.18.32 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14".into(),
            );
        }
        // Gemini CLI（contentGenerator.ts）：GeminiCLI-<客户端>/<版本>/<模型> (<平台>; <架构>; <界面>)；
        // x-goog-api-client 是 @google/genai 1.30.0 的默认值；privileged-user-id 是每次安装一个的 UUID。
        "google-ai-studio" => {
            h.insert(
                "User-Agent".into(),
                "GeminiCLI-tui/0.61.0/{model} (linux; x64; terminal)".into(),
            );
            h.insert(
                "x-goog-api-client".into(),
                "google-genai-sdk/1.30.0 gl-node/v22.19.0".into(),
            );
            h.insert(
                "x-gemini-api-privileged-user-id".into(),
                uuid::Uuid::new_v4().to_string(),
            );
        }
        _ => {}
    }
    h
}

/// 协议在界面与渠道名里的叫法。
fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::OpenAiChat => "chat",
        Protocol::OpenAiResponses => "responses",
        Protocol::Anthropic => "messages",
        Protocol::Gemini => "gemini",
    }
}

fn route_base(preset: &Preset, protocol: Protocol) -> Option<&'static str> {
    preset
        .routes
        .iter()
        .find(|(p, _)| *p == protocol)
        .map(|(_, b)| *b)
}

fn channel_name(preset: &Preset, protocol: Protocol) -> String {
    if preset.routes.len() > 1 {
        format!("{} · {}", preset.name, protocol_label(protocol))
    } else {
        preset.name.to_string()
    }
}

// ============================================================================
// 模型列表
// ============================================================================

/// 候选模型：界面上一行一个，勾选后成为条目。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCandidate {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub context: u64,
    pub vision: bool,
    pub pdf: bool,
    /// None 表示接口看不出来。
    pub free: Option<bool>,
    /// None 表示接口看不出来。
    pub tools: Option<bool>,
    /// 为什么不能选（空 = 能选）。
    pub blocked: String,
    pub note: String,
}

impl ModelCandidate {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id, "name": self.name, "protocol": self.protocol.as_str(),
            "context": self.context, "vision": self.vision, "pdf": self.pdf,
            "free": self.free, "tools": self.tools,
            "selectable": self.blocked.is_empty(), "blocked": self.blocked, "note": self.note,
        })
    }
}

const NO_TOOLS: &str = "不支持工具调用，Claude Code 用不了";

#[derive(Deserialize)]
struct OrList {
    data: Vec<OrModel>,
}
#[derive(Deserialize)]
struct OrModel {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<OrArch>,
    #[serde(default)]
    pricing: Option<OrPricing>,
    #[serde(default)]
    supported_parameters: Vec<String>,
    #[serde(default)]
    expiration_date: Option<String>,
}
#[derive(Deserialize)]
struct OrArch {
    #[serde(default)]
    input_modalities: Vec<String>,
}
#[derive(Deserialize)]
struct OrPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
}

/// OpenRouter `/api/v1/models`。免费 = 输入输出价格都是字符串 "0"（"-1" 是浮动价，不算）。
pub fn parse_openrouter(body: &str) -> Result<Vec<ModelCandidate>, String> {
    let list: OrList = serde_json::from_str(body).map_err(|e| format!("模型列表格式不对：{e}"))?;
    Ok(list
        .data
        .into_iter()
        .map(|m| {
            let free = m
                .pricing
                .as_ref()
                .map(|p| p.prompt.as_deref() == Some("0") && p.completion.as_deref() == Some("0"));
            let inputs = m
                .architecture
                .map(|a| a.input_modalities)
                .unwrap_or_default();
            let tools = m.supported_parameters.iter().any(|p| p == "tools");
            ModelCandidate {
                name: if m.name.is_empty() {
                    m.id.clone()
                } else {
                    m.name
                },
                id: m.id,
                protocol: Protocol::OpenAiChat,
                context: m.context_length.unwrap_or(0),
                vision: inputs.iter().any(|i| i == "image"),
                pdf: inputs.iter().any(|i| i == "file"),
                free,
                tools: Some(tools),
                blocked: if tools {
                    String::new()
                } else {
                    NO_TOOLS.into()
                },
                note: m
                    .expiration_date
                    .map(|d| format!("{d} 下线"))
                    .unwrap_or_default(),
            }
        })
        .collect())
}

#[derive(Deserialize)]
struct GeminiList {
    #[serde(default)]
    models: Vec<GeminiModel>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiModel {
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

/// Gemini `/v1beta/models`：只留能 generateContent 的。接口里没有价格与模态字段。
pub fn parse_gemini(body: &str) -> Result<Vec<ModelCandidate>, String> {
    let list: GeminiList =
        serde_json::from_str(body).map_err(|e| format!("模型列表格式不对：{e}"))?;
    Ok(list
        .models
        .into_iter()
        .filter(|m| {
            m.supported_generation_methods
                .iter()
                .any(|x| x == "generateContent")
        })
        .map(|m| {
            let id = m
                .name
                .strip_prefix("models/")
                .unwrap_or(&m.name)
                .to_string();
            // 语音、生图、电脑操作这类专用变体也能 generateContent，但当不了 Claude Code 的主模型
            let special = ["tts", "image", "computer-use", "audio", "live", "robotics"]
                .iter()
                .any(|k| id.contains(k));
            let gemini = id.starts_with("gemini") && !special;
            ModelCandidate {
                name: if m.display_name.is_empty() {
                    id.clone()
                } else {
                    m.display_name
                },
                protocol: Protocol::Gemini,
                context: m.input_token_limit.unwrap_or(0),
                // Gemini 系列都是多模态；Gemma 等开放模型在 API 上只收文本
                vision: gemini,
                pdf: gemini,
                free: None,
                tools: if gemini { Some(true) } else { None },
                blocked: String::new(),
                note: if special {
                    "专用模型（语音 / 生图 / 电脑操作），当不了 Claude Code 的主模型".into()
                } else if gemini {
                    String::new()
                } else {
                    "不是 Gemini 系列，工具调用可能不支持".into()
                },
                id,
            }
        })
        .collect())
}

/// models.dev 里只取 `opencode` 这一节；其余几百个提供商用 IgnoredAny 跳过，
/// 不在 512MB 的机器上把 5MB 的 JSON 整棵树建出来。
#[derive(Deserialize)]
struct ModelsDev {
    #[serde(default)]
    opencode: Option<DevProvider>,
}
#[derive(Deserialize)]
struct DevProvider {
    #[serde(default)]
    npm: String,
    #[serde(default)]
    models: HashMap<String, DevModel>,
}
#[derive(Deserialize)]
struct DevModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tool_call: Option<bool>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    modalities: Option<DevModalities>,
    #[serde(default)]
    limit: Option<DevLimit>,
    #[serde(default)]
    cost: Option<DevCost>,
    #[serde(default)]
    provider: Option<DevModelProvider>,
}
#[derive(Deserialize)]
struct DevModalities {
    #[serde(default)]
    input: Vec<String>,
}
#[derive(Deserialize)]
struct DevLimit {
    #[serde(default)]
    context: Option<u64>,
}
#[derive(Deserialize)]
struct DevCost {
    #[serde(default)]
    input: Option<f64>,
}
#[derive(Deserialize)]
struct DevModelProvider {
    #[serde(default)]
    npm: Option<String>,
}
#[derive(Deserialize)]
struct ZenList {
    data: Vec<ZenModel>,
}
#[derive(Deserialize)]
struct ZenModel {
    id: String,
}

/// Zen：`/zen/v1/models` 给出线上真有的模型（只有 id），models.dev 补协议、上下文、模态与价格。
/// OpenCode 自己也是这样判断免费的（cost.input == 0）。
pub fn parse_zen(zen_body: &str, dev_body: &str) -> Result<Vec<ModelCandidate>, String> {
    let live: ZenList =
        serde_json::from_str(zen_body).map_err(|e| format!("Zen 模型列表格式不对：{e}"))?;
    let dev: ModelsDev =
        serde_json::from_str(dev_body).map_err(|e| format!("models.dev 格式不对：{e}"))?;
    let provider = dev.opencode.unwrap_or(DevProvider {
        npm: String::new(),
        models: HashMap::new(),
    });
    Ok(live
        .data
        .into_iter()
        .filter_map(|z| {
            let meta = provider.models.get(&z.id);
            if meta.and_then(|m| m.status.as_deref()) == Some("deprecated") {
                return None;
            }
            let npm = meta
                .and_then(|m| m.provider.as_ref())
                .and_then(|p| p.npm.clone())
                .unwrap_or_else(|| provider.npm.clone());
            let inputs = meta
                .and_then(|m| m.modalities.as_ref())
                .map(|m| m.input.clone())
                .unwrap_or_default();
            let free = meta
                .and_then(|m| m.cost.as_ref())
                .and_then(|c| c.input)
                .map(|c| c == 0.0);
            let tools = meta.and_then(|m| m.tool_call);
            let (protocol, mut blocked) = match npm.as_str() {
                "@ai-sdk/openai" => (Protocol::OpenAiResponses, String::new()),
                "@ai-sdk/anthropic" => (Protocol::Anthropic, String::new()),
                "@ai-sdk/google" => (
                    Protocol::Gemini,
                    "Gemini 系列请用 Google AI Studio 预设".to_string(),
                ),
                _ => (Protocol::OpenAiChat, String::new()),
            };
            if blocked.is_empty() && free == Some(true) {
                blocked = "免费额度只允许在 OpenCode 里用".into();
            }
            if blocked.is_empty() && tools == Some(false) {
                blocked = NO_TOOLS.into();
            }
            Some(ModelCandidate {
                name: meta
                    .map(|m| m.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| z.id.clone()),
                protocol,
                context: meta
                    .and_then(|m| m.limit.as_ref())
                    .and_then(|l| l.context)
                    .unwrap_or(0),
                vision: inputs.iter().any(|i| i == "image"),
                pdf: inputs.iter().any(|i| i == "pdf"),
                free,
                tools,
                note: if meta.is_none() {
                    "models.dev 里没有它的资料，上下文与模态按默认值".into()
                } else {
                    String::new()
                },
                blocked,
                id: z.id,
            })
        })
        .collect())
}

async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<String, String> {
    let mut req = client.get(url).timeout(Duration::from_secs(25));
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("请求 {url} 失败：{e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("读取 {url} 失败：{e}"))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("{url} 返回 {}：{snippet}", status.as_u16()));
    }
    Ok(text)
}

/// 拉模型列表时带上该预设的客户端头（和真实请求一致；{model} 这类占位没有具体模型可填，去掉）。
fn listing_headers(preset: &Preset) -> Vec<(String, String)> {
    client_headers(preset)
        .into_iter()
        .map(|(k, v)| (k, v.replace("/{model}", "").replace("{model}", "")))
        .collect()
}

pub async fn discover(
    client: &reqwest::Client,
    preset: &Preset,
    api_key: &str,
) -> Result<Vec<ModelCandidate>, String> {
    let mut headers = listing_headers(preset);
    let mut models = match preset.models {
        ModelSource::OpenRouter => {
            let body = fetch_text(client, "https://openrouter.ai/api/v1/models", &headers).await?;
            parse_openrouter(&body)?
        }
        ModelSource::Gemini => {
            if api_key.is_empty() {
                return Err("Google AI Studio 列模型需要 key，先把 key 填上".into());
            }
            headers.push(("x-goog-api-key".into(), api_key.to_string()));
            let base = route_base(preset, Protocol::Gemini).unwrap_or_default();
            let body = fetch_text(
                client,
                &format!("{base}/v1beta/models?pageSize=1000"),
                &headers,
            )
            .await?;
            parse_gemini(&body)?
        }
        ModelSource::Zen => {
            if !api_key.is_empty() {
                headers.push(("authorization".into(), format!("Bearer {api_key}")));
            }
            let (zen, dev) = tokio::join!(
                fetch_text(client, "https://opencode.ai/zen/v1/models", &headers),
                fetch_text(client, "https://models.dev/api.json", &[]),
            );
            parse_zen(&zen?, &dev?)?
        }
    };
    // 能选的在前；其中免费的在前、确定支持工具的在前；最后按名字
    let rank = |m: &ModelCandidate| {
        (
            !m.blocked.is_empty(),
            m.free != Some(true),
            m.tools != Some(true),
        )
    };
    models.sort_by(|a, b| (rank(a), a.id.as_str()).cmp(&(rank(b), b.id.as_str())));
    Ok(models)
}

// ============================================================================
// 接口
// ============================================================================

pub async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let out: Vec<Value> = PRESETS
        .iter()
        .map(|p| {
            json!({
                "id": p.id, "name": p.name, "summary": p.summary, "key_url": p.key_url,
                "client": p.client, "list_needs_key": p.list_needs_key, "notes": p.notes,
                "headers": client_headers(p).keys().collect::<Vec<_>>(),
                "routes": p.routes.iter().map(|(proto, base)| json!({
                    "protocol": proto.as_str(), "base_url": base, "channel": channel_name(p, *proto),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
pub struct ModelsPayload {
    #[serde(default)]
    api_key: String,
}

pub async fn models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(p): Json<ModelsPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let preset = find(&id).ok_or_else(|| bad_request("没有这个预设"))?;
    // 没填 key 时，用这个预设已建渠道里的 key（再次应用、只补模型的情况）
    let mut key = p.api_key.trim().to_string();
    if key.is_empty() && preset.list_needs_key {
        key = existing_key(&state, preset).await.unwrap_or_default();
    }
    match discover(&state.client, preset, &key).await {
        Ok(models) => Ok(Json(json!({
            "ok": true,
            "models": models.iter().map(ModelCandidate::to_json).collect::<Vec<_>>(),
        }))
        .into_response()),
        Err(e) => Ok(Json(json!({"ok": false, "error": e})).into_response()),
    }
}

/// 该预设已建渠道里的第一把 key。
async fn existing_key(state: &Arc<AppState>, preset: &'static Preset) -> Option<String> {
    let routes: Vec<(String, String)> = preset
        .routes
        .iter()
        .map(|(proto, base)| (proto.as_str().to_string(), base.to_string()))
        .collect();
    state
        .db
        .read(move |conn| {
            for (proto, base) in &routes {
                let key: Option<String> = conn
                    .query_row(
                        "SELECT k.api_key FROM upstream_keys k JOIN channels c ON c.id = k.channel_id
                         WHERE c.protocol = ?1 AND c.base_url = ?2 ORDER BY k.id LIMIT 1",
                        params![proto, base],
                        |r| r.get(0),
                    )
                    .optional()?;
                if key.is_some() {
                    return Ok(key);
                }
            }
            Ok(None)
        })
        .await
        .ok()
        .flatten()
}

#[derive(Deserialize)]
pub struct ApplyModel {
    id: String,
    protocol: String,
    #[serde(default)]
    context: u64,
    #[serde(default)]
    vision: bool,
    #[serde(default)]
    pdf: bool,
}

#[derive(Deserialize)]
pub struct ApplyPayload {
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    key_label: String,
    #[serde(default = "default_tier")]
    tier: i32,
    models: Vec<ApplyModel>,
}
fn default_tier() -> i32 {
    1
}

/// 应用预设：按协议分组，每组找到（或建出）对应渠道，补 key、补条目。整个过程一个事务。
pub async fn apply(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(p): Json<ApplyPayload>,
) -> Result<Response, Response> {
    require_auth(&state, &headers)?;
    let preset = find(&id).ok_or_else(|| bad_request("没有这个预设"))?;
    if p.models.is_empty() {
        return Err(bad_request("至少勾选一个模型"));
    }
    let mut groups: BTreeMap<&'static str, (Protocol, &'static str, Vec<ApplyModel>)> =
        BTreeMap::new();
    for m in p.models {
        let protocol = Protocol::parse(&m.protocol)
            .ok_or_else(|| bad_request(&format!("不认识的协议：{}", m.protocol)))?;
        let base = route_base(preset, protocol).ok_or_else(|| {
            bad_request(&format!("{} 预设不支持 {} 协议", preset.name, m.protocol))
        })?;
        groups
            .entry(protocol.as_str())
            .or_insert_with(|| (protocol, base, Vec::new()))
            .2
            .push(m);
    }
    let api_key = p.api_key.trim().to_string();
    let key_label = if p.key_label.trim().is_empty() {
        "预设".to_string()
    } else {
        p.key_label.trim().to_string()
    };
    let tier = p.tier;

    // 没填 key 时，要应用的每个渠道都得已经有 key（再次应用、只补模型）；
    // 在进事务之前查清楚，给一句人话，而不是让事务回滚成一个 500。
    if api_key.is_empty() {
        let targets: Vec<(String, String)> = groups
            .values()
            .map(|(proto, base, _)| (proto.as_str().to_string(), base.to_string()))
            .collect();
        let missing = state
            .db
            .read(move |conn| {
                for (proto, base) in &targets {
                    let n: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM upstream_keys k JOIN channels c ON c.id = k.channel_id
                         WHERE c.protocol = ?1 AND c.base_url = ?2",
                        params![proto, base],
                        |r| r.get(0),
                    )?;
                    if n == 0 {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await
            .map_err(internal)?;
        if missing {
            return Err(bad_request(
                "这个预设还没有可用的 key，第一次应用要把 key 填上",
            ));
        }
    }

    let out = mutate(&state, move |conn| {
        let tx = conn.transaction()?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut channels = Vec::new();
        let (mut keys_added, mut entries_added, mut entries_existing, mut headers_added) = (0, 0, 0, 0);
        for (protocol, base, models) in groups.into_values() {
            let name = channel_name(preset, protocol);
            // 同一预设、同一协议只建一个渠道：按「协议 + 地址」认领已有渠道
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM channels WHERE protocol = ?1 AND base_url = ?2 ORDER BY id LIMIT 1",
                    params![protocol.as_str(), base],
                    |r| r.get(0),
                )
                .optional()?;
            let (channel_id, created) = match existing {
                Some(id) => {
                    // 认领已有渠道时只补它缺的客户端头，已有的（包括用户改过的）一个不动
                    let raw: String = tx.query_row(
                        "SELECT extra_headers FROM channels WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )?;
                    let mut have: BTreeMap<String, String> =
                        serde_json::from_str(&raw).unwrap_or_default();
                    let mut added = 0;
                    for (k, v) in client_headers(preset) {
                        if !have.keys().any(|h| h.eq_ignore_ascii_case(&k)) {
                            have.insert(k, v);
                            added += 1;
                        }
                    }
                    if added > 0 {
                        tx.execute(
                            "UPDATE channels SET extra_headers = ?2 WHERE id = ?1",
                            params![id, serde_json::to_string(&have).unwrap_or_else(|_| "{}".into())],
                        )?;
                        headers_added += added;
                    }
                    (id, false)
                }
                None => {
                    let headers_json = serde_json::to_string(&client_headers(preset))
                        .unwrap_or_else(|_| "{}".into());
                    let notes = match preset.client {
                        Some(c) => format!("预设：{}；请求头模仿 {c}", preset.name),
                        None => format!("预设：{}", preset.name),
                    };
                    tx.execute(
                        "INSERT INTO channels (name, protocol, base_url, extra_headers, enabled, notes, created_ms)
                         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
                        params![name, protocol.as_str(), base, headers_json, notes, now],
                    )?;
                    (tx.last_insert_rowid(), true)
                }
            };
            if !api_key.is_empty() {
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM upstream_keys WHERE channel_id = ?1 AND api_key = ?2",
                        params![channel_id, api_key],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_none() {
                    tx.execute(
                        "INSERT INTO upstream_keys (channel_id, label, api_key, enabled, created_ms)
                         VALUES (?1, ?2, ?3, 1, ?4)",
                        params![channel_id, key_label, api_key, now],
                    )?;
                    keys_added += 1;
                }
            }
            let has_key: i64 = tx.query_row(
                "SELECT COUNT(*) FROM upstream_keys WHERE channel_id = ?1",
                params![channel_id],
                |r| r.get(0),
            )?;
            if has_key == 0 {
                // 上面已经预检过；只有预检之后 key 恰好被删掉才会到这里。整个事务回滚，
                // 不留一个没 key 的空渠道。
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            for m in models {
                let dup: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM entries WHERE channel_id = ?1 AND upstream_model = ?2",
                        params![channel_id, m.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if dup.is_some() {
                    entries_existing += 1;
                    continue;
                }
                tx.execute(
                    "INSERT INTO entries (channel_id, upstream_model, tier, max_context, vision, pdf, enabled, notes, created_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, '', ?7)",
                    params![
                        channel_id,
                        m.id,
                        tier,
                        if m.context == 0 { 128_000 } else { m.context.min(u32::MAX as u64) },
                        m.vision as i64,
                        m.pdf as i64,
                        now
                    ],
                )?;
                entries_added += 1;
            }
            channels.push(json!({"id": channel_id, "name": name, "created": created}));
        }
        tx.commit()?;
        Ok(json!({
            "channels": channels,
            "keys_added": keys_added,
            "entries_added": entries_added,
            "entries_existing": entries_existing,
            "headers_added": headers_added,
        }))
    })
    .await?;
    Ok(Json(out).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_免费看价格_不支持工具的不能选() {
        let body = r#"{"data":[
          {"id":"a/free-one:free","name":"Free One","context_length":262144,
           "architecture":{"input_modalities":["text","image","file"]},
           "pricing":{"prompt":"0","completion":"0"},"supported_parameters":["tools","reasoning"],
           "expiration_date":"2026-12-31"},
          {"id":"openrouter/auto","pricing":{"prompt":"-1","completion":"-1"},"supported_parameters":["tools"]},
          {"id":"b/no-tools:free","pricing":{"prompt":"0","completion":"0"},"supported_parameters":["temperature"]}
        ]}"#;
        let m = parse_openrouter(body).unwrap();
        assert_eq!(m[0].free, Some(true));
        assert!(m[0].vision && m[0].pdf);
        assert_eq!(m[0].context, 262144);
        assert!(m[0].blocked.is_empty());
        assert_eq!(m[0].note, "2026-12-31 下线");
        // 浮动价（-1）不算免费
        assert_eq!(m[1].free, Some(false));
        assert_eq!(m[1].name, "openrouter/auto");
        assert_eq!(m[2].blocked, NO_TOOLS);
    }

    #[test]
    fn gemini_只留能对话的_去掉前缀_专用变体标出来() {
        let body = r#"{"models":[
          {"name":"models/gemini-3.8-flash","displayName":"Gemini 3.8 Flash","inputTokenLimit":1048576,
           "supportedGenerationMethods":["generateContent","countTokens"]},
          {"name":"models/text-embedding-004","supportedGenerationMethods":["embedContent"]},
          {"name":"models/gemini-2.5-flash-preview-tts","supportedGenerationMethods":["generateContent"]},
          {"name":"models/gemma-3-27b-it","supportedGenerationMethods":["generateContent"]}
        ]}"#;
        let m = parse_gemini(body).unwrap();
        assert_eq!(m.len(), 3, "embedding 模型不能 generateContent，应被滤掉");
        assert_eq!(m[0].id, "gemini-3.8-flash");
        assert_eq!(m[0].context, 1048576);
        assert_eq!(m[0].tools, Some(true));
        assert!(m[0].vision);
        assert!(m[1].note.contains("专用模型"));
        assert_eq!(m[1].tools, None);
        assert!(m[2].note.contains("不是 Gemini"));
        assert_eq!(m[0].free, None, "接口看不出免费与否，不能瞎标");
    }

    #[test]
    fn zen_按_npm_分协议_免费和_gemini_不能选_下线的不列() {
        let zen = r#"{"object":"list","data":[
          {"id":"claude-x"},{"id":"gpt-x"},{"id":"glm-x"},{"id":"big-pickle"},
          {"id":"gemini-x"},{"id":"old-x"},{"id":"unknown-x"}]}"#;
        let dev = r#"{"someone-else":{"models":{"zzz":{}}},"opencode":{"npm":"@ai-sdk/openai-compatible","models":{
          "claude-x":{"name":"Claude X","tool_call":true,"modalities":{"input":["text","image","pdf"]},
                      "limit":{"context":200000},"cost":{"input":3},"provider":{"npm":"@ai-sdk/anthropic"}},
          "gpt-x":{"tool_call":true,"cost":{"input":1.25},"provider":{"npm":"@ai-sdk/openai"}},
          "glm-x":{"tool_call":true,"cost":{"input":0.6}},
          "big-pickle":{"tool_call":true,"cost":{"input":0}},
          "gemini-x":{"tool_call":true,"cost":{"input":2},"provider":{"npm":"@ai-sdk/google"}},
          "old-x":{"status":"deprecated","cost":{"input":1}}
        }}}"#;
        let m = parse_zen(zen, dev).unwrap();
        let get = |id: &str| m.iter().find(|x| x.id == id).unwrap();
        assert!(m.iter().all(|x| x.id != "old-x"), "下线的模型不列");
        assert_eq!(get("claude-x").protocol, Protocol::Anthropic);
        assert!(get("claude-x").vision && get("claude-x").pdf);
        assert_eq!(get("claude-x").context, 200000);
        assert_eq!(get("gpt-x").protocol, Protocol::OpenAiResponses);
        assert_eq!(
            get("glm-x").protocol,
            Protocol::OpenAiChat,
            "没写 npm 的用提供商默认值"
        );
        assert!(get("glm-x").blocked.is_empty());
        assert!(
            get("big-pickle").blocked.contains("OpenCode"),
            "免费模型只能在 OpenCode 里用"
        );
        assert!(get("gemini-x").blocked.contains("Google AI Studio"));
        assert!(get("unknown-x").note.contains("models.dev"));
    }

    #[test]
    fn 每个预设的协议都有入口且头只在该有的预设上() {
        for p in PRESETS {
            assert!(!p.routes.is_empty(), "{} 没有入口", p.id);
            let h = client_headers(p);
            match p.id {
                "opencode-zen" => assert!(h.is_empty(), "Zen 不伪装 OpenCode"),
                "google-ai-studio" => assert!(h["User-Agent"].contains("{model}")),
                _ => assert!(h.contains_key("User-Agent")),
            }
        }
        // 每次建渠道生成新的安装 id
        let g = find("google-ai-studio").unwrap();
        assert_ne!(
            client_headers(g)["x-gemini-api-privileged-user-id"],
            client_headers(g)["x-gemini-api-privileged-user-id"]
        );
    }
}
