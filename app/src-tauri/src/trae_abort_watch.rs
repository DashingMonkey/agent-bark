//! TRAE Hooks 没有中断事件；每秒增量读取 renderer.log 中明确的 canceled 终态。
//! 非官方补充信号，格式变化时退回原有超时回收。只影响已由 hook 建档的会话。

use crate::state::{lock, read, session_key, AppState};
use bark_core::{EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;

pub const EVENT_PREFIX: &str = "trae-log-abort:";
const MAX_READ: u64 = 256 * 1024;
const MARKER: &str = "[info] [ai-chat/v2] [DoneHandler] Stream done event received ";

#[derive(Debug)]
struct Aborted {
    session_id: String,
    timestamp: i64,
}

fn parse_line(line: &str) -> Option<Aborted> {
    let (time, payload) = line.split_once(MARKER)?;
    let timestamp = chrono::DateTime::parse_from_rfc3339(time.trim())
        .ok()?
        .timestamp_millis();
    let value: Value = serde_json::from_str(payload.trim()).ok()?;
    if value.get("status")?.as_str()? != "canceled" {
        return None;
    }
    let session_id = value.get("sessionId")?.as_str()?.trim();
    if session_id.is_empty() {
        return None;
    }
    Some(Aborted {
        session_id: session_id.into(),
        timestamp,
    })
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
    fn poll(&mut self, path: &Path) -> std::io::Result<Vec<Aborted>> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let recreated = matches!((self.created, meta.created().ok()), (Some(a), Some(b)) if a != b);
        if !self.initialized || recreated || meta.len() < self.offset {
            // 新窗口/日志轮转时读有限尾部，避免漏掉首次扫描之前刚发生的中止。
            // 时间戳在投递和管道消费时各校验一次，旧记录不能关闭新回合。
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
        let mut signals = Vec::new();
        // 按字节留半行，UTF-8 字符被分批写入时也不会损坏。
        for byte in bytes {
            if byte == b'\n' {
                if !self.skip_line {
                    if let Ok(line) = std::str::from_utf8(&self.carry) {
                        if let Some(signal) = parse_line(line) {
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

fn log_roots() -> Vec<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = Some(
        bark_adapters::InstallEnv::detect()
            .home
            .join("Library/Application Support"),
    );
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = Some(
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| bark_adapters::InstallEnv::detect().home.join(".config")),
    );
    base.into_iter()
        .flat_map(|p| [p.join("Trae CN/logs"), p.join("Trae/logs")])
        .collect()
}

fn renderer_logs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let newest = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let b = name.as_bytes();
            b.len() == 15
                && b[8] == b'T'
                && b.iter()
                    .enumerate()
                    .all(|(i, c)| i == 8 || c.is_ascii_digit())
                && e.path().is_dir()
        })
        .max_by_key(|e| e.file_name());
    let Some(dir) = newest else { return Vec::new() };
    let Ok(windows) = std::fs::read_dir(dir.path()) else {
        return Vec::new();
    };
    windows
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = name.strip_prefix("window")?;
            if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let path = entry.path().join("renderer.log");
            path.is_file().then_some(path)
        })
        .collect()
}

pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    tauri::async_runtime::spawn(async move {
        let roots = log_roots();
        let mut tails: HashMap<PathBuf, Tail> = HashMap::new();
        loop {
            if tx.is_closed() {
                return;
            }
            if read(&state.config).agent_enabled("trae-code") {
                let files: Vec<_> = roots.iter().flat_map(|p| renderer_logs(p)).collect();
                tails.retain(|path, _| files.contains(path));
                for path in files {
                    let signals = match tails.entry(path.clone()).or_default().poll(&path) {
                        Ok(signals) => signals,
                        Err(error) => {
                            tracing::debug!(?path, %error, "TRAE 中止日志读取失败");
                            continue;
                        }
                    };
                    for signal in signals {
                        let key = session_key("trae-code", &signal.session_id);
                        let Some(session) = lock(&state.active_sessions).get(&key).cloned() else {
                            continue;
                        };
                        if signal.timestamp < session.last_activity {
                            continue;
                        }
                        let mut event = NormalizedEvent::from_raw(
                            "trae-code",
                            EventKind::RunAborted,
                            &json!({"session_id": signal.session_id, "cwd": session.cwd, "message": "回合被手动中止"}),
                        );
                        event.id =
                            format!("{EVENT_PREFIX}{}:{}", event.session_id, signal.timestamp);
                        event.timestamp = signal.timestamp;
                        // 确定性 id 由事件管道去重；不按会话设冷却，连续两轮中止都应生效。
                        if let Err(error) = tx.try_send(event) {
                            tracing::warn!(%error, "TRAE 中止信号投递失败，超时回收兜底");
                        }
                    }
                }
            } else {
                tails.clear();
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CANCELED: &str = "2026-10-14T13:24:42.141+08:00 [info] [ai-chat/v2] [DoneHandler] Stream done event received {\"sessionId\":\"s1\",\"status\":\"canceled\",\"agentMessageId\":\"m1\"}";

    #[test]
    fn only_explicit_canceled_done_is_a_signal() {
        let signal = parse_line(CANCELED).unwrap();
        assert_eq!(signal.session_id, "s1");
        assert_eq!(signal.timestamp, 1791955482141);
        for line in [
            CANCELED.replace("canceled", "completed"),
            CANCELED.replace("canceled", "failed"),
            CANCELED.replace("DoneHandler", "ToolHandler"),
            CANCELED.replace("s1", ""),
            CANCELED.replace("2026-10-14T13:24:42.141+08:00", "invalid"),
        ] {
            assert!(parse_line(&line).is_none());
        }
    }

    #[test]
    fn tail_handles_partial_utf8_truncation_and_repeated_turns() {
        let path = std::env::temp_dir().join(format!("bark-trae-{}.log", std::process::id()));
        let line = CANCELED.replace("m1", "中文");
        let split = line.find('中').unwrap() + 1;
        std::fs::write(&path, &line.as_bytes()[..split]).unwrap();
        let mut tail = Tail::default();
        assert!(tail.poll(&path).unwrap().is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&line.as_bytes()[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        assert!(tail.poll(&path).unwrap().is_empty());
        writeln!(file, "{CANCELED}").unwrap();
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        drop(file);
        std::fs::write(&path, format!("{CANCELED}\n")).unwrap();
        assert_eq!(tail.poll(&path).unwrap().len(), 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn discovery_includes_all_windows_of_latest_launch() {
        let root = std::env::temp_dir().join(format!("bark-trae-windows-{}", std::process::id()));
        for dir in [
            "20261013T120000/window1",
            "20261014T132416/window1",
            "20261014T132416/window2",
            "invalid/window1",
        ] {
            let dir = root.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("renderer.log"), "").unwrap();
        }
        let files = renderer_logs(&root);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|p| p
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .ends_with("20261014T132416")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
