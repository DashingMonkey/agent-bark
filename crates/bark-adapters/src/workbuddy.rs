//! WorkBuddy 的会话状态 completed 同时覆盖正常完成与取消。
//! 2026-10-14 本机样本：取消回合写 assistant/incomplete + skipRun +
//! error.message="Interrupted by user"；正常回合写 assistant/completed。
//! 2026-11-27（5.12.1 / ACP）：回合终态改为写 `acp-terminal-state` 行
//! （stopReason=end_turn / error / cancelled），会话行本身只回到 `active`，
//! 不再写 completed——状态机侧由 watch.rs 的 `active` 跃迁兜住，这里补终态细分。
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
            // 5.12.1（ACP）的回合终态：优先于 message 判定，stopReason 区分完成/取消/失败。
            "acp-terminal-state" => return classify_acp_terminal(&row, updated_at),
            // 与 acp-terminal-state 同批写入的旁路记录（asar 内 Sc 集合），
            // 出现在终态行之后但只是统计信息，不能当成「更新的活动」。
            "turn-metrics" | "request-recovery" | "model-usage" | "credit-usage" | "context-usage" => {
                continue
            }
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

/// 5.12.1（ACP）的回合终态行，样本：
/// `{"type":"acp-terminal-state","stopReason":"end_turn",
///   "providerData":{"skipRun":true,"acpTerminalState":true,"finishReason":"stop"}}`
///
/// 该行没有 sessionId（文件本身按 `{session}.jsonl` 定位，无需再校验）；
/// 与 message 一样受提交滞后窗口保护，避免旧回合的终态覆盖下一次跃迁。
fn classify_acp_terminal(row: &Value, updated_at: i64) -> Option<EventKind> {
    let timestamp = row.get("timestamp")?.as_i64()?;
    let lag = updated_at.checked_sub(timestamp)?;
    if timestamp <= 0 || !(0..=MAX_COMMIT_LAG_MS).contains(&lag) {
        return None;
    }
    let stop_reason = row.get("stopReason").and_then(Value::as_str).unwrap_or_default();
    let finish_reason = row
        .pointer("/providerData/finishReason")
        .and_then(Value::as_str);
    // eslint 风格的空值检查在这里不适用：error 可能是字符串、对象或 null
    let has_error = row.get("error").is_some_and(|v| !v.is_null());
    let cancelled = row
        .pointer("/providerData/cancelReason")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if has_error || stop_reason == "error" {
        return Some(EventKind::RunFailed);
    }
    if cancelled
        || matches!(
            stop_reason,
            "cancelled" | "canceled" | "interrupted" | "killed" | "stopped"
        )
    {
        return Some(EventKind::RunAborted);
    }
    (stop_reason == "end_turn" || finish_reason == Some("stop")).then_some(EventKind::RunCompleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acp_terminal(stop_reason: &str, finish: &str) -> Value {
        json!({"type":"acp-terminal-state", "timestamp":10_000, "stopReason":stop_reason,
            "providerData":{"skipRun":true,"acpTerminalState":true,"finishReason":finish}})
    }

    /// 5.12.1 起正常结束只写 acp-terminal-state，会话行停在 active；
    /// 这条判定不成立就会把每次正常完成显示成「思考中」永不结束。
    #[test]
    fn acp_terminal_state_classifies_end_turn_cancel_and_error() {
        let done = acp_terminal("end_turn", "stop");
        assert_eq!(
            classify_tail(&format!("{done}\n"), "s1", 10_100),
            Some(EventKind::RunCompleted)
        );
        // 真实样本里 context-usage 比终态行晚 56ms 落盘，不能挡住反向遍历
        let tail = format!("{done}\n{}", json!({"type":"context-usage","timestamp":10_056}));
        assert_eq!(classify_tail(&tail, "s1", 10_100), Some(EventKind::RunCompleted));
        // 用户中断
        let cancelled = acp_terminal("cancelled", "stop");
        assert_eq!(
            classify_tail(&format!("{cancelled}\n"), "s1", 10_100),
            Some(EventKind::RunAborted)
        );
        // providerData.cancelReason 同样表示中断
        let mut with_reason = cancelled.clone();
        with_reason["providerData"]["cancelReason"] = json!("user");
        assert_eq!(
            classify_tail(&format!("{with_reason}\n"), "s1", 10_100),
            Some(EventKind::RunAborted)
        );
        // 失败
        let failed = acp_terminal("error", "error");
        assert_eq!(
            classify_tail(&format!("{failed}\n"), "s1", 10_100),
            Some(EventKind::RunFailed)
        );
        // 提交滞后超出窗口：不认，免得旧回合终态覆盖下一次跃迁
        assert_eq!(classify_tail(&format!("{done}\n"), "s1", 20_000), None);
    }

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
