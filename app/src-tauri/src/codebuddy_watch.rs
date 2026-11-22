//! CodeBuddy 回合终态核验（读 hook 声明的 `transcript_path`；非官方格式）。
//!
//! **为什么需要它**：CodeBuddy（尤其是 IDE / VS Code 扩展）的 `Stop` hook 是
//! fire-and-forget，且 IDE 里任务完成、窗口关闭、宿主重启等场景都可能漏掉这一条；
//! 现有 5 分钟核验只唤醒数据库监控与 Codex/Qoder 的观察器，CodeBuddy 没有任何
//! 本地证据源，于是一次漏收的 `Stop` 会把会话永远钉在「思考中」。用户点「检测
//! 运行状态」也只是把同一条空核验再跑一遍。
//!
//! **CodeBuddy 自己的落盘形态**（hook 的 `transcript_path` 每轮都会带，未实测）：
//! - CLI / WebUI：`~/.codebuddy/projects/<mangled-cwd>/<session_id>.jsonl`，
//!   每行一条 OpenAI Responses 风格记录。助手回答是
//!   `{"type":"message","role":"assistant","status":"completed",...}`；
//!   工具调用 / 工具结果是独立记录，`status` 还有 `incomplete`。
//! - IDE / VS Code 扩展：`CodeBuddyExtension/Data/.../history/<ws>/<conversation>/index.json`，
//!   内含 `messages[]`（按顺序的 user / assistant / tool）与 `requests[]`（逐次模型调用）。
//!   助手消息的正文/状态在 `messages/<id>.json` 的 `message` JSON 里（与 CLI 同一套
//!   响应对象），`index.json` 本身只维护顺序。
//!
//! **判据收得很紧**（宁可保留最后已知状态，也不误报）：
//! - **只认最后一条消息是助手回答**，且 `status == completed` 且没有失败/中断标记
//!   （`error` / `isApiErrorMessage` / `aborted` / `providerData.skipRun`）；
//!   后面还有 `function_call` / `function_call_result` / `reasoning` / `user` / `tool`
//!   或任何**非元数据**记录（元数据白名单见 [`is_metadata_record`]），都不算完成
//!   ——工具还在跑、模型还在生成，回合没有结束；
//! - 助手记录本身没有时间戳 = 无法判断归属，直接让上一条候选失效（不沿用旧结论）；
//! - 时间戳必须不早于会话当前回合起点与最近一次 hook 活动；旧回合的完成记录
//!   不能收掉新回合；
//! - 状态机正在等待用户回答/授权（WaitingInput / WaitingPermission）时不补完成：
//!   提问本身可能就是最后一条 `completed` 文本，收档会把真正在等你的会话关掉；
//! - 终态候选必须**稳定 5 秒**才投递：模型可能先落一条文本消息、稍后才追加
//!   `function_call`，不能在半回合上误报完成。这个窗口只是代理证据——若实测到
//!   更大的「文本记录 → 工具记录」间隔，改 [`SETTLE`] 即可；
//! - IDE 的结论**以 `messages/<id>.json` 为准**（每轮重读，索引不变也能看到 body
//!   后落盘）；body 缺失/不可解析时才退回索引条目里的内联副本；
//! - 读不到 / 格式漂移 / 找不到文件 → 保留最后已知状态，等 hook 或下一次核验；
//! - **只读**：不写 CodeBuddy 任何文件；`transcript_path` 缺失或非法时不做全盘扫描，
//!   也不按 session id 查找（CodeBuddy 没有 Qoder 那种精确查找兜底）。

use crate::state::{lock, read, AppState, SessionPhase, SessionStatus};
use bark_core::event::{truncate_chars, MAX_MESSAGE_CHARS};
use bark_core::{EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

/// 合成事件的 id 前缀：状态机据此识别「这是本地 transcript 核验的结论」，
/// 与 hook 事件区分开（回合校验、补投有效期都靠它）。
pub const EVENT_PREFIX: &str = "codebuddy-transcript:";

/// 只读有限尾部 / 单行上限：CLI transcript 可达数 MB，而终态证据就是助手回答本身
/// （长代码输出可能远超 512KB），取 4MB 与 IDE 的上限对齐；超过就当无法核验。
const MAX_READ: u64 = 4 * 1024 * 1024;
/// IDE `index.json` + 单条 message 文件的上限；超过就放弃核验，不整份读进内存。
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 终态候选的稳定窗（见模块注释）。5 秒是「文本记录之后多久不再出现工具/推理记录」
/// 的保守近似，实测到更大间隔时调大即可。
const SETTLE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Terminal {
    /// 去重键：同一条消息重读、主动核验重放都不重复投递。
    key: String,
    /// 助手回答的落盘时间（unix 毫秒）
    timestamp: i64,
    /// 助手回答正文（可为空；空时投递侧用本地化默认文案）
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
    Tool,
}

/// 一条「助手回答」记录（CLI 记录与 IDE 消息共用）。
#[derive(Debug, Clone)]
struct Assistant {
    timestamp: i64,
    /// 显式 `status=completed` 且没有失败/中断标记
    completed: bool,
    message: String,
    key: String,
}

/// IDE `index.json` 的解析结果：索引只维护消息顺序，正文/状态在 `messages/<id>.json`。
/// 索引按 (size, mtime) 缓存；最后一条消息与它的内联副本一起留下，供每轮复用。
struct IndexCache {
    signature: (u64, Option<SystemTime>),
    last: Option<Value>,
    /// 最后一条消息的内联副本（body 文件缺失/不可解析时的兜底）
    inline: Option<Assistant>,
}

/// 单个会话终端（一个 transcript 文件 / index.json）的增量读取状态。
#[derive(Default)]
struct Tail {
    /// CLI JSONL 游标
    offset: u64,
    created: Option<SystemTime>,
    initialized: bool,
    carry: Vec<u8>,
    skip_line: bool,
    /// EOF 处已尝试解析的 carry 长度（内容没变就不重复解析）
    eof_seen: usize,
    last_role: Option<Role>,
    assistant: Option<Assistant>,
    last_user_ts: i64,
    /// IDE `index.json` 的解析缓存（索引本身很少变）
    index: Option<IndexCache>,
    /// 由索引缓存派生的当前候选：body 文件每轮都重看，索引不重解析
    index_assistant: Option<Assistant>,
    /// 当前候选首次被看到的时刻（稳定窗）
    seen: Option<(String, Instant)>,
    /// 已投递过的 key（主动核验重读时去重）
    emitted: HashSet<String>,
}

impl Tail {
    fn reset_jsonl(&mut self) {
        self.offset = 0;
        self.created = None;
        self.initialized = false;
        self.carry.clear();
        self.skip_line = false;
        self.eof_seen = 0;
        self.last_role = None;
        self.assistant = None;
        self.last_user_ts = 0;
        self.seen = None;
    }

    /// 解析一条完整的 JSONL 记录（不完整的行由 [`Self::poll_jsonl`] 攒在 carry 里）。
    fn observe_line(&mut self, line: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            self.observe_value(&value);
        }
    }

    /// 解析一条记录。判据收得很紧：任何**不是元数据**的新记录都表示回合还在跑，
    /// 都会让上一条助手回答不再算数。
    fn observe_value(&mut self, value: &Value) {
        let Some(ty) = value.get("type").and_then(Value::as_str) else {
            return;
        };
        match ty {
            "message" => match value.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let ts = record_ts(value).unwrap_or(0);
                    self.last_user_ts = self.last_user_ts.max(ts);
                    self.last_role = Some(Role::User);
                    self.assistant = None;
                }
                Some("assistant") => {
                    // 先失效上一条：没有时间戳的助手记录同样证明「最后一条已经不是
                    // 那条已完成的回答」，不能沿用旧候选（否则会在半回合上误报）。
                    self.last_role = Some(Role::Assistant);
                    self.assistant = None;
                    let Some(timestamp) = record_ts(value) else {
                        return;
                    };
                    let message = assistant_text(value);
                    let key =
                        record_key(value).unwrap_or_else(|| format!("{timestamp}:{}", message.len()));
                    self.assistant = Some(Assistant {
                        timestamp,
                        completed: is_completed_assistant(value),
                        message,
                        key,
                    });
                }
                // 工具结果是 message + role=tool：助手文本之后还有工具在跑，不能收档。
                Some("tool") => {
                    self.last_role = Some(Role::Tool);
                    self.assistant = None;
                }
                // 宿主写入的 system / 其它消息不改变「最后一条是不是助手回答」。
                _ => {}
            },
            // 只忽略真正的元数据；其余任何记录（function_call / function_call_result /
            // reasoning / 未来新增的 item 类型）都说明回合还在跑。
            _ if is_metadata_record(ty) => {}
            _ => {
                self.last_role = Some(Role::Tool);
                self.assistant = None;
            }
        }
    }

    fn poll_jsonl(&mut self, path: &Path) -> std::io::Result<()> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        let replaced = recreated || meta.len() < self.offset;
        if !self.initialized || replaced {
            // 首次（或主动核验重读）只读有限尾部；跳过的半个首行由 skip_line 丢弃。
            self.reset_jsonl();
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = self.offset > 0;
            self.created = meta.created().ok();
            self.initialized = true;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(meta.len().saturating_sub(self.offset).min(MAX_READ))
            .read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        for byte in bytes {
            if byte == b'\n' {
                if !self.skip_line {
                    let line = std::mem::take(&mut self.carry);
                    if let Ok(line) = std::str::from_utf8(&line) {
                        self.observe_line(line);
                    }
                }
                self.carry.clear();
                self.skip_line = false;
                self.eof_seen = 0;
            } else if !self.skip_line {
                self.carry.push(byte);
                // 超长行（不认识的新 schema）直接丢弃到行尾，避免 carry 无界。
                if self.carry.len() >= MAX_READ as usize {
                    self.carry.clear();
                    self.skip_line = true;
                    self.eof_seen = 0;
                }
            }
        }
        // 最后一条记录可能没有换行（有实现一次写完、不补尾换行）：若它已经是完整
        // JSON 就先按记录处理一次。内容没变不重复解析；补上换行后会再解析一次，
        // observe_value 是幂等的。
        if !self.skip_line && self.carry.len() != self.eof_seen {
            self.eof_seen = self.carry.len();
            if let Ok(value) = serde_json::from_slice::<Value>(&self.carry) {
                self.observe_value(&value);
            }
        }
        Ok(())
    }

    fn poll_index(&mut self, path: &Path) -> std::io::Result<()> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_INDEX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "index.json too large",
            ));
        }
        let signature = (meta.len(), meta.modified().ok());
        if self.index.as_ref().is_none_or(|cache| cache.signature != signature) {
            let bytes = std::fs::read(path)?;
            // 解析失败时不改缓存：下一轮重试（index.json 可能正在整份重写），
            // 期间 candidate 也不会被调用。
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            self.index = Some(IndexCache::parse(signature, &value));
        }
        let Some(cache) = self.index.as_ref() else {
            return Ok(());
        };
        // 状态在 body 文件里：索引没变时它仍可能晚落盘/半写，必须每轮重看。
        self.index_assistant = resolve_assistant(cache, path);
        Ok(())
    }

    fn poll(&mut self, path: &Path) -> std::io::Result<()> {
        if is_index(path) {
            self.poll_index(path)
        } else {
            self.poll_jsonl(path)
        }
    }

    /// 当前 transcript 是否给出一个「本回合已完成」的明确证据。
    ///
    /// 只在稳定窗到期后才克隆正文：每秒轮询不该反复搬运整段回答。
    /// `now` 由调用方传入，测试可以快进稳定窗而不真的 sleep。
    fn candidate(&mut self, started_at: i64, last_activity: i64, now: Instant) -> Option<Terminal> {
        let Some((key, timestamp)) = self
            .current_assistant()
            .map(|assistant| (assistant.key.clone(), assistant.timestamp))
        else {
            self.seen = None;
            return None;
        };
        // 旧回合的完成记录不能收掉新回合：必须晚于本回合起点与最近一次 hook 活动。
        if timestamp < started_at.max(last_activity) {
            self.seen = None;
            return None;
        }
        if self.emitted.contains(&key) {
            self.seen = None;
            return None;
        }
        match &self.seen {
            Some((seen, first)) if seen == &key => {
                if now.saturating_duration_since(*first) < SETTLE {
                    return None;
                }
            }
            _ => {
                self.seen = Some((key, now));
                return None;
            }
        }
        Some(Terminal {
            key: key.clone(),
            timestamp,
            message: self.current_assistant()?.message.clone(),
        })
    }

    /// 当前终点证据（不克隆正文）：CLI 要求最后一条是助手记录；IDE 用 body 文件的结论。
    fn current_assistant(&self) -> Option<&Assistant> {
        if let Some(assistant) = self.index_assistant.as_ref() {
            return assistant.completed.then_some(assistant);
        }
        let assistant = self.assistant.as_ref()?;
        if self.last_role != Some(Role::Assistant)
            || !assistant.completed
            || self.last_user_ts > assistant.timestamp
        {
            return None;
        }
        Some(assistant)
    }
}

fn is_index(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("index.json"))
}

impl IndexCache {
    fn parse(signature: (u64, Option<SystemTime>), root: &Value) -> Self {
        let last = root
            .get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.last())
            .cloned();
        let inline = last.as_ref().and_then(inline_assistant);
        Self { signature, last, inline }
    }
}

/// 当前候选：**body 文件优先**（索引只维护顺序，状态在 `messages/<id>.json`），
/// body 缺失/不可解析时才退回索引条目里的内联副本。body 每轮都重看，索引不重解析。
fn resolve_assistant(cache: &IndexCache, index_path: &Path) -> Option<Assistant> {
    let last = cache.last.as_ref()?;
    if last.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let body = last
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_safe_message_id(id))
        .and_then(|id| read_body(index_path.parent()?, id, record_ts(last)));
    body.or_else(|| cache.inline.clone())
}

/// 读 IDE 的 `messages/<id>.json`。正文/状态以它为准；文件不可读/半写/格式不符时
/// 返回 None，交给调用方退回内联副本——下一轮重试，不做负缓存。
fn read_body(dir: &Path, id: &str, entry_ts: Option<i64>) -> Option<Assistant> {
    let path = dir.join("messages").join(format!("{id}.json"));
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_INDEX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > MAX_INDEX_BYTES {
        return None;
    }
    let wrapper: Value = serde_json::from_slice(&bytes).ok()?;
    let message = wrapper.get("message").and_then(parse_message)?;
    // 没有显式时间戳就判断不了回合归属：不收档（不拿 mtime 猜）。
    let timestamp = entry_ts
        .or_else(|| record_ts(&wrapper))
        .or_else(|| record_ts(&message))?;
    Some(Assistant {
        timestamp,
        completed: is_completed_assistant(&message),
        message: assistant_text(&message),
        key: format!("{id}:{timestamp}"),
    })
}

/// 索引条目自带的内联副本：`message` 可能是 JSON 字符串或对象；部分版本把
/// `status` / `content` 直接写在条目上。没有显式时间戳就不收档。
fn inline_assistant(entry: &Value) -> Option<Assistant> {
    if entry.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = entry.get("message").and_then(parse_message).or_else(|| {
        (entry.get("status").is_some() || entry.get("content").is_some()).then(|| entry.clone())
    })?;
    let timestamp = record_ts(entry).or_else(|| record_ts(&message))?;
    let text = assistant_text(&message);
    Some(Assistant {
        timestamp,
        completed: is_completed_assistant(&message),
        message: text.clone(),
        key: format!(
            "{}:{timestamp}",
            record_key(entry).unwrap_or_else(|| text.len().to_string())
        ),
    })
}

/// 消息 id 只允许单段文件名：它来自 agent 的记录，不能拼出跨目录路径。
fn is_safe_message_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id != "."
        && id != ".."
        && !id.chars().any(|c| matches!(c, '/' | '\\' | ':'))
}

fn parse_message(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        Value::Object(_) => Some(value.clone()),
        _ => None,
    }
}

/// 明确的「助手回答已完成」：显式 `status=completed`，且没有失败/中断标记。
fn is_completed_assistant(value: &Value) -> bool {
    record_status(value).is_some_and(|status| status.eq_ignore_ascii_case("completed"))
        && !has_failure_marker(value)
}

/// 失败/中断标记。各入口写法不同（CLI 记录顶层、`providerData` 内、消息对象内），
/// 命中任何一个都不按完成处理——宁可保留最后已知状态，也不报假完成。
fn has_failure_marker(value: &Value) -> bool {
    fn flagged(value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Bool(flag) => *flag,
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(fields) => !fields.is_empty(),
            Value::Number(number) => number.as_i64() != Some(0),
        }
    }
    ["aborted", "isApiErrorMessage"]
        .iter()
        .any(|key| value.get(*key).is_some_and(flagged))
        || value.pointer("/providerData/skipRun").is_some_and(flagged)
        || value.get("error").is_some_and(flagged)
        || value.pointer("/providerData/error").is_some_and(flagged)
}

/// 不改变「最后一条是不是助手回答」的元数据记录（宿主自己写的会话/回合统计）。
/// **白名单**：不认识的新类型一律按「回合还在跑」处理，避免半回合误报。
fn is_metadata_record(ty: &str) -> bool {
    matches!(
        ty,
        "session-meta" | "file-history-snapshot" | "turn-metrics" | "summary" | "ai-title"
    )
}

/// 助手消息正文：`message.content` 里 `output_text` / `text` 片段拼接；兼容标量。
fn assistant_text(value: &Value) -> String {
    let content = value
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| value.get("content"));
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("output_text" | "text")
                )
            })
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

fn record_key(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| {
            value
                .pointer("/providerData/messageId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        })
        .map(str::to_owned)
}

/// 回合状态：CLI 记录在顶层 `status`，部分入口在 `providerData.status`；
/// 两处都认，避免版本漂移后把明确的 `completed` 读丢。
fn record_status(value: &Value) -> Option<&str> {
    value
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .pointer("/providerData/status")
                .and_then(Value::as_str)
        })
        .filter(|status| !status.is_empty())
}

fn record_ts(value: &Value) -> Option<i64> {
    for key in ["timestamp", "createdAt", "created_at", "time"] {
        let Some(raw) = value.get(key) else { continue };
        if let Some(n) = raw.as_i64() {
            return Some(normalize_ts(n));
        }
        if let Some(s) = raw.as_str() {
            if let Ok(n) = s.parse::<i64>() {
                return Some(normalize_ts(n));
            }
            if let Ok(at) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(at.timestamp_millis());
            }
        }
    }
    None
}

/// CodeBuddy 官方落盘是 epoch 毫秒；个别旧记录/测试夹具会给秒级时间戳，
/// 不归一化会被 `started_at`（毫秒）永远判成「旧回合」。
fn normalize_ts(n: i64) -> i64 {
    if n > 0 && n < 1_000_000_000_000 {
        n.saturating_mul(1000)
    } else {
        n
    }
}

/// Windows 盘符/UNC 路径在非 Windows 主机上 `Path::is_absolute()` 为 false；
/// CodeBuddy 的 hook 路径可能来自 Windows 侧（导入/远程），按原始字符串判定。
fn is_windows_absolute(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\'))
        || raw.starts_with("\\\\")
}

fn transcript_path(session: &SessionStatus) -> Option<PathBuf> {
    let raw = session.transcript_path.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() || is_windows_absolute(raw) {
        path
    } else if !session.cwd.is_empty() {
        Path::new(&session.cwd).join(path)
    } else {
        return None;
    };
    // 子代理日志在 `<主会话>/subagents/` 下：它的完成不代表主会话这个回合（与 Qoder 同口径）。
    if is_subagent_transcript(&path) {
        return None;
    }
    // hook 给的是会话目录时，IDE 的会话记录就在它下面的 index.json。
    Some(if path.is_dir() {
        path.join("index.json")
    } else {
        path
    })
}

fn is_subagent_transcript(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("subagents"))
}

/// 管道消费前的回合校验：本地证据的助手回答时间不得早于该会话当前回合的起点；
/// 会话已经进入等待用户回答/授权时也不收档（提问本身可能就是最后一条 completed 文本）。
///
/// 只看 `started_at`，与 Qoder 同口径：更细的「最后一次 hook 活动」由 watcher 在
/// 投递前判断（`Tail::candidate`）。管道里 hook 事件与本地证据事件可能排队交错；
/// 若这里再比 `last_activity`，一条晚到的工具 hook 会让已经投递的完成事件被静默丢掉，
/// 而 watcher 已按「已投递」去重，反而永久卡住。排队期间真开了新回合时
/// `started_at` 会前移，这里照样能挡住旧完成。
pub fn still_current(session: Option<&SessionStatus>, event: &NormalizedEvent) -> bool {
    session.is_some_and(|session| {
        event.timestamp >= session.started_at
            && !matches!(
                session.phase,
                SessionPhase::WaitingInput | SessionPhase::WaitingPermission
            )
    })
}

/// 助手正文为空时给通知/事件流的默认文案（会推送，按界面语言取词）。
fn completion_message(text: &str, lang: bark_core::Lang) -> String {
    let text = text.trim();
    if !text.is_empty() {
        return truncate_chars(text, MAX_MESSAGE_CHARS);
    }
    match lang {
        bark_core::Lang::Zh => "CodeBuddy 回合已完成（来自本地会话记录）".into(),
        bark_core::Lang::En => {
            "CodeBuddy turn completed (from the local session transcript)".into()
        }
    }
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    // 文件 IO 放独立线程：与 codex_watch / qoder_watch 同一套理由，不阻塞事件管道。
    let result = std::thread::Builder::new()
        .name("codebuddy-transcript-watch".into())
        .spawn(move || {
            let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
            let mut generation = bark_adapters::watch::recheck_generation();
            while !tx.is_closed() {
                let requested = bark_adapters::watch::recheck_generation();
                if requested != generation {
                    // 主动核验：重读 transcript，补偿漏收的终态（emitted 负责去重）。
                    for tail in tails.values_mut() {
                        tail.initialized = false;
                        tail.index = None;
                    }
                    generation = requested;
                }
                if read(&state.config).agent_disabled("codebuddy") {
                    tails.clear();
                } else {
                    let sessions: Vec<SessionStatus> = {
                        let active = lock(&state.active_sessions);
                        active
                            .values()
                            .filter(|s| s.agent == "codebuddy" && s.parent.is_none())
                            .cloned()
                            .collect()
                    };
                    let now = Instant::now();
                    let mut paths = HashSet::new();
                    for session in &sessions {
                        let Some(path) = transcript_path(session) else {
                            continue;
                        };
                        paths.insert(path.clone());
                        let tail = tails.entry(path.clone()).or_default();
                        if let Err(error) = tail.poll(&path) {
                            tracing::debug!(
                                ?path,
                                %error,
                                "CodeBuddy transcript 读取失败，保留当前状态，等待可靠信号"
                            );
                            continue;
                        }
                        // 等待用户回答/授权时不要用「最后一条助手文本」收档：提问本身
                        // 可能就是最后一条 completed 文本，收掉会把真正在等你的会话关掉。
                        // 只补「思考中 / 工具中」；tail 仍保留，避免相位来回切换时丢去重键。
                        if !matches!(
                            session.phase,
                            SessionPhase::Thinking | SessionPhase::ToolRunning
                        ) {
                            continue;
                        }
                        let Some(terminal) =
                            tail.candidate(session.started_at, session.last_activity, now)
                        else {
                            continue;
                        };
                        let mut event = NormalizedEvent::from_raw(
                            "codebuddy",
                            EventKind::RunCompleted,
                            &json!({
                                "session_id": session.session_id.as_str(),
                                "cwd": session.cwd.as_str(),
                                "message": completion_message(&terminal.message, state.lang()),
                            }),
                        );
                        event.id = format!("{EVENT_PREFIX}{}:{}", session.session_id, terminal.key);
                        event.timestamp = terminal.timestamp;
                        if event.project.is_none() {
                            event.project = session.project.clone();
                        }
                        match tx.try_send(event) {
                            Ok(()) => {
                                tracing::info!(
                                    session = %session.session_id,
                                    "CodeBuddy transcript 记录到回合完成，已合成 run_completed"
                                );
                                tail.emitted.insert(terminal.key);
                                tail.seen = None;
                            }
                            // 队列满：不登记已投递，下个周期重试（事件 id 相同，daemon 去重）
                            Err(mpsc::error::TrySendError::Full(_)) => {}
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                    }
                    tails.retain(|path, _| paths.contains(path));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "CodeBuddy 回合终态核验启动失败，保留当前状态，等待可靠信号");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_core::Lang;
    use std::io::Write;

    const USER: &str = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"修一下"}],"timestamp":1780000000000}"#;
    const DONE: &str = r#"{"type":"message","role":"assistant","status":"completed","id":"a1","content":[{"type":"output_text","text":"修好了"}],"timestamp":1780000002000}"#;
    const TOOL: &str = r#"{"type":"function_call","name":"Bash","arguments":"{}","id":"c1","timestamp":1780000001000}"#;

    fn tail_with(line: &str) -> Tail {
        let mut tail = Tail::default();
        tail.observe_line(line);
        tail
    }

    fn temp_root(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agent-bark-codebuddy-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn completed_assistant_is_a_terminal_candidate() {
        let mut tail = tail_with(USER);
        tail.observe_line(DONE);
        let now = Instant::now();
        assert!(
            tail.candidate(1780000000000, 1780000000000, now).is_none(),
            "首次看到候选只登记稳定窗"
        );
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("completed 的助手回答必须能收档");
        assert_eq!(terminal.timestamp, 1780000002000);
        assert_eq!(terminal.message, "修好了");
    }

    #[test]
    fn tool_record_after_assistant_cancels_completion() {
        let mut tail = tail_with(USER);
        tail.observe_line(DONE);
        tail.observe_line(TOOL);
        let now = Instant::now();
        assert!(
            tail.candidate(1780000000000, 1780000000000, now + SETTLE * 2)
                .is_none(),
            "助手文本后还有工具调用 = 回合仍在跑"
        );
    }

    #[test]
    fn new_user_prompt_clears_previous_completion() {
        let mut tail = tail_with(DONE);
        tail.observe_line(
            r#"{"type":"message","role":"user","content":[],"timestamp":1780000003000}"#,
        );
        assert!(
            tail.candidate(1780000000000, 1780000000000, Instant::now() + SETTLE * 2)
                .is_none(),
            "新提示属于下一回合，旧完成不能收掉它"
        );
    }

    #[test]
    fn incomplete_assistant_is_not_a_completion() {
        let mut tail = tail_with(&DONE.replace("\"completed\"", "\"incomplete\""));
        assert!(
            tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
            "incomplete 只表示被丢弃/被取代，不是完成"
        );
    }

    #[test]
    fn timestamp_less_assistant_record_cancels_previous_completion() {
        let mut tail = tail_with(DONE);
        tail.observe_line(
            r#"{"type":"message","role":"assistant","status":"completed","id":"a2","content":[]}"#,
        );
        assert!(
            tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
            "没有时间戳的助手记录无法判断归属，不能沿用上一条的完成"
        );
    }

    #[test]
    fn metadata_record_keeps_completion_but_activity_record_cancels_it() {
        let mut tail = tail_with(DONE);
        tail.observe_line(r#"{"type":"turn-metrics","timestamp":1780000004000}"#);
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        assert!(
            tail.candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1)
            )
            .is_some(),
            "宿主元数据不改变消息顺序，不能挡住完成"
        );
        tail.observe_line(r#"{"type":"reasoning","id":"r1","timestamp":1780000004000}"#);
        assert!(
            tail.candidate(
                1780000000000,
                1780000000000,
                now + SETTLE * 4
            )
            .is_none(),
            "推理记录代表新一轮模型调用，旧完成必须失效"
        );
    }

    #[test]
    fn failed_or_aborted_assistant_is_not_a_completion() {
        for marker in [
            r#""aborted":true,"#,
            r#""isApiErrorMessage":true,"#,
            r#""error":{"message":"boom"},"#,
            r#""providerData":{"skipRun":true},"#,
        ] {
            let line = DONE.replacen(r#""status":"completed","#, &format!("{marker}\"status\":\"completed\","), 1);
            let mut tail = tail_with(&line);
            assert!(
                tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
                "命中失败/中断标记不能按完成处理：{marker}"
            );
        }
        // 标记为 false / 空错误对象时仍是正常完成。
        let mut tail = tail_with(&DONE.replacen(
            r#""status":"completed","#,
            r#""aborted":false,"error":{},"status":"completed","#,
            1,
        ));
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        assert!(tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1)
            )
            .is_some());
    }

    #[test]
    fn seconds_timestamp_is_normalized_to_millis() {
        let mut tail = tail_with(&DONE.replace("1780000002000", "1780000002"));
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("秒级时间戳也要按毫秒参与回合比较");
        assert_eq!(terminal.timestamp, 1780000002000);
    }

    #[test]
    fn later_hook_activity_blocks_stale_completion() {
        let mut tail = tail_with(DONE);
        assert!(
            tail.candidate(1780000000000, 1780000004000, Instant::now() + SETTLE * 2)
                .is_none(),
            "助手回答之后还有更新的 hook 活动 = 回合仍在跑"
        );
    }

    #[test]
    fn emitted_terminal_is_not_repeated() {
        let mut tail = tail_with(DONE);
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .unwrap();
        tail.emitted.insert(terminal.key);
        assert!(
            tail.candidate(
                1780000000000,
                1780000000000,
                now + SETTLE * 2 + Duration::from_millis(2)
            )
            .is_none(),
            "主动核验重读不得重复通知"
        );
    }

    #[test]
    fn jsonl_tail_reads_appended_lines_and_ignores_partial_tail() {
        let root = temp_root("tail");
        let path = root.join("s1.jsonl");
        std::fs::write(&path, format!("{USER}\n")).unwrap();
        let mut tail = Tail::default();
        tail.poll(&path).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{DONE}").unwrap();
        drop(file);
        tail.poll(&path).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        assert!(tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1)
            )
            .is_some());
        // 只写了半行（没有换行）：已有判定不受影响，等下一轮补齐。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(
            file,
            r#"{{"type":"message","role":"assistant","status":"completed""#
        )
        .unwrap();
        drop(file);
        tail.poll(&path).unwrap();
        assert!(tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE * 2 + Duration::from_millis(2)
            )
            .is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn final_record_without_trailing_newline_is_observed() {
        let root = temp_root("no-newline");
        let path = root.join("s1.jsonl");
        // 常见实现：最后一条记录写完不补换行。
        std::fs::write(&path, format!("{USER}\n{DONE}")).unwrap();
        let mut tail = Tail::default();
        tail.poll(&path).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("没有换行的最后一条记录也要能读出来");
        assert_eq!(terminal.message, "修好了");
        // 内容没变时下一轮不重复解析，但判定仍在。
        tail.poll(&path).unwrap();
        assert_eq!(tail.eof_seen, DONE.len());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn large_assistant_record_is_observed() {
        let root = temp_root("large");
        let path = root.join("s1.jsonl");
        let text = "长".repeat(200_000); // 约 600KB，超过旧版 512KB 上限
        let record = serde_json::json!({
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "id": "big",
            "content": [{"type": "output_text", "text": text}],
            "timestamp": 1780000002000_i64,
        })
        .to_string();
        std::fs::write(&path, format!("{USER}\n{record}")).unwrap();
        let mut tail = Tail::default();
        tail.poll(&path).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("长回答（代码输出）不能被当成超长坏行丢掉");
        assert!(terminal.message.len() > 500_000);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recheck_reread_does_not_repeat_emitted_completion() {
        let root = temp_root("recheck");
        let path = root.join("s1.jsonl");
        std::fs::write(&path, format!("{USER}\n{DONE}\n")).unwrap();
        let mut tail = Tail::default();
        tail.poll(&path).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .unwrap();
        tail.emitted.insert(terminal.key);
        // 模拟主动核验：重新初始化并重读尾部。
        tail.initialized = false;
        tail.poll(&path).unwrap();
        assert!(
            tail.candidate(
                1780000000000,
                1780000000000,
                now + SETTLE * 2 + Duration::from_millis(2)
            )
            .is_none(),
            "主动核验重读不得对同一条回答重复通知"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_with_message_file_completes() {
        let root = temp_root("index");
        let conversation = root.join("history").join("ws").join("conv-1");
        let messages = conversation.join("messages");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"u1","role":"user"},{"id":"a1","role":"assistant"}],"requests":[]}"#,
        )
        .unwrap();
        std::fs::write(
            messages.join("a1.json"),
            serde_json::json!({
                "role": "assistant",
                "createdAt": 1780000002000_i64,
                "message": serde_json::json!({
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "改完了"}],
                }).to_string(),
            })
            .to_string(),
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("IDE 助手消息文件里的 status=completed 必须能收档");
        assert_eq!(terminal.timestamp, 1780000002000);
        assert_eq!(terminal.message, "改完了");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_with_inline_message_status_completes() {
        let root = temp_root("index-inline");
        let conversation = root.join("conv");
        std::fs::create_dir_all(&conversation).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"u1","role":"user"},{"id":"a1","role":"assistant","status":"completed","timestamp":1780000002000,"content":[{"type":"output_text","text":"inline"}]}]}"#,
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("index.json 条目自带 status=completed 也要认");
        assert_eq!(terminal.message, "inline");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_picks_up_body_file_written_after_index() {
        let root = temp_root("index-late-body");
        let conversation = root.join("conv");
        let messages = conversation.join("messages");
        std::fs::create_dir_all(&messages).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"u1","role":"user"},{"id":"a1","role":"assistant"}]}"#,
        )
        .unwrap();
        // 索引已更新、body 还没写完（读到的是半截 JSON）。
        std::fs::write(messages.join("a1.json"), r#"{"role":"assis"#).unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        // body 补齐（index.json 本身不变）；下一轮必须重看 body。
        std::fs::write(
            messages.join("a1.json"),
            serde_json::json!({
                "role": "assistant",
                "createdAt": 1780000002000_i64,
                "message": serde_json::json!({
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "改完了"}],
                }).to_string(),
            })
            .to_string(),
        )
        .unwrap();
        let now = Instant::now();
        tail.poll(&conversation.join("index.json")).unwrap();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("索引不变时 body 后落盘也要能补到完成（不能缓存成没有结论）");
        assert_eq!(terminal.message, "改完了");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_body_file_wins_over_inline_preview() {
        let root = temp_root("index-shadow");
        let conversation = root.join("conv");
        let messages = conversation.join("messages");
        std::fs::create_dir_all(&messages).unwrap();
        // 条目只带内容预览、没有状态；权威状态在 body 文件里。
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"a1","role":"assistant","content":[{"type":"output_text","text":"预览"}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            messages.join("a1.json"),
            serde_json::json!({
                "role": "assistant",
                "createdAt": 1780000002000_i64,
                "message": serde_json::json!({
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "正文"}],
                }).to_string(),
            })
            .to_string(),
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        let now = Instant::now();
        assert!(tail.candidate(1780000000000, 1780000000000, now).is_none());
        let terminal = tail
            .candidate(
                1780000000000,
                1780000000000,
                now + SETTLE + Duration::from_millis(1),
            )
            .expect("内联预览不能遮蔽 body 文件里的显式完成状态");
        assert_eq!(terminal.message, "正文");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_rejects_path_like_message_id() {
        let root = temp_root("index-traversal");
        let conversation = root.join("conv");
        std::fs::create_dir_all(conversation.join("messages")).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"../evil","role":"assistant"}]}"#,
        )
        .unwrap();
        // 若按 id 直接拼路径，会读到 conversation/evil.json 这份"完成"记录。
        std::fs::write(
            conversation.join("evil.json"),
            serde_json::json!({
                "createdAt": 1780000002000_i64,
                "message": serde_json::json!({
                    "status": "completed",
                    "content": [{"type": "output_text", "text": "被越权读到"}],
                }).to_string(),
            })
            .to_string(),
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        assert!(
            tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
            "消息 id 必须是单段文件名，不能拼出跨目录路径"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn transcript_path_rejects_subagent_logs() {
        let mut status = session(SessionPhase::Thinking, 1000);
        status.transcript_path =
            Some("C:/x/.codebuddy/projects/p/s1/subagents/agent-a.jsonl".into());
        assert!(
            transcript_path(&status).is_none(),
            "子代理日志的完成不代表主会话这个回合"
        );
        status.transcript_path = Some("C:/x/.codebuddy/projects/p/s1.jsonl".into());
        assert_eq!(
            transcript_path(&status),
            Some(PathBuf::from("C:/x/.codebuddy/projects/p/s1.jsonl"))
        );
        assert!(is_subagent_transcript(Path::new("C:/a/SubAgents/b.jsonl")));
        assert!(!is_subagent_transcript(Path::new("C:/a/s1.jsonl")));
        assert!(is_windows_absolute("C:/x"));
        assert!(is_windows_absolute("c:\\x"));
        assert!(is_windows_absolute("\\\\server\\share"));
        assert!(!is_windows_absolute("C:relative"));
    }

    #[test]
    fn ide_index_with_tool_as_last_message_is_not_completion() {
        let root = temp_root("index-tool");
        let conversation = root.join("conv");
        std::fs::create_dir_all(&conversation).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"u1","role":"user"},{"id":"a1","role":"assistant"},{"id":"t1","role":"tool"}]}"#,
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        assert!(
            tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
            "最后一条是工具消息 = 回合仍在跑"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ide_index_without_status_keeps_last_known_state() {
        let root = temp_root("index-nostatus");
        let conversation = root.join("conv");
        std::fs::create_dir_all(&conversation).unwrap();
        std::fs::write(
            conversation.join("index.json"),
            r#"{"messages":[{"id":"u1","role":"user"},{"id":"a1","role":"assistant","content":[{"type":"output_text","text":"嗯"}]}]}"#,
        )
        .unwrap();
        let mut tail = Tail::default();
        tail.poll(&conversation.join("index.json")).unwrap();
        assert!(
            tail.candidate(0, 0, Instant::now() + SETTLE * 2).is_none(),
            "没有显式 status 时不能靠猜"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn completion_message_falls_back_by_language() {
        assert!(completion_message("", Lang::Zh).contains("CodeBuddy"));
        assert!(completion_message("", Lang::En).contains("CodeBuddy"));
        assert_eq!(completion_message("done", Lang::Zh), "done");
    }

    fn session(phase: SessionPhase, started_at: i64) -> SessionStatus {
        SessionStatus {
            lifecycle: Default::default(),
            active_descendants: 0,
            agent: "codebuddy".into(),
            session_id: "s1".into(),
            project: None,
            cwd: "C:/work".into(),
            transcript_path: None,
            phase,
            prompt: None,
            last_tool: None,
            turn_count: 1,
            tool_calls: 0,
            started_at,
            last_activity: started_at,
            parent: None,
            parent_key: None,
        }
    }

    fn completion_event(timestamp: i64) -> NormalizedEvent {
        let mut event = NormalizedEvent::from_raw(
            "codebuddy",
            EventKind::RunCompleted,
            &json!({ "session_id": "s1" }),
        );
        event.timestamp = timestamp;
        event
    }

    #[test]
    fn still_current_rejects_old_turn_and_waiting_phases() {
        let event = completion_event(2000);
        assert!(still_current(
            Some(&session(SessionPhase::Thinking, 1000)),
            &event
        ));
        assert!(still_current(
            Some(&session(SessionPhase::ToolRunning, 1000)),
            &event
        ));
        assert!(
            !still_current(Some(&session(SessionPhase::WaitingInput, 1000)), &event),
            "等待用户回答时不能用最后一条助手文本收档"
        );
        assert!(
            !still_current(
                Some(&session(SessionPhase::WaitingPermission, 1000)),
                &event
            ),
            "等待授权时同理"
        );
        assert!(
            !still_current(Some(&session(SessionPhase::Thinking, 3000)), &event),
            "事件早于当前回合起点 = 上一回合的旧完成"
        );
        assert!(!still_current(None, &event), "已收档/被移除的会话不补");
    }
}
