//! TRAE Token 用量采集（2026-11 起 TraeCode / TraeWork 合并后的统一会话库）。
//!
//! 数据源：`%APPDATA%\Trae CN\ModularData\ai-agent\database.db`（SQLCipher 4，
//! 解密复用 [`bark_adapters::trae_db`]）。只读 `server_history_info` 表里
//! `source = 'llm_default'` 的行——**每次模型调用一行**：
//!
//! - `token_usage`：服务端回传的单次总 token（prompt + completion，含 reasoning），
//!   内置模型（积分）与外接模型都有；
//! - `item_token_usage`：该条消息内容的 token（不含 reasoning），缺精确字段时当输出；
//! - `extra_info`：`exact_prompt_tokens_v1` / `exact_output_tokens_v1` /
//!   `exact_total_tokens_v1`，内置模型另有 `exact_reasoning_tokens_v1`
//!   （语义标注 `provider_raw_usage_completion_includes_reasoning`）。
//!
//! 口径与 usage_sources.rs 的其他源保持一致：`input_new` 不含缓存（TRAE 本地不落
//! 缓存字段，cache_read / cache_write 恒 0）；`output` 只记**可见输出**（已扣掉
//! reasoning），`reasoning` 单独记——UI 的「总 token」= input + cache + output，
//! 与 Gemini 等源同口径（reasoning 不在合计里）。
//!
//! 成本控制：整库 130MB+，解密一次几百毫秒，因此
//! - 30s 一轮，且主库 / `-wal` 的 (size, mtime) 指纹没变就完全跳过；
//! - 只读游标之后的行（回看 5 分钟容忍乱序），主键 `history_id` 幂等去重；
//! - 明文库写临时文件、查完立即删除（AI 对话明文不留在磁盘上）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::usage_collect::Cursors;
use crate::usage_store::{UsageRecord, UsageStore};

/// 轮询间隔：token 统计不需要秒级新鲜度，避开每轮解密整库的开销
pub const POLL: Duration = Duration::from_secs(30);
/// 游标回看窗口（秒）：容忍落盘乱序与时钟回拨，重复行靠主键去重
const LOOKBACK_SECS: i64 = 300;

fn db_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| {
        PathBuf::from(d)
            .join("Trae CN")
            .join("ModularData")
            .join("ai-agent")
            .join("database.db")
    })
}

/// 明文库临时文件：查完即删（Drop 兜底，正常路径也会显式删）
struct TempDb(PathBuf);

impl Drop for TempDb {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("TRAE 明文临时库删除失败（{}）：{e}", self.0.display()),
        }
    }
}

pub fn collect(store: &UsageStore, c: &mut Cursors) {
    let Some(db) = db_path() else { return };
    // 库不存在 = 用户没装 / 没用过，静默跳过（不刷日志）
    let Some(fp) = bark_adapters::trae_db::fingerprint(&db) else { return };
    // 主库与 -wal 都没变：不解密
    if c.trae_fp == Some(fp) {
        return;
    }
    match read_records(&db, c.trae_ts) {
        Ok((recs, newest)) => {
            c.trae_fp = Some(fp);
            if c.trae_err.take().is_some() {
                tracing::info!("TRAE 用量采集已恢复");
            }
            if !recs.is_empty() {
                let added = store.insert(&recs);
                tracing::debug!("TRAE 用量新增 {added} 条（读到 {} 条）", recs.len());
            }
            c.trae_ts = newest.max(c.trae_ts);
        }
        Err(e) => {
            // 同一错误只记一次：宿主写入时偶发撕裂页很常见，下一轮自愈
            let msg = format!("{e:#}");
            if c.trae_err.as_deref() != Some(msg.as_str()) {
                tracing::warn!("TRAE 用量采集失败（下轮重试）：{msg}");
                c.trae_err = Some(msg);
            }
        }
    }
}

/// 解密整库 → 查 `llm_default` 行 → 立刻删明文。返回（记录，最新 created_at 秒）。
fn read_records(db: &Path, since: i64) -> anyhow::Result<(Vec<UsageRecord>, i64)> {
    let mut plain = bark_adapters::trae_db::decrypt_db(db)?;
    let tmp = TempDb(std::env::temp_dir().join(format!("agent-bark-trae-{}.db", std::process::id())));
    std::fs::write(&tmp.0, &plain).map_err(|e| anyhow::anyhow!("写明文临时库失败: {e}"))?;
    // 明文整库在内存里用完即清零（尽力而为；磁盘副本由 TempDb 删除）
    plain.fill(0);
    drop(plain);

    let conn = Connection::open_with_flags(&tmp.0, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("打开明文临时库失败: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT history_id, COALESCE(conversation_id, ''), created_at, token_usage, \
                    item_token_usage, COALESCE(extra_info, '') \
               FROM server_history_info \
              WHERE source = 'llm_default' AND token_usage > 0 AND created_at >= ?1 \
              ORDER BY created_at",
        )
        .map_err(|e| anyhow::anyhow!("查询 server_history_info 失败（schema 可能已变）: {e}"))?;

    let from = (since - LOOKBACK_SECS).max(0);
    let rows = stmt
        .query_map([from], |r| {
            Ok((
                r.get::<_, String>(0).unwrap_or_default(),
                r.get::<_, String>(1).unwrap_or_default(),
                r.get::<_, i64>(2).unwrap_or(0),
                r.get::<_, i64>(3).unwrap_or(0),
                r.get::<_, i64>(4).unwrap_or(0),
                r.get::<_, String>(5).unwrap_or_default(),
            ))
        })
        .map_err(|e| anyhow::anyhow!("读取 server_history_info 失败: {e}"))?;

    let mut out = Vec::new();
    let mut newest = since;
    for row in rows {
        let (history_id, conversation_id, created_at, total, item, extra) =
            row.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?;
        if history_id.is_empty() || created_at <= 0 {
            continue;
        }
        newest = newest.max(created_at);
        let Some(rec) = to_record(&history_id, &conversation_id, created_at, total, item, &extra) else {
            continue;
        };
        out.push(rec);
    }
    Ok((out, newest))
}

/// 单行 → UsageRecord。精确字段优先，缺字段时退回总量/条目量。
fn to_record(
    history_id: &str,
    conversation_id: &str,
    created_at: i64,
    total: i64,
    item: i64,
    extra_json: &str,
) -> Option<UsageRecord> {
    let extra: serde_json::Value = serde_json::from_str(extra_json).unwrap_or(serde_json::Value::Null);
    let exact_prompt = num(&extra, "exact_prompt_tokens_v1");
    let exact_output = num(&extra, "exact_output_tokens_v1");
    let exact_reasoning = num(&extra, "exact_reasoning_tokens_v1").unwrap_or(0);
    let model = ["config_name", "auto_router_model_name_v1"]
        .iter()
        .find_map(|k| extra.get(*k).and_then(|v| v.as_str()))
        .unwrap_or_default()
        .to_string();

    let (input, output, reasoning) = match (exact_prompt, exact_output) {
        (Some(p), Some(o)) => {
            // exact_output 含 reasoning（见模块头注），拆开记，避免合计重复计入
            let r = exact_reasoning.clamp(0, o);
            (p, o - r, r)
        }
        _ => {
            // 早期记录没有 exact_*：token_usage 是总量，item 是可见输出
            let o = item.clamp(0, total);
            (total - o, o, 0)
        }
    };
    if input + output + reasoning <= 0 {
        return None;
    }
    // created_at 实测是秒；防御性兼容毫秒（> 1e11 视为毫秒）
    let ts = if created_at > 100_000_000_000 { created_at } else { created_at * 1000 };
    Some(UsageRecord {
        agent: "trae-code".into(),
        record_id: history_id.to_string(),
        session_id: conversation_id.to_string(),
        ts,
        model,
        input_new: input.max(0),
        cache_read: 0,
        cache_write: 0,
        output: output.max(0),
        reasoning: reasoning.max(0),
        credits: None,
    })
}

/// exact_* 字段实测是字符串（"24422"），也容忍数字
fn num(v: &serde_json::Value, key: &str) -> Option<i64> {
    let x = v.get(key)?;
    x.as_i64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fields_split_reasoning_out_of_output() {
        let extra = r#"{"config_name":"Doubao-Seed-2.1-Pro","exact_output_tokens_v1":"3710",
                        "exact_prompt_tokens_v1":"225219","exact_reasoning_tokens_v1":"1986",
                        "exact_total_tokens_v1":"228929"}"#;
        let r = to_record("h1", "c1", 1_795_705_000, 228_929, 1_637, extra).unwrap();
        assert_eq!(r.agent, "trae-code");
        assert_eq!(r.record_id, "h1");
        assert_eq!(r.session_id, "c1");
        assert_eq!(r.model, "Doubao-Seed-2.1-Pro");
        assert_eq!(r.input_new, 225_219);
        assert_eq!(r.reasoning, 1_986);
        assert_eq!(r.output, 3_710 - 1_986, "output 必须是可见输出，reasoning 另记");
        assert_eq!(r.ts, 1_795_705_000_000);
    }

    #[test]
    fn falls_back_to_total_and_item_without_exact_fields() {
        let extra = r#"{"config_name":"glm-5.2"}"#;
        let r = to_record("h2", "c2", 1_700_000_000, 20_168, 197, extra).unwrap();
        assert_eq!(r.input_new, 20_168 - 197);
        assert_eq!(r.output, 197);
        assert_eq!(r.reasoning, 0);
        assert_eq!(r.model, "glm-5.2");
    }

    #[test]
    fn skips_zero_usage_and_bad_json() {
        assert!(to_record("h3", "", 1_700_000_000, 0, 0, "not json").is_none());
        let r = to_record("h4", "", 1_700_000_000, 10, 10, "not json");
        assert!(r.is_some(), "坏 JSON 不该丢行，退回总量口径");
    }

    /// 端到端冒烟：解密**真实**的 TRAE 会话库并查询（验证固定密钥仍有效、
    /// `server_history_info` schema 未变、rusqlite 能打开 reserve=80 的明文库）。
    /// 默认跳过，本机装了 TRAE 时手动跑：
    /// `cargo test -p agent-bark-app --lib live_db_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_db_smoke() {
        let Some(db) = db_path() else { return };
        if !db.exists() {
            eprintln!("跳过：本机没有 TRAE 会话库（{}）", db.display());
            return;
        }
        let (recs, newest) = read_records(&db, 0).expect("解密 + 查询应成功");
        eprintln!("records={} newest={newest}", recs.len());
        assert!(!recs.is_empty(), "应能读到 llm_default 行");
        assert!(recs.iter().all(|r| r.agent == "trae-code"));

        // 入库 + 聚合链路（主键幂等：第二次插入应为 0）
        let dir = std::env::temp_dir().join(format!("bark-trae-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = UsageStore::new(dir.join("usage.sqlite3"));
        store.set_enabled(true);
        assert_eq!(store.insert(&recs), recs.len(), "首轮应全部入库");
        assert_eq!(store.insert(&recs), 0, "重读必须幂等");
        let agents = store.by_agent("2000-01-01", "2100-01-01");
        let trae = agents.iter().find(|a| a.agent == "trae-code").expect("聚合里应有 trae-code");
        assert_eq!(trae.requests as usize, recs.len());
        eprintln!("aggregate requests={} tokens={}", trae.requests, trae.input_new + trae.output);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
