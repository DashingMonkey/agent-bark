//! 其余「业界已验证」源的采集（定义移植自 ccusage 20.0.51 各 adapter 的字段口径）。
//!
//! 本机未安装的源也实现 parser：用户装好即自动出数（首轮回补历史）。
//! 拿不到可靠定义的（Antigravity 的 SQLite 快照 schema、Trae 系加密库、
//! 豆包、Cursor 官方 API）**不猜口径**——不采集即不列出。
//!
//! CodeBuddy 有两种落盘形态（定义移植自 tokscale `sessions/tencent_buddy.rs`）：
//! - CLI：`~/.codebuddy/projects/**/*.jsonl`（含 subagents）。用量多数写在
//!   `function_call` 而非 assistant message 上——只读消息会漏掉绝大部分；
//! - IDE / VS Code 扩展：`CodeBuddyExtension/Logs/**/Tencent-Cloud.coding-copilot/*.log`
//!   的 `[AgentReporter] ... Agent execution successful with usage: {...}`，
//!   同一行会镜像写多个日志 sink，按 agent + 秒 + 用量四元组去重。
//!
//! 口径假设（与各源官方语义一致，单测钉死）：
//! - Claude / Droid：input_tokens 不含缓存读取；
//! - Gemini / Qwen：promptTokenCount 含 cachedContentTokenCount，需拆；
//! - OpenCode：tokens_input 不含 cache.read；
//! - Copilot OTEL：gen_ai.usage.input_tokens 视为含缓存，拆出 cache_read；
//! - CodeBuddy：cachedMissTokens 本身不含缓存；其余 input 仅在 total 能证明
//!   含缓存时才拆（`total == input + output`），模糊样本保持原值。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::usage_collect::{tail_lines, Cursors};
use crate::usage_store::{UsageRecord, UsageStore};

pub fn poll(store: &UsageStore, c: &mut Cursors, home: &Path) {
    for path in claude_files(home) {
        let stem = stem_of(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| parse_claude_line(l, &stem, i))
            .collect();
        store.insert(&recs);
    }
    for (root, agent) in [(".gemini", "gemini-cli"), (".qwen", "qwen-code")] {
        for path in chat_files(&home.join(root)) {
            let stem = stem_of(&path);
            let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
            let recs: Vec<UsageRecord> = lines
                .iter()
                .enumerate()
                .filter_map(|(i, l)| parse_gemini_line(l, &stem, i, agent))
                .collect();
            store.insert(&recs);
        }
    }
    for path in named_files(&home.join(".copilot").join("session-state"), "events.jsonl", 2) {
        let session = parent_name(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines.iter().filter_map(|l| parse_copilot_line(l, &session)).collect();
        store.insert(&recs);
    }
    for path in named_files(&home.join(".factory").join("sessions"), ".jsonl", 2) {
        let stem = stem_of(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| parse_droid_line(l, &stem, i))
            .collect();
        store.insert(&recs);
    }
    for path in jsonl_under(&home.join(".grok"), 4) {
        let stem = stem_of(&path);
        let session = parent_name(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| parse_grok_line(l, &stem, i, &session))
            .collect();
        store.insert(&recs);
    }
    collect_codebuddy(store, c, home);
    collect_codebuddy_logs(store, c, home);
    collect_opencode(store, c, home);
}

// ---- 文件发现 ----

fn stem_of(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}
fn parent_name(p: &Path) -> String {
    p.parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

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

fn is_jsonl(p: &Path) -> bool {
    p.extension().map(|e| e == "jsonl").unwrap_or(false)
}

/// Claude transcript：projects 下含 subagents 子目录，一并计入（子代理也烧 token）
fn claude_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&home.join(".claude").join("projects"), 5, &mut out, &is_jsonl);
    out
}

/// Gemini / Qwen：tmp/<project>/chats/*.jsonl
fn chat_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(
        &root.join("tmp"),
        4,
        &mut out,
        &|p| is_jsonl(p) && p.parent().and_then(|d| d.file_name()).map(|n| n == "chats").unwrap_or(false),
    );
    out
}

fn named_files(root: &Path, name: &str, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if name.starts_with('.') {
        walk(root, depth, &mut out, &|p| {
            p.file_name().map(|n| n.to_string_lossy().ends_with(name)).unwrap_or(false)
        });
    } else {
        walk(root, depth, &mut out, &|p| p.file_name().map(|n| n == name).unwrap_or(false));
    }
    out
}

fn jsonl_under(root: &Path, depth: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, depth, &mut out, &is_jsonl);
    out
}

// ---- 行 parser ----

fn ts_of(rec: &serde_json::Value) -> Option<i64> {
    match rec.get("timestamp")? {
        serde_json::Value::String(s) => chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis()),
        serde_json::Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn num(v: &serde_json::Value, keys: &[&str]) -> i64 {
    for k in keys {
        if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
            return n;
        }
    }
    0
}

fn str_of(v: &serde_json::Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

pub fn parse_claude_line(line: &str, stem: &str, idx: usize) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    if rec.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let msg = rec.get("message")?;
    let usage = msg.get("usage")?;
    // 新版 Claude 把一次请求的多段用量放进 usage.iterations；旧版直接平铺
    let parts: Vec<&serde_json::Value> = match usage.get("iterations").and_then(|v| v.as_array()) {
        Some(arr) => arr.iter().collect(),
        None => vec![usage],
    };
    let (mut input, mut out, mut cw, mut cr) = (0, 0, 0, 0);
    for u in parts {
        input += num(u, &["input_tokens"]);
        out += num(u, &["output_tokens"]);
        cw += num(u, &["cache_creation_input_tokens"]);
        cr += num(u, &["cache_read_input_tokens"]);
    }
    if input == 0 && out == 0 && cr == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: "claude-code".into(),
        record_id: str_of(&rec, &["uuid", "id"]).filter_empty().unwrap_or_else(|| format!("{stem}:{idx}")),
        session_id: str_of(&rec, &["sessionId"]).filter_empty().unwrap_or_else(|| stem.to_string()),
        ts: ts_of(&rec)?,
        model: str_of(msg, &["model"]),
        input_new: input,
        cache_read: cr,
        cache_write: cw,
        output: out,
        reasoning: 0,
        credits: None,
    })
}

pub fn parse_gemini_line(line: &str, stem: &str, idx: usize, agent: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    let u = rec.get("usageMetadata")?;
    let prompt = num(u, &["promptTokenCount"]);
    let cached = num(u, &["cachedContentTokenCount"]);
    let out = num(u, &["candidatesTokenCount"]);
    let thoughts = num(u, &["thoughtsTokenCount"]);
    if prompt == 0 && out == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: agent.into(),
        record_id: str_of(&rec, &["id"]).filter_empty().unwrap_or_else(|| format!("{stem}:{idx}")),
        session_id: str_of(&rec, &["sessionId"]).filter_empty().unwrap_or_else(|| stem.to_string()),
        ts: ts_of(&rec)?,
        model: str_of(&rec, &["model"]),
        input_new: (prompt - cached).max(0),
        cache_read: cached,
        cache_write: 0,
        output: out,
        reasoning: thoughts,
        credits: None,
    })
}

pub fn parse_copilot_line(line: &str, session: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    // OTEL 属性可能挂在根 / data / attributes 任一层
    let attrs = [Some(&rec), rec.get("data"), rec.get("attributes"), rec.get("data").and_then(|d| d.get("attributes"))]
        .into_iter()
        .flatten()
        .find(|v| v.get("gen_ai.usage.input_tokens").is_some())?;
    let input = num(attrs, &["gen_ai.usage.input_tokens"]);
    let out = num(attrs, &["gen_ai.usage.output_tokens"]);
    let cr = num(attrs, &["gen_ai.usage.cache_read.input_tokens"]);
    let cw = num(attrs, &["gen_ai.usage.cache_write.input_tokens", "gen_ai.usage.cache_creation.input_tokens"]);
    let reasoning = num(attrs, &["gen_ai.usage.reasoning.output_tokens", "gen_ai.usage.reasoning_tokens"]);
    if input == 0 && out == 0 {
        return None;
    }
    let ts = ts_of(&rec)?;
    Some(UsageRecord {
        agent: "copilot-cli".into(),
        record_id: str_of(&rec, &["id"]).filter_empty().unwrap_or_else(|| format!("{session}:{ts}")),
        session_id: session.to_string(),
        ts,
        model: str_of(attrs, &["gen_ai.request.model"]),
        input_new: (input - cr).max(0),
        cache_read: cr,
        cache_write: cw,
        output: out,
        reasoning,
        credits: None,
    })
}

pub fn parse_droid_line(line: &str, stem: &str, idx: usize) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    let u = rec
        .get("tokenUsage")
        .or_else(|| rec.get("message").and_then(|m| m.get("tokenUsage")))
        .or_else(|| rec.get("usage"))?;
    let input = num(u, &["inputTokens", "input_tokens", "promptTokens"]);
    let out = num(u, &["outputTokens", "output_tokens", "completionTokens"]);
    let cr = num(u, &["cacheReadTokens", "cache_read_input_tokens"]);
    let cw = num(u, &["cacheWriteTokens", "cache_creation_input_tokens"]);
    let reasoning = num(u, &["thinkingTokens", "reasoningTokens"]);
    if input == 0 && out == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: "droid".into(),
        record_id: str_of(&rec, &["id", "uuid"]).filter_empty().unwrap_or_else(|| format!("{stem}:{idx}")),
        session_id: str_of(&rec, &["sessionId"]).filter_empty().unwrap_or_else(|| stem.to_string()),
        ts: ts_of(&rec)?,
        model: str_of(&rec, &["model"]).filter_empty().unwrap_or_else(|| str_of(rec.get("message").unwrap_or(&rec), &["model"])),
        input_new: input,
        cache_read: cr,
        cache_write: cw,
        output: out,
        reasoning,
        credits: None,
    })
}

pub fn parse_grok_line(line: &str, stem: &str, idx: usize, session: &str) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let update = rec.get("update");
    let kind2 = update.and_then(|u| u.get("type")).and_then(|v| v.as_str()).unwrap_or("");
    if kind != "turn_completed" && kind2 != "turn_completed" {
        return None;
    }
    let u = rec
        .get("usage")
        .or_else(|| update.and_then(|x| x.get("usage")))
        .or_else(|| rec.get("summary").and_then(|s| s.get("usage")))?;
    let input = num(u, &["inputTokens", "input_tokens", "promptTokens"]);
    let out = num(u, &["outputTokens", "output_tokens", "completionTokens"]);
    let cr = num(u, &["cacheReadTokens", "cache_read_input_tokens"]);
    let cw = num(u, &["cacheWriteTokens", "cache_creation_input_tokens"]);
    let reasoning = num(u, &["reasoningTokens", "thinkingTokens"]);
    if input == 0 && out == 0 {
        return None;
    }
    Some(UsageRecord {
        agent: "grok".into(),
        record_id: str_of(&rec, &["id"]).filter_empty().unwrap_or_else(|| format!("{stem}:{idx}")),
        session_id: str_of(&rec, &["sessionId"]).filter_empty().unwrap_or_else(|| session.to_string()),
        ts: ts_of(&rec)?,
        model: str_of(&rec, &["model"]),
        input_new: (input - cr).max(0),
        cache_read: cr,
        cache_write: cw,
        output: out,
        reasoning,
        credits: None,
    })
}

trait FilterEmpty {
    fn filter_empty(self) -> Option<String>;
}
impl FilterEmpty for String {
    fn filter_empty(self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self)
        }
    }
}

// ---- CodeBuddy：CLI JSONL + IDE 扩展日志 ----

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// CLI transcript：`projects/<mangled-cwd>/<session>.jsonl`；
/// 子代理在 `<session>/subagents/agent-*.jsonl`，各自带独立 sessionId，分别计。
/// 只认 `projects/`：WorkBuddy 与 CodeBuddy 共用 `~/.codebuddy`，但只写日志/诊断目录；
/// 不跟随 `CODEBUDDY_CONFIG_DIR`（与 tokscale 的扫描口径保持一致）。
fn codebuddy_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&home.join(".codebuddy").join("projects"), 5, &mut out, &is_jsonl);
    out
}

/// IDE / VS Code 扩展日志根：按三种平台的原生默认布局从 home 推导
/// （Windows `%LOCALAPPDATA%` / `%APPDATA%`、macOS `Library/Application Support`、
/// Linux XDG 默认目录）。不存在的路径天然是 no-op；不依赖 dirs，测试可指向临时 home。
fn codebuddy_log_files(home: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let extension_roots = [
        home.join("AppData").join("Local").join("CodeBuddyExtension").join("Logs"),
        home.join("Library").join("Application Support").join("CodeBuddyExtension").join("Logs"),
        home.join(".local").join("share").join("CodeBuddyExtension").join("Logs"),
    ];
    for base in [
        home.join("AppData").join("Roaming"),
        home.join("Library").join("Application Support"),
        home.join(".config"),
    ] {
        roots.push(base.join("CodeBuddy CN").join("logs"));
        roots.push(base.join("Code").join("logs"));
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    // CodeBuddyExtension/Logs 是 CodeBuddy 自己的宿主日志树（只有它自己的扩展），
    // 同 tokscale 全部收；VS Code 的 logs 树兄弟扩展太多，只在命中扩展目录时收。
    for base in extension_roots {
        for host in ["CodeBuddyIDE", "VSCode"] {
            collect_all_logs(&base.join(host), 8, &mut out, &mut seen);
        }
    }
    for root in roots {
        find_copilot_logs(&root, 8, &mut out, &mut seen);
    }
    out
}

/// VS Code 日志树是 `logs/<ts>/window<N>/exthost/<扩展 id>/*.log`，
/// 兄弟扩展目录很多：只下探目录，命中扩展目录才收 `.log`。
fn find_copilot_logs(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if name == "tencent-cloud.coding-copilot" {
            collect_all_logs(&p, 2, out, seen);
        } else {
            find_copilot_logs(&p, depth - 1, out, seen);
        }
    }
}

fn collect_all_logs(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            collect_all_logs(&p, depth - 1, out, seen);
        } else if p.extension().map(|x| x.eq_ignore_ascii_case("log")).unwrap_or(false) && seen.insert(p.clone()) {
            out.push(p);
        }
    }
}

fn first_i64(v: &serde_json::Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|k| v.get(*k).and_then(|x| x.as_i64()))
}
fn first_present(v: &serde_json::Value, keys: &[&str]) -> i64 {
    first_i64(v, keys).unwrap_or(0).max(0)
}
fn first_positive(v: &serde_json::Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_i64()).filter(|n| *n > 0))
        .or_else(|| first_i64(v, keys))
        .unwrap_or(0)
        .max(0)
}

/// CodeBuddy 的 usage 对象（CLI JSONL 与扩展日志同一套转换）。
/// 返回 (input_new, cache_read, cache_write, output, reasoning)。
fn codebuddy_usage_tokens(usage: &serde_json::Value) -> Option<(i64, i64, i64, i64, i64)> {
    let cache_read = first_positive(
        usage,
        &["cache_read_input_tokens", "cacheReadInputTokens", "cacheTokens", "prompt_cache_hit_tokens", "cached_tokens"],
    );
    let output = first_present(usage, &["output_tokens", "outputTokens", "completion_tokens"]);
    let cache_write = first_positive(
        usage,
        &["cache_creation_input_tokens", "cacheCreationInputTokens", "cachedWriteTokens", "prompt_cache_write_tokens"],
    );
    let reasoning = first_present(usage, &["completion_thinking_tokens", "completionThinkingTokens", "reasoningTokens"]);
    // cachedMissTokens 本身就是「不含缓存的新输入」；显式 0 也有效
    let miss = first_i64(usage, &["cachedMissTokens", "cacheMissTokens"]).map(|n| n.max(0));
    let mut input = match miss {
        Some(n) => n,
        None => first_present(usage, &["input_tokens", "inputTokens", "prompt_tokens"]),
    };
    // 其余 input 字段是否含缓存不确定：只有 total == input + output 能证明含缓存
    if miss.is_none() && cache_read > 0 {
        if let Some(total) = first_i64(usage, &["total_tokens", "totalTokens"]) {
            if total.max(0) == input + output {
                input = (input - cache_read).max(0);
            }
        }
    }
    let sum = input + output + cache_read + cache_write + reasoning;
    (sum > 0).then_some((input, cache_read, cache_write, output, reasoning))
}

pub fn parse_codebuddy_line(line: &str, stem: &str, fallback_ts: i64) -> Option<UsageRecord> {
    let rec: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = rec.get("type")?.as_str()?;
    let is_assistant = kind == "message" && rec.get("role").and_then(|v| v.as_str()) == Some("assistant");
    // 一次模型响应要么落在 function_call，要么落在 assistant message，用量只在其中一条
    if !is_assistant && kind != "function_call" {
        return None;
    }
    if rec.get("status").and_then(|v| v.as_str()).is_some_and(|s| s != "completed") {
        return None;
    }
    let pd = rec.get("providerData");
    let usage = rec
        .get("message")
        .and_then(|m| m.get("usage"))
        .or_else(|| pd.and_then(|p| p.get("usage")))
        .or_else(|| pd.and_then(|p| p.get("rawUsage")))?;
    let (input_new, cache_read, cache_write, output, reasoning) = codebuddy_usage_tokens(usage)?;

    let session = str_of(&rec, &["sessionId"]).filter_empty().unwrap_or_else(|| stem.to_string());
    let ts = rec.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(fallback_ts);
    let key = pd
        .and_then(|p| {
            let id = str_of(p, &["messageId"]);
            if id.is_empty() {
                Some(str_of(p, &["traceId"]))
            } else {
                Some(id)
            }
        })
        .and_then(FilterEmpty::filter_empty)
        .or_else(|| str_of(&rec, &["id"]).filter_empty())
        // 无 id 时用记录时间戳而非文件 mtime：文件追加后重扫时 key 仍稳定
        .unwrap_or_else(|| format!("{stem}:{ts}"));
    let model = pd
        .map(|p| {
            let m = str_of(p, &["model"]);
            if m.is_empty() {
                str_of(p, &["requestModelId"])
            } else {
                m
            }
        })
        .filter(|m| !m.is_empty())
        .or_else(|| rec.get("message").map(|m| str_of(m, &["model"])).filter(|m| !m.is_empty()))
        .unwrap_or_default();
    Some(UsageRecord {
        agent: "codebuddy".into(),
        record_id: format!("{session}:{key}"),
        session_id: session,
        ts,
        model,
        input_new,
        cache_read,
        cache_write,
        output,
        reasoning,
        credits: None,
    })
}

/// 日志时间戳形如 `[2026/7/1 16:56:02.200]` 或 `2026-07-01 17:00:59.790 [info]`，
/// 无时区，按本机时区解释。
fn parse_cb_log_ts(line: &str) -> Option<i64> {
    let raw = if let Some(rest) = line.strip_prefix('[') {
        rest.split_once(']')?.0.trim()
    } else {
        line.split_once(" [").map(|(t, _)| t.trim()).unwrap_or_else(|| line.trim())
    };
    let (date, time) = raw.split_once(' ')?;
    let sep = if date.contains('/') { '/' } else { '-' };
    let parts: Vec<u32> = date.split(sep).filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        return None;
    }
    let normalized = format!("{:04}-{:02}-{:02} {}", parts[0], parts[1], parts[2], time);
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, fmt) {
            return naive.and_local_timezone(chrono::Local).earliest().map(|d| d.timestamp_millis());
        }
    }
    None
}

fn bracket_value_after(line: &str, marker: &str) -> Option<String> {
    let after = line.split(marker).nth(1)?;
    let start = after.find('[')?;
    let rest = &after[start + 1..];
    let end = rest.find(']')?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `[...] [CraftInvokableAgent] [agent-1] Model prepared: Kimi-K2.7-Code (kimi-k2.7)`
fn parse_codebuddy_prepared(line: &str) -> Option<(String, String, Option<i64>)> {
    if !line.contains("[CraftInvokableAgent]") || !line.contains("Model prepared:") {
        return None;
    }
    let agent = bracket_value_after(line, "[CraftInvokableAgent]")?;
    let after = line.split("Model prepared:").nth(1)?.trim();
    let model = after
        .rsplit_once('(')
        .and_then(|(_, tail)| tail.split_once(')').map(|(m, _)| m.trim()))
        .filter(|m| !m.is_empty())
        .unwrap_or(after)
        .to_string();
    Some((agent, model, parse_cb_log_ts(line)))
}

/// `[...] [AgentReporter] [agent-1] Agent execution successful with usage: {...}`
/// 返回 (agent_id, 结算毫秒, (input_new, cache_read, cache_write, output, reasoning))
fn parse_codebuddy_log_usage(line: &str) -> Option<(String, Option<i64>, (i64, i64, i64, i64, i64))> {
    if !line.contains("[AgentReporter]") || !line.contains("Agent execution successful with usage:") {
        return None;
    }
    let agent = bracket_value_after(line, "[AgentReporter]")?;
    let json = line.split("Agent execution successful with usage:").nth(1)?.trim();
    let usage: serde_json::Value = serde_json::from_str(&json[..=json.rfind('}')?]).ok()?;
    let tokens = codebuddy_usage_tokens(&usage)?;
    Some((agent, parse_cb_log_ts(line), tokens))
}

/// 同一结算行会镜像写进扩展自己的日志和宿主的输出通道日志，毫秒级时间可能差几毫秒。
/// 按「agent + 结算秒 + 用量四元组」去重：秒内同 agent 的重复镜像合并，真实的下一次
/// 执行至少间隔数秒，不会被误合并。
fn codebuddy_log_dedup_key(agent: &str, settled_ts: i64, t: (i64, i64, i64, i64, i64)) -> String {
    let sec = settled_ts.div_euclid(1000);
    format!("ext:{agent}:{sec}:{}:{}:{}:{}:{}", t.0, t.1, t.2, t.3, t.4)
}

fn collect_codebuddy(store: &UsageStore, c: &mut Cursors, home: &Path) {
    for path in codebuddy_files(home) {
        let stem = stem_of(&path);
        let fallback_ts = mtime_ms(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        let recs: Vec<UsageRecord> = lines
            .iter()
            .filter_map(|l| parse_codebuddy_line(l, &stem, fallback_ts))
            .collect();
        store.insert(&recs);
    }
}

fn collect_codebuddy_logs(store: &UsageStore, c: &mut Cursors, home: &Path) {
    for path in codebuddy_log_files(home) {
        let fallback_ts = mtime_ms(&path);
        let lines = tail_lines(&path, c.files.entry(path.clone()).or_insert(0));
        if lines.is_empty() {
            continue;
        }
        let prepared = c.cb_prepared.entry(path).or_default();
        let mut recs = Vec::new();
        for line in &lines {
            if let Some((agent, model, anchor_ts)) = parse_codebuddy_prepared(line) {
                prepared.insert(agent, (model, anchor_ts));
                continue;
            }
            let Some((agent, settled, tokens)) = parse_codebuddy_log_usage(line) else { continue };
            // 结算一出现就消费锚点：下一条结算不能复用它，否则会把两次执行之间
            // 的空档算进同一次生成（tokscale 同口径）
            let anchor = prepared.remove(&agent);
            let (model, anchor_ts) = anchor.unwrap_or_default();
            let settled_ts = settled.unwrap_or(fallback_ts);
            // 归日取生成开始（锚点）而不是结算，与 tokscale 的 SessionSpan 一致
            let ts = match anchor_ts {
                Some(a) if settled.is_some_and(|s| s > a) => a,
                _ => settled_ts,
            };
            recs.push(UsageRecord {
                agent: "codebuddy".into(),
                record_id: codebuddy_log_dedup_key(&agent, settled_ts, tokens),
                session_id: agent,
                ts,
                model,
                input_new: tokens.0,
                cache_read: tokens.1,
                cache_write: tokens.2,
                output: tokens.3,
                reasoning: tokens.4,
                credits: None,
            });
        }
        store.insert(&recs);
    }
}

// ---- OpenCode SQLite ----

fn collect_opencode(store: &UsageStore, c: &mut Cursors, home: &Path) {
    let mut roots = vec![home.join(".local").join("share").join("opencode")];
    if let Some(d) = dirs::data_dir() {
        roots.push(d.join("opencode"));
    }
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !(name == "opencode.db" || (name.starts_with("opencode-") && name.ends_with(".db"))) {
                continue;
            }
            let Some(conn) = crate::usage_collect::ro_conn(&p) else { continue };
            // 新版表名 message，旧版 session_message
            let table = ["message", "session_message"]
                .into_iter()
                .find(|t| conn.prepare(&format!("SELECT 1 FROM {t} LIMIT 1")).is_ok());
            let Some(table) = table else { continue };
            let cols: Vec<String> = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .and_then(|mut s| {
                    s.query_map([], |r| r.get::<_, String>(1))
                        .map(|it| it.flatten().collect())
                })
                .unwrap_or_default();
            let want = [
                "id",
                "time_created",
                "tokens_input",
                "tokens_output",
                "tokens_cache_read",
                "tokens_cache_write",
                "tokens_reasoning",
                "session_id",
                "model",
            ];
            let sel: Vec<&str> = want.iter().copied().filter(|w| cols.iter().any(|c| c == w)).collect();
            if !sel.contains(&"id") || !sel.contains(&"time_created") {
                continue;
            }
            let from = c.oc_ts.get(&p).copied().unwrap_or(0);
            let sql = format!("SELECT {} FROM {table} WHERE time_created >= ?1", sel.join(", "));
            let Ok(mut stmt) = conn.prepare(&sql) else { continue };
            // 列类型随版本漂移（INTEGER / TEXT 都见过）：按动态 Value 读再转
            let as_i64 = |v: Option<rusqlite::types::Value>| -> i64 {
                match v {
                    Some(rusqlite::types::Value::Integer(n)) => n,
                    Some(rusqlite::types::Value::Real(f)) => f as i64,
                    Some(rusqlite::types::Value::Text(s)) => s.parse().unwrap_or(0),
                    _ => 0,
                }
            };
            let as_str = |v: Option<rusqlite::types::Value>| -> String {
                match v {
                    Some(rusqlite::types::Value::Text(s)) => s,
                    Some(rusqlite::types::Value::Integer(n)) => n.to_string(),
                    _ => String::new(),
                }
            };
            let rows = stmt.query_map([from], |r| {
                let get = |name: &str| -> Option<rusqlite::types::Value> {
                    let i = sel.iter().position(|s| *s == name)?;
                    r.get(i).ok()
                };
                Ok((
                    as_str(get("id")),
                    as_i64(get("time_created")),
                    as_i64(get("tokens_input")),
                    as_i64(get("tokens_output")),
                    as_i64(get("tokens_cache_read")),
                    as_i64(get("tokens_cache_write")),
                    as_i64(get("tokens_reasoning")),
                    as_str(get("session_id")),
                    as_str(get("model")),
                ))
            });
            let Ok(rows) = rows else { continue };
            let mut recs = Vec::new();
            for (id, ts, input, out, cr, cw, reason, session, model) in rows.flatten() {
                if id.is_empty() || ts == 0 {
                    continue;
                }
                c.oc_ts.insert(p.clone(), c.oc_ts.get(&p).copied().unwrap_or(0).max(ts));
                recs.push(UsageRecord {
                    agent: "opencode".into(),
                    record_id: id,
                    session_id: session,
                    ts,
                    model,
                    input_new: input,
                    cache_read: cr,
                    cache_write: cw,
                    output: out,
                    reasoning: reason,
                    credits: None,
                });
            }
            store.insert(&recs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_plain_and_iterations() {
        let plain = r#"{"type":"assistant","uuid":"u1","sessionId":"s1","timestamp":"2026-11-05T06:54:22.914Z","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":100,"output_tokens":50,"cache_creation_input_tokens":10,"cache_read_input_tokens":800}}}"#;
        let r = parse_claude_line(plain, "f", 0).unwrap();
        assert_eq!(r.input_new, 100);
        assert_eq!(r.cache_read, 800);
        assert_eq!(r.cache_write, 10);
        assert_eq!(r.record_id, "u1");
        let iter = r#"{"type":"assistant","uuid":"u2","sessionId":"s1","timestamp":"2026-11-05T06:54:22.914Z","message":{"model":"m","usage":{"iterations":[{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100},{"input_tokens":20,"output_tokens":7,"cache_read_input_tokens":200}]}}}"#;
        let r2 = parse_claude_line(iter, "f", 1).unwrap();
        assert_eq!(r2.input_new, 30);
        assert_eq!(r2.output, 12);
        assert_eq!(r2.cache_read, 300);
        assert!(parse_claude_line(r#"{"type":"user"}"#, "f", 0).is_none());
    }

    #[test]
    fn gemini_prompt_includes_cache() {
        let line = r#"{"id":"g1","sessionId":"s","timestamp":"2026-11-05T06:54:22.914Z","model":"gemini-3-pro","usageMetadata":{"promptTokenCount":1000,"candidatesTokenCount":80,"thoughtsTokenCount":20,"cachedContentTokenCount":900,"totalTokenCount":1100}}"#;
        let r = parse_gemini_line(line, "f", 0, "gemini-cli").unwrap();
        assert_eq!(r.input_new, 100);
        assert_eq!(r.cache_read, 900);
        assert_eq!(r.output, 80);
        assert_eq!(r.reasoning, 20);
    }

    #[test]
    fn copilot_otel_attrs_nested() {
        let line = r#"{"id":"e1","timestamp":"2026-11-05T06:54:22.914Z","type":"assistant.message","data":{"attributes":{"gen_ai.usage.input_tokens":500,"gen_ai.usage.output_tokens":40,"gen_ai.usage.cache_read.input_tokens":300,"gen_ai.request.model":"gpt-5"}}}"#;
        let r = parse_copilot_line(line, "sess").unwrap();
        assert_eq!(r.input_new, 200);
        assert_eq!(r.cache_read, 300);
        assert_eq!(r.output, 40);
        assert_eq!(r.model, "gpt-5");
        assert_eq!(r.session_id, "sess");
    }

    #[test]
    fn droid_token_usage_aliases() {
        let line = r#"{"id":"d1","sessionId":"s","timestamp":1794000000000,"model":"claude-sonnet-4-5","tokenUsage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":500,"thinkingTokens":5}}"#;
        let r = parse_droid_line(line, "f", 0).unwrap();
        assert_eq!(r.input_new, 100);
        assert_eq!(r.cache_read, 500);
        assert_eq!(r.reasoning, 5);
    }

    #[test]
    fn grok_turn_completed_only() {
        let line = r#"{"type":"turn_completed","id":"t1","timestamp":1794000000000,"usage":{"inputTokens":100,"outputTokens":20,"cacheReadTokens":60}}"#;
        let r = parse_grok_line(line, "f", 0, "s").unwrap();
        assert_eq!(r.input_new, 40);
        assert_eq!(r.cache_read, 60);
        assert!(parse_grok_line(r#"{"type":"turn_started"}"#, "f", 0, "s").is_none());
    }

    #[test]
    fn codebuddy_assistant_usage_splits_inclusive_cache() {
        // tokscale 真机夹具：total == input + output 证明 input 含缓存 → 拆出 14720
        let line = r#"{"id":"assistant-1","timestamp":1780000000100,"type":"message","role":"assistant","status":"completed","sessionId":"session-1","providerData":{"model":"glm-5.2","messageId":"msg-1"},"message":{"usage":{"input_tokens":24486,"output_tokens":3,"total_tokens":24489,"cache_read_input_tokens":14720}}}"#;
        let r = parse_codebuddy_line(line, "f", 0).unwrap();
        assert_eq!(r.agent, "codebuddy");
        assert_eq!(r.input_new, 9766);
        assert_eq!(r.cache_read, 14720);
        assert_eq!(r.output, 3);
        assert_eq!(r.model, "glm-5.2");
        assert_eq!(r.session_id, "session-1");
        assert_eq!(r.record_id, "session-1:msg-1");
    }

    #[test]
    fn codebuddy_function_call_usage_and_ambiguous_input() {
        // 用量多数落在 function_call；rawUsage 无 total 时不能证明 input 含缓存 → 保持原值
        let line = r#"{"id":"call-1","timestamp":1780000000100,"type":"function_call","sessionId":"session-2","providerData":{"requestModelId":"minimax-m3-pay","messageId":"msg-2","rawUsage":{"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":3}}}"#;
        let r = parse_codebuddy_line(line, "f", 0).unwrap();
        assert_eq!(r.input_new, 10);
        assert_eq!(r.cache_read, 3);
        assert_eq!(r.output, 2);
        assert_eq!(r.model, "minimax-m3-pay");
        assert_eq!(r.record_id, "session-2:msg-2");
    }

    #[test]
    fn codebuddy_cached_miss_is_exclusive_and_zero_is_authoritative() {
        let line = r#"{"id":"m1","timestamp":1780000000100,"type":"message","role":"assistant","status":"completed","sessionId":"s","providerData":{"messageId":"m-1"},"message":{"usage":{"inputTokens":100,"outputTokens":5,"cacheTokens":100,"cachedMissTokens":0}}}"#;
        let r = parse_codebuddy_line(line, "f", 0).unwrap();
        assert_eq!(r.input_new, 0);
        assert_eq!(r.cache_read, 100);
        assert_eq!(r.output, 5);
    }

    #[test]
    fn codebuddy_skips_incomplete_and_non_usage_records() {
        let incomplete = r#"{"id":"a","timestamp":1780000000100,"type":"message","role":"assistant","status":"incomplete","sessionId":"s","providerData":{"messageId":"m"},"message":{"usage":{"inputTokens":10,"outputTokens":2}}}"#;
        assert!(parse_codebuddy_line(incomplete, "f", 0).is_none());
        let reasoning = r#"{"id":"r","timestamp":1780000000100,"type":"reasoning","sessionId":"s"}"#;
        assert!(parse_codebuddy_line(reasoning, "f", 0).is_none());
        let no_usage = r#"{"id":"u","timestamp":1780000000100,"type":"message","role":"assistant","status":"completed","sessionId":"s","providerData":{"messageId":"m"}}"#;
        assert!(parse_codebuddy_line(no_usage, "f", 0).is_none());
    }

    #[test]
    fn codebuddy_extension_log_anchor_and_usage() {
        let anchor = r#"[2026/7/1 16:56:01.100] [Info] [CraftInvokableAgent] [agent-1]  Model prepared: Kimi-K2.7-Code (kimi-k2.7)"#;
        let (agent, model, ts) = parse_codebuddy_prepared(anchor).unwrap();
        assert_eq!(agent, "agent-1");
        assert_eq!(model, "kimi-k2.7");
        assert!(ts.is_some());

        let usage = r#"[2026/7/1 16:56:02.200] [Info] [AgentReporter] [agent-1]  Agent execution successful with usage: {"inputTokens":140732,"outputTokens":635,"totalTokens":141367,"cacheTokens":76032,"cachedWriteTokens":0,"cachedMissTokens":64700,"lastTokens":71051,"credit":10.38}"#;
        let (agent, settled, tokens) = parse_codebuddy_log_usage(usage).unwrap();
        assert_eq!(agent, "agent-1");
        assert!(settled.is_some());
        assert_eq!(tokens, (64700, 76032, 0, 635, 0));
        assert_eq!(tokens.0 + tokens.1 + tokens.2 + tokens.3 + tokens.4, 141367);
    }

    #[test]
    fn codebuddy_extension_log_mirrored_sinks_share_dedup_key() {
        let a = r#"[2026/7/1 16:56:02.200] [info] [AgentReporter] [agent-1] Agent execution successful with usage: {"inputTokens":10,"outputTokens":2,"totalTokens":12}"#;
        let b = r#"2026-07-01 16:56:02.201 [info] [AgentReporter] [agent-1] Agent execution successful with usage: {"inputTokens":10,"outputTokens":2,"totalTokens":12}"#;
        let (agent_a, settled_a, t_a) = parse_codebuddy_log_usage(a).unwrap();
        let (agent_b, settled_b, t_b) = parse_codebuddy_log_usage(b).unwrap();
        assert_eq!(
            codebuddy_log_dedup_key(&agent_a, settled_a.unwrap(), t_a),
            codebuddy_log_dedup_key(&agent_b, settled_b.unwrap(), t_b),
            "镜像 sink 的毫秒偏差不能产生两条记录"
        );
    }

    #[test]
    fn codebuddy_collect_cli_and_extension_logs_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "bark-cb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let home = dir.join("home");
        let project = home.join(".codebuddy").join("projects").join("c-Users-me-repo");
        std::fs::create_dir_all(&project).unwrap();
        let assistant = r#"{"id":"assistant-1","timestamp":1780000000100,"type":"message","role":"assistant","status":"completed","sessionId":"session-1","providerData":{"model":"glm-5.2","messageId":"msg-1"},"message":{"usage":{"input_tokens":24486,"output_tokens":3,"total_tokens":24489,"cache_read_input_tokens":14720}}}"#;
        let call = r#"{"id":"call-1","timestamp":1780000000100,"type":"function_call","sessionId":"session-1","providerData":{"requestModelId":"minimax-m3-pay","messageId":"msg-2","rawUsage":{"prompt_tokens":10,"completion_tokens":2,"prompt_cache_hit_tokens":3}}}"#;
        std::fs::write(project.join("s1.jsonl"), format!("{assistant}\n{call}\n")).unwrap();
        let subagents = project.join("s1").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let sub = r#"{"id":"sub-msg","timestamp":1780000000200,"type":"message","role":"assistant","status":"completed","sessionId":"sub-1","providerData":{"messageId":"sub-m1"},"message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#;
        std::fs::write(subagents.join("agent-x.jsonl"), format!("{sub}\n")).unwrap();

        let log_dir = home
            .join("AppData")
            .join("Roaming")
            .join("Code")
            .join("logs")
            .join("20260701T000000")
            .join("window1")
            .join("exthost")
            .join("Tencent-Cloud.coding-copilot");
        std::fs::create_dir_all(&log_dir).unwrap();
        let log = concat!(
            "[2026/7/1 16:56:01.100] [Info] [CraftInvokableAgent] [agent-1]  Model prepared: Kimi-K2.7-Code (kimi-k2.7)\n",
            "[2026/7/1 16:56:02.200] [Info] [AgentReporter] [agent-1]  Agent execution successful with usage: {\"inputTokens\":140732,\"outputTokens\":635,\"totalTokens\":141367,\"cacheTokens\":76032,\"cachedWriteTokens\":0,\"cachedMissTokens\":64700}\n"
        );
        std::fs::write(log_dir.join("exthost.log"), log).unwrap();

        let store = UsageStore::new(dir.join("usage.sqlite3"));
        store.set_enabled(true);
        let mut c = Cursors::default();
        collect_codebuddy(&store, &mut c, &home);
        collect_codebuddy_logs(&store, &mut c, &home);

        let rows = store.by_agent("2000-01-01", "2100-01-01");
        let cb = rows.iter().find(|r| r.agent == "codebuddy").expect("CodeBuddy 未入库");
        assert_eq!(cb.requests, 4, "CLI 2 条 + subagent 1 条 + 扩展日志 1 条");
        assert_eq!(cb.input_new, 9766 + 10 + 5 + 64700);
        assert_eq!(cb.cache_read, 14720 + 3 + 76032);
        assert_eq!(cb.output, 3 + 2 + 1 + 635);

        // 重启式重扫（全新游标）幂等
        let mut c2 = Cursors::default();
        collect_codebuddy(&store, &mut c2, &home);
        collect_codebuddy_logs(&store, &mut c2, &home);
        let again = store.by_agent("2000-01-01", "2100-01-01");
        assert_eq!(again.iter().find(|r| r.agent == "codebuddy").unwrap().requests, 4);

        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}