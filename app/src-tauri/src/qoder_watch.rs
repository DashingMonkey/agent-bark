//! Qoder（国际版 / 国内版 Qoder CN）回合失败核验（非官方 transcript 读取，
//! 2026-11-16 本机验证）。
//!
//! **问题**：Qoder 的模型请求被服务端拒绝（额度用尽、鉴权失效等）时**不发任何
//! hook**——既没有 `Stop`，也没有 `SessionEnd`。会话于是永远停在「思考中」，
//! 用户不主动移除就一直挂着。
//!
//! 2026-11-16 15:23 本机实测（Qoder CN 0.5.12，账户额度用尽）：hooks 侧最后一条是
//! 15:20:57 的 `UserPromptSubmit`，之后 transcript 在 15:23:18 追加一条
//! **合成助手消息**（`message.model = "<synthetic>"`）：
//!
//! ```json
//! {"type":"assistant","timestamp":"2026-11-16T07:23:18.887Z",
//!  "message":{"model":"<synthetic>","role":"assistant",
//!             "content":[{"type":"text","text":"inference.remoteChat.stream failed (403): {\"pricingUrl\":\"https://qoder.com.cn/pricing?client=qoder\"}"}]},
//!  "isApiErrorMessage":true,"error":"authentication_failed"}
//! ```
//!
//! **对策**：daemon 每秒增量尾读**当前回合**的 transcript，看到这种引擎自己写的
//! 失败标记就合成 `run_failed`，走既有历史 / 通知 / 流光 / 音效管道。
//!
//! **判据收得很紧**（宁可保留最后已知状态，也不误报）：
//! - 只认 `type=assistant` 且 `isApiErrorMessage=true`（或 `model="<synthetic>"`）
//!   的记录；`aborted=true` 的合成消息属于用户主动中断，不按失败处理；
//! - 必须是**当前回合**：错误时间戳不早于会话的 `started_at`；之后又有 `user`
//!   记录（新提示）或正常助手回复（重试成功）都不算；
//! - 会话必须仍在运行中表里，且事件管道消费前再核验一次回合起点
//!   （防 watcher 与 hook 排队交错时用旧失败收掉新回合）；
//! - **只读**：不写 Qoder 任何文件。路径优先用 hook 声明的 `transcript_path`，
//!   缺失时按**精确 session id** 在 `~/.qoder/projects/<项目>/<id>.jsonl` 与
//!   `~/.qoder-cn/projects/...` 下查找，不遍历历史内容、不按 mtime 猜会话；
//! - 读不到 / 格式漂移 / 找不到文件 → 保留最后已知状态，等 hook 或下一次核验。

use crate::state::{lock, read, AppState, SessionStatus};
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
pub const EVENT_PREFIX: &str = "qoder-transcript:";

/// 只读有限尾部：transcript 可达数 MB，一回合只关心最后这些字节。
const MAX_READ: u64 = 512 * 1024;
/// 找不到 transcript 时的重试间隔。Qoder 先建会话、后落文件（实测同一秒内），
/// 但目录扫描不该每秒都做——索引不到就 10 秒后再试。
const LOOKUP_RETRY: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 一条引擎写下的 API 错误记录（当前回合的明确失败证据）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiError {
    /// transcript 记录时间（unix 毫秒）
    timestamp: i64,
    /// 记录里的 `error` 字段（如 `authentication_failed`），可为空
    code: String,
    /// 助手消息正文（如 `inference.remoteChat.stream failed (403): {...}`）
    text: String,
}

/// 单个会话 transcript 的增量读取状态。
///
/// 每个文件一个实例，跨回合复用：判定时用**当前** `started_at` 过滤，所以上一个
/// 回合遗留的错误不会被当成本回合的失败（时间戳更早 + 其后通常还有新 `user` 记录）。
#[derive(Default)]
struct Tail {
    offset: u64,
    created: Option<SystemTime>,
    initialized: bool,
    carry: Vec<u8>,
    skip_line: bool,
    /// 最近一条 `user` 记录的时间：晚于错误时间 = 用户已经发下一条提示，旧错误作废
    last_user_ts: i64,
    /// 最近一条 assistant 记录如果是 API 错误，保留它；正常回复会清空
    api_error: Option<ApiError>,
    /// 已经投递过的错误时间戳：重读尾部（主动核验）与每秒轮询都不重复通知
    emitted_ts: Option<i64>,
}

impl Tail {
    fn reset_records(&mut self) {
        self.last_user_ts = 0;
        self.api_error = None;
        self.emitted_ts = None;
    }

    /// 解析一条完整的 JSONL 记录（不完整的行由 [`Self::poll`] 攒在 carry 里）。
    fn observe(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else { return };
        let Some(timestamp) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.timestamp_millis())
        else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("user") => self.last_user_ts = self.last_user_ts.max(timestamp),
            Some("assistant") => {
                let aborted = value.get("aborted").and_then(Value::as_bool).unwrap_or(false);
                // 引擎两种写法都实测存在：落盘的合成消息带 isApiErrorMessage；
                // 内存里先置 model="<synthetic>"，落盘前才补标记——两个都认。
                let marked = value
                    .get("isApiErrorMessage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let synthetic = value.pointer("/message/model").and_then(Value::as_str)
                    == Some("<synthetic>");
                if (marked || synthetic) && !aborted {
                    self.api_error = Some(ApiError {
                        timestamp,
                        code: value
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        text: assistant_text(&value),
                    });
                } else {
                    // 正常助手回复（可能发生在一次失败重试之后）：上一轮的 API 错误
                    // 已不再是本回合的终态，不能据此收档。
                    self.api_error = None;
                }
            }
            _ => {}
        }
    }

    /// 当前回合的失败证据；`started_at` 是该会话本回合的起点（unix 毫秒）。
    fn candidate(&self, started_at: i64) -> Option<&ApiError> {
        let error = self.api_error.as_ref()?;
        if error.timestamp < started_at {
            return None;
        }
        if self.last_user_ts > error.timestamp {
            return None;
        }
        if self.emitted_ts == Some(error.timestamp) {
            return None;
        }
        Some(error)
    }

    /// 增量读取到文件尾；只把新增的完整行交给 [`Self::observe`]。
    fn poll(&mut self, path: &Path) -> std::io::Result<()> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        let replaced = recreated || meta.len() < self.offset;
        if !self.initialized || replaced {
            // 首次只读有限尾部；跳过的半个首行由 skip_line 丢弃。
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = self.offset > 0;
            self.carry.clear();
            if replaced {
                self.reset_records();
            }
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
                        self.observe(line);
                    }
                }
                self.carry.clear();
                self.skip_line = false;
            } else if !self.skip_line {
                self.carry.push(byte);
                // 超长行（不认识的新 schema）直接丢弃到行尾，避免 carry 无界。
                if self.carry.len() >= MAX_READ as usize {
                    self.carry.clear();
                    self.skip_line = true;
                }
            }
        }
        Ok(())
    }
}

/// 助手消息正文：`message.content` 里 `type=text` 的片段拼接。
fn assistant_text(value: &Value) -> String {
    match value.pointer("/message/content") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    }
}

/// 从错误正文里取 HTTP 状态（实测形如 `inference.remoteChat.stream failed (403): ...`）。
fn http_status(text: &str) -> Option<&str> {
    let start = text.find("failed (")? + "failed (".len();
    let rest = &text[start..];
    let code = &rest[..rest.find(')')?];
    (!code.is_empty() && code.bytes().all(|b| b.is_ascii_digit())).then_some(code)
}

/// 取错误正文里服务端给的 pricing 链接。
///
/// 实测额度用尽时 403 响应体就是 `{"pricingUrl":"https://qoder.com.cn/pricing?client=qoder"}`；
/// 把它带进通知，比让用户自己回 Qoder 界面找入口更有用。
fn pricing_url(text: &str) -> Option<&str> {
    let start = text.find("\"pricingUrl\"")? + "\"pricingUrl\"".len();
    let rest = text[start..].trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let url = &rest[..rest.find('"')?];
    (!url.is_empty()).then_some(url)
}

/// 错误记录 → 通知/历史里的失败原因。`lang`：message 会进事件流与推送通知，
/// 必须按界面语言生成（与 codex_watch 同一约定）。
///
/// 403 + pricing 链接是本机实测的额度用尽形态；`authentication_failed` 在 Qoder 自己的
/// 代码里也兼作「登录失效」信号（`onAuthExpired`），所以文案把两种可能都说清楚，
/// 不硬下「额度用尽」的结论。其余错误保留引擎原文（截断后），便于排查。
fn failure_message(error: &ApiError, lang: bark_core::Lang) -> String {
    let text = error.text.trim();
    if let Some(url) = pricing_url(text) {
        return match (lang, http_status(text)) {
            (bark_core::Lang::Zh, Some(code)) => {
                format!("Qoder 账户额度或套餐不足（HTTP {code}），请查看 {url}")
            }
            (bark_core::Lang::Zh, None) => format!("Qoder 账户额度或套餐不足，请查看 {url}"),
            (bark_core::Lang::En, Some(code)) => {
                format!("Qoder quota or plan limit reached (HTTP {code}); see {url}")
            }
            (bark_core::Lang::En, None) => format!("Qoder quota or plan limit reached; see {url}"),
        };
    }
    if error.code == "authentication_failed" {
        return match lang {
            bark_core::Lang::Zh => "Qoder 登录状态失效或账户额度不足，请重新登录或检查账户额度".into(),
            bark_core::Lang::En => {
                "Qoder login expired or account quota insufficient; sign in again or check your quota".into()
            }
        };
    }
    if text.is_empty() {
        return match lang {
            bark_core::Lang::Zh if error.code.is_empty() => "Qoder 回合失败：模型请求被拒绝".into(),
            bark_core::Lang::Zh => format!("Qoder 回合失败：{}", error.code),
            bark_core::Lang::En if error.code.is_empty() => {
                "Qoder turn failed: model request rejected".into()
            }
            bark_core::Lang::En => format!("Qoder turn failed: {}", error.code),
        };
    }
    truncate_chars(text, MAX_MESSAGE_CHARS)
}

/// 管道消费前的回合校验：错误时间戳不早于该会话当前回合的起点。
///
/// watcher 投递与 hook 投递共用一个队列；排队期间完全可能已经开了新回合
/// （`UserPromptSubmit` 重置 `started_at`）。此时上一回合的失败不能再把新回合收掉，
/// 也不能弹失败通知。
pub fn still_current(session: Option<&SessionStatus>, event: &NormalizedEvent) -> bool {
    session.is_some_and(|session| event.timestamp >= session.started_at)
}

/// session_id → transcript 的精确查找结果缓存。
#[derive(Default)]
struct Lookup {
    found: HashMap<String, PathBuf>,
    /// 没找到的时刻：10 秒内不重复扫目录（会话刚建、文件还没落盘的常见情形）
    missing_at: HashMap<String, Instant>,
}

impl Lookup {
    fn resolve(&mut self, roots: &[PathBuf], sid: &str, now: Instant) -> Option<PathBuf> {
        if let Some(path) = self.found.get(sid) {
            if path.is_file() {
                return Some(path.clone());
            }
            self.found.remove(sid);
        }
        if self
            .missing_at
            .get(sid)
            .is_some_and(|at| now.duration_since(*at) < LOOKUP_RETRY)
        {
            return None;
        }
        match find_transcript(roots, sid) {
            Some(path) => {
                self.missing_at.remove(sid);
                self.found.insert(sid.to_string(), path.clone());
                Some(path)
            }
            None => {
                self.missing_at.insert(sid.to_string(), now);
                None
            }
        }
    }
}

/// Qoder 两个版本各自的会话日志根目录（存在哪个扫哪个）。
fn transcript_roots(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".qoder").join("projects"),
        home.join(".qoder-cn").join("projects"),
    ]
}

/// 按**精确 session id** 在各项目目录下找 `<sid>.jsonl`。
///
/// 不按 mtime / 最近修改猜：同名文件必须就是这条会话自己的日志，否则宁可不核验。
fn find_transcript(roots: &[PathBuf], sid: &str) -> Option<PathBuf> {
    // 只接受单段文件名：session id 来自外部输入，不能拼出跨目录路径。
    if sid.is_empty()
        || sid.len() > 256
        || sid == "."
        || sid == ".."
        || sid.chars().any(|c| matches!(c, '/' | '\\' | ':'))
    {
        return None;
    }
    let name = format!("{sid}.jsonl");
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            if entry.path().join(&name).is_file() {
                return Some(entry.path().join(&name));
            }
        }
    }
    None
}

/// hook 声明的路径优先；文件还没落盘、指向子代理日志或没有该字段时，退回按 session id 查找。
fn transcript_of(
    session: &SessionStatus,
    lookup: &mut Lookup,
    roots: &[PathBuf],
    now: Instant,
) -> Option<PathBuf> {
    if let Some(path) = &session.transcript_path {
        let path = PathBuf::from(path);
        // 子代理日志在 `<主会话>/subagents/` 下：它的失败不是主回合的失败，
        // 主会话若也失败会有自己的 transcript 记录。
        if is_subagent_transcript(&path) {
            return None;
        }
        if path.is_file() {
            return Some(path);
        }
    }
    lookup.resolve(roots, &session.session_id, now)
}

fn is_subagent_transcript(path: &Path) -> bool {
    path.components()
        .any(|part| part.as_os_str().eq_ignore_ascii_case("subagents"))
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    // 文件 IO 放独立线程：与 codex_watch 同一套理由，不阻塞 Tauri/tokio 事件管道。
    let result = std::thread::Builder::new()
        .name("qoder-transcript-watch".into())
        .spawn(move || {
            let roots = transcript_roots(&bark_adapters::InstallEnv::detect().home);
            let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
            let mut lookup = Lookup::default();
            let mut generation = bark_adapters::watch::recheck_generation();
            while !tx.is_closed() {
                let requested = bark_adapters::watch::recheck_generation();
                if requested != generation {
                    // 主动核验：重读有限尾部，补偿漏收的行（emitted_ts 负责去重）。
                    for tail in tails.values_mut() {
                        tail.initialized = false;
                    }
                    lookup = Lookup::default();
                    generation = requested;
                }
                if read(&state.config).agent_disabled("qoder") {
                    tails.clear();
                    lookup = Lookup::default();
                } else {
                    let sessions: Vec<SessionStatus> = {
                        let active = lock(&state.active_sessions);
                        active.values().filter(|s| s.agent == "qoder").cloned().collect()
                    };
                    let now = Instant::now();
                    let mut paths = HashSet::new();
                    // 子代理没有独立的 projects 日志（它们在父会话目录的 subagents/ 下）：
                    // 主会话的失败会把自己的子代理一起收掉，这里不单独盯子代理。
                    for session in sessions.iter().filter(|s| s.parent.is_none()) {
                        let Some(path) = transcript_of(session, &mut lookup, &roots, now) else {
                            continue;
                        };
                        paths.insert(path.clone());
                        let tail = tails.entry(path.clone()).or_default();
                        if let Err(error) = tail.poll(&path) {
                            tracing::debug!(
                                ?path,
                                %error,
                                "Qoder transcript 读取失败，保留当前状态，等待可靠信号"
                            );
                            continue;
                        }
                        let Some(error) = tail.candidate(session.started_at).cloned() else {
                            continue;
                        };
                        let mut event = NormalizedEvent::from_raw(
                            "qoder",
                            EventKind::RunFailed,
                            &json!({
                                "session_id": session.session_id.as_str(),
                                "cwd": session.cwd.as_str(),
                                "message": failure_message(&error, state.lang()),
                            }),
                        );
                        event.id = format!("{EVENT_PREFIX}{}:{}", session.session_id, error.timestamp);
                        event.timestamp = error.timestamp;
                        if event.project.is_none() {
                            event.project = session.project.clone();
                        }
                        match tx.try_send(event) {
                            Ok(()) => {
                                tracing::info!(
                                    session = %session.session_id,
                                    "Qoder transcript 记录到模型请求失败，已合成 run_failed"
                                );
                                tail.emitted_ts = Some(error.timestamp);
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
        tracing::warn!(%error, "Qoder 回合终态核验启动失败，保留当前状态，等待可靠信号");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_core::Lang;
    use std::fs::OpenOptions;
    use std::io::Write;

    /// 2026-11-16 15:23 本机额度用尽的真实记录（逐字取自 transcript 末尾）。
    const REAL_QUOTA_LINE: &str = r#"{"type":"assistant","uuid":"0f464f9f-8663-4c01-b430-00df0843aefe","timestamp":"2026-11-16T07:23:18.887Z","message":{"id":"820f7687-1e01-455b-b7cc-dbebaa44ff75","type":"message","role":"assistant","model":"<synthetic>","stop_reason":null,"stop_sequence":null,"container":null,"context_management":null,"content":[{"type":"text","text":"inference.remoteChat.stream failed (403): {\"pricingUrl\":\"https://qoder.com.cn/pricing?client=qoder\"}"}]},"isApiErrorMessage":true,"error":"authentication_failed"}"#;
    const PROMPT: &str = r#"{"type":"user","timestamp":"2026-11-16T07:20:58.088Z","message":{"role":"user","content":[{"type":"text","text":"work"}]}}"#;

    fn millis(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    fn temp_root(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agent-bark-qoder-{tag}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn real_quota_line_is_detected_as_turn_failure() {
        let mut tail = Tail::default();
        tail.observe(PROMPT);
        tail.observe(REAL_QUOTA_LINE);
        let error = tail
            .candidate(millis("2026-11-16T07:20:58.088Z"))
            .expect("额度用尽的合成助手消息必须判回合失败");
        assert_eq!(error.code, "authentication_failed");
        assert_eq!(error.timestamp, millis("2026-11-16T07:23:18.887Z"));
        let message = failure_message(error, Lang::Zh);
        assert!(message.contains("HTTP 403"), "{message}");
        assert!(message.contains("https://qoder.com.cn/pricing?client=qoder"), "{message}");
        let message_en = failure_message(error, Lang::En);
        assert!(message_en.contains("HTTP 403"), "{message_en}");
        assert!(message_en.contains("https://qoder.com.cn/pricing?client=qoder"), "{message_en}");
    }

    #[test]
    fn only_the_current_turn_error_counts() {
        let prompt_at = millis("2026-11-16T07:20:58.088Z");
        let error_at = millis("2026-11-16T07:23:18.887Z");
        let mut tail = Tail::default();
        tail.observe(PROMPT);
        tail.observe(REAL_QUOTA_LINE);
        assert!(tail.candidate(prompt_at).is_some());
        assert!(
            tail.candidate(error_at + 1).is_none(),
            "错误早于本回合起点 = 上一回合的旧账，不能收掉当前会话"
        );
        // 用户又发了一条提示：旧错误属于上一回合，不能压掉新回合。
        tail.observe(r#"{"type":"user","timestamp":"2026-11-16T07:25:00.000Z","message":{"role":"user","content":[]}}"#);
        assert!(tail.candidate(prompt_at).is_none());
    }

    #[test]
    fn normal_reply_after_error_means_retry_succeeded() {
        let mut tail = Tail::default();
        tail.observe(REAL_QUOTA_LINE);
        assert!(tail.candidate(0).is_some());
        tail.observe(r#"{"type":"assistant","timestamp":"2026-11-16T07:23:30.000Z","message":{"role":"assistant","model":"auto","content":[{"type":"text","text":"ok"}]}}"#);
        assert!(
            tail.candidate(0).is_none(),
            "错误之后又有正常助手回复 = 重试成功，不能报失败"
        );
    }

    #[test]
    fn user_aborted_synthetic_message_is_not_a_failure() {
        let mut tail = Tail::default();
        tail.observe(&REAL_QUOTA_LINE.replace(
            "\"isApiErrorMessage\":true",
            "\"isApiErrorMessage\":true,\"aborted\":true",
        ));
        assert!(
            tail.candidate(0).is_none(),
            "用户主动中断由 SessionEnd 收档，不能报成回合失败"
        );
    }

    #[test]
    fn emitted_failure_is_not_repeated_on_reread() {
        let mut tail = Tail::default();
        tail.observe(REAL_QUOTA_LINE);
        let error = tail.candidate(0).cloned().unwrap();
        tail.emitted_ts = Some(error.timestamp);
        assert!(tail.candidate(0).is_none(), "主动核验重读尾部不得重复通知");
        // 新回合（更晚的起点）里同一个错误也不能再响一次。
        assert!(tail.candidate(error.timestamp + 1).is_none());
    }

    #[test]
    fn failure_message_variants_stay_actionable() {
        let auth = ApiError {
            timestamp: 1,
            code: "authentication_failed".into(),
            text: "boom".into(),
        };
        assert!(failure_message(&auth, Lang::Zh).contains("登录状态失效"));
        assert!(failure_message(&auth, Lang::En).contains("login expired"));
        let other = ApiError {
            timestamp: 1,
            code: "server_error".into(),
            text: "upstream unavailable".into(),
        };
        assert_eq!(failure_message(&other, Lang::Zh), "upstream unavailable");
        assert_eq!(failure_message(&other, Lang::En), "upstream unavailable");
        let empty = ApiError { timestamp: 1, code: String::new(), text: String::new() };
        assert!(failure_message(&empty, Lang::Zh).contains("模型请求被拒绝"));
        assert!(failure_message(&empty, Lang::En).contains("model request rejected"));
    }

    #[test]
    fn transcript_lookup_is_exact_and_read_only() {
        let root = temp_root("lookup");
        let project = root.join("D--work-demo");
        std::fs::create_dir_all(&project).unwrap();
        let file = project.join("sid-1.jsonl");
        std::fs::write(&file, "").unwrap();
        assert_eq!(find_transcript(std::slice::from_ref(&root), "sid-1"), Some(file));
        assert!(
            find_transcript(std::slice::from_ref(&root), "sid").is_none(),
            "必须精确匹配，不能按前缀猜会话"
        );
        assert!(find_transcript(std::slice::from_ref(&root), "../sid-1").is_none());
        assert!(find_transcript(std::slice::from_ref(&root), "").is_none());
        assert!(find_transcript(&[root.join("missing")], "sid-1").is_none());
        // 相对路径 + 正斜杠：Windows 的 components() 同样认 '/'，两平台都能测到分隔逻辑
        assert!(is_subagent_transcript(Path::new(
            "q/.qoder-cn/projects/p/s1/subagents/agent-a.jsonl"
        )));
        assert!(!is_subagent_transcript(Path::new(
            "q/.qoder-cn/projects/p/s1.jsonl"
        )));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn tail_reads_appended_lines_and_ignores_partial_tail() {
        let root = temp_root("tail");
        let path = root.join("s1.jsonl");
        std::fs::write(&path, format!("{PROMPT}\n")).unwrap();
        let mut tail = Tail::default();
        tail.poll(&path).unwrap();
        assert!(tail.candidate(0).is_none(), "只有提示、没有失败，不能报错");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{REAL_QUOTA_LINE}").unwrap();
        drop(file);
        tail.poll(&path).unwrap();
        assert!(tail.candidate(0).is_some());
        // 只写了半行（没有换行）：当前判定不受影响，等下一轮补齐。
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(
            file,
            r#"{{"type":"assistant","timestamp":"2026-11-16T07:30:00Z","message":{{"model":"auto","content":[]}}"#
        )
        .unwrap();
        drop(file);
        tail.poll(&path).unwrap();
        assert!(tail.candidate(0).is_some(), "半行不能改变已有判定");
        std::fs::remove_dir_all(&root).ok();
    }
}
