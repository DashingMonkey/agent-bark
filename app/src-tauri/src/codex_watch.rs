//! Codex 错误终态补偿（非官方 transcript 格式，2026-10-15 本机验证）。
//! 只跟踪 hook 已建档的会话，只将 task_complete.error 视为回合失败。
//! 普通 error / 工具失败 / token_count 不是终态，不据此推断失败。

use crate::state::{lock, read, session_key, AppState};
use bark_core::event::CodexContext;
use bark_core::{EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

pub const EVENT_PREFIX: &str = "codex-transcript-failed:";
const MAX_READ: u64 = 512 * 1024;
const MAX_TURNS: usize = 4096;

#[derive(Clone)]
struct Turn {
    id: String,
    path: Option<PathBuf>,
    started_at: i64,
    closed: bool,
    background: bool,
    is_subagent: bool,
}

/// 回合身份独立于通用的 10 秒回声窗口：长工具收尾可能迟到数分钟。
#[derive(Default)]
pub struct Turns {
    current: HashMap<String, Turn>,
    closed: HashSet<(String, String)>,
    closed_order: VecDeque<(String, String)>,
}

impl Turns {
    pub fn accept(&mut self, event: &NormalizedEvent) -> bool {
        let Some(context) = &event.codex else {
            return true;
        };
        let sid = &event.session_id;
        if let Some(tid) = &context.turn_id {
            if self.closed.contains(&(sid.clone(), tid.clone())) {
                return false;
            }
            if let Some(turn) = self.current.get(sid) {
                if turn.id != *tid && (!context.is_user_prompt || event.timestamp < turn.started_at)
                {
                    return false;
                }
            }
            let needs_entry = self.current.get(sid).is_none_or(|turn| turn.id != *tid);
            if needs_entry {
                if self.current.len() >= MAX_TURNS && !self.current.contains_key(sid) {
                    if let Some(oldest) = self
                        .current
                        .iter()
                        .min_by_key(|(_, t)| t.started_at)
                        .map(|(key, _)| key.clone())
                    {
                        self.current.remove(&oldest);
                    }
                }
                self.current.insert(
                    sid.clone(),
                    Turn {
                        id: tid.clone(),
                        path: context.transcript_path.as_ref().map(PathBuf::from),
                        // daemon 晚启动，第一次只见到工具 hook：不假定它是回合起点。
                        started_at: if context.is_user_prompt {
                            event.timestamp
                        } else {
                            0
                        },
                        closed: false,
                        background: context.is_background,
                        is_subagent: event.is_subagent,
                    },
                );
            }
        }
        let Some(turn) = self.current.get_mut(sid) else {
            return !context.is_background;
        };
        if turn.closed || turn.background || context.is_background {
            turn.background |= context.is_background;
            return false;
        }
        if turn.path.is_none() {
            turn.path = context.transcript_path.as_ref().map(PathBuf::from);
        }
        turn.is_subagent |= event.is_subagent;
        if event.id.starts_with(EVENT_PREFIX) && event.timestamp < turn.started_at {
            return false;
        }
        if matches!(
            event.kind,
            EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunAborted
        ) {
            turn.closed = true;
            let key = (sid.clone(), turn.id.clone());
            if self.closed.insert(key.clone()) {
                self.closed_order.push_back(key);
            }
            while self.closed_order.len() > MAX_TURNS {
                if let Some(key) = self.closed_order.pop_front() {
                    self.closed.remove(&key);
                }
            }
        }
        true
    }

    fn watching(&self) -> Vec<(String, Turn)> {
        self.current
            .iter()
            .filter(|(_, t)| !t.closed && !t.background)
            .map(|(sid, turn)| (sid.clone(), turn.clone()))
            .collect()
    }
}

/// 某些 Codex hook 未给出 transcript_path；按精确会话 id 查本机会话索引。
/// 不遍历全部历史，也不创建或修改 Codex 数据库；schema 变更时回退超时回收。
fn lookup_transcript(database: &Path, sid: &str) -> Option<PathBuf> {
    let connection =
        rusqlite::Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    connection.busy_timeout(Duration::from_millis(50)).ok()?;
    let path: String = connection
        .query_row(
            "SELECT rollout_path FROM threads WHERE id = ?1",
            [sid],
            |row| row.get(0),
        )
        .ok()?;
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[derive(Debug)]
struct Failure {
    turn_id: String,
    timestamp: i64,
    message: String,
}

fn parse_line(line: &str) -> Option<Failure> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "task_complete" {
        return None;
    }
    let error = payload.get("error")?.as_object()?;
    let code = error
        .get("codex_error_info")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if code.is_empty() && message.is_empty() {
        return None;
    }
    let turn_id = payload.get("turn_id")?.as_str()?.trim();
    if turn_id.is_empty() {
        return None;
    }
    let timestamp = chrono::DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()?
        .timestamp_millis();
    let message = if code == "usage_limit_exceeded" {
        if message.is_empty() {
            "额度已用完（usage_limit_exceeded）".into()
        } else {
            format!("额度已用完（usage_limit_exceeded）：{message}")
        }
    } else if message.is_empty() {
        format!("Codex 回合失败：{code}")
    } else {
        message.to_owned()
    };
    Some(Failure {
        turn_id: turn_id.into(),
        timestamp,
        message,
    })
}

impl Failure {
    fn event(
        &self,
        sid: &str,
        turn: &Turn,
        session: &crate::state::SessionStatus,
    ) -> Option<NormalizedEvent> {
        if self.turn_id != turn.id || self.timestamp < turn.started_at {
            return None;
        }
        let mut event = NormalizedEvent::from_raw(
            "codex",
            EventKind::RunFailed,
            &json!({
                "session_id": sid, "cwd": session.cwd, "message": self.message,
            }),
        );
        event.id = format!("{EVENT_PREFIX}{sid}:{}", self.turn_id);
        event.timestamp = self.timestamp;
        event.is_subagent = turn.is_subagent || session.parent.is_some();
        event.codex = Some(CodexContext {
            turn_id: Some(self.turn_id.clone()),
            ..Default::default()
        });
        Some(event)
    }
}

#[derive(Default)]
struct Tail {
    offset: u64,
    created: Option<SystemTime>,
    initialized: bool,
    carry: Vec<u8>,
    skip_line: bool,
}

impl Tail {
    fn poll(&mut self, path: &Path) -> std::io::Result<Vec<Failure>> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        if !self.initialized || recreated || meta.len() < self.offset {
            // 只读有限尾部；按 turn_id + 时间戳过滤旧回合，启动时不补发历史失败。
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = self.offset > 0;
            self.carry.clear();
            self.created = meta.created().ok();
            self.initialized = true;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take((meta.len() - self.offset).min(MAX_READ))
            .read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        let mut failures = Vec::new();
        for byte in bytes {
            if byte == b'\n' {
                if !self.skip_line {
                    if let Ok(line) = std::str::from_utf8(&self.carry) {
                        if let Some(failure) = parse_line(line) {
                            failures.push(failure);
                        }
                    }
                }
                self.carry.clear();
                self.skip_line = false;
            } else if !self.skip_line {
                self.carry.push(byte);
                if self.carry.len() >= MAX_READ as usize {
                    self.carry.clear();
                    self.skip_line = true;
                }
            }
        }
        Ok(failures)
    }
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    // 文件 IO 在独立线程中执行，不阻塞 Tauri/tokio 的事件管道。
    let result = std::thread::Builder::new().name("codex-terminal-watch".into()).spawn(move || {
        let home = std::env::var_os("CODEX_HOME").map(PathBuf::from)
            .unwrap_or_else(|| bark_adapters::InstallEnv::detect().home.join(".codex"));
        let database = home.join("state_5.sqlite");
        let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
        let mut pending: HashMap<String, NormalizedEvent> = HashMap::new();
        while !tx.is_closed() {
            if !read(&state.config).agent_disabled("codex") {
                let watching = lock(&state.codex_turns).watching();
                let sessions = lock(&state.active_sessions).clone();
                let mut paths = HashSet::new();
                for (sid, mut turn) in watching {
                    let Some(session) = sessions.get(&session_key("codex", &sid)) else { continue };
                    if turn.path.is_none() {
                        turn.path = lookup_transcript(&database, &sid);
                        if let Some(current) = lock(&state.codex_turns).current.get_mut(&sid) {
                            if current.id == turn.id { current.path = turn.path.clone(); }
                        }
                    }
                    let Some(path) = &turn.path else { continue };
                    paths.insert(path.clone());
                    match tails.entry(path.clone()).or_default().poll(path) {
                        Ok(failures) => for failure in failures {
                            if let Some(event) = failure.event(&sid, &turn, session) {
                                pending.insert(event.id.clone(), event);
                            }
                        },
                        Err(error) => tracing::debug!(?path, %error, "Codex transcript 读取失败，超时回收兜底"),
                    }
                }
                tails.retain(|path, _| paths.contains(path));
                // 满队列重试，不先登记终态；真正的回合身份校验留在管道消费时完成。
                pending.retain(|_, event| {
                    if !sessions.contains_key(&session_key("codex", &event.session_id)) { return false; }
                    match tx.try_send(event.clone()) {
                        Ok(()) => false,
                        Err(mpsc::error::TrySendError::Full(_)) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    }
                });
            } else {
                tails.clear();
                pending.clear();
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    if let Err(error) = result {
        tracing::warn!(%error, "Codex 终态监控启动失败，超时回收兜底");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_adapters::HookAdapter;
    use std::io::Write;

    const FAILURE: &str = r#"{"timestamp":"2026-10-15T02:38:45.966Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","last_agent_message":null,"error":{"message":"You’ve hit your usage limit.","codex_error_info":"usage_limit_exceeded"}}}"#;
    const PREDICTION: &str = "This is an app-generated request for a suggested next user message. The user did not write this message.\n\nPredict the single message.";

    fn hook(name: &str, turn: &str, time: i64, prompt: &str) -> NormalizedEvent {
        let mut event = bark_adapters::claude_style::codex().normalize(name, &json!({
            "session_id": "s1", "turn_id": turn, "transcript_path": "rollout.jsonl", "prompt": prompt,
        })).unwrap();
        event.timestamp = time;
        event
    }

    #[test]
    fn only_explicit_task_complete_errors_are_failures() {
        let failure = parse_line(FAILURE).unwrap();
        assert_eq!(failure.turn_id, "turn-1");
        assert_eq!(failure.timestamp, 1792031925966);
        assert!(failure.message.starts_with("额度已用完"));
        for replacement in [
            FAILURE.replace("task_complete", "error"),
            FAILURE.replace("task_complete", "token_count"),
            FAILURE.replace("task_complete", "item_completed"),
            FAILURE.replace("event_msg", "response_item"),
            FAILURE.replace("turn-1", ""),
            FAILURE.replace("2026-10-15T02:38:45.966Z", "invalid"),
        ] {
            assert!(parse_line(&replacement).is_none());
        }
        let mut normal: Value = serde_json::from_str(FAILURE).unwrap();
        normal["payload"]["error"] = Value::Null;
        assert!(parse_line(&normal.to_string()).is_none());
        normal["payload"]["error"] = json!({});
        assert!(parse_line(&normal.to_string()).is_none());
        normal["payload"]["error"] = json!({"message": "Authentication failed"});
        assert_eq!(
            parse_line(&normal.to_string()).unwrap().message,
            "Authentication failed"
        );
    }

    #[test]
    fn ended_turn_cannot_reopen_after_grace_or_replace_new_turn() {
        let mut turns = Turns::default();
        assert!(turns.accept(&hook("UserPromptSubmit", "turn-1", 1000, "work")));
        let mut failure = hook("Stop", "turn-1", 2000, "");
        failure.kind = EventKind::RunFailed;
        failure.id = format!("{EVENT_PREFIX}s1:turn-1");
        assert!(turns.accept(&failure));
        assert!(!turns.accept(&hook("PostToolUse", "turn-1", 27000, "")));
        assert!(!turns.accept(&hook("PermissionRequest", "turn-1", 28000, "")));
        assert!(!turns.accept(&hook("Stop", "turn-1", 29000, "")));
        assert!(turns.accept(&hook("UserPromptSubmit", "turn-2", 30000, "retry")));
        assert!(!turns.accept(&failure));
        assert!(!turns.accept(&hook("UserPromptSubmit", "turn-1", 31000, "late retry")));
        assert!(!turns.accept(&hook("PostToolUse", "turn-1", 32000, "")));
        assert!(turns.accept(&hook("PostToolUse", "turn-2", 33000, "")));
        assert_eq!(turns.watching()[0].1.id, "turn-2");
    }

    #[test]
    fn prediction_hooks_never_enter_pipeline_or_watch_list() {
        let mut turns = Turns::default();
        assert!(!turns.accept(&hook("UserPromptSubmit", "prediction", 1000, PREDICTION)));
        assert!(turns.watching().is_empty());
        for name in [
            "PreToolUse",
            "PostToolUse",
            "PermissionRequest",
            "Stop",
            "SessionEnd",
        ] {
            assert!(!turns.accept(&hook(name, "prediction", 2000, "")), "{name}");
        }
        // 真实用户回合、另一个并行会话不受影响。
        let mut other = hook("UserPromptSubmit", "other", 3000, "work");
        other.session_id = "s2".into();
        assert!(turns.accept(&other));
        assert!(turns.accept(&hook("UserPromptSubmit", "real", 4000, "work")));
        assert_eq!(turns.watching().len(), 2);
    }

    #[test]
    fn bootstrap_old_failure_and_queued_failure_cannot_end_new_turn() {
        let mut turns = Turns::default();
        assert!(turns.accept(&hook("UserPromptSubmit", "turn-1", 1000, "work")));
        assert!(turns.accept(&hook("UserPromptSubmit", "turn-2", 3000, "new work")));
        let mut old = hook("Stop", "turn-1", 2000, "");
        old.id = format!("{EVENT_PREFIX}s1:turn-1");
        old.kind = EventKind::RunFailed;
        assert!(!turns.accept(&old));
        let (_, turn) = turns.watching().pop().unwrap();
        let session = crate::state::SessionStatus {
            agent: "codex".into(),
            session_id: "s1".into(),
            project: None,
            cwd: "C:/work".into(),
            phase: crate::state::SessionPhase::Thinking,
            prompt: None,
            last_tool: None,
            turn_count: 1,
            tool_calls: 0,
            started_at: 3000,
            last_activity: 4000,
            parent: None,
            parent_key: None,
        };
        let failure = parse_line(FAILURE).unwrap();
        assert!(failure.event("s1", &turn, &session).is_none());
        let matching = Turn {
            id: "turn-1".into(),
            ..turn
        };
        let event = failure.event("s1", &matching, &session).unwrap();
        assert_eq!(event.kind, EventKind::RunFailed);
        assert_eq!(event.cwd, "C:/work");
        assert_eq!(event.id, format!("{EVENT_PREFIX}s1:turn-1"));
    }

    #[test]
    fn missing_metadata_remains_compatible() {
        let mut event = hook("UserPromptSubmit", "turn-1", 1000, "work");
        event.codex = None;
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("codex").is_none());
        let event: NormalizedEvent = serde_json::from_value(json).unwrap();
        assert!(Turns::default().accept(&event));
    }

    #[test]
    fn tail_handles_split_utf8_oversized_lines_and_truncation() {
        let path =
            std::env::temp_dir().join(format!("bark-codex-tail-{}.jsonl", std::process::id()));
        let line = FAILURE.replace("You’ve hit your usage limit.", "额度耗尽");
        let split = line.find('额').unwrap() + 1;
        std::fs::write(&path, &line.as_bytes()[..split]).unwrap();
        let mut tail = Tail::default();
        assert!(tail.poll(&path).unwrap().is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&line.as_bytes()[split..]).unwrap();
        writeln!(file).unwrap();
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        assert!(tail.poll(&path).unwrap().is_empty());
        file.write_all(&vec![b'x'; MAX_READ as usize + 10]).unwrap();
        writeln!(file, "\n{FAILURE}").unwrap();
        assert!(tail.poll(&path).unwrap().is_empty());
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        drop(file);
        std::fs::write(&path, format!("{FAILURE}\n")).unwrap();
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn transcript_lookup_is_read_only_and_uses_exact_session_id() {
        let path =
            std::env::temp_dir().join(format!("bark-codex-index-{}.sqlite", std::process::id()));
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT);
            INSERT INTO threads VALUES ('s1', 'C:/codex/one.jsonl'), ('s2', 'C:/codex/two.jsonl');",
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            lookup_transcript(&path, "s1"),
            Some(PathBuf::from("C:/codex/one.jsonl"))
        );
        assert!(lookup_transcript(&path, "missing").is_none());
        assert!(lookup_transcript(&path, "s1' OR 1=1 --").is_none());
        std::fs::remove_file(&path).unwrap();
        assert!(lookup_transcript(&path, "s1").is_none());
        assert!(!path.exists());
    }
}
