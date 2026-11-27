//! Antigravity 本地辅助核验（2026-11-27 本机实测，Antigravity 2.35.2 / macOS）。
//!
//! 解决两类 Antigravity hooks 自身无法单独闭环的问题：
//!
//! 1. **手动中止不触发 `Stop` hook**：思考中点击停止时，宿主立即在 `language_server.log`
//!    写下 `CancelCascadeInvocation requested: cascade_id="<id>"` 与
//!    `Cancel requested for cascade <id>`，但不会执行 `Stop` hook，会话因而卡在「思考中」。
//!    本模块在独立线程每秒增量尾读 Antigravity 日志，识别明确取消记录后合成 `RunAborted`，
//!    走统一 `evidence` 注册表校验回合时间后收场（不通知、不亮失败色/完成色）。
//! 2. **并行工具调用冲掉等待相位**：模型在同一回合内把 `ask_question`（或 `ask_permission`）
//!    与其它轻量工具（如 `update_focus`）并行调用时，后者的 `PreToolUse` / `PostToolUse`
//!    会紧随其后到达，若不保留等待态，橙色等待指示会被 1ms 后的工具事件打回思考中。
//!    这里按会话跟踪尚未收尾的等待工具，在对应 `PostToolUse`、下一次 `PreInvocation`
//!    或终态事件到达前维持等待相位。
//!
//! 非官方日志格式：日志不存在、读取失败或格式变更时静默保留最后已知状态，绝不推断成功或失败。

use crate::state::{lock, read, session_key, AppState, SessionStatus};
use bark_adapters::antigravity::base_tool_name;
use bark_core::{EventKind, NormalizedEvent};
use chrono::{Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

/// 本地日志合成中止事件的 id 前缀（在 `evidence` 注册表登记）。
pub const EVENT_PREFIX: &str = "antigravity-log-abort:";

const MAX_READ: u64 = 256 * 1024;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_WAIT_SESSIONS: usize = 4096;
const MAX_WAITS_PER_SESSION: usize = 32;

/// 每个 Antigravity 会话尚未收尾的交互式等待工具列表（规范化后的工具末段名）。
#[derive(Default)]
pub struct Waits {
    pending: HashMap<String, Vec<String>>,
}

impl Waits {
    fn clear(&mut self, sid: &str) {
        self.pending.remove(sid);
    }

    fn record_wait(&mut self, sid: &str, tool: &str) {
        if !self.pending.contains_key(sid) && self.pending.len() >= MAX_WAIT_SESSIONS {
            if let Some(any) = self.pending.keys().next().cloned() {
                self.pending.remove(&any);
            }
        }
        let entries = self.pending.entry(sid.to_string()).or_default();
        if entries.len() < MAX_WAITS_PER_SESSION {
            entries.push(base_tool_name(tool).to_ascii_lowercase());
        }
    }

    fn resolve_tool(&mut self, sid: &str, tool: &str) {
        let Some(entries) = self.pending.get_mut(sid) else {
            return;
        };
        let target = base_tool_name(tool).to_ascii_lowercase();
        if let Some(pos) = entries.iter().position(|w| w == &target) {
            entries.remove(pos);
        }
        if entries.is_empty() {
            self.pending.remove(sid);
        }
    }

    /// 管道入口更新：维护每个会话当前尚未收尾的等待工具。
    pub fn observe(&mut self, event: &NormalizedEvent) {
        let sid = &event.session_id;
        match event.kind {
            EventKind::InputRequired | EventKind::PermissionRequired => {
                if let Some(tool) = event.tool_name.as_deref() {
                    self.record_wait(sid, tool);
                }
            }
            EventKind::ToolFinished | EventKind::ToolFailed => {
                if let Some(tool) = event.tool_name.as_deref() {
                    self.resolve_tool(sid, tool);
                }
            }
            // PreInvocation（无 tool_name 的 Activity）：说明上一批所有并行工具已全部结束、
            // 正在发起下一次模型调用，清空残留等待以防漏收 PostToolUse 时永久卡在等待中。
            EventKind::Activity if event.tool_name.is_none() => self.clear(sid),
            EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunAborted => {
                self.clear(sid);
            }
            _ => {}
        }
    }

    /// 同回合有未收尾的等待工具时，其它并行工具的 PreToolUse / PostToolUse 不得冲掉等待相位。
    pub fn preserve_wait(&self, event: &NormalizedEvent) -> bool {
        if !matches!(
            event.kind,
            EventKind::Activity | EventKind::ToolFinished | EventKind::ToolFailed
        ) {
            return false;
        }
        // PreInvocation（无 tool_name）在 observe 阶段已清表；这里只兜底带工具名的事件。
        if event.kind == EventKind::Activity && event.tool_name.is_none() {
            return false;
        }
        self.pending
            .get(&event.session_id)
            .is_some_and(|entries| !entries.is_empty())
    }

    #[cfg(test)]
    fn is_waiting(&self, sid: &str) -> bool {
        self.pending.get(sid).is_some_and(|entries| !entries.is_empty())
    }
}

/// 管道消费前入口：本地中止事件先核验是否仍属当前活动，普通 hook 事件更新等待工具表。
pub fn accept(state: &AppState, event: &NormalizedEvent) -> bool {
    if event.id.starts_with(EVENT_PREFIX) {
        let current = lock(&state.active_sessions)
            .get(&session_key(&event.agent, &event.session_id))
            .cloned();
        if !still_current(current.as_ref(), event) {
            return false;
        }
    }
    lock(&state.antigravity_waits).observe(event);
    true
}

/// 本地日志中止事件只对仍在运行且活动时间不晚于取消时刻的会话生效。
pub fn still_current(session: Option<&SessionStatus>, event: &NormalizedEvent) -> bool {
    session.is_some_and(|s| event.timestamp >= s.last_activity)
}

/// 同回合有未结束的等待工具时，阻止并行工具心跳覆盖等待相位。
pub fn preserve_wait(state: &AppState, event: &NormalizedEvent) -> bool {
    lock(&state.antigravity_waits).preserve_wait(event)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Aborted {
    session_id: String,
    timestamp: i64,
}

/// 从单行 Antigravity 日志解析明确的用户取消信号。
///
/// 本机 2026-11-27 实测（Antigravity 2.35.2 / `language_server.log`）两条明确取消日志：
/// - `... I1127 21:44:40.852475 160984 rpcs_backend.go:484] CancelCascadeInvocation requested: cascade_id="<uuid>" ...`
/// - `... I1127 21:44:40.852518 160984 cascade_manager.go:2678] Cancel requested for cascade <uuid> (kill_background_tasks=false)`
fn parse_line(line: &str, ref_date: NaiveDate) -> Option<Aborted> {
    let (header, msg) = line.split_once("] ")?;
    let session_id = extract_cancel_id(msg)?;
    let timestamp = parse_glog_timestamp(header, ref_date)?;
    Some(Aborted {
        session_id: session_id.to_string(),
        timestamp,
    })
}

fn is_valid_cascade_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn extract_cancel_id(msg: &str) -> Option<&str> {
    let msg = msg.trim();
    if let Some(rest) = msg.strip_prefix("Cancel requested for cascade ") {
        let id = rest
            .split_whitespace()
            .next()?
            .trim_matches(|c| matches!(c, '"' | '\'' | ',' | ';'));
        return is_valid_cascade_id(id).then_some(id);
    }
    if let Some(rest) = msg.strip_prefix("CancelCascadeInvocation requested:") {
        let after = rest.split("cascade_id=").nth(1)?;
        let id = if let Some(quoted) = after.strip_prefix('"') {
            quoted.split_once('"')?.0
        } else {
            after.split_whitespace().next()?
        };
        return is_valid_cascade_id(id).then_some(id);
    }
    None
}

/// 解析 glog 头里的本地时间（如 `I1127 21:44:40.852475`；兼容前置 `ERROR: logging before google.Init: `）。
fn parse_glog_timestamp(header: &str, ref_date: NaiveDate) -> Option<i64> {
    let body = header
        .trim()
        .strip_prefix("ERROR: logging before google.Init:")
        .map(str::trim)
        .unwrap_or_else(|| header.trim());
    let mut parts = body.split_whitespace();
    let level_md = parts.next()?;
    let time_str = parts.next()?;

    // 兼容未来可能改写为 RFC3339 的日志格式。
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(level_md) {
        return Some(dt.timestamp_millis());
    }

    let bytes = level_md.as_bytes();
    if bytes.len() != 5 || !matches!(bytes[0], b'I' | b'W' | b'E' | b'F') {
        return None;
    }
    let month: u32 = level_md[1..3].parse().ok()?;
    let day: u32 = level_md[3..5].parse().ok()?;
    let time = NaiveTime::parse_from_str(time_str, "%H:%M:%S%.f")
        .or_else(|_| NaiveTime::parse_from_str(time_str, "%H:%M:%S"))
        .ok()?;
    let date = resolve_glog_date(month, day, ref_date)?;
    let naive = date.and_time(time);
    Local
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| Local.from_local_datetime(&naive).latest())
        .map(|dt| dt.timestamp_millis())
}

/// glog 不写年份：在 `ref_date` 所属年的前/当/后三年里取距离 `ref_date` 最近的有效日期，
/// 自然处理跨年（12/31 ↔ 01/01）与闰日（02/29）。
fn resolve_glog_date(month: u32, day: u32, ref_date: NaiveDate) -> Option<NaiveDate> {
    let year = ref_date.year();
    [year - 1, year, year + 1]
        .into_iter()
        .filter_map(|y| NaiveDate::from_ymd_opt(y, month, day))
        .min_by_key(|d| (*d - ref_date).num_days().abs())
}

#[derive(Default)]
struct Tail {
    offset: u64,
    created: Option<SystemTime>,
    #[cfg(unix)]
    ino: Option<u64>,
    initialized: bool,
    carry: Vec<u8>,
    skip_line: bool,
}

impl Tail {
    fn poll(&mut self, path: &Path, ref_date: NaiveDate) -> std::io::Result<Vec<Aborted>> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        #[cfg(unix)]
        let inode_changed = {
            use std::os::unix::fs::MetadataExt;
            matches!(self.ino, Some(prev) if prev != meta.ino())
        };
        #[cfg(not(unix))]
        let inode_changed = false;
        if !self.initialized || recreated || inode_changed || meta.len() < self.offset {
            // 首次或日志轮转时读有限尾部，避免漏掉首次扫描前刚发生的取消；
            // 旧取消由时间戳校验挡住，不会误伤新回合。
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = self.offset > 0;
            self.carry.clear();
            self.created = meta.created().ok();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                self.ino = Some(meta.ino());
            }
            self.initialized = true;
        } else if meta.len().saturating_sub(self.offset) > MAX_READ {
            // 空闲许久后日志大幅增长：直接跳到最新 256 KiB 尾部，单轮即可追平 EOF。
            self.offset = meta.len().saturating_sub(MAX_READ);
            self.skip_line = true;
            self.carry.clear();
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(meta.len().saturating_sub(self.offset).min(MAX_READ))
            .read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        let mut signals = Vec::new();
        for byte in bytes {
            if byte == b'\n' {
                if !self.skip_line {
                    if let Ok(line) = std::str::from_utf8(&self.carry) {
                        if let Some(signal) = parse_line(line, ref_date) {
                            signals.push(signal);
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
        Ok(signals)
    }
}

/// 枚举本机可能存在的 Antigravity 日志文件（App `language_server.log` + CLI/IDE 最新日志）。
fn discover_log_files(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let mut push_file = |p: PathBuf| {
        if p.is_file() && seen.insert(p.clone()) {
            out.push(p);
        }
    };

    #[cfg(target_os = "macos")]
    {
        push_file(home.join("Library/Logs/Antigravity/language_server.log"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
            push_file(appdata.join("Antigravity/logs/language_server.log"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            push_file(local.join("Antigravity/logs/language_server.log"));
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let cfg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        push_file(cfg.join("Antigravity/logs/language_server.log"));
    }

    for dir_name in ["antigravity", "antigravity-cli", "antigravity-ide"] {
        let log_dir = home.join(".gemini").join(dir_name).join("log");
        if let Some(latest) = newest_log_in(&log_dir) {
            push_file(latest);
        }
    }

    out
}

fn newest_log_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_file() {
                return None;
            }
            if !p
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
            {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, p))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p)
}

fn abort_message(lang: bark_core::Lang) -> &'static str {
    match lang {
        bark_core::Lang::Zh => "用户已中止 Antigravity 回合",
        bark_core::Lang::En => "Antigravity turn aborted by user",
    }
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    // 文件 IO 在独立线程执行，不阻塞 Tauri / tokio 事件管道。
    let result = std::thread::Builder::new()
        .name("antigravity-abort-watch".into())
        .spawn(move || {
            let home = bark_adapters::InstallEnv::detect().home;
            let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
            let mut generation = bark_adapters::watch::recheck_generation();
            while !tx.is_closed() {
                let requested = bark_adapters::watch::recheck_generation();
                if requested != generation {
                    for tail in tails.values_mut() {
                        tail.initialized = false;
                    }
                    generation = requested;
                }
                if read(&state.config).agent_disabled("antigravity") {
                    tails.clear();
                } else {
                    let sessions: HashMap<String, SessionStatus> = {
                        let active = lock(&state.active_sessions);
                        active
                            .values()
                            .filter(|s| s.agent == "antigravity")
                            .map(|s| (s.session_id.clone(), s.clone()))
                            .collect()
                    };
                    if !sessions.is_empty() {
                        let files = discover_log_files(&home);
                        tails.retain(|path, _| files.contains(path));
                        let ref_date = Local::now().date_naive();
                        let mut latest_by_sid: HashMap<String, i64> = HashMap::new();
                        for path in files {
                            let signals = match tails
                                .entry(path.clone())
                                .or_default()
                                .poll(&path, ref_date)
                            {
                                Ok(signals) => signals,
                                Err(error) => {
                                    tracing::debug!(
                                        ?path,
                                        %error,
                                        "Antigravity 日志读取失败，保留当前状态"
                                    );
                                    continue;
                                }
                            };
                            for signal in signals {
                                let entry = latest_by_sid.entry(signal.session_id).or_insert(0);
                                if signal.timestamp > *entry {
                                    *entry = signal.timestamp;
                                }
                            }
                        }
                        let lang = state.lang();
                        for (sid, ts) in latest_by_sid {
                            let Some(session) = sessions.get(&sid) else {
                                continue;
                            };
                            if ts < session.last_activity {
                                continue;
                            }
                            let mut event = NormalizedEvent::from_raw(
                                "antigravity",
                                EventKind::RunAborted,
                                &json!({
                                    "session_id": sid,
                                    "cwd": session.cwd,
                                    "message": abort_message(lang),
                                }),
                            );
                            event.id = format!("{EVENT_PREFIX}{sid}:{ts}");
                            event.timestamp = ts;
                            if event.project.is_none() {
                                event.project = session.project.clone();
                            }
                            match tx.try_send(event) {
                                Ok(()) => {
                                    tracing::info!(
                                        session = %sid,
                                        "Antigravity 日志记录到用户取消，已合成 run_aborted"
                                    );
                                }
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    tracing::debug!(
                                        session = %sid,
                                        "事件队列已满，Antigravity 中止信号跳过本轮"
                                    );
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => return,
                            }
                        }
                    }
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        });
    if let Err(error) = result {
        tracing::warn!(%error, "Antigravity 中止监控启动失败，保留当前状态");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const RPC_CANCEL: &str = r#"ERROR: logging before google.Init: I1127 21:44:40.852475  160984 rpcs_backend.go:484] CancelCascadeInvocation requested: cascade_id="99512cde-43e4-4f1e-b746-8b296d7bf568" kill_background_tasks=false client="unknown""#;
    const MGR_CANCEL: &str = r#"ERROR: logging before google.Init: I1127 21:44:40.852518  160984 cascade_manager.go:2678] Cancel requested for cascade 99512cde-43e4-4f1e-b746-8b296d7bf568 (kill_background_tasks=false)"#;
    const CLI_CANCEL: &str = r#"I1127 21:44:40.852518     184 cascade_manager.go:2678] Cancel requested for cascade 99512cde-43e4-4f1e-b746-8b296d7bf568 (kill_background_tasks=false)"#;

    #[test]
    fn parses_real_antigravity_cancel_lines() {
        let ref_date = NaiveDate::from_ymd_opt(2026, 11, 27).unwrap();
        let a = parse_line(RPC_CANCEL, ref_date).expect("RPC 取消行必须识别");
        let b = parse_line(MGR_CANCEL, ref_date).expect("Manager 取消行必须识别");
        let c = parse_line(CLI_CANCEL, ref_date).expect("无前缀 CLI 取消行必须识别");
        assert_eq!(a.session_id, "99512cde-43e4-4f1e-b746-8b296d7bf568");
        assert_eq!(b.session_id, "99512cde-43e4-4f1e-b746-8b296d7bf568");
        assert_eq!(c.session_id, "99512cde-43e4-4f1e-b746-8b296d7bf568");
        assert_eq!(a.timestamp, b.timestamp, "同一毫秒内两行时间戳一致");
        assert_eq!(b.timestamp, c.timestamp);

        // 非取消行与非法 id 一律忽略
        for line in [
            "ERROR: logging before google.Init: I1127 21:44:37.950682  160690 command_hook_executor.go:71] JSON hook command stderr: agent-bark",
            r#"I1127 21:44:40.852518 184 cascade_manager.go:2678] Cancel requested for cascade ../bad"#,
            r#"I1127 21:44:40.852475 184 rpcs_backend.go:484] CancelCascadeInvocation requested: cascade_id="""#,
            "not a glog line] Cancel requested for cascade 99512cde-43e4-4f1e-b746-8b296d7bf568",
        ] {
            assert!(parse_line(line, ref_date).is_none(), "误判行: {line}");
        }
    }

    #[test]
    fn resolves_glog_year_across_new_year_boundary() {
        let jan1 = NaiveDate::from_ymd_opt(2027, 1, 1).unwrap();
        assert_eq!(
            resolve_glog_date(12, 31, jan1),
            NaiveDate::from_ymd_opt(2026, 12, 31)
        );
        let dec31 = NaiveDate::from_ymd_opt(2026, 12, 31).unwrap();
        assert_eq!(
            resolve_glog_date(1, 1, dec31),
            NaiveDate::from_ymd_opt(2027, 1, 1)
        );
    }

    #[test]
    fn tail_handles_partial_utf8_and_log_truncation() {
        let unique = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agent-bark-ag-watch-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("language_server.log");
        let ref_date = NaiveDate::from_ymd_opt(2026, 11, 27).unwrap();

        let half = &MGR_CANCEL.as_bytes()[..40];
        std::fs::write(&path, half).unwrap();
        let mut tail = Tail::default();
        assert!(tail.poll(&path, ref_date).unwrap().is_empty());

        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&MGR_CANCEL.as_bytes()[40..]).unwrap();
        f.write_all(b"\n").unwrap();
        writeln!(f, "{RPC_CANCEL}").unwrap();
        drop(f);

        assert_eq!(tail.poll(&path, ref_date).unwrap().len(), 2);
        assert!(tail.poll(&path, ref_date).unwrap().is_empty());

        // 模拟应用重启截断重写日志（新文件短于旧偏移）
        std::fs::write(&path, format!("{MGR_CANCEL}\n")).unwrap();
        assert_eq!(tail.poll(&path, ref_date).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn waits_preserve_until_matching_post_tool_or_pre_invocation() {
        let mut waits = Waits::default();
        let make = |kind: EventKind, tool: Option<&str>| {
            let mut ev = NormalizedEvent::from_raw(
                "antigravity",
                kind,
                &json!({ "conversationId": "c1" }),
            );
            ev.tool_name = tool.map(str::to_string);
            ev
        };

        // 1) ask_question 开启等待
        let ask = make(EventKind::InputRequired, Some("ask_question"));
        waits.observe(&ask);
        assert!(waits.is_waiting("c1"));

        // 2) 并行工具 update_focus 的 PreToolUse / PostToolUse 不得冲掉等待相位
        let pre_focus = make(EventKind::Activity, Some("update_focus"));
        waits.observe(&pre_focus);
        assert!(waits.preserve_wait(&pre_focus));

        let post_focus = make(EventKind::ToolFinished, Some("update_focus"));
        waits.observe(&post_focus);
        assert!(waits.preserve_wait(&post_focus));

        // 3) ask_question 自己的 PostToolUse 解除等待
        let post_ask = make(EventKind::ToolFinished, Some("ask_question"));
        waits.observe(&post_ask);
        assert!(!waits.preserve_wait(&post_ask));
        assert!(!waits.is_waiting("c1"));

        // 4) 万一漏收 PostToolUse，下一次 PreInvocation（无 tool_name）也能兜底清掉等待
        waits.observe(&make(EventKind::PermissionRequired, Some("ask_permission")));
        assert!(waits.is_waiting("c1"));
        let pre_inv = make(EventKind::Activity, None);
        waits.observe(&pre_inv);
        assert!(!waits.preserve_wait(&pre_inv));
        assert!(!waits.is_waiting("c1"));
    }
}
