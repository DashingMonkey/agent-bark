//! Codex 本地状态核验（非官方 transcript 格式，2026-10-29 本机验证）。
//! SQLite 只定位 rollout；按精确回合读取提问、回答与明确终态。
//! 日志静默、读取失败、普通工具错误均不代表回合结束。

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

pub const EVENT_PREFIX: &str = "codex-transcript:";
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
    awaiting_async_input: Option<i64>,
    first_observed_at: i64,
    observed_at: i64,
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
                        awaiting_async_input: None,
                        first_observed_at: event.timestamp,
                        observed_at: event.timestamp,
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
        if event.id.starts_with(EVENT_PREFIX)
            && event.timestamp < turn.awaiting_async_input.unwrap_or(turn.observed_at)
            && matches!(
                event.kind,
                EventKind::PermissionRequired | EventKind::ToolFinished
            )
        {
            return false;
        }
        if context.is_user_prompt || event.tool_name.as_deref() == Some("__user_input_answered") {
            turn.awaiting_async_input = None;
        }
        if event.kind == EventKind::PermissionRequired
            && event
                .tool_name
                .as_deref()
                .is_some_and(|name| name.rsplit('.').next() == Some("request_user_input_async"))
        {
            turn.awaiting_async_input = Some(event.timestamp);
        }
        turn.observed_at = turn.observed_at.max(event.timestamp);
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

    /// 等待仅约束显示相位。活动、统计、工具失败仍进入事件管道。
    pub fn preserve_wait(&self, event: &NormalizedEvent) -> bool {
        self.current.get(&event.session_id).is_some_and(|turn| !turn.closed
            && turn.awaiting_async_input.is_some()
            && matches!(event.kind, EventKind::Activity | EventKind::ToolFinished | EventKind::ToolFailed))
    }

    fn lower_bound(turn: &Turn) -> i64 {
        if turn.started_at > 0 { turn.started_at } else { turn.first_observed_at }
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
/// 不遍历全部历史，也不创建或修改 Codex 数据库；schema 变更时保留当前状态。
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
struct Signal {
    kind: EventKind,
    suffix: String,
    tool: Option<String>,
    turn_id: String,
    timestamp: i64,
    message: String,
}

fn parse_line(line: &str) -> Option<Signal> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type")?.as_str()? != "event_msg" {
        return None;
    }
    let payload = value.get("payload")?;
    let ty = payload.get("type")?.as_str()?;
    if !matches!(ty, "task_complete" | "turn_aborted") {
        return None;
    }
    let turn_id = payload.get("turn_id")?.as_str()?.trim();
    if turn_id.is_empty() {
        return None;
    }
    let timestamp = chrono::DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
        .ok()?
        .timestamp_millis();
    if ty == "turn_aborted" || payload.get("error").is_none_or(Value::is_null) {
        return Some(Signal {
            kind: if ty == "turn_aborted" {
                EventKind::RunAborted
            } else {
                EventKind::RunCompleted
            },
            suffix: ty.into(),
            tool: None,
            turn_id: turn_id.into(),
            timestamp,
            message: if ty == "turn_aborted" {
                "用户已中止 Codex 回合".into()
            } else {
                payload
                    .get("last_agent_message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex 回合已完成")
                    .into()
            },
        });
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
    Some(Signal {
        kind: EventKind::RunFailed,
        suffix: String::new(),
        tool: None,
        turn_id: turn_id.into(),
        timestamp,
        message,
    })
}

impl Signal {
    fn event(
        &self,
        sid: &str,
        turn: &Turn,
        session: &crate::state::SessionStatus,
    ) -> Option<NormalizedEvent> {
        if self.turn_id != turn.id || self.timestamp < Turns::lower_bound(turn) {
            return None;
        }
        // 重读尾部时，旧提问/回答不得覆盖更新的 hook 状态。
        if matches!(
            self.kind,
            EventKind::PermissionRequired | EventKind::ToolFinished
        ) && self.timestamp < turn.awaiting_async_input.unwrap_or(session.last_activity)
        {
            return None;
        }
        let mut event = NormalizedEvent::from_raw(
            "codex",
            self.kind,
            &json!({
                "session_id": sid, "cwd": session.cwd, "message": self.message,
            }),
        );
        event.id = format!("{EVENT_PREFIX}{sid}:{}", self.turn_id);
        if !self.suffix.is_empty() {
            event.id.push_str(&format!(":{}", self.suffix));
        }
        event.tool_name = self.tool.clone();
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
    turn_id: Option<String>,
    // 大文件尾部可能没有 task_started；仅用已知起点之后的记录补齐回合身份。
    turn_hint: Option<(String, i64)>,
    // call_id -> (tool_name, timestamp)。异步 accepted 不移除，直到用户消息出现。
    questions: HashMap<String, (String, i64)>,
}

impl Tail {
    fn parse(&mut self, line: &str) -> Option<Signal> {
        if let Some(signal) = parse_line(line) {
            self.questions.clear();
            return Some(signal);
        }
        let value: Value = serde_json::from_str(line).ok()?;
        let payload = value.get("payload")?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(value.get("timestamp")?.as_str()?)
            .ok()?
            .timestamp_millis();
        let ty = payload.get("type")?.as_str()?;
        if self.turn_id.is_none() {
            self.turn_id = self
                .turn_hint
                .as_ref()
                .filter(|(_, started)| *started > 0 && timestamp >= *started)
                .map(|(id, _)| id.clone());
        }
        if value.get("type")?.as_str()? == "event_msg" {
            if ty == "task_started" {
                self.turn_id = payload
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.questions.clear();
            } else if ty == "user_message" && !self.questions.is_empty() {
                self.questions.clear();
                return Some(Signal {
                    kind: EventKind::ToolFinished,
                    suffix: format!("answer:{timestamp}"),
                    tool: Some("__user_input_answered".into()),
                    turn_id: self.turn_id.clone()?,
                    timestamp,
                    message: String::new(),
                });
            }
            return None;
        }
        if value.get("type")?.as_str()? != "response_item" {
            return None;
        }
        // 桌面端的异步回答实测只追加 role=user 的 response_item，未必有 user_message。
        // 有来源标记时必须是真用户，避免系统注入/代理消息误解除等待。
        if ty == "message"
            && payload.get("role").and_then(Value::as_str) == Some("user")
            && !self.questions.is_empty()
            && payload
                .pointer("/internal_chat_message_metadata_passthrough/content_item_metadata")
                .and_then(Value::as_array)
                .is_none_or(|items| {
                    items.iter().any(|item| {
                        item.pointer("/provenance/type").and_then(Value::as_str) == Some("user")
                    })
                })
        {
            self.questions.clear();
            return Some(Signal {
                kind: EventKind::ToolFinished,
                suffix: format!("answer:{timestamp}"),
                tool: Some("__user_input_answered".into()),
                turn_id: self.turn_id.clone()?,
                timestamp,
                message: String::new(),
            });
        }
        let call = payload.get("call_id")?.as_str()?;
        if ty == "function_call" {
            let name = payload.get("name")?.as_str()?;
            if !bark_adapters::claude_style::is_codex_input_tool(name) {
                return None;
            }
            let turn_id = self.turn_id.clone()?;
            // 有界缓存；超限保留等待，不凭缓存溢出推断用户回答。
            if self.questions.len() >= 64 {
                return None;
            }
            self.questions.insert(call.into(), (name.into(), timestamp));
            let args: Value = serde_json::from_str(payload.get("arguments")?.as_str()?).ok()?;
            let message = args
                .get("questions")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|q| {
                            q.get("question")
                                .or_else(|| q.get("title"))
                                .and_then(Value::as_str)
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| "请选择或回答问题".into());
            return Some(Signal {
                kind: EventKind::PermissionRequired,
                suffix: format!("question:{call}"),
                tool: Some(name.into()),
                turn_id,
                timestamp,
                message,
            });
        }
        if ty == "function_call_output" {
            let (name, _) = self.questions.get(call)?;
            let output = payload.get("output")?;
            let parsed: Value = if let Some(s) = output.as_str() {
                serde_json::from_str(s).unwrap_or(Value::Null)
            } else {
                output.clone()
            };
            if name.rsplit('.').next() == Some("request_user_input_async")
                && parsed.get("accepted").and_then(Value::as_bool) == Some(true)
            {
                return None;
            }
            self.questions.remove(call);
            if !self.questions.is_empty() {
                return None;
            }
            return Some(Signal {
                kind: EventKind::ToolFinished,
                suffix: format!("answer:{call}"),
                tool: Some("__user_input_answered".into()),
                turn_id: self.turn_id.clone()?,
                timestamp,
                message: String::new(),
            });
        }
        None
    }

    fn poll(&mut self, path: &Path) -> std::io::Result<Vec<Signal>> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        let replaced = recreated || meta.len() < self.offset;
        if !self.initialized || replaced {
            // 只读有限尾部；按 turn_id + 时间戳过滤旧回合，启动时不补发历史失败。
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = self.offset > 0;
            self.carry.clear();
            if replaced {
                self.questions.clear();
                self.turn_id = None;
            }
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
                    let line = std::mem::take(&mut self.carry);
                    if let Ok(line) = std::str::from_utf8(&line) {
                        if let Some(failure) = self.parse(line) {
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

fn replaces_pending(old: &NormalizedEvent, event: &NormalizedEvent) -> bool {
    let terminal = |e: &NormalizedEvent| matches!(e.kind,
        EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunAborted);
    // 同会话的新回合可以替换旧回合终态；同回合则终态优先、取最后状态。
    old.codex.as_ref().and_then(|c| c.turn_id.as_ref()) != event.codex.as_ref().and_then(|c| c.turn_id.as_ref())
        || ((terminal(event) || !terminal(old)) && event.timestamp >= old.timestamp)
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    // 文件 IO 在独立线程中执行，不阻塞 Tauri/tokio 的事件管道。
    let result = std::thread::Builder::new().name("codex-terminal-watch".into()).spawn(move || {
        let home = std::env::var_os("CODEX_HOME").map(PathBuf::from)
            .unwrap_or_else(|| bark_adapters::InstallEnv::detect().home.join(".codex"));
        let database = home.join("state_5.sqlite");
        let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
        let mut generation = bark_adapters::watch::recheck_generation();
        let mut pending: HashMap<String, NormalizedEvent> = HashMap::new();
        while !tx.is_closed() {
            let requested = bark_adapters::watch::recheck_generation();
            if requested != generation {
                // 主动核验重读有限尾部，补偿漏收事件；事件 id 与回合封口负责去重。
                for tail in tails.values_mut() { tail.initialized = false; }
                generation = requested;
            }
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
                    let tail = tails.entry(path.clone()).or_default();
                    tail.turn_hint = Some((turn.id.clone(), Turns::lower_bound(&turn)));
                    match tail.poll(path) {
                        Ok(failures) => for failure in failures {
                            if let Some(event) = failure.event(&sid, &turn, session) {
                                // 同批只提交最后一个状态，避免已答完的历史问题闪橙色。
                                if pending.get(&sid).is_none_or(|old| replaces_pending(old, &event))
                                {
                                    pending.insert(sid.clone(), event);
                                }
                            }
                        },
                        Err(error) => tracing::debug!(?path, %error, "Codex transcript 读取失败，保留当前状态，等待可靠信号"),
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
        tracing::warn!(%error, "Codex 终态监控启动失败，保留当前状态，等待可靠信号");
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
        assert_eq!(
            parse_line(&normal.to_string()).unwrap().kind,
            EventKind::RunCompleted
        );
        normal["payload"]["type"] = json!("turn_aborted");
        assert_eq!(
            parse_line(&normal.to_string()).unwrap().kind,
            EventKind::RunAborted
        );
        normal["payload"]["type"] = json!("task_complete");
        normal["payload"]["error"] = json!({});
        assert!(parse_line(&normal.to_string()).is_none());
        normal["payload"]["error"] = json!({"message": "Authentication failed"});
        assert_eq!(
            parse_line(&normal.to_string()).unwrap().message,
            "Authentication failed"
        );
    }

    fn record(ty: &str, payload: Value) -> String {
        json!({"timestamp":"2026-10-29T03:00:00Z", "type":ty, "payload":payload}).to_string()
    }

    #[test]
    fn question_waits_for_answer_and_async_acceptance_does_not_resume() {
        for name in ["request_user_input", "functions.request_user_input_async"] {
            let mut tail = Tail::default();
            tail.parse(&record(
                "event_msg",
                json!({"type":"task_started", "turn_id":"t1"}),
            ));
            let question = tail
                .parse(&record(
                    "response_item",
                    json!({
                        "type":"function_call", "name":name, "call_id":"q1",
                        "arguments":r#"{"questions":[{"question":"请选择部署环境"}]}"#,
                    }),
                ))
                .unwrap();
            assert_eq!(question.kind, EventKind::PermissionRequired);
            assert_eq!(question.message, "请选择部署环境");
            assert_eq!(question.turn_id, "t1");
            if name.ends_with("async") {
                assert!(tail.parse(&record("response_item", json!({
                    "type":"function_call_output", "call_id":"q1", "output":"{\"accepted\":true}",
                }))).is_none());
                let answer = tail
                    .parse(&record("event_msg", json!({"type":"user_message"})))
                    .unwrap();
                assert_eq!(answer.kind, EventKind::ToolFinished);
            } else {
                assert!(tail
                    .parse(&record(
                        "response_item",
                        json!({
                            "type":"function_call_output", "call_id":"unrelated", "output":"{}",
                        })
                    ))
                    .is_none());
                let answer = tail.parse(&record("response_item", json!({
                    "type":"function_call_output", "call_id":"q1", "output":"{\"answers\":{}}",
                }))).unwrap();
                assert_eq!(answer.kind, EventKind::ToolFinished);
            }
            assert!(tail.questions.is_empty());
        }
    }

    #[test]
    fn failed_question_call_releases_wait_and_new_turn_drops_old_question() {
        let mut tail = Tail::default();
        tail.parse(&record(
            "event_msg",
            json!({"type":"task_started", "turn_id":"t1"}),
        ));
        let call = record(
            "response_item",
            json!({"type":"function_call", "name":"request_user_input_async",
            "call_id":"q1", "arguments":"{}"}),
        );
        tail.parse(&call).unwrap();
        assert_eq!(
            tail.parse(&record(
                "response_item",
                json!({"type":"function_call_output",
            "call_id":"q1", "output":"tool unavailable"})
            ))
            .unwrap()
            .kind,
            EventKind::ToolFinished
        );
        tail.parse(&call).unwrap();
        tail.parse(&record(
            "event_msg",
            json!({"type":"task_started", "turn_id":"t2"}),
        ));
        assert!(tail.questions.is_empty());
        assert!(tail
            .parse(&record(
                "response_item",
                json!({"type":"function_call_output",
            "call_id":"q1", "output":"{}"})
            ))
            .is_none());
    }

    #[test]
    fn desktop_async_answer_resumes_but_injected_message_does_not() {
        let mut tail = Tail {
            turn_hint: Some(("t1".into(), 1)),
            ..Default::default()
        };
        tail.parse(&record(
            "response_item",
            json!({"type":"function_call",
            "name":"request_user_input_async", "call_id":"q1", "arguments":"{}"}),
        ))
        .unwrap();
        for source in ["system", "user"] {
            let answer = tail.parse(&record(
                "response_item",
                json!({"type":"message", "role":"user",
                    "internal_chat_message_metadata_passthrough": {
                        "content_item_metadata":[{"provenance":{"type":source}}]
                    }
                }),
            ));
            if source == "system" {
                assert!(answer.is_none());
            } else {
                assert_eq!(answer.unwrap().kind, EventKind::ToolFinished);
            }
        }
        assert!(tail.questions.is_empty());
    }

    #[test]
    fn known_turn_start_allows_bounded_tail_without_replaying_older_questions() {
        let mut tail = Tail {
            turn_hint: Some(("t1".into(), 1)),
            ..Default::default()
        };
        let call = record(
            "response_item",
            json!({"type":"function_call", "name":"request_user_input",
            "call_id":"q1", "arguments":"{}"}),
        );
        assert_eq!(tail.parse(&call).unwrap().turn_id, "t1");
        for started in [0, i64::MAX] {
            let mut tail = Tail {
                turn_hint: Some(("t1".into(), started)),
                ..Default::default()
            };
            assert!(tail.parse(&call).is_none());
        }
    }

    #[test]
    fn async_wait_survives_background_activity_until_user_answers() {
        let mut turns = Turns::default();
        turns.accept(&hook("UserPromptSubmit", "t1", 1000, "work"));
        let mut question = hook("PermissionRequest", "t1", 2000, "");
        question.tool_name = Some("functions.request_user_input_async".into());
        assert!(turns.accept(&question));
        assert!(turns.accept(&hook("PreToolUse", "t1", 3000, "")));
        assert!(turns.preserve_wait(&hook("PreToolUse", "t1", 3000, "")));
        assert!(turns.accept(&hook("PostToolUse", "t1", 4000, "")));
        let mut answer = hook("PostToolUse", "t1", 5000, "");
        answer.tool_name = Some("__user_input_answered".into());
        assert!(turns.accept(&answer));
        assert!(turns.accept(&hook("PreToolUse", "t1", 6000, "")));
        question.timestamp = 6500;
        assert!(turns.accept(&question));
        assert!(turns.accept(&hook("Stop", "t1", 7000, "")));
    }

    #[test]
    fn pending_terminal_cannot_hide_question_from_next_turn() {
        let terminal = hook("Stop", "t1", 2000, "");
        let old_question = hook("PermissionRequest", "t1", 1000, "");
        let new_question = hook("PermissionRequest", "t2", 3000, "");
        assert!(!replaces_pending(&terminal, &old_question));
        assert!(replaces_pending(&old_question, &terminal));
        assert!(replaces_pending(&terminal, &new_question));
    }

    #[test]
    fn midturn_observation_supplies_hint_and_delayed_answer_survives_background_activity() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-10-29T03:00:00Z").unwrap().timestamp_millis();
        let mut question = hook("PreToolUse", "t1", at, "");
        question.codex.as_mut().unwrap().transcript_path = None;
        question.kind = EventKind::PermissionRequired;
        question.tool_name = Some("request_user_input_async".into());
        let mut turns = Turns::default();
        assert!(turns.accept(&question));
        assert!(turns.accept(&hook("PreToolUse", "t1", at + 2000, "")));
        let turn = &turns.current["s1"];
        assert_eq!(turn.started_at, 0);
        assert_eq!(Turns::lower_bound(turn), at);
        let mut tail = Tail { turn_hint: Some((turn.id.clone(), Turns::lower_bound(turn))), ..Default::default() };
        assert_eq!(tail.parse(&record("response_item", json!({
            "type":"function_call", "name":"request_user_input_async", "call_id":"q", "arguments":"{}"
        }))).unwrap().turn_id, "t1");
        let mut answer = hook("PostToolUse", "t1", at + 1000, "");
        answer.id = format!("{EVENT_PREFIX}s1:t1:answer");
        answer.tool_name = Some("__user_input_answered".into());
        assert!(turns.accept(&answer), "回答晚到不应被较新的后台活动挡掉");
        assert!(!turns.preserve_wait(&hook("PreToolUse", "t1", at + 3000, "")));
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
            lifecycle: Default::default(),
            active_descendants: 0,
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
