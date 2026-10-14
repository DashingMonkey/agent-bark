//! 只读尾随豆包运行日志。仅接受 SDK 明确确认成功的用户停止事件，按消息 ID 关联。
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_READ: u64 = 1024 * 1024;
const MARKER: &str =
    "[neotix] [office-api-logger] \"[webview][unknown][flow-web][logifier-report]: ";

fn parse_line(line: &str) -> Option<(String, String)> {
    let (_, payload) = line.split_once(MARKER)?;
    let payload = payload.strip_suffix("\", source:  (0)")?;
    let value: Value = serde_json::from_str(payload).ok()?;
    if value.get("eventName")?.as_str()? != "sdk.message.break.request" {
        return None;
    }
    let meta = value.get("meta")?;
    if meta.get("break_reason_name")?.as_str()? != "click_break_button"
        || meta.get("result")?.as_str()? != "success"
    {
        return None;
    }
    let conv = meta.get("conversation_id")?.as_str()?;
    let message = meta.get("message_id")?.as_str()?;
    if [conv, message]
        .iter()
        .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some((conv.into(), message.into()))
}

#[derive(Default)]
struct Tail {
    offset: u64,
    created: Option<SystemTime>,
    carry: Vec<u8>,
    skip_line: bool,
}

impl Tail {
    fn poll(&mut self, path: &Path) -> std::io::Result<Vec<(String, String)>> {
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        let created = meta.created().ok();
        if meta.len() < self.offset
            || matches!((self.created, created), (Some(a), Some(b)) if a != b)
        {
            self.offset = 0;
            self.carry.clear();
            self.skip_line = false;
        }
        self.created = created;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(MAX_READ).read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        let mut out = Vec::new();
        for b in bytes {
            if b == b'\n' {
                if !self.skip_line {
                    if let Ok(line) = std::str::from_utf8(&self.carry) {
                        if let Some(signal) = parse_line(line.trim_end_matches('\r')) {
                            out.push(signal);
                        }
                    }
                }
                self.carry.clear();
                self.skip_line = false;
            } else if !self.skip_line {
                self.carry.push(b);
                if self.carry.len() >= MAX_READ as usize {
                    self.carry.clear();
                    self.skip_line = true;
                }
            }
        }
        Ok(out)
    }
}

#[derive(Default)]
pub(crate) struct CancelLog {
    root: Option<PathBuf>,
    tails: HashMap<PathBuf, Tail>,
    cancelled: HashMap<(String, String), u64>,
    sequence: u64,
}

impl CancelLog {
    pub(crate) fn new(root: PathBuf) -> Self {
        let mut log = Self {
            root: Some(root),
            ..Self::default()
        };
        // 启用时从 EOF 开始；重启不重放历史停止事件。后续新增/轮转文件从头读。
        for path in log.files() {
            if let Ok(meta) = std::fs::metadata(&path) {
                log.tails.insert(
                    path,
                    Tail {
                        offset: meta.len(),
                        created: meta.created().ok(),
                        ..Tail::default()
                    },
                );
            }
        }
        log
    }

    fn files(&self) -> Vec<PathBuf> {
        let Some(root) = &self.root else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                (name.starts_with("saman_") && name.ends_with(".log")).then(|| e.path())
            })
            .collect()
    }

    pub(crate) fn poll(&mut self) {
        let files = self.files();
        self.tails.retain(|path, _| files.contains(path));
        for path in files {
            match self.tails.entry(path.clone()).or_default().poll(&path) {
                Ok(signals) => {
                    for signal in signals {
                        self.sequence += 1;
                        self.cancelled.insert(signal, self.sequence);
                    }
                }
                Err(error) => tracing::debug!(?path, %error, "豆包停止日志读取失败"),
            }
        }
        if self.cancelled.len() > 4096 {
            let cutoff = self.sequence.saturating_sub(2048);
            self.cancelled.retain(|_, seq| *seq > cutoff);
        }
    }

    pub(crate) fn matches(&self, conv: &str, messages: &[String]) -> bool {
        // 多条任务并存时，一条被停止不能把另一条仍在运行的任务收场。
        !messages.is_empty()
            && messages
                .iter()
                .all(|id| self.cancelled.contains_key(&(conv.into(), id.clone())))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn line(conv: &str, message: &str) -> String {
        format!(
            "[23572:20220:1014/141159.928:INFO:CONSOLE(0)] {MARKER}{}\", source:  (0)\n",
            serde_json::json!({
                "version":1,"type":"event","level":"info","eventName":"sdk.message.break.request",
                "meta":{"break_reason":"7","break_reason_name":"click_break_button","conversation_id":conv,
                "message_id":message,"result":"success","session_state":"6","status":"4"}
            })
        )
    }

    #[test]
    fn only_confirmed_user_cancel_is_accepted() {
        let line = line("1001", "123");
        assert_eq!(parse_line(line.trim()), Some(("1001".into(), "123".into())));
        for invalid in [
            line.replace("success", "dispatch"),
            line.replace("success", "error"),
            line.replace("click_break_button", "network_error"),
            line.replace("1001", ""),
            line.replace("sdk.message.break.request", "sdk.message.break.caller"),
            line.replace("office-api-logger", "user-message"),
        ] {
            assert!(parse_line(invalid.trim()).is_none());
        }
    }

    /// 本机只读复核真实日志格式；不发布事件、不改宿主文件。
    #[test]
    #[ignore]
    fn doubao_live_cancel_records_decode() {
        let root = dirs::data_local_dir()
            .unwrap()
            .join("DoubaoWork/User Data/sdk_storage/log");
        let log = CancelLog::new(root);
        let mut count = 0;
        for path in log.files() {
            let text = std::fs::read_to_string(path).unwrap();
            count += text.lines().filter_map(parse_line).count();
        }
        assert!(count > 0, "本机必须有 SDK 确认用户停止的日志样本");
        eprintln!("成功解析 {count} 条真实用户停止确认日志");
    }

    #[test]
    fn tail_skips_history_handles_partial_lines_and_rotation() {
        let root = std::env::temp_dir().join(format!("bark-doubao-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("saman_2026.1014.0.log");
        std::fs::write(&path, line("1001", "1")).unwrap();
        let mut log = CancelLog::new(root.clone());
        log.poll();
        assert!(!log.matches("1001", &["1".into()]));
        let record = line("1001", "2");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&record.as_bytes()[..70]).unwrap();
        log.poll();
        assert!(!log.matches("1001", &["2".into()]));
        file.write_all(&record.as_bytes()[70..]).unwrap();
        log.poll();
        assert!(log.matches("1001", &["2".into()]));
        assert!(!log.matches("1002", &["2".into()]));
        assert!(!log.matches("1001", &["2".into(), "3".into()]));
        drop(file);
        std::fs::write(&path, line("1001", "3")).unwrap();
        log.poll();
        assert!(log.matches("1001", &["3".into()]));
        std::fs::write(root.join("saman_2026.1014.1.log"), line("1001", "4")).unwrap();
        log.poll();
        assert!(log.matches("1001", &["4".into()]));
        std::fs::remove_dir_all(root).unwrap();
    }
}
