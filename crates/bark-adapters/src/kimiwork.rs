//! Kimi Work（Kimi 电脑客户端「Work」模式）监控型适配器 —— **非官方**。
//!
//! 架构（2026-09 在本机 Kimi Work 实测逆向 + 官方帮助中心「以 Kimi Code 为内核」
//! 交叉印证）：桌面客户端（Electron，数据在 `%APPDATA%\kimi-desktop\`）内嵌一个
//! 叫 **daimon** 的守护进程（`daimon-bundle/`：自带 Node + Python 运行时），
//! 以 **Kimi Code 为内核**执行 Goal 模式任务。内核有自己的 KIMI_CODE_HOME：
//!
//! ```text
//! %APPDATA%\kimi-desktop\daimon-share\daimon\runtime\kimi-code\home\
//! ├── session_index.jsonl        每行 {sessionId, sessionDir, workDir}（追加写）
//! └── sessions/wd_<工作区>_<hash>/<会话id>/
//!     ├── state.json             标题 / createdAt / updatedAt
//!     └── agents/main/wire.jsonl 会话事件流（协议 1.3）
//! ```
//!
//! 会话布局与 Kimi Code CLI 完全同构（本机逐文件核对，sessions.v2.json 把应用层
//! conversation 映射到 `kernelSessionDir` 即上述目录）。wire.jsonl 记录类型 →
//! 统一事件的映射按本机真实样本校准（4 回合 = 3 次 end_turn + 1 次 turn.cancel，
//! 全部闭环）：
//!
//! | wire 记录 | 统一事件 |
//! | --- | --- |
//! | `turn.prompt` | activity（回合开始，`input[*].text` 为任务文案） |
//! | loop `tool.call` | activity（工具运行，带工具名） |
//! | loop `tool.result` | tool_finished |
//! | loop `step.end` finishReason=`end_turn` | run_completed（**协议 1.3 没有 turn.end**） |
//! | `turn.cancel` | run_aborted（用户手动中止，立即释放） |
//! | 其余（metadata / config.update / usage.record / permission.* / content.part 等） | 忽略 |
//!
//! 为什么不做 Hook 型：内核读的 `runtime/kimi-code/config.toml` 是**应用托管生成**
//! 的（内含 Daimon 权限规则、设备元数据，随应用启动重写），写入的 `[[hooks]]`
//! 活不过一次 Kimi Work 重启；Watch 型零写盘，不受托管重写影响。
//!
//! 已知边界（运行态未实测，靠判死兜底）：
//! - 只看 `agents/main/wire.jsonl`（主 agent）；Agent Swarm 子智能体不出事件
//! - 协议 1.3 只有审批**解决**记录（`permission.record_approval_result`），没有
//!   审批**请求**记录——「等待确认」相位推不出来，表现为执行工具直到审批完成
//! - 通知正文用 `state.json` 的会话标题（content.part 是流式分片，不拼装正文）

use crate::watch::{emit, sleep_interruptible, WatchAdapter};
use bark_core::{now_millis, truncate_chars, AgentKind, EventKind, NormalizedEvent};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// daimon 托管配置：`agents.defaults.agentFile` 指向内核 config.toml，
/// 其父目录即 `runtime/kimi-code/`，`home/` 是兄弟目录——这是数据盘迁移后
/// 仍能找对位置的唯一权威指针。
fn daimon_config_path() -> Option<PathBuf> {
    Some(
        dirs::data_dir()?
            .join("kimi-desktop")
            .join("daimon-share")
            .join("daimon")
            .join("config.json"),
    )
}

/// 内核 KIMI_CODE_HOME 解析（给定应用数据根，纯函数便于测试）：
/// 先读 daimon config.json 里登记的 agentFile，失败退回固定相对路径；
/// 两者都以「目录真实存在」为准。
fn runtime_home_under(app_data: &Path) -> Option<PathBuf> {
    let daimon_dir = app_data.join("kimi-desktop").join("daimon-share").join("daimon");
    // config.json 是权威指针但可能缺失/损坏/指向已迁移的旧位置——best-effort，
    // 任何一步失败都落到固定相对路径兜底
    let from_config = (|| -> Option<PathBuf> {
        let config = std::fs::read_to_string(daimon_dir.join("config.json")).ok()?;
        let v: Value = serde_json::from_str(&config).ok()?;
        let agent_file = v
            .get("agents")?
            .get("defaults")?
            .get("agentFile")?
            .as_str()?;
        let home = Path::new(agent_file).parent()?.join("home");
        home.is_dir().then_some(home)
    })();
    from_config.or_else(|| {
        let fallback = daimon_dir.join("runtime").join("kimi-code").join("home");
        fallback.is_dir().then_some(fallback)
    })
}

fn runtime_home() -> Option<PathBuf> {
    dirs::data_dir().and_then(|d| runtime_home_under(&d))
}

/// wire.jsonl 一条记录 → (统一事件, 工具名, 通知文案)。映射表见模块头注释。
pub(crate) fn classify(r: &Value) -> Option<(EventKind, Option<String>, String)> {
    let ty = r.get("type").and_then(|v| v.as_str())?;
    match ty {
        "turn.prompt" => {
            let msg = r
                .get("input")
                .and_then(|i| i.as_array())
                .and_then(|a| a.iter().find_map(|p| p.get("text")).and_then(|t| t.as_str()))
                .unwrap_or("");
            Some((EventKind::Activity, None, truncate_chars(msg.trim(), 200)))
        }
        "context.append_loop_event" => {
            let e = r.get("event")?;
            match e.get("type").and_then(|v| v.as_str())? {
                "tool.call" => {
                    let name = e
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let tool = (!name.is_empty()).then_some(name);
                    Some((EventKind::Activity, tool, String::new()))
                }
                "tool.result" => Some((EventKind::ToolFinished, None, String::new())),
                // 协议 1.3 没有 turn.end：模型给出最终回答就是最后一步的
                // finishReason=end_turn（本机 4 回合样本 = 3×end_turn + 1×cancel 全闭环）
                "step.end" if e.get("finishReason").and_then(|v| v.as_str()) == Some("end_turn") => {
                    Some((EventKind::RunCompleted, None, String::new()))
                }
                _ => None,
            }
        }
        "turn.cancel" => Some((EventKind::RunAborted, None, String::new())),
        _ => None,
    }
}

/// 单会话的轮询状态
struct SessionTracker {
    /// wire.jsonl 已消费到的字节偏移（只推进到换行边界，保证 UTF-8 读取安全）
    offset: u64,
    /// 是否有回合在跑（turn.prompt 置位，end_turn / turn.cancel 复位）
    running: bool,
    /// 最近一次发事件的时间（运行态心跳节流用）
    last_emit_ms: i64,
    /// 最近一次回合任务文案（心跳正文的兜底）
    last_prompt: String,
    /// 跨轮不完整的尾行
    pending: String,
    last_seen_round: u64,
}

/// 从一段文本里切出完整行；无换行结尾的残行存回 `pending`。
/// 返回 (完整行列表, 完整部分消耗的字节数——即 pending 里被消费掉的字节数)。
fn split_complete_lines(chunk: &str, pending: &mut String) -> (Vec<String>, usize) {
    pending.push_str(chunk);
    let split = pending.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let complete: String = pending.drain(..split).collect();
    let lines = complete
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    (lines, split)
}

/// 启动回看：从 wire.jsonl 尾部样本推导「是否有回合正在跑」。
/// 返回 (要发的事件, 是否运行中)。样本里最后一个回合级信号决定状态：
/// turn.prompt / tool.call 之后没有 end_turn / turn.cancel 就是在跑。
/// 首见不发历史终态（与 WorkBuddy 的 `first` 播种同口径）：
/// 否则 daemon 启动会把全部历史完成会话重放成通知（启动风暴）。
fn bootstrap_tail(tail: &str) -> (Vec<(EventKind, Option<String>, String)>, bool) {
    let mut running = false;
    let mut last_prompt = String::new();
    for line in tail.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let ty = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "turn.prompt" => {
                running = true;
                if let Some((_, _, msg)) = classify(&r) {
                    last_prompt = msg;
                }
            }
            "context.append_loop_event" => {
                if let Some(e) = r.get("event") {
                    match e.get("type").and_then(|v| v.as_str()) {
                        Some("tool.call") => running = true,
                        // tool.result / step.end(tool_use) 只是步骤推进，
                        // 只有 end_turn 才是回合完成
                        Some("step.end")
                            if e.get("finishReason").and_then(|v| v.as_str()) == Some("end_turn") =>
                        {
                            running = false
                        }
                        _ => {}
                    }
                }
            }
            "turn.cancel" => running = false,
            _ => {}
        }
    }
    let events = running
        .then(|| vec![(EventKind::Activity, None, last_prompt)])
        .unwrap_or_default();
    (events, running)
}

/// 会话的 state.json 标题（终态通知正文用）；读不到就空着
fn session_title(session_dir: &Path) -> String {
    std::fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// 轮询周期与节流常量
const POLL_SECS: u64 = 3;
/// 运行态无新记录时的心跳重发间隔：60s 远小于 daemon 侧 10 分钟判死线
/// （纯思考的长回合没有 tool.call，靠它保活）
const HEARTBEAT_MS: i64 = 60_000;
/// 首见会话时的回看窗口（字节）
const BOOTSTRAP_TAIL: u64 = 64 * 1024;
/// 跨轮残行缓冲上限：单条 wire 记录不应超过；超限视为异常，放弃本行
const PENDING_CAP: usize = 1024 * 1024;
/// 会话跟踪表上限（超出淘汰最旧；会话文件基本只增不减）
const TRACKER_MAX: usize = 1024;
/// 快照连续失败每满该轮数打一条 warn（约 1 分钟一次）
const WARN_EVERY_ROUNDS: u32 = 20;

pub struct KimiWorkWatch;

impl KimiWorkWatch {
    fn sessions_dir() -> Option<PathBuf> {
        runtime_home().map(|h| h.join("sessions"))
    }
}

impl WatchAdapter for KimiWorkWatch {
    fn kind(&self) -> AgentKind {
        AgentKind::KimiWork
    }

    fn is_installed(&self) -> bool {
        // daimon 配置或内核 home 任一存在即视为装了 Kimi 电脑客户端
        daimon_config_path().is_some_and(|p| p.exists()) || runtime_home().is_some()
    }

    fn is_available(&self) -> bool {
        Self::sessions_dir().is_some_and(|p| p.is_dir())
    }

    fn unavailable_reason(&self) -> Option<String> {
        if self.is_available() {
            return None;
        }
        Some(
            "未找到 Kimi Work 的 agent 内核数据目录（daimon-share/daimon/runtime/kimi-code/home）。\
             可能未安装 Kimi 电脑客户端、从未运行过 Work 任务，或 Work 数据迁移过存储盘"
                .into(),
        )
    }

    fn spawn(
        &self,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> anyhow::Result<std::thread::JoinHandle<()>> {
        let home = runtime_home().ok_or_else(|| anyhow::anyhow!("未找到 Kimi Work 内核数据目录"))?;
        let kind = self.kind();
        Ok(std::thread::spawn(move || {
            let sessions_root = home.join("sessions");
            let index_path = home.join("session_index.jsonl");
            let mut trackers: HashMap<String, SessionTracker> = HashMap::new();
            let mut round: u64 = 0;
            let mut fail_count: u32 = 0;
            loop {
                round += 1;
                match poll_round(
                    &stop,
                    &tx,
                    kind,
                    &sessions_root,
                    &index_path,
                    &mut trackers,
                    round,
                ) {
                    Ok(()) => fail_count = 0,
                    Err(e) => {
                        fail_count += 1;
                        if fail_count.is_multiple_of(WARN_EVERY_ROUNDS) {
                            tracing::warn!(
                                "Kimi Work 会话目录轮询已连续失败 {fail_count} 次（最近错误：{e:#}）。\
                                 若持续出现，说明内核数据布局已变，监控实际处于失效状态。"
                            );
                        }
                    }
                }
                if !sleep_interruptible(&stop, POLL_SECS) {
                    break;
                }
            }
        }))
    }
}

/// 一轮轮询：刷新会话索引 → 逐会话消费 wire.jsonl 增量 → 运行态心跳 → 淘汰。
/// 单会话的文件级失败不影响其它会话（只计入整体失败计数）。
fn poll_round(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    sessions_root: &Path,
    index_path: &Path,
    trackers: &mut HashMap<String, SessionTracker>,
    round: u64,
) -> anyhow::Result<()> {
    // 会话 id → workDir（索引是追加写的小文件，整读无压力）
    let mut cwd_by_id: HashMap<String, String> = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(index_path) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if let (Some(id), Some(dir)) = (
                v.get("sessionId").and_then(|s| s.as_str()),
                v.get("workDir").and_then(|s| s.as_str()),
            ) {
                cwd_by_id.insert(id.to_string(), dir.to_string());
            }
        }
    }

    // 枚举 sessions/<wd>/<会话id>/agents/main/wire.jsonl
    let mut wires: Vec<(String, PathBuf)> = Vec::new();
    for wd in std::fs::read_dir(sessions_root)?.flatten() {
        let Ok(sessions) = std::fs::read_dir(wd.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            let wire = session.path().join("agents").join("main").join("wire.jsonl");
            if wire.is_file() {
                let sid = session.file_name().to_string_lossy().into_owned();
                wires.push((sid, wire));
            }
        }
    }

    let now = now_millis();
    for (sid, wire_path) in wires {
        let session_dir = wire_path
            .parent()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let cwd = cwd_by_id.get(&sid).cloned().unwrap_or_default();
        let size = match std::fs::metadata(&wire_path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let tracker = trackers.entry(sid.clone()).or_insert_with(|| {
            // 首见：回看尾部判断是否正有回合在跑，偏移直接推到 EOF
            // （历史终态不重放，避免启动风暴；运行中的会话立即点亮）
            let (events, running) = std::fs::read_to_string(&wire_path)
                .map(|full| {
                    let skip = full.len().saturating_sub(BOOTSTRAP_TAIL as usize);
                    let tail_start = full[skip..]
                        .find('\n')
                        .map(|p| skip + p + 1)
                        .unwrap_or(skip);
                    bootstrap_tail(&full[tail_start..])
                })
                .unwrap_or((Vec::new(), false));
            let last_emit_ms = if events.is_empty() { 0 } else { now };
            for (ev, tool, msg) in events {
                emit(stop, tx, kind, ev, &sid, &cwd, &msg, tool.as_deref());
            }
            SessionTracker {
                offset: size,
                running,
                last_emit_ms,
                last_prompt: String::new(),
                pending: String::new(),
                last_seen_round: round,
            }
        });
        tracker.last_seen_round = round;

        if size < tracker.offset {
            // 文件被截断/重建（异常路径）：按首见处理，偏移跳到当前 EOF
            tracker.offset = size;
            tracker.pending.clear();
        }
        if size > tracker.offset {
            match read_increment(&wire_path, tracker.offset) {
                Ok(chunk) => {
                    let (lines, consumed) = split_complete_lines(&chunk, &mut tracker.pending);
                    for line in lines {
                        let Ok(r) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        if let Some((ev, tool, msg)) = classify(&r) {
                            let msg = if ev == EventKind::RunCompleted || ev == EventKind::RunAborted {
                                session_title(&session_dir)
                            } else {
                                msg
                            };
                            emit(stop, tx, kind, ev, &sid, &cwd, &msg, tool.as_deref());
                            tracker.last_emit_ms = now;
                        }
                        match r.get("type").and_then(|v| v.as_str()) {
                            Some("turn.prompt") => {
                                tracker.running = true;
                                tracker.last_prompt = r
                                    .get("input")
                                    .and_then(|i| i.as_array())
                                    .and_then(|a| a.iter().find_map(|p| p.get("text")).and_then(|t| t.as_str()))
                                    .unwrap_or("")
                                    .to_string();
                            }
                            Some("turn.cancel") => tracker.running = false,
                            Some("context.append_loop_event") => {
                                if let Some(e) = r.get("event") {
                                    if e.get("type").and_then(|v| v.as_str()) == Some("step.end")
                                        && e.get("finishReason").and_then(|v| v.as_str()) == Some("end_turn")
                                    {
                                        tracker.running = false;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    tracker.offset += consumed as u64;
                }
                Err(e) => {
                    tracing::debug!("读取 Kimi Work 会话流失败（{}）：{e}", wire_path.display());
                }
            }
        }
        // 残行异常膨胀：单条记录不应这么大，放弃缓冲（下轮从新行继续）
        if tracker.pending.len() > PENDING_CAP {
            tracker.pending.clear();
            tracker.offset = size;
        }
        // 运行态心跳：纯思考的长回合没有任何 tool.call，靠它保活到判死线之外
        if tracker.running && now - tracker.last_emit_ms > HEARTBEAT_MS {
            emit(
                stop,
                tx,
                kind,
                EventKind::Activity,
                &sid,
                &cwd,
                &tracker.last_prompt,
                None,
            );
            tracker.last_emit_ms = now;
        }
    }

    // 淘汰最旧：会话目录基本只增不减，跟踪表不能无界
    if trackers.len() > TRACKER_MAX {
        let mut by_seen: Vec<(String, u64)> = trackers
            .iter()
            .map(|(k, v)| (k.clone(), v.last_seen_round))
            .collect();
        by_seen.sort_by_key(|(_, seen)| *seen);
        let overflow = trackers.len() - TRACKER_MAX;
        for (sid, _) in by_seen.into_iter().take(overflow) {
            trackers.remove(&sid);
        }
    }
    Ok(())
}

/// 从指定字节偏移读到 EOF（偏移恒在换行边界上，UTF-8 读取安全）
fn read_increment(path: &Path, offset: u64) -> anyhow::Result<String> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    f.read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn turn_prompt(text: &str) -> Value {
        json!({
            "type": "turn.prompt",
            "input": [{ "type": "text", "text": text }],
            "origin": { "kind": "user" },
            "time": 1781079396440i64
        })
    }

    fn loop_event(t: &str, extra: Value) -> Value {
        let mut e = json!({ "type": t });
        if let (Some(obj), Some(ex)) = (e.as_object_mut(), extra.as_object()) {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        json!({
            "type": "context.append_loop_event",
            "event": e,
            "time": 1781079396444i64
        })
    }

    /// 真实样本的映射钉死（记录形状取自本机 wire.jsonl，协议 1.3）
    #[test]
    fn classify_maps_real_records() {
        let (ev, tool, msg) = classify(&turn_prompt("帮我清理硬盘")).expect("turn.prompt 必须映射");
        assert_eq!(ev, EventKind::Activity);
        assert_eq!(tool, None);
        assert_eq!(msg, "帮我清理硬盘");

        let tool_call = loop_event(
            "tool.call",
            json!({ "uuid": "t1", "name": "Bash", "args": { "command": "ls" } }),
        );
        let (ev, tool, _) = classify(&tool_call).expect("tool.call 必须映射");
        assert_eq!(ev, EventKind::Activity);
        assert_eq!(tool.as_deref(), Some("Bash"), "工具名驱动「执行工具」相位");

        let (ev, tool, _) = classify(&loop_event(
            "tool.result",
            json!({ "parentUuid": "t1", "result": { "output": "ok" } }),
        ))
        .expect("tool.result 必须映射");
        assert_eq!(ev, EventKind::ToolFinished);
        assert_eq!(tool, None);

        let (ev, _, _) = classify(&loop_event(
            "step.end",
            json!({ "finishReason": "end_turn", "step": 3 }),
        ))
        .expect("step.end(end_turn) 必须映射");
        assert_eq!(ev, EventKind::RunCompleted, "end_turn 是协议 1.3 的回合完成信号");

        let (ev, _, _) = classify(&json!({ "type": "turn.cancel", "turnId": 0 })).expect("turn.cancel 必须映射");
        assert_eq!(ev, EventKind::RunAborted, "用户中止立即释放");
    }

    /// 无通知语义的记录一律忽略（含「审批解决」与流式分片）
    #[test]
    fn classify_ignores_noise_records() {
        for r in [
            json!({ "type": "metadata", "protocol_version": "1.3" }),
            json!({ "type": "config.update", "profileName": "agent" }),
            json!({ "type": "usage.record", "usage": {} }),
            json!({ "type": "permission.set_mode", "mode": "manual" }),
            json!({ "type": "permission.record_approval_result", "result": { "decision": "approved" } }),
            json!({ "type": "tools.update_store", "key": "todo" }),
            // finishReason=tool_use 是步骤推进（回合还在跑），不是完成
            loop_event("step.end", json!({ "finishReason": "tool_use" })),
            loop_event("content.part", json!({ "part": { "type": "think" } })),
            loop_event("step.begin", json!({ "step": 1 })),
        ] {
            assert!(classify(&r).is_none(), "应忽略: {r}");
        }
    }

    /// 启动回看：尾部停在工具调用后（回合在跑）→ 发一次 Activity 心跳；
    /// 尾部是 end_turn / turn.cancel（已结束）→ 不发历史终态（避免启动风暴）
    #[test]
    fn bootstrap_tail_only_reports_running_sessions() {
        let running = format!(
            "{}\n{}\n{}\n",
            json!(turn_prompt("长任务")),
            json!(loop_event("tool.call", json!({ "name": "Bash" }))),
            json!(loop_event("step.end", json!({ "finishReason": "tool_use" })))
        );
        let (events, running_flag) = bootstrap_tail(&running);
        assert!(running_flag, "尾部无终态信号 → 回合在跑");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, EventKind::Activity);
        assert_eq!(events[0].2, "长任务", "心跳正文带任务文案");

        let finished = format!(
            "{}\n{}\n",
            json!(turn_prompt("已完成的事")),
            json!(loop_event("step.end", json!({ "finishReason": "end_turn" })))
        );
        let (events, running_flag) = bootstrap_tail(&finished);
        assert!(!running_flag);
        assert!(events.is_empty(), "历史完成不得重放");

        let cancelled = format!("{}\n{}\n", json!(turn_prompt("x")), json!({ "type": "turn.cancel", "turnId": 0 }));
        let (events, running_flag) = bootstrap_tail(&cancelled);
        assert!(!running_flag);
        assert!(events.is_empty(), "历史中止不得重放");
    }

    /// 跨轮残行：无换行结尾的半行要等下一个 chunk 拼完才解析
    #[test]
    fn split_complete_lines_buffers_partial_tail() {
        let mut pending = String::new();
        let (lines, consumed) = split_complete_lines("{\"type\":\"turn.ca", &mut pending);
        assert!(lines.is_empty() && consumed == 0, "半行不产出也不消耗");

        let (lines, consumed) = split_complete_lines("ncel\"}\n{\"type\":\"usage.record\"}\n{partial", &mut pending);
        assert_eq!(lines.len(), 2);
        assert_eq!(consumed, "{\"type\":\"turn.cancel\"}\n{\"type\":\"usage.record\"}\n".len());
        assert_eq!(pending, "{partial", "尾部残行暂存");

        let (lines, _) = split_complete_lines("_rest\"}\n", &mut pending);
        assert_eq!(lines.len(), 1, "残行拼完后可解析");
        assert!(pending.is_empty());
    }

    /// home 解析：config.json 的 agentFile 优先（数据盘迁移后仍准），固定路径兜底
    #[test]
    fn runtime_home_prefers_daimon_config_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("agent-bark-kimiwork-{}", uuid::Uuid::new_v4().simple()));
        let app_data = dir.join("AppData");
        let runtime = app_data
            .join("kimi-desktop")
            .join("daimon-share")
            .join("daimon")
            .join("runtime")
            .join("kimi-code");
        std::fs::create_dir_all(runtime.join("home").join("sessions")).unwrap();

        // 无 config.json：兜底固定路径命中
        let home = runtime_home_under(&app_data).expect("兜底路径应命中");
        assert!(home.ends_with("kimi-code/home"));

        // config.json 登记的 agentFile 指向别处（模拟数据盘迁移）→ 跟随指针
        let migrated = dir.join("D盘").join("kimi-data").join("runtime").join("kimi-code");
        std::fs::create_dir_all(migrated.join("home")).unwrap();
        std::fs::write(
            app_data.join("kimi-desktop").join("daimon-share").join("daimon").join("config.json"),
            serde_json::to_string(&json!({
                "agents": { "defaults": { "agentFile": migrated.join("config.toml").to_string_lossy() } }
            }))
            .unwrap(),
        )
        .unwrap();
        let home = runtime_home_under(&app_data).expect("应跟随 agentFile 指针");
        assert_eq!(home, migrated.join("home"), "迁移后的 home 应被解析到");

        // agentFile 指向不存在的目录 → 回落固定路径
        std::fs::write(
            app_data.join("kimi-desktop").join("daimon-share").join("daimon").join("config.json"),
            serde_json::to_string(&json!({
                "agents": { "defaults": { "agentFile": "Q:\\nowhere\\config.toml" } }
            }))
            .unwrap(),
        )
        .unwrap();
        let home = runtime_home_under(&app_data).expect("指针失效应回落固定路径");
        assert!(home.ends_with("kimi-code/home"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端（纯函数拼装）：首见运行中会话发心跳，随后的增量按序产出事件
    #[test]
    fn steady_state_event_order() {
        // 首见：文件已有「在跑」的尾部
        let (events, running) = bootstrap_tail(&format!(
            "{}\n{}\n",
            json!(turn_prompt("清理任务")),
            json!(loop_event("tool.call", json!({ "name": "Bash" })))
        ));
        assert!(running && events.len() == 1);

        // 增量：工具收尾 → 完成
        let batch = format!(
            "{}\n{}\n{}\n",
            json!(loop_event("tool.result", json!({}))),
            json!(loop_event("step.end", json!({ "finishReason": "end_turn" }))),
            json!({ "type": "usage.record", "usage": {} })
        );
        let evs: Vec<EventKind> = batch
            .lines()
            .filter_map(|l| classify(&serde_json::from_str(l).unwrap()).map(|(e, _, _)| e))
            .collect();
        assert_eq!(evs, vec![EventKind::ToolFinished, EventKind::RunCompleted]);
    }
}
