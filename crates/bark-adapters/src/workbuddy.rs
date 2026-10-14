//! WorkBuddy 的会话状态 completed 同时覆盖正常完成与取消。
//! 2026-10-14 本机样本：取消回合写 assistant/incomplete + skipRun +
//! error.message="Interrupted by user"；正常回合写 assistant/completed。
//! 只在会话终态跃迁时读有限 JSONL 尾部，补充明确的取消原因。

use bark_core::EventKind;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const TAIL_BYTES: u64 = 256 * 1024;
// 回合记录先落盘、会话库随后提交；不让旧回合的取消覆盖之后的 completed。
const MAX_COMMIT_LAG_MS: i64 = 5_000;

fn transcript_path(projects: &Path, session: &str) -> Option<PathBuf> {
    if session.is_empty()
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    std::fs::read_dir(projects)
        .ok()?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path().join(format!("{session}.jsonl")))
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

pub(crate) fn terminal_override(
    projects: &Path,
    session: &str,
    updated_at: i64,
) -> Option<EventKind> {
    let path = transcript_path(projects, session)?;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let offset = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(len - offset).read_to_end(&mut bytes).ok()?;
    let start = if offset > 0 {
        bytes.iter().position(|b| *b == b'\n')? + 1
    } else {
        0
    };
    classify_tail(
        std::str::from_utf8(&bytes[start..]).ok()?,
        session,
        updated_at,
    )
}

fn classify_tail(tail: &str, session: &str, updated_at: i64) -> Option<EventKind> {
    for line in tail.lines().rev().filter(|s| !s.trim().is_empty()) {
        // 尾部半行/损坏不能跳过去命中旧回合。
        let row: Value = serde_json::from_str(line).ok()?;
        match row.get("type")?.as_str()? {
            "session-meta" | "file-history-snapshot" => continue,
            "message" => {}
            _ => return None, // 更新的推理/工具活动意味着旧终态已经过时。
        }
        if row.get("sessionId")?.as_str()? != session
            || row.get("role")?.as_str()? != "assistant"
            || row.get("status")?.as_str()? != "incomplete"
            || row.pointer("/providerData/skipRun")?.as_bool()? != true
            || row.pointer("/providerData/error/message")?.as_str()? != "Interrupted by user"
        {
            return None;
        }
        let timestamp = row.get("timestamp")?.as_i64()?;
        let lag = updated_at.checked_sub(timestamp)?;
        return (timestamp > 0 && (0..=MAX_COMMIT_LAG_MS).contains(&lag))
            .then_some(EventKind::RunAborted);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn interrupted() -> Value {
        json!({"type":"message", "role":"assistant", "status":"incomplete",
            "sessionId":"s1", "timestamp":10_000,
            "providerData":{"skipRun":true,"error":{"message":"Interrupted by user"}},
            "content":[{"type":"output_text","text":"Interrupted by user"}]})
    }

    #[test]
    fn interrupted_completed_session_becomes_aborted() {
        let tail = format!("{}\n{}\n", interrupted(), json!({"type":"session-meta"}));
        assert_eq!(
            classify_tail(&tail, "s1", 10_030),
            Some(EventKind::RunAborted)
        );
        assert_eq!(classify_tail(&tail, "s2", 10_030), None);
        assert_eq!(classify_tail(&tail, "s1", 20_000), None);
        assert_eq!(classify_tail(&tail, "s1", 9_999), None);
    }

    #[test]
    fn later_turn_or_partial_record_never_reuses_old_cancellation() {
        for later in [
            json!({"type":"message","role":"user","sessionId":"s1"}),
            json!({"type":"message","role":"assistant","status":"completed","sessionId":"s1"}),
            json!({"type":"reasoning"}),
            json!({"type":"function_call_result"}),
        ] {
            assert_eq!(
                classify_tail(&format!("{}\n{later}\n", interrupted()), "s1", 10_500),
                None
            );
        }
        assert_eq!(
            classify_tail(&format!("{}\n{{\"type\":", interrupted()), "s1", 10_500),
            None
        );
        assert_eq!(
            classify_tail(&interrupted().to_string(), "s1", 10_500),
            Some(EventKind::RunAborted)
        );
    }

    #[test]
    fn text_mentions_and_tool_cancellation_are_not_turn_abort() {
        for (key, value) in [
            ("role", json!("user")),
            ("status", json!("completed")),
            ("type", json!("function_call_result")),
        ] {
            let mut row = interrupted();
            row[key] = value;
            assert_eq!(classify_tail(&row.to_string(), "s1", 10_030), None);
        }
        let mut row = interrupted();
        row["providerData"] = json!({});
        assert_eq!(classify_tail(&row.to_string(), "s1", 10_030), None);
    }

    #[test]
    fn reads_matching_transcript_tail_and_ignores_other_sessions() {
        let root = std::env::temp_dir().join(format!("bark-workbuddy-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let filler = "中".repeat(TAIL_BYTES as usize);
        std::fs::write(
            project.join("s1.jsonl"),
            format!("{filler}\n{}\n", interrupted()),
        )
        .unwrap();
        assert_eq!(
            terminal_override(&root, "s1", 10_030),
            Some(EventKind::RunAborted)
        );
        assert_eq!(terminal_override(&root, "missing", 10_030), None);
        assert_eq!(terminal_override(&root, "../s1", 10_030), None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
