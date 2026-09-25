//! 存储层：SQLite 表结构、读写连接、以及给热路径用的配置快照。

pub mod db;
pub mod maintenance;
pub mod schema;
pub mod settings;

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

pub use db::{AttemptLogRow, Db, DbResult, RequestLogRow, Write};
// ModelEcho 与 Settings 一并从 settings 模块透出，供 api / admin 使用。
pub use settings::{ModelEcho, Settings};

/// 上游协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// OpenAI Chat Completions（含绝大多数兼容站）。
    OpenAiChat,
    /// OpenAI Responses API。
    OpenAiResponses,
    /// Gemini 原生 generateContent / streamGenerateContent。
    Gemini,
    /// Anthropic Messages 原生（直接透传）。
    Anthropic,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "openai_chat",
            Protocol::OpenAiResponses => "openai_responses",
            Protocol::Gemini => "gemini",
            Protocol::Anthropic => "anthropic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai_chat" | "openai" | "chat" => Some(Protocol::OpenAiChat),
            "openai_responses" | "responses" => Some(Protocol::OpenAiResponses),
            "gemini" | "gemini_native" => Some(Protocol::Gemini),
            "anthropic" | "claude" => Some(Protocol::Anthropic),
            _ => None,
        }
    }
}

/// 渠道的请求按哪个客户端的样子发（请求头之外的那部分：请求体字段、会话级 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientProfile {
    /// 不模仿：按转换器的产出原样发。
    #[default]
    None,
    /// 照 OpenCode 访问 Zen 的样子发（`upstream/client_profile.rs`）。
    OpenCode,
}

impl ClientProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientProfile::None => "",
            ClientProfile::OpenCode => "opencode",
        }
    }

    /// 不认识的值当作不模仿（旧库、手改的库都不至于让渠道加载失败）。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "opencode" => ClientProfile::OpenCode,
            _ => ClientProfile::None,
        }
    }
}

/// 渠道：一个上游服务（协议 + base_url + 附加头），下面挂若干 key 与若干模型条目。
#[derive(Debug, Clone)]
pub struct ChannelCfg {
    pub id: i64,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub extra_headers: Vec<(String, String)>,
    pub enabled: bool,
    pub client_profile: ClientProfile,
    /// 强制把思考开到最大（预设渠道默认开）。
    pub max_thinking: bool,
}

/// 上游 key。冷却按 (key, model) 记，熔断按 (channel, model) 记。
#[derive(Debug, Clone)]
pub struct KeyCfg {
    pub id: i64,
    pub channel_id: i64,
    pub label: String,
    pub api_key: String,
    pub enabled: bool,
}

/// 条目：渠道下的某个上游模型，带层级与能力字段。
#[derive(Debug, Clone)]
pub struct EntryCfg {
    pub id: i64,
    pub channel_id: i64,
    pub upstream_model: String,
    /// 数字越小越优先；只有整层不可用才降级到下一层。
    pub tier: i32,
    pub max_context: u32,
    pub vision: bool,
    pub pdf: bool,
    pub enabled: bool,
    /// 阶梯试出来的可用思考参数形式（'' = 还没试过）。
    pub thinking_mode: String,
}

/// 下游 key（只存哈希）。
#[derive(Debug, Clone)]
pub struct DownstreamKeyCfg {
    pub id: i64,
    pub name: String,
    pub key_hash: String,
    pub key_prefix: String,
    pub enabled: bool,
}

/// 一个可选的上游目标：条目 × 该渠道的一个启用 key。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub channel: ChannelCfg,
    pub key: KeyCfg,
    pub entry: EntryCfg,
}

/// 配置快照。整份替换，热路径只读它，不查库。
#[derive(Debug, Clone)]
pub struct Pool {
    pub channels: HashMap<i64, ChannelCfg>,
    /// 只含启用的条目，按 (tier, channel_id, id) 排好序。
    pub entries: Vec<EntryCfg>,
    /// channel_id → 启用的 key 列表。
    pub keys: HashMap<i64, Vec<KeyCfg>>,
    /// key_hash(hex) → 下游 key。
    pub downstream: HashMap<String, DownstreamKeyCfg>,
    pub settings: Settings,
    pub loaded_ms: i64,
}

impl Pool {
    pub fn load(conn: &Connection) -> DbResult<Pool> {
        let mut channels = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, name, protocol, base_url, extra_headers, enabled, client_profile, max_thinking
                 FROM channels",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })?;
            for row in rows {
                let (id, name, protocol, base_url, extra_headers, enabled, profile, max_thinking) =
                    row?;
                let Some(protocol) = Protocol::parse(&protocol) else {
                    tracing::warn!(channel = %name, protocol, "未知的上游协议，已跳过该渠道");
                    continue;
                };
                let extra_headers = serde_json::from_str::<HashMap<String, String>>(&extra_headers)
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>();
                channels.insert(
                    id,
                    ChannelCfg {
                        id,
                        name,
                        protocol,
                        base_url,
                        extra_headers,
                        enabled: enabled != 0,
                        client_profile: ClientProfile::parse(&profile),
                        max_thinking: max_thinking != 0,
                    },
                );
            }
        }

        let mut keys: HashMap<i64, Vec<KeyCfg>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, label, api_key, enabled FROM upstream_keys WHERE enabled = 1 ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(KeyCfg {
                    id: r.get(0)?,
                    channel_id: r.get(1)?,
                    label: r.get(2)?,
                    api_key: r.get(3)?,
                    enabled: r.get::<_, i64>(4)? != 0,
                })
            })?;
            for row in rows {
                let k = row?;
                keys.entry(k.channel_id).or_default().push(k);
            }
        }

        let mut entries = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id, channel_id, upstream_model, tier, max_context, vision, pdf, enabled, thinking_mode
                 FROM entries WHERE enabled = 1
                 ORDER BY tier, channel_id, id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(EntryCfg {
                    id: r.get(0)?,
                    channel_id: r.get(1)?,
                    upstream_model: r.get(2)?,
                    tier: r.get(3)?,
                    max_context: r.get::<_, i64>(4)?.max(0) as u32,
                    vision: r.get::<_, i64>(5)? != 0,
                    pdf: r.get::<_, i64>(6)? != 0,
                    enabled: r.get::<_, i64>(7)? != 0,
                    thinking_mode: r.get::<_, String>(8).unwrap_or_default(),
                })
            })?;
            for row in rows {
                let e = row?;
                // 渠道被禁用或缺 key 的条目直接不进池。
                let Some(ch) = channels.get(&e.channel_id) else {
                    continue;
                };
                if !ch.enabled || !keys.contains_key(&e.channel_id) {
                    continue;
                }
                entries.push(e);
            }
        }

        let mut downstream = HashMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, name, key_hash, key_prefix, enabled FROM downstream_keys")?;
            let rows = stmt.query_map([], |r| {
                Ok(DownstreamKeyCfg {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    key_hash: r.get(2)?,
                    key_prefix: r.get(3)?,
                    enabled: r.get::<_, i64>(4)? != 0,
                })
            })?;
            for row in rows {
                let k = row?;
                downstream.insert(k.key_hash.clone(), k);
            }
        }

        let settings = load_settings(conn)?;

        Ok(Pool {
            channels,
            entries,
            keys,
            downstream,
            settings,
            loaded_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    /// 展开成「条目 × 该渠道启用的 key」的候选列表，顺序即优先级
    /// （先按层级，再按条目定义顺序）。
    pub fn candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::with_capacity(self.entries.len() * 2);
        for entry in &self.entries {
            let Some(channel) = self.channels.get(&entry.channel_id) else {
                continue;
            };
            let Some(keys) = self.keys.get(&entry.channel_id) else {
                continue;
            };
            for key in keys {
                out.push(Candidate {
                    channel: channel.clone(),
                    key: key.clone(),
                    entry: entry.clone(),
                });
            }
        }
        out
    }

    /// 池里出现的所有层级，从小到大。
    pub fn tiers(&self) -> Vec<i32> {
        let mut tiers: Vec<i32> = self.entries.iter().map(|e| e.tier).collect();
        tiers.sort_unstable();
        tiers.dedup();
        tiers
    }
}

/// 从 `settings` 表读运行期设置（单份 JSON，缺省时用默认值）。
pub fn load_settings(conn: &Connection) -> DbResult<Settings> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'runtime'",
            [],
            |r| r.get(0),
        )
        .ok();
    match raw {
        Some(raw) => Ok(serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "运行期设置解析失败，使用默认值");
            Settings::default()
        })),
        None => Ok(Settings::default()),
    }
}

/// 把运行期设置写回 `settings` 表。
pub fn save_settings(conn: &Connection, s: &Settings) -> DbResult<()> {
    let json = serde_json::to_string(s).expect("设置序列化不应失败");
    conn.execute(
        "INSERT INTO settings (key, value) VALUES ('runtime', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        [json],
    )?;
    Ok(())
}

/// 配置快照的持有者，整份原子替换。
pub struct Snapshot {
    pool: ArcSwap<Pool>,
}

impl Snapshot {
    pub fn new(pool: Pool) -> Arc<Self> {
        Arc::new(Self {
            pool: ArcSwap::from_pointee(pool),
        })
    }

    /// 热路径读取：一次原子取指针，无锁。
    pub fn load(&self) -> arc_swap::Guard<Arc<Pool>> {
        self.pool.load()
    }

    /// 从数据库重新加载整份配置（后台改动后调用）。
    pub async fn reload(&self, db: &Arc<Db>) -> DbResult<()> {
        let pool = db.read(Pool::load).await?;
        tracing::info!(
            channels = pool.channels.len(),
            entries = pool.entries.len(),
            downstream_keys = pool.downstream.len(),
            "配置快照已重载"
        );
        self.pool.store(Arc::new(pool));
        Ok(())
    }
}
