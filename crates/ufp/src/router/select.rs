//! 候选选择（D6/D9）。
//!
//! 规则：
//! 1. **能力过滤**：带图片 / 带文档 / token 估算超过条目的上下文窗口 → 该条目出局。
//!    这是「静默跳过」，不算故障，也不会影响熔断计数。
//! 2. **分层优先**：先只在最小可用层里挑；整层都被冷却/熔断才降级到下一层。
//! 3. **层内会话粘性**：会话已落在这个层里的某个条目上就直接用它；否则按
//!    `hash(会话 id, 条目, key)` 排序，不同会话摊到不同 key 上，同一会话稳定。
//! 4. 返回的是**有序候选列表**，转发层按顺序试；层内不可用的排在最后。
//!
//! 一个都不剩时的报错要能区分原因（超长 / 缺多模态能力 / 全在冷却），
//! 以便回给 Claude Code 一个说得清的错误。

use std::collections::BTreeMap;

use crate::health::{Breakers, Cooldowns};
use crate::router::session::Sessions;
use crate::store::{Candidate, Pool};

/// 为本次请求准备的预留 token（输出侧不确定，留一点余量）。
const OUTPUT_RESERVE_TOKENS: u64 = 2048;

#[derive(Debug, Clone, Copy, Default)]
pub struct RequestNeeds {
    /// 请求正文的 token 估算（`crate::tokens::estimate_request`）。
    pub est_tokens: u64,
    /// 请求里是否含图片。
    pub vision: bool,
    /// 请求里是否含文档（PDF）。
    pub pdf: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// 池里没有任何启用的条目。
    NoEntries,
    /// 有条目，但上下文窗口都放不下。
    TooLong,
    /// 有条目，但没有一个支持请求里的模态。
    NoCapability(&'static str),
    /// 能力合适的条目全在冷却或熔断中。
    AllUnavailable,
}

#[derive(Debug, Clone)]
pub struct Plan {
    /// 按优先级排列的候选。
    pub candidates: Vec<Candidate>,
    /// 是否已经降级到更低的层。
    pub degraded_from_tier: Option<i32>,
    /// 实际使用的最优先层。
    pub tier: i32,
    /// 被能力过滤掉的候选数（用于日志与后台诊断）。
    pub skipped_context: usize,
    pub skipped_vision: usize,
    pub skipped_pdf: usize,
}

pub fn plan(
    pool: &Pool,
    breakers: &Breakers,
    cooldowns: &Cooldowns,
    sessions: &Sessions,
    session_id: Option<&str>,
    needs: RequestNeeds,
) -> Result<Plan, SelectError> {
    let all = pool.candidates();
    if all.is_empty() {
        return Err(SelectError::NoEntries);
    }

    let mut eligible = Vec::with_capacity(all.len());
    let (mut skip_ctx, mut skip_vision, mut skip_pdf) = (0usize, 0usize, 0usize);
    for c in all {
        if needs.vision && !c.entry.vision {
            skip_vision += 1;
            continue;
        }
        if needs.pdf && !c.entry.pdf {
            skip_pdf += 1;
            continue;
        }
        if needs.est_tokens + OUTPUT_RESERVE_TOKENS > c.entry.max_context as u64 {
            skip_ctx += 1;
            continue;
        }
        eligible.push(c);
    }
    if eligible.is_empty() {
        if skip_pdf > 0 {
            return Err(SelectError::NoCapability("document"));
        }
        if skip_vision > 0 {
            return Err(SelectError::NoCapability("image"));
        }
        if skip_ctx > 0 {
            return Err(SelectError::TooLong);
        }
        return Err(SelectError::NoEntries);
    }

    let sticky = session_id.and_then(|sid| sessions.get(sid));
    let settings = &pool.settings;
    let lowest_tier = eligible.iter().map(|c| c.entry.tier).min().unwrap_or(1);

    // 按层分组：每层拆成「可用」与「暂不可用（冷却/熔断）」两段。
    let mut by_tier: BTreeMap<i32, (Vec<Candidate>, Vec<Candidate>)> = BTreeMap::new();
    for c in eligible {
        let cooling = cooldowns.check(c.key.id, &c.entry.upstream_model).is_some();
        let breaker_ok =
            breakers.is_available(c.channel.id, &c.entry.upstream_model, &settings.breaker);
        let entry = by_tier.entry(c.entry.tier).or_default();
        if cooling || !breaker_ok {
            entry.1.push(c);
        } else {
            entry.0.push(c);
        }
    }

    // 层内排序：粘性命中优先，其余按 hash 分摊（同一会话顺序稳定）。
    let hash_seed = session_id.unwrap_or("");
    for (available, _) in by_tier.values_mut() {
        available.sort_by_key(|c| {
            let is_sticky = sticky
                .as_ref()
                .map(|s| s.entry_id == c.entry.id && s.key_id == c.key.id)
                .unwrap_or(false);
            // 0 = 粘性命中（排最前），其余按哈希
            (
                if is_sticky { 0u8 } else { 1u8 },
                stable_hash(hash_seed, c.entry.id, c.key.id),
            )
        });
    }

    // 组顺序：先放「第一个有可用候选的层」（该层先可用、后暂不可用），
    // 再按层序补上其余层；没有任何可用候选时，各层的暂不可用候选按层序排。
    let mut ordered: Vec<Candidate> = Vec::with_capacity(eligible_count(&by_tier));
    let mut tail: Vec<Candidate> = Vec::new();
    let mut chosen_tier = None;
    for (tier, (available, unavailable)) in by_tier.iter() {
        if chosen_tier.is_none() && !available.is_empty() {
            chosen_tier = Some(*tier);
            ordered.extend(available.iter().cloned());
            ordered.extend(unavailable.iter().cloned());
        } else {
            tail.extend(available.iter().cloned());
            tail.extend(unavailable.iter().cloned());
        }
    }
    ordered.extend(tail);
    if ordered.is_empty() {
        return Err(SelectError::AllUnavailable);
    }

    let tier = chosen_tier.unwrap_or(lowest_tier);
    Ok(Plan {
        candidates: ordered,
        degraded_from_tier: if tier > lowest_tier {
            Some(lowest_tier)
        } else {
            None
        },
        tier,
        skipped_context: skip_ctx,
        skipped_vision: skip_vision,
        skipped_pdf: skip_pdf,
    })
}

fn eligible_count(by_tier: &BTreeMap<i32, (Vec<Candidate>, Vec<Candidate>)>) -> usize {
    by_tier.values().map(|(a, b)| a.len() + b.len()).sum()
}

/// 稳定哈希（FNV-1a）：同一会话在同一个层里总是落在同一个 key 上，
/// 不同会话则均匀摊开。不依赖 `RandomState`，重启后仍然一致。
fn stable_hash(seed: &str, entry_id: i64, key_id: i64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |byte: u8| {
        h ^= byte as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in seed.as_bytes() {
        mix(*b);
    }
    for b in entry_id.to_le_bytes() {
        mix(b);
    }
    for b in key_id.to_le_bytes() {
        mix(b);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChannelCfg, EntryCfg, KeyCfg, Protocol, Settings};

    fn pool_with(entries: Vec<(i32, i64, u32, bool)>, keys_per_channel: usize) -> Pool {
        // entries: (tier, channel_id, max_context, vision)
        let mut channels = std::collections::HashMap::new();
        let mut keys = std::collections::HashMap::new();
        let mut entry_cfgs = Vec::new();
        for (tier, channel_id, max_context, vision) in entries {
            channels.entry(channel_id).or_insert_with(|| ChannelCfg {
                id: channel_id,
                name: format!("ch{channel_id}"),
                protocol: Protocol::OpenAiChat,
                base_url: "http://localhost".into(),
                extra_headers: Vec::new(),
                enabled: true,
            });
            let ks = keys.entry(channel_id).or_insert_with(Vec::new);
            if ks.is_empty() {
                for i in 0..keys_per_channel {
                    ks.push(KeyCfg {
                        id: channel_id * 100 + i as i64,
                        channel_id,
                        label: format!("k{i}"),
                        api_key: "sk-test".into(),
                        enabled: true,
                    });
                }
            }
            entry_cfgs.push(EntryCfg {
                id: channel_id * 10 + tier as i64,
                channel_id,
                upstream_model: format!("model-{channel_id}"),
                tier,
                max_context,
                vision,
                pdf: false,
                enabled: true,
            });
        }
        Pool {
            channels,
            entries: entry_cfgs,
            keys,
            downstream: std::collections::HashMap::new(),
            settings: Settings::default(),
            loaded_ms: 0,
        }
    }

    #[test]
    fn 只在最优先层里选() {
        let pool = pool_with(vec![(1, 1, 200_000, true), (2, 2, 200_000, true)], 2);
        let plan = plan(
            &pool,
            &Breakers::new(),
            &Cooldowns::new(),
            &Sessions::new(),
            Some("s1"),
            RequestNeeds::default(),
        )
        .unwrap();
        assert_eq!(plan.tier, 1);
        assert_eq!(plan.candidates.len(), 4, "两层各 2 个候选都要带上做兜底");
        assert_eq!(plan.candidates[0].entry.tier, 1);
    }

    #[test]
    fn 上下文放不下就跳过并报错() {
        let pool = pool_with(vec![(1, 1, 8_000, true)], 1);
        let err = plan(
            &pool,
            &Breakers::new(),
            &Cooldowns::new(),
            &Sessions::new(),
            None,
            RequestNeeds {
                est_tokens: 100_000,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err, SelectError::TooLong);
    }

    #[test]
    fn 缺多模态能力时报_缺少能力() {
        let pool = pool_with(vec![(1, 1, 200_000, false)], 1);
        let err = plan(
            &pool,
            &Breakers::new(),
            &Cooldowns::new(),
            &Sessions::new(),
            None,
            RequestNeeds {
                vision: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err, SelectError::NoCapability("image"));
    }

    #[test]
    fn 层内会话粘性优先() {
        let pool = pool_with(vec![(1, 1, 200_000, true)], 3);
        let sessions = Sessions::new();
        // 直接塞一条粘性映射，跳过需要落库的 set()（内存 map 对 crate 内可见）。
        {
            let mut map = sessions.map.lock().unwrap();
            map.insert(
                "s1".into(),
                crate::router::session::StickyEntry {
                    channel_id: 1,
                    key_id: 102,
                    entry_id: 11,
                    upstream_model: "model-1".into(),
                    updated_ms: 0,
                    persisted_ms: 0,
                },
            );
        }
        let plan = plan(
            &pool,
            &Breakers::new(),
            &Cooldowns::new(),
            &sessions,
            Some("s1"),
            RequestNeeds::default(),
        )
        .unwrap();
        assert_eq!(plan.candidates[0].key.id, 102, "粘性命中的 key 应排最前");
    }

    #[test]
    fn 不同会话分摊到不同_key() {
        let pool = pool_with(vec![(1, 1, 200_000, true)], 4);
        let mut first_keys = std::collections::HashSet::new();
        for i in 0..8 {
            let sid = format!("session-{i}");
            let plan = plan(
                &pool,
                &Breakers::new(),
                &Cooldowns::new(),
                &Sessions::new(),
                Some(&sid),
                RequestNeeds::default(),
            )
            .unwrap();
            first_keys.insert(plan.candidates[0].key.id);
        }
        assert!(first_keys.len() > 1, "8 个会话不该全落在同一个 key 上");
    }
}
