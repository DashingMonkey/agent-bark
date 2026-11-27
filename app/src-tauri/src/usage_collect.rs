//! Token 用量采集器：**独立通道**，不进通知事件管道（理由见 usage_store.rs 头注）。
//!
//! 节奏：文件尾读 / db 增量 5s 一轮；DSH 解压贵，30s 一轮；TRAE 要解密整库，
//! 30s 一轮且主库/-wal 指纹没变就跳过；zstd 文件（DSH /
//! Codex 归档 rollout）都按 mtime/size 门控，没变不解压；5min 兜底把两个 db
//! 游标归零全量重扫（主键幂等，重扫不重复累计），补「状态晚收敛 / 时钟回拨」
//! 漏掉的行。开关关闭时 sync() 停线程，全部节奏停摆。
//!
//! 归一口径：input_new 一律不含缓存读取。ZCode 的 input_tokens 含缓存要拆；
//! Qoder custom transcript 的 input_tokens 也含缓存（与旧 token_info 同口径）；
//! DSH / Kimi 的 input 本就不含缓存；WorkBuddy 两种形态（camel / openai）都兼容。

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::Connection;

use crate::state::{lock, AppState};
use crate::usage_store::UsageRecord;

const TICK: Duration = Duration::from_millis(200);
const POLL_FAST: Duration = Duration::from_secs(5);
const POLL_DSH: Duration = Duration::from_secs(30);
const SWEEP: Duration = Duration::from_secs(300);
/// ZCode 游标回看窗口：容忍乱序落盘，重叠部分靠主键去重
const ZCODE_LOOKBACK_MS: i64 = 60_000;

/// 有本地 token 采集器、且不区分模型的 agent。
/// 新增采集器时同步这里；tests::every_agent_kind_is_classified_once 会强制
/// 它与 AgentKind::ALL 对齐。仅测试引用，生产构建不编译。
#[cfg(test)]
pub const USAGE_SOURCE_AGENTS: &[&str] = &[
    "antigravity",
    "claude-code",
    "codebuddy",
    "codex",
    "copilot-cli",
    "droid",
    "dsh",
    "gemini-cli",
    "grok",
    "kimi-code",
    "kimi-work",
    "opencode",
    "qwen-code",
    "trae-code",
    "workbuddy",
    "zcode",
];

/// 已安装但没有本地用量采集器的 agent：用量页脚注用。
/// 新增采集器时同步移除这里；与 usage_sources.rs 头注的「不猜口径」清单一致。
pub const NO_USAGE_SOURCE_AGENTS: &[&str] = &[
    "cursor",
    "doubao-work",
];

/// 有采集器但只有条件源的 agent：Qoder 仅 custom endpoint 模型写 usage。
pub const CONDITIONAL_USAGE_AGENTS: &[&str] = &["qoder"];

/// 开关同步：停旧线程、按当前开关起新线程。启动与 save_config 都调它。
pub fn sync(state: &Arc<AppState>) {
    let enabled = state.usage.is_enabled();
    let mut guard = lock(&state.usage_thread);
    if let Some((stop, handle)) = guard.take() {
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
    }
    if enabled {
        let stop = Arc::new(AtomicBool::new(false));
        let s = state.clone();
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || run(s, stop2));
        *guard = Some((stop, handle));
    }
}

#[derive(Default)]
pub(crate) struct Cursors {
    /// 文件字节游标：只推进到最后一个完整行，半行留到下轮
    pub(crate) files: HashMap<PathBuf, u64>,
    /// zstd 文件 (mtime 秒, size)：没变就不解压（DSH 会话、Codex 归档 rollout）
    zst_meta: HashMap<PathBuf, (u64, u64)>,
    zcode_ts: i64,
    qoder_ts: i64,
    /// OpenCode 各 db 的 time_created 游标
    pub(crate) oc_ts: HashMap<PathBuf, i64>,
    /// CodeBuddy 扩展日志：每文件的 `Model prepared:` 锚点（agent_id → (model, 毫秒)）
    pub(crate) cb_prepared: HashMap<PathBuf, HashMap<String, (String, Option<i64>)>>,
    /// TRAE 会话库：`server_history_info.created_at` 游标（秒）
    pub(crate) trae_ts: i64,
    /// TRAE 会话库：主库 + `-wal` 的 (size, mtime) 指纹，没变就不解密
    pub(crate) trae_fp: Option<bark_adapters::trae_db::DbFingerprint>,
    /// TRAE 采集最近一次错误（同一错误只记一次日志）
    pub(crate) trae_err: Option<String>,
}

fn run(state: Arc<AppState>, stop: Arc<AtomicBool>) {
    let mut c = Cursors::default();
    let mut next_fast = Instant::now();
    let mut next_dsh = Instant::now();
    let mut next_trae = Instant::now();
    let mut next_sweep = Instant::now() + SWEEP;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= next_fast {
            poll_fast(&state.usage, &mut c);
            next_fast = now + POLL_FAST;
        }
        if now >= next_dsh {
            poll_dsh(&state.usage, &mut c);
            next_dsh = now + POLL_DSH;
        }
        if now >= next_trae {
            crate::usage_trae::collect(&state.usage, &mut c);
            next_trae = now + crate::usage_trae::POLL;
        }
        if now >= next_sweep {
            c.zcode_ts = 0;
            c.qoder_ts = 0;
            next_sweep = now + SWEEP;
        }
        std::thread::sleep(TICK);
    }
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn poll_fast(store: &crate::usage_store::UsageStore, c: &mut Cursors) {
    let Some(home) = home() else { return };
    collect_zcode(store, c, &home);
    collect_qoder(store, c);
    collect_qoder_transcripts(store, c, &home);
    collect_codex(store, c, &home);
    for path in workbuddy_files(&home) {
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines.iter().filter_map(|l| parse_workbuddy_line(l)).collect();
        store.insert(&recs);
    }
    for path in kimi_files(&home) {
        let session = session_id_from_path(&path);
        let agent = if path.components().any(|c| c.as_os_str() == ".kimi-work") { "kimi-work" } else { "kimi-code" };
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines
            .iter()
            .filter_map(|l| parse_kimi_line(l, &session, agent))
            .collect();
        store.insert(&recs);
    }
    crate::usage_sources::poll(store, c, &home);
}

fn poll_dsh(store: &crate::usage_store::UsageStore, c: &mut Cursors) {
    let Some(home) = home() else { return };
    for path in dsh_files(&home) {
        let Some(key) = zst_meta_key(&path) else { continue };
        if c.zst_meta.get(&path) == Some(&key) {
            continue;
        }
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let session = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(text) = read_zst(&path) else { continue };
        let recs: Vec<UsageRecord> = text
            .lines()
            .filter_map(|l| parse_dsh_line(l, &stem, &session))
            .collect();
        store.insert(&recs);
        c.zst_meta.insert(path, key);
    }
}

/// zstd 文件的变更判据 (mtime 秒, size)；取不到元数据就当没变，跳过
fn zst_meta_key(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((mtime, meta.len()))
}

fn read_zst(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut text = String::new();
    zstd::stream::read::Decoder::new(file)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Codex 活跃会话是明文 rollout，尾读增量；新版桌面端会把不活跃会话压缩成
/// `rollout-*.jsonl.zst`（内容不变），按 mtime/size 门控解压全量重扫，主键去重。
fn collect_codex(store: &crate::usage_store::UsageStore, c: &mut Cursors, home: &Path) {
    for path in codex_files(home) {
        if path.extension().map(|e| e == "zst").unwrap_or(false) {
            let Some(key) = zst_meta_key(&path) else { continue };
            if c.zst_meta.get(&path) == Some(&key) {
                continue;
            }
            let Some(text) = read_zst(&path) else { continue };
            let recs: Vec<UsageRecord> = text.lines().filter_map(parse_codex_line).collect();
            store.insert(&recs);
            c.zst_meta.insert(path, key);
        } else {
            let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
            let recs: Vec<UsageRecord> = lines.iter().filter_map(|l| parse_codex_line(l)).collect();
            store.insert(&recs);
        }
    }
}

// ---- 文件发现 ----

fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, pred: &dyn Fn(&Path) -> bool) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, depth - 1, out, pred);
        } else if pred(&p) {
            out.push(p);
        }
    }
}

fn codex_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(
        &home.join(".codex").join("sessions"),
        6,
        &mut out,
        &|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("rollout-") && (n.ends_with(".jsonl") || n.ends_with(".jsonl.zst"))
                })
                .unwrap_or(false)
        },
    );
    out
}

fn workbuddy_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(
        &home.join(".workbuddy").join("projects"),
        2,
        &mut out,
        &|p| p.extension().map(|e| e == "jsonl").unwrap_or(false),
    );
    out
}

fn kimi_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in [".kimi-code", ".kimi-work"] {
        walk(
            &home.join(root).join("sessions"),
            6,
            &mut out,
            &|p| p.file_name().map(|n| n == "wire.jsonl").unwrap_or(false),
        );
    }
    out
}

fn dsh_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(
        &home.join(".dsh").join("sessions"),
        3,
        &mut out,
        &|p| {
            p.file_name()
                .map(|n| {
                    let n = n.to_string_lossy();
                    n.starts_with("session.v") && n.ends_with(".jsonl.zstd")
                })
                .unwrap_or(false)
        },
    );
    out
}

/// wire.jsonl 路径里的 session_<id> 目录名即会话 id
fn session_id_from_path(path: &Path) -> String {
    path.components()
        .find_map(|c| {
            let s = c.as_os_str().to_string_lossy();
            s.starts_with("session_").then(|| s.into_owned())
        })
        .unwrap_or_default()
}

/// 只读新增的**完整行**；文件被截断/轮转（size < 游标）时从头重扫，主键去重兜底。
pub(crate) fn tail_lines(path: &Path, cursor: &mut u64) -> Vec<String> {
    let Ok(meta) = std::fs::metadata(path) else { return Vec::new() };
    let size = meta.len();
    if size < *cursor {
        *cursor = 0;
    }
    if size == *cursor {
        return Vec::new();
    }
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    if f.seek(SeekFrom::Start(*cursor)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let Some(pos) = text.rfind('\n') else { return Vec::new() };
    let lines: Vec<String> = text[..pos].lines().map(|l| l.to_string()).collect();
    *cursor += (pos + 1) as u64;
    lines
}

// ---- db 源 ----

pub(crate) fn ro_conn(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(Duration::from_millis(50)).ok()?;
    Some(conn)
}

fn collect_zcode(store: &crate::usage_store::UsageStore, c: &mut Cursors, home: &Path) {
    let path = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    let Some(conn) = ro_conn(&path) else { return };
    let from = (c.zcode_ts - ZCODE_LOOKBACK_MS).max(0);
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, session_id, started_at, model_id, input_tokens, cache_read_input_tokens,
                cache_creation_input_tokens, output_tokens, reasoning_tokens
         FROM model_usage WHERE status = 'completed' AND started_at >= ?1",
    ) else {
        return;
    };
    let rows = stmt.query_map([from], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    });
    let Ok(rows) = rows else { return };
    let mut recs = Vec::new();
    for (id, session, ts, model, input, cache, cache_w, out, reason) in rows.flatten() {
        c.zcode_ts = c.zcode_ts.max(ts);
        recs.push(UsageRecord {
            agent: "zcode".into(),
            record_id: id,
            session_id: session,
            ts,
            model,
            // ZCode 口径：input_tokens 含缓存读取，拆出才是新输入
            input_new: (input - cache).max(0),
            cache_read: cache,
            cache_write: cache_w,
            output: out,
            reasoning: reason,
            credits: None,
        });
    }
    store.insert(&recs);
}

fn qoder_dbs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(cfg) = dirs::config_dir() {
        for id in ["com.qodercn.app.stable", "com.qoder.app.stable"] {
            out.push(cfg.join(id).join("main.sqlite"));
        }
    }
    if let Some(h) = home() {
        // 旧版国际版布局（agent-trail 同款数据源）：chat_message.token_info
        out.push(h.join("Library/Application Support/Qoder/SharedClientCache/cache/db/local.db"));
    }
    out
}

fn collect_qoder(store: &crate::usage_store::UsageStore, c: &mut Cursors) {
    for path in qoder_dbs() {
        let Some(conn) = ro_conn(&path) else { continue };
        let from = c.qoder_ts;
        // 新 schema（CN / 现国际版）只写 credits 不写 token；credits 采集已下线：
        // BYOK 没有 credits，平台模型的 credits 也不再展示。token 改从 transcript 读。
        // 旧 schema：token_info 里有真 token（prompt/completion/cached）。
        // 注意：与新 transcript 源的 record_id 空间不同；极老版本同时存在两份
        // 数据时理论上会双记（本机 .qoder/projects 为空，暂无实测）。
        let old = conn.prepare(
            "SELECT id, session_id, gmt_create, token_info FROM chat_message
             WHERE token_info IS NOT NULL AND gmt_create >= ?1",
        );
        if let Ok(mut stmt) = old {
            if let Ok(rows) = stmt.query_map([from], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            }) {
                let mut recs = Vec::new();
                for (id, session, ts, info) in rows.flatten() {
                    c.qoder_ts = c.qoder_ts.max(ts);
                    if let Some(mut rec) = parse_qoder_token_info(&info, &id, &session, ts) {
                        rec.record_id = format!("{id}:t");
                        recs.push(rec);
                    }
                }
                store.insert(&recs);
            }
        }
    }
}

/// Qoder 的 token 只在 custom endpoint 模型的 transcript 里出现：
/// 预设 BYOK / 平台模型不写 usage。international 与 CN 两个配置目录都扫，
/// 包含主会话目录下的 `subagents/*.jsonl`（子代理同样烧 token）。
fn qoder_transcript_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in [".qoder", ".qoder-cn"] {
        walk(
            &home.join(root).join("projects"),
            6,
            &mut out,
            &|p| p.extension().map(|e| e == "jsonl").unwrap_or(false),
        );
    }
    out
}

fn collect_qoder_transcripts(store: &crate::usage_store::UsageStore, c: &mut Cursors, home: &Path) {
    for path in qoder_transcript_files(home) {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        if lines.is_empty() {
            continue;
        }
        let recs: Vec<UsageRecord> = lines
            .iter()
            .filter_map(|l| parse_qoder_transcript_line(l, &stem))
            .collect();
        store.insert(&recs);
    }
}

fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// Qoder transcript 的 assistant 记录里，usage 只挂在一次响应拆出的某一条
/// （thinking / tool_use / text 共享同一个 `message.id`），所以 record_id 用
/// session + message.id，`INSERT OR IGNORE` 自然去掉同一响应的重复记录。
///
/// 口径（2026-11-21 在 DeepSeek custom endpoint 实测）：`input_tokens` 含
/// `cache_read_input_tokens`，`input_new` 要减掉。Anthropic-compatible 端点
/// 是否也含缓存尚未实测，先按已验证的 OpenAI/DeepSeek 口径处理。
///
/// 实测 `usage.iterations` 恒为空、平铺字段即全量；若将来 Qoder 把用量挪进
/// iterations，需要像 parse_claude_line 一样汇总（顶层数值可能不全）。
pub fn parse_qoder_transcript_line(line: &str, stem: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    if rec.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let msg = rec.get("message")?;
    let usage = msg.get("usage")?;
    let n = |k: &str| usage.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = n("input_tokens");
    let cache = n("cache_read_input_tokens");
    let cache_w = n("cache_creation_input_tokens");
    let output = n("output_tokens");
    if input == 0 && cache == 0 && cache_w == 0 && output == 0 {
        return None;
    }
    let session = {
        let sid = str_field(&rec, "sessionId");
        if sid.is_empty() { stem.to_string() } else { sid.to_string() }
    };
    let message_id = {
        let id = str_field(msg, "id");
        if id.is_empty() { str_field(usage, "request_id") } else { id }
    };
    if message_id.is_empty() {
        return None;
    }
    Some(UsageRecord {
        agent: "qoder".into(),
        record_id: format!("{session}:{message_id}"),
        session_id: session,
        ts: ms_from_iso(str_field(&rec, "timestamp"))?,
        model: str_field(msg, "model").to_string(),
        input_new: (input - cache).max(0),
        cache_read: cache,
        cache_write: cache_w,
        output,
        reasoning: 0,
        credits: None,
    })
}

// ---- 行 parser（纯函数，单测覆盖口径）----

fn ms_from_iso(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

pub fn parse_codex_line(line: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    if rec.get("type")?.as_str()? != "token_usage_record" {
        return None;
    }
    let p = rec.get("payload")?;
    let u = p.get("usage")?;
    let n = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let input = n("input_tokens");
    let cached = n("cached_input_tokens");
    let ts = ms_from_iso(rec.get("timestamp")?.as_str()?)?;
    let record_id = p
        .get("response_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            format!(
                "{}:{}",
                p.get("thread_id").and_then(|v| v.as_str()).unwrap_or(""),
                rec.get("ordinal").and_then(|v| v.as_i64()).unwrap_or(0)
            )
        });
    Some(UsageRecord {
        agent: "codex".into(),
        record_id,
        session_id: p
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        ts,
        model: String::new(),
        input_new: (input - cached).max(0),
        cache_read: cached,
        cache_write: n("cache_write_input_tokens"),
        output: n("output_tokens"),
        reasoning: n("reasoning_output_tokens"),
        credits: None,
    })
}

pub fn parse_workbuddy_line(line: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    let pd = rec.get("providerData")?;
    let ts = rec.get("timestamp")?.as_i64()?;
    let id = rec.get("id")?.as_str()?.to_string();
    let session = rec.get("sessionId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let model = pd.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let (input, out, cached) = if let Some(u) = pd.get("usage") {
        let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let cached = u
            .get("inputTokensDetails")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.get("cached_tokens").and_then(|v| v.as_i64()))
                    .sum()
            })
            .unwrap_or(0);
        (g("inputTokens"), g("outputTokens"), cached)
    } else if let Some(u) = pd.get("rawUsage") {
        let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        (g("prompt_tokens"), g("completion_tokens"), g("prompt_cache_hit_tokens"))
    } else {
        return None;
    };
    if input == 0 && out == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: "workbuddy".into(),
        record_id: id,
        session_id: session,
        ts,
        model,
        input_new: (input - cached).max(0),
        cache_read: cached,
        cache_write: 0,
        output: out,
        reasoning: 0,
        credits: None,
    })
}

pub fn parse_kimi_line(line: &str, session: &str, agent: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    if rec.get("type")?.as_str()? != "usage.record" {
        return None;
    }
    let u = rec.get("usage")?;
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let ts = rec.get("time")?.as_i64()?;
    let model = rec.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let agent_id = rec.get("agentId").and_then(|v| v.as_str()).unwrap_or("main");
    Some(UsageRecord {
        agent: agent.to_string(),
        record_id: format!("{agent_id}-{ts}-{model}"),
        session_id: session.to_string(),
        ts,
        model,
        input_new: g("inputOther"),
        cache_read: g("inputCacheRead"),
        cache_write: g("inputCacheCreation"),
        output: g("output"),
        reasoning: 0,
        credits: None,
    })
}

pub fn parse_dsh_line(line: &str, file_stem: &str, session: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    if rec.get("type")?.as_str()? != "assistant/message" {
        return None;
    }
    let data = rec.get("data")?;
    let u = data.get("usage")?;
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let (input, out, cache) = (g("inputTokens"), g("outputTokens"), g("cacheReadTokens"));
    if input == 0 && out == 0 {
        return None;
    }
    let seq = rec.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
    Some(UsageRecord {
        agent: "dsh".into(),
        // stem 对所有文件都叫 session.vN.jsonl，必须带 session 才跨会话唯一；
        // 同会话 v3/v4 并存时 stem 再区分一层
        record_id: format!("{session}:{file_stem}:{seq}"),
        session_id: session.to_string(),
        ts: rec.get("time")?.as_i64()?,
        model: data.get("model").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        // DSH 口径：inputTokens 本就不含缓存，与 ZCode 相反
        input_new: input,
        cache_read: cache,
        cache_write: g("cacheWriteTokens"),
        output: out,
        reasoning: 0,
        credits: None,
    })
}

pub fn parse_qoder_token_info(info: &str, id: &str, session: &str, ts: i64) -> Option<UsageRecord> {
    let u: serde_json::Value = serde_json::from_str(info).ok()?;
    let g = |k: &str| u.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let (input, out, cached) = (g("prompt_tokens"), g("completion_tokens"), g("cached_tokens"));
    if input == 0 && out == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: "qoder".into(),
        record_id: id.to_string(),
        session_id: session.to_string(),
        ts,
        model: String::new(),
        input_new: (input - cached).max(0),
        cache_read: cached,
        cache_write: 0,
        output: out,
        reasoning: 0,
        credits: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_request_level_not_thread_cumulative() {
        let line = r#"{"timestamp":"2026-11-05T06:54:22.914Z","ordinal":16,"type":"token_usage_record","payload":{"thread_id":"t1","turn_id":"u1","session_id":"s1","response_id":"resp_1","usage":{"input_tokens":31782,"cached_input_tokens":30080,"cache_write_input_tokens":0,"output_tokens":187,"reasoning_output_tokens":84,"total_tokens":31969},"thread_token_usage":{"input_tokens":999999}}}"#;
        let r = parse_codex_line(line).unwrap();
        assert_eq!(r.record_id, "resp_1");
        assert_eq!(r.input_new, 1702);
        assert_eq!(r.cache_read, 30080);
        assert_eq!(r.output, 187);
        assert_eq!(r.reasoning, 84);
        assert_eq!(r.ts, 1793861662914);
        assert!(parse_codex_line(r#"{"type":"event_msg"}"#).is_none());
    }

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bark-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn codex_discovery_covers_plain_and_zst() {
        let home = temp_home("codex-disc");
        let day = home.join(".codex").join("sessions").join("2026").join("11").join("19");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-a.jsonl"), "").unwrap();
        std::fs::write(day.join("rollout-b.jsonl.zst"), "").unwrap();
        std::fs::write(day.join("rollout-c.jsonl.tmp"), "").unwrap();
        std::fs::write(day.join("other.jsonl"), "").unwrap();
        let mut found = codex_files(&home);
        found.sort();
        assert_eq!(found, vec![day.join("rollout-a.jsonl"), day.join("rollout-b.jsonl.zst")]);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 新版桌面端把不活跃 rollout 压成 .jsonl.zst：要采到、门控生效、重扫不重复累计
    #[test]
    fn codex_zst_archive_collects_idempotently() {
        let home = temp_home("codex-zst");
        let day = home.join(".codex").join("sessions").join("2026").join("11").join("12");
        std::fs::create_dir_all(&day).unwrap();
        let mk = |resp: &str| {
            format!(
                r#"{{"timestamp":"2026-11-05T06:54:22.914Z","ordinal":16,"type":"token_usage_record","payload":{{"thread_id":"t1","session_id":"s1","response_id":"{resp}","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}}}"#
            )
        };
        let arch = day.join("rollout-x.jsonl.zst");
        std::fs::write(&arch, zstd::encode_all(mk("resp_a").as_bytes(), 0).unwrap()).unwrap();
        let plain = day.join("rollout-y.jsonl");
        std::fs::write(&plain, format!("{}\n", mk("resp_p"))).unwrap();

        let store = crate::usage_store::UsageStore::new(home.join("usage.sqlite3"));
        store.set_enabled(true);
        let mut c = Cursors::default();
        collect_codex(&store, &mut c, &home);
        let agents = store.by_agent("2000-01-01", "2100-01-01");
        let codex = agents.iter().find(|a| a.agent == "codex").unwrap();
        assert_eq!(codex.requests, 2);
        assert_eq!(codex.input_new, 120);
        assert_eq!(codex.cache_read, 80);
        assert_eq!(codex.output, 20);

        // 没变化：门控跳过，不重复累计
        collect_codex(&store, &mut c, &home);
        assert_eq!(store.by_agent("2000-01-01", "2100-01-01")[0].requests, 2);

        // 重新归档（size 变）带重复行：全量重扫但主键去重
        let dup = format!("{}\n{}\n", mk("resp_a"), mk("resp_a"));
        std::fs::write(&arch, zstd::encode_all(dup.as_bytes(), 0).unwrap()).unwrap();
        collect_codex(&store, &mut c, &home);
        assert_eq!(store.by_agent("2000-01-01", "2100-01-01")[0].requests, 2);

        // 归档里新增请求要采到
        let more = format!("{}\n{}\n", mk("resp_a"), mk("resp_b"));
        std::fs::write(&arch, zstd::encode_all(more.as_bytes(), 0).unwrap()).unwrap();
        collect_codex(&store, &mut c, &home);
        assert_eq!(store.by_agent("2000-01-01", "2100-01-01")[0].requests, 3);

        drop(store);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn workbuddy_camel_and_openai_forms() {
        let camel = r#"{"id":"m1","sessionId":"s1","timestamp":1794298324760,"providerData":{"model":"deepseek-v4.1-flash","usage":{"inputTokens":45307,"outputTokens":92,"inputTokensDetails":[{"cached_tokens":256}]}}}"#;
        let r = parse_workbuddy_line(camel).unwrap();
        assert_eq!(r.input_new, 45051);
        assert_eq!(r.cache_read, 256);
        let openai = r#"{"id":"m2","sessionId":"s1","timestamp":1794298324761,"providerData":{"rawUsage":{"prompt_tokens":100,"completion_tokens":10,"prompt_cache_hit_tokens":40}}}"#;
        let r2 = parse_workbuddy_line(openai).unwrap();
        assert_eq!(r2.input_new, 60);
        assert_eq!(r2.cache_read, 40);
        assert_eq!(r2.output, 10);
    }

    #[test]
    fn dsh_input_excludes_cache() {
        let line = r#"{"type":"assistant/message","seq":15,"time":1790176458208,"data":{"model":"deepseek-flash","usage":{"inputTokens":1828,"outputTokens":148,"cacheReadTokens":6784,"cacheWriteTokens":0,"totalTokens":8760}}}"#;
        let r = parse_dsh_line(line, "session.v4", "sess1").unwrap();
        assert_eq!(r.input_new, 1828);
        assert_eq!(r.cache_read, 6784);
        assert_eq!(r.record_id, "sess1:session.v4:15");
    }

    #[test]
    fn kimi_usage_record_fields() {
        let line = r#"{"type":"usage.record","agentId":"main","model":"Zhipu AI Coding Plan/glm-5.3-flash","usage":{"inputOther":24739,"output":82,"inputCacheRead":0,"inputCacheCreation":0},"usageScope":"turn","time":1790524503936}"#;
        let r = parse_kimi_line(line, "session_x", "kimi-code").unwrap();
        assert_eq!(r.input_new, 24739);
        assert_eq!(r.output, 82);
        assert_eq!(r.session_id, "session_x");
        assert!(parse_kimi_line(r#"{"type":"llm.request"}"#, "s", "kimi-code").is_none());
    }

    #[test]
    fn qoder_token_info_splits_cache() {
        let r = parse_qoder_token_info(
            r#"{"prompt_tokens":1000,"completion_tokens":50,"cached_tokens":800}"#,
            "m1",
            "s1",
            123,
        )
        .unwrap();
        assert_eq!(r.input_new, 200);
        assert_eq!(r.cache_read, 800);
        assert!(parse_qoder_token_info("{}", "m", "s", 0).is_none());
    }

    #[test]
    fn qoder_transcript_usage_fields() {
        let line = r#"{"type":"assistant","timestamp":"2026-11-21T14:30:37.510Z","sessionId":"s1","message":{"id":"m2","model":"deepseek-flash","content":[],"usage":{"input_tokens":34680,"output_tokens":225,"cache_read_input_tokens":26496,"cache_creation_input_tokens":0,"request_id":"r2"}}}"#;
        let r = parse_qoder_transcript_line(line, "stem").unwrap();
        assert_eq!(r.agent, "qoder");
        assert_eq!(r.record_id, "s1:m2");
        assert_eq!(r.model, "deepseek-flash");
        assert_eq!(r.input_new, 8184);
        assert_eq!(r.cache_read, 26496);
        assert_eq!(r.output, 225);
        assert!(parse_qoder_transcript_line(r#"{"type":"assistant","message":{"id":"m"}}"#, "stem").is_none());
    }

    #[test]
    fn qoder_transcript_collects_once_per_response() {
        let dir = std::env::temp_dir().join(format!("bark-qoder-usage-{}", std::process::id()));
        let home = dir.join("home");
        let project = home.join(".qoder-cn").join("projects").join("p");
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-11-21T14:30:35.049Z","sessionId":"s1","message":{"id":"m1","model":"deepseek-flash","content":[],"usage":{"input_tokens":26355,"output_tokens":167,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"request_id":"r1"}}}"#,
            // 同一响应的另一条拆分记录也带 usage：按 message.id 只计一次
            r#"{"type":"assistant","timestamp":"2026-11-21T14:30:35.049Z","sessionId":"s1","message":{"id":"m1","model":"deepseek-flash","content":[],"usage":{"input_tokens":26355,"output_tokens":167,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"request_id":"r1"}}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-21T14:30:37.510Z","sessionId":"s1","message":{"id":"m2","model":"deepseek-flash","content":[],"usage":{"input_tokens":34680,"output_tokens":225,"cache_read_input_tokens":26496,"cache_creation_input_tokens":0,"request_id":"r2"}}}"#,
            // 没有 usage 的拆分记录：忽略
            r#"{"type":"assistant","timestamp":"2026-11-21T14:30:37.510Z","sessionId":"s1","message":{"id":"m2","model":"deepseek-flash","content":[]}}"#,
        ].join("\n") + "\n";
        std::fs::write(project.join("s1.jsonl"), lines).unwrap();

        let store = crate::usage_store::UsageStore::new(home.join("usage.sqlite3"));
        store.set_enabled(true);
        let mut c = Cursors::default();
        collect_qoder_transcripts(&store, &mut c, &home);
        let agents = store.by_agent("2000-01-01", "2100-01-01");
        let q = agents.iter().find(|a| a.agent == "qoder").unwrap();
        assert_eq!(q.requests, 2);
        assert_eq!(q.input_new, 26355 + (34680 - 26496));
        assert_eq!(q.cache_read, 26496);
        assert_eq!(q.output, 392);

        // 重启式重扫（全新游标）幂等
        let mut c2 = Cursors::default();
        collect_qoder_transcripts(&store, &mut c2, &home);
        let again = store.by_agent("2000-01-01", "2100-01-01");
        assert_eq!(again.iter().find(|a| a.agent == "qoder").unwrap().requests, 2);

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_agent_kind_is_classified_once() {
        use bark_core::AgentKind;
        for kind in AgentKind::ALL {
            let id = kind.id();
            let lists = [
                USAGE_SOURCE_AGENTS.contains(&id),
                NO_USAGE_SOURCE_AGENTS.contains(&id),
                CONDITIONAL_USAGE_AGENTS.contains(&id),
            ];
            assert_eq!(
                lists.into_iter().filter(|b| *b).count(),
                1,
                "{id} 必须恰好归入一类用量源"
            );
        }
        for list in [USAGE_SOURCE_AGENTS, NO_USAGE_SOURCE_AGENTS, CONDITIONAL_USAGE_AGENTS] {
            for &id in list {
                assert!(AgentKind::from_id(id).is_some(), "{id} 不在 AgentKind::ALL 里");
            }
        }
    }

    #[test]
    fn tail_handles_partial_line_and_truncation() {
        let dir = std::env::temp_dir().join(format!("bark-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(&p, "a\nb\n").unwrap();
        let mut cur = 0u64;
        assert_eq!(tail_lines(&p, &mut cur), vec!["a", "b"]);
        std::fs::write(&p, "a\nb\nc").unwrap(); // 半行不消费
        assert!(tail_lines(&p, &mut cur).is_empty());
        std::fs::write(&p, "a\nb\nc\n").unwrap();
        assert_eq!(tail_lines(&p, &mut cur), vec!["c"]);
        std::fs::write(&p, "x\n").unwrap(); // 截断 → 从头
        assert_eq!(tail_lines(&p, &mut cur), vec!["x"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真机冒烟：对本机实际数据源跑一轮采集（只读源、写临时库）。
    /// 依赖本机装了哪些 agent，故 ignore；手动 `cargo test -- --ignored live_smoke`。
    #[test]
    #[ignore]
    fn live_smoke_real_sources() {
        let dir = std::env::temp_dir().join(format!("bark-usage-smoke-{}", std::process::id()));
        let store = crate::usage_store::UsageStore::new(dir.join("usage.sqlite3"));
        store.set_enabled(true);
        let mut c = Cursors::default();
        poll_fast(&store, &mut c);
        poll_dsh(&store, &mut c);
        let agents = store.by_agent("2000-01-01", "2100-01-01");
        for a in &agents {
            println!(
                "SMOKE {} requests={} in={} cache={} out={} credits={}",
                a.agent, a.requests, a.input_new, a.cache_read, a.output, a.credits
            );
        }
        // 逐源打印；本机没装 / 会话为空的源只提示不硬断言（冒烟测跨机器可用）
        for want in ["antigravity", "zcode", "workbuddy", "dsh", "codex", "qoder"] {
            if !agents.iter().any(|a| a.agent == want && a.requests > 0) {
                println!("SMOKE {want} 本机无数据（未安装或会话为空）");
            }
        }
        assert!(!agents.is_empty(), "本机所有源都没采到数据");
        // 幂等：再跑一轮新增应为 0
        let mut c2 = Cursors::default();
        poll_fast(&store, &mut c2);
        let again = store.by_agent("2000-01-01", "2100-01-01");
        assert_eq!(
            agents.iter().map(|a| a.requests).sum::<i64>(),
            again.iter().map(|a| a.requests).sum::<i64>()
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}