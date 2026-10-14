//! 豆包工作（字节跳动 DoubaoWork 桌面客户端）监控型适配器 —— **非官方**。
//!
//! 架构（2026-10-10 在本机 DoubaoWork 2.32.3 实测逆向）：Chromium 壳
//! （`%LOCALAPPDATA%\DoubaoWork\User Data\<profile>\`）+ 本地 agent 运行时
//! （agent_infra.dll，负责文件/Shell 等本地工具执行）。Agent 模式的每个会话
//! 以 OpenAI 风格对话流落盘在应用 profile 内：
//!
//! ```text
//! User Data/<profile>/.doubaowork/agent_mode/workspace/.sessions/<会话id>/
//! ├── agents/<agentid>/
//! │   ├── system/assignment.md    逐回合追加的需求流水（含 "- Role: main" 头）
//! │   └── system/trajectory.jsonl 会话对话流（append-only，跨回合共享）
//! ├── board.md                    任务公告板（运行期持续改写）
//! └── memory/MEMORY.md
//! ```
//!
//! **三个只读数据源，合并生命周期信号**：
//!
//! 1. trajectory.jsonl —— 只记录**循环内部**的消息：`role=user`（每条用户输入，
//!    含排队连发）、assistant 带 `tool_calls`（工具运行；`interaction.ask` 是向
//!    用户提问）、`role=tool`（工具结果；**也是提问被回答的解除信号**）。
//!    **最终回复不落盘**——本机完整会话（十几次回复）无一条无 `tool_calls` 的
//!    assistant 行（2026-10-10 实测证伪了「OpenAI 循环最终回答即完成」的假设，
//!    保留该映射只是兜底：未来版本若开始写终行，daemon 侧终态回声会去重）。
//!
//! 2. 渲染进程 IndexedDB（`Default/IndexedDB/chrome_doubaowork-chat_0.indexeddb.leveldb`）
//!    —— 会话记录的 `extra` JSON 里有 `running_messages`（运行中消息 id →
//!    started_at_ms）：**回合开始时写入条目、回合真正结束时清除**（会话目录里
//!    没有任何完成标记，这是唯一的完成信号；分代 compaction 快照 + 当前 .log
//!    逐点核对过：运行中非空、结束后为 `{}`）。`agent_task_param.workspace`
//!    还带着项目目录，通知由此能带项目名。
//!
//! 3. `sdk_storage/log/saman_*.log` 的 `sdk.message.break.request`：
//!    `break_reason_name=click_break_button` 且 `result=success` 是明确的用户停止；
//!    用 conversation_id + message_id 与 IndexedDB 运行条目关联，旧回合不能关闭新回合。
//!
//! | 信号 | 统一事件 |
//! | --- | --- |
//! | trajectory `role=user` | activity（回合开始 / 排队消息，content 为任务文案） |
//! | trajectory `tool_calls[]` | activity（工具运行，带工具名） |
//! | trajectory `interaction.ask` | **input_required**（向用户提问，带问题文案） |
//! | trajectory `role=tool` | tool_finished（工具收尾 / 提问被回答的解除信号） |
//! | SDK 确认用户停止，且消息 ID 匹配 | **run_aborted**（已终止） |
//! | **IndexedDB 条目清除** | **run_completed**（等待 6s 补齐取消日志；提问挂起时延迟 60s 判定） |
//! | trajectory 无 `tool_calls` 的 assistant 行 | run_completed（兜底，见上） |
//!
//! IndexedDB 读取方式：只读扫描 WAL（`.log`）与 SST（`.ldb`），按 LevelDB sequence
//! 合并同一记录的新旧版本；校验物理块、重组分段、解开 Snappy 与 V8 单/双字节字符串，
//! 再解析 `extra` JSON。缓存未变化文件，轮转后旧状态不能覆盖新状态，不持锁、不写宿主文件。
//!
//! 为什么不做 Hook 型：豆包工作没有 hooks / CLI 扩展点；Watch 型零写盘。
//!
//! 已知边界：
//! - **失败原因仍不可见**；取消日志缺失/格式漂移，或未观测到对应运行消息 ID 时，
//!   IndexedDB 条目清除仍回退为完成，失败或停止的任务可能误报「任务完成」；
//!   数据源失效时退化为事件静止 + 10 分钟判死（不误报）
//! - 只看主 agent（`assignment.md` 头部 `Role: main`）；子智能体的 trajectory
//!   不出事件（它们的「完成」不是任务的完成，且没有后续事件能收档）
//! - IndexedDB 不可读时（目录缺失 / schema 漂移）降级为 trajectory 单源：
//!   完成信号缺失，轨迹停止增长 15 分钟后按「中止」收场（不亮失败色），
//!   下次活动自动恢复——不会永远卡思考色
//! - 已确认完成后停止心跳并屏蔽迟到工具记录；IndexedDB 已确认空闲的会话，
//!   需等下一次运行跃迁再开启，避免迟落盘的旧 user 消息复活已结束回合。
//! - 数据布局按 2026-10 版本校准（2.32.3 存储；2.32.6 取消日志），应用大版本升级可能失效

use crate::doubaowork_idb::IdbLogTail;
use crate::doubaowork_log::CancelLog;
use crate::jsonl::{bootstrap_sample, read_bootstrap, split_complete_lines};
use crate::watch::{emit, emit_lost_row, sleep_interruptible, WatchAdapter};
use bark_core::{now_millis, truncate_chars, AgentKind, EventKind, NormalizedEvent};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 应用数据根：`%LOCALAPPDATA%\DoubaoWork\User Data`
fn user_data_root() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("DoubaoWork").join("User Data"))
}

/// 所有 profile 下的会话根（正常只有 Default；多 profile 全收，防漏）。
fn sessions_roots_under(user_data: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(profiles) = std::fs::read_dir(user_data) else {
        return out;
    };
    for p in profiles.flatten() {
        let dir = p
            .path()
            .join(".doubaowork")
            .join("agent_mode")
            .join("workspace")
            .join(".sessions");
        if dir.is_dir() {
            out.push(dir);
        }
    }
    out
}

fn sessions_roots() -> Vec<PathBuf> {
    user_data_root().map(|r| sessions_roots_under(&r)).unwrap_or_default()
}

/// 所有 profile 下的 IndexedDB 库目录（`<profile>/IndexedDB/*.indexeddb.leveldb`）。
/// 会话状态在 `chrome_doubaowork-chat` 库里，但多扫几个库便宜且抗拆分。
fn indexeddb_dirs_under(user_data: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(profiles) = std::fs::read_dir(user_data) else {
        return out;
    };
    for profile in profiles.flatten() {
        let Ok(dbs) = std::fs::read_dir(profile.path().join("IndexedDB")) else {
            continue;
        };
        for db in dbs.flatten() {
            let dir = db.path();
            if dir.is_dir()
                && dir
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().ends_with(".indexeddb.leveldb"))
            {
                out.push(dir);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// IndexedDB 增量尾随
// ---------------------------------------------------------------------------

/// 一次扫描产出的会话状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdbUpdate {
    /// 会话 id（与 .sessions/<会话id> 目录名同源）
    pub conv: String,
    /// running_messages 非空 = 有回合在跑
    pub running: bool,
    pub message_ids: Vec<String>,
    /// agent_task_param.workspace（项目目录；拿不到为空串）
    pub workspace: String,
}

/// 全部 IndexedDB 库的增量游标集合（跨轮复用，放互斥锁里轮询）
type IdbTails = std::sync::Mutex<IdbLogTail>;

// ---------------------------------------------------------------------------
// trajectory 分类与轮询
// ---------------------------------------------------------------------------

/// `tool_calls[].function.name`
fn tool_name(call: &Value) -> Option<&str> {
    call.get("function")?.get("name")?.as_str()
}

/// `interaction.ask` 的问题文案：`arguments.questions[0].display_message`
/// （arguments 在本产品里已是对象；异常形状一律回空串，不影响事件本身）
fn ask_question_text(call: &Value) -> String {
    let q = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.get("questions"))
        .and_then(|qs| qs.as_array())
        .and_then(|qs| qs.first())
        .and_then(|q| q.get("display_message"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    truncate_chars(q.trim(), 200)
}

/// content 归一为纯文本：字符串直接用；数组取 `type:text` 分片的 text 拼接
fn content_text(r: &Value) -> String {
    match r.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                let is_text = p.get("type").and_then(|t| t.as_str()) == Some("text");
                is_text.then(|| p.get("text").and_then(|t| t.as_str()).unwrap_or("")).map(str::to_string)
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// trajectory.jsonl 一条消息 → (统一事件, 工具名, 通知文案)。
/// 注意：本产品**不在 trajectory 里写最终回复**（见模块头），无 `tool_calls`
/// 的 assistant 行映射成 run_completed 只是兜底（未来版本若开始写终行能提前
/// 触发；与本轮 IndexedDB 清除信号重复时由 daemon 终态回声去重）。
pub(crate) fn classify(r: &Value) -> Option<(EventKind, Option<String>, String)> {
    let role = r.get("role").and_then(|v| v.as_str())?;
    match role {
        "user" => {
            let msg = content_text(r);
            Some((EventKind::Activity, None, truncate_chars(msg.trim(), 200)))
        }
        "assistant" => {
            let calls = r
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .filter(|c| !c.is_empty());
            match calls {
                None => {
                    let done = !content_text(r).trim().is_empty();
                    done.then(|| (EventKind::RunCompleted, None, String::new()))
                }
                Some(calls) => {
                    if let Some(ask) = calls.iter().find(|c| tool_name(c) == Some("interaction.ask")) {
                        Some((EventKind::InputRequired, None, ask_question_text(ask)))
                    } else {
                        let name = calls.iter().find_map(|c| tool_name(c)).unwrap_or("");
                        let tool = (!name.is_empty()).then(|| name.to_string());
                        Some((EventKind::Activity, tool, String::new()))
                    }
                }
            }
        }
        // 工具收尾（含 interaction.ask 的回答结果——等待解除、回合继续）
        "tool" => Some((EventKind::ToolFinished, None, String::new())),
        _ => None,
    }
}

/// 该 agent 目录是否主智能体：`system/assignment.md` 头部的 `- Role:` 行。
/// 读不到 / 没写 Role 按主智能体处理（宁可多看不漏看）。
fn is_main_agent(agent_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(agent_dir.join("system").join("assignment.md")) else {
        return true;
    };
    match text.lines().find_map(|l| l.trim().strip_prefix("- Role:")) {
        Some(role) => role.trim().eq_ignore_ascii_case("main"),
        None => true,
    }
}

/// 单会话的轮询状态
struct SessionTracker {
    /// 会话 id（.sessions/<会话id> 目录名；闭环「文件消失」与 IndexedDB 关联时用）
    sid: String,
    message_ids: Vec<String>,
    aborted: bool,
    /// trajectory.jsonl 已读取的字节偏移（未完成的行保存在 pending）
    offset: u64,
    /// 已确认收场；缓存的运行状态与迟到工具记录不能重新开启回合。
    settled: bool,
    /// trajectory 视角的「有回合在跑」（user/工具行置位；IndexedDB 可用时以它为准）
    running: bool,
    /// 卡在 interaction.ask 等用户回答（提问期间不发心跳——心跳会把 daemon 侧
    /// WaitingInput 打回思考色，等待色不能自己冲掉）
    waiting_ask: bool,
    /// IndexedDB 视角的运行中（None = 尚无数据 / 不可读——降级为 trajectory 单源）
    idb_running: Option<bool>,
    /// 提问挂起时条目被清除：延迟到该时刻（毫秒时间戳）再判完成，期间回答则取消
    pending_complete_at: Option<i64>,
    /// 最近一次发事件的时间（运行态心跳节流用）
    last_emit_ms: i64,
    /// 最近一次 trajectory 增长的时间（降级模式的停摆判定用）
    last_growth_ms: i64,
    /// 最近一次回合任务文案（心跳正文 / 完成通知正文）
    last_prompt: String,
    /// 项目目录（IndexedDB 的 agent_task_param.workspace，非空即回填）
    workspace: String,
    /// 跨轮不完整的尾行
    pending: String,
    last_seen_round: u64,
}

/// 启动回看：从 trajectory.jsonl 尾部样本推导「是否有回合正在跑/等回答」。
/// 返回 (要发的事件, trajectory 是否在跑, 是否等回答, 是否已确认完成)。最后一个回合级信号决定：
/// - user / 工具运行 / 工具收尾 → 运行中（发 Activity 点亮）
/// - `interaction.ask` → 等回答（也点亮一次，但 running=false——等待态不心跳）
/// - 最终回答 → 已结束（不发历史终态，避免启动风暴）
///
/// 空日志表示尚无信号，不能与明确完成混为一谈（云端任务可能只有 IndexedDB 在跑）。
type BootstrapTail = (Vec<(EventKind, Option<String>, String)>, bool, bool, bool);

fn bootstrap_tail(tail: &str) -> BootstrapTail {
    let mut active: Option<bool> = None;
    let mut waiting = false;
    let mut last_prompt = String::new();
    for line in tail.lines() {
        let Ok(r) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some((ev, _, msg)) = classify(&r) else { continue };
        let role_is_user = r.get("role").and_then(|v| v.as_str()) == Some("user");
        match ev {
            EventKind::InputRequired => {
                active = Some(false);
                waiting = true;
            }
            // 工具收尾后回合仍在继续（循环里 tool 结果后面必然还有 assistant 行）
            EventKind::ToolFinished => {
                active = Some(true);
                waiting = false;
            }
            EventKind::Activity => {
                active = Some(true);
                waiting = false;
                if role_is_user && !msg.is_empty() {
                    last_prompt = msg;
                }
            }
            _ => {
                active = Some(false);
                waiting = false;
            }
        }
    }
    let events = match active {
        Some(true) => vec![(EventKind::Activity, None, last_prompt.clone())],
        // 等待态也点亮（蓝色 Activity，不重放等待通知——daemon 重启不该再打扰）
        Some(false) if waiting => vec![(EventKind::Activity, None, last_prompt)],
        _ => Vec::new(),
    };
    (events, active == Some(true), waiting, active == Some(false) && !waiting)
}

/// 轮询周期与节流常量
const POLL_SECS: u64 = 3;
/// 运行态无新记录时的心跳重发间隔：60s 远小于 daemon 侧 10 分钟判死线
/// （云端生图 / 长思考期间没有任何本地事件，靠它保活）
const HEARTBEAT_MS: i64 = 60_000;
/// 首见会话时的回看窗口（字节）
const BOOTSTRAP_TAIL: u64 = 64 * 1024;
/// 跨轮残行缓冲上限：单条消息不应超过；超限视为异常，放弃本行
const PENDING_CAP: usize = 1024 * 1024;
/// 会话跟踪表上限（超出淘汰最旧；会话文件基本只增不减）
const TRACKER_MAX: usize = 1024;
/// 轮询连续失败每满该轮数打一条 warn（约 1 分钟一次）
const WARN_EVERY_ROUNDS: u32 = 20;
/// 提问挂起时条目被清除的判定延迟：回答若会清除条目，trajectory 的 tool 结果行
/// 会在这之前到达（本机实测提问-回答间隔秒级到分钟级都远小于它）
const ASK_CLEAR_GRACE_MS: i64 = 60_000;
// 数据库空闲可能先于取消确认日志到达，留两个轮询周期供日志补齐。
const COMPLETE_GRACE_MS: i64 = 6_000;
/// 降级模式（IndexedDB 不可读）的停摆判定：轨迹停止增长这么久按「中止」收场
/// （RunAborted 不通知不亮失败色；下次活动自动恢复）。长云端静默阶段在降级模式下
/// 会被误收场，但降级本身已无完成信号，两害取其轻。
const STALL_ABORT_MS: i64 = 15 * 60_000;
/// 首见时无 IndexedDB 种子的停摆判定：trajectory 这么久没有文件活动视为空闲
/// （不点亮、running=false；后续任何真实事件照常接管）。种子只覆盖当前 .log
/// 里出现过的会话，compaction 会把老会话的最新状态挪进 .ldb 读不到，靠它兜底。
const BOOTSTRAP_IDLE_AFTER_MS: i64 = 15 * 60_000;

pub struct DoubaoWorkWatch;

impl WatchAdapter for DoubaoWorkWatch {
    fn kind(&self) -> AgentKind {
        AgentKind::DoubaoWork
    }

    fn is_installed(&self) -> bool {
        user_data_root().is_some_and(|p| p.is_dir())
    }

    fn is_available(&self) -> bool {
        !sessions_roots().is_empty()
    }

    fn unavailable_reason(&self) -> Option<String> {
        if self.is_available() {
            return None;
        }
        Some(
            "未找到豆包工作的会话目录（User Data/<profile>/.doubaowork/.../.sessions）。\
             可能未安装豆包工作，或装了但从未使用过 Agent 模式"
                .into(),
        )
    }

    fn spawn(
        &self,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> anyhow::Result<std::thread::JoinHandle<()>> {
        let roots = sessions_roots();
        if roots.is_empty() {
            anyhow::bail!("未找到豆包工作会话目录");
        }
        let kind = self.kind();
        Ok(std::thread::spawn(move || {
            let idb_tails: Vec<IdbTails> = indexeddb_dirs_under(
                &user_data_root().unwrap_or_else(|| PathBuf::from(".")),
            )
            .into_iter()
            .map(|d| IdbTails::new(IdbLogTail::new(d)))
            .collect();
            // tracker 键 = trajectory.jsonl 绝对路径（跨 profile 天然不冲突）
            let mut trackers: HashMap<String, SessionTracker> = HashMap::new();
            let mut cancel_log = CancelLog::new(user_data_root().unwrap_or_default().join("sdk_storage/log"));
            let mut ctx = ScanCtx {
                cancel_log: &mut cancel_log,
                roots: &roots,
                idb_tails: &idb_tails,
                // IndexedDB 会话状态表（跨轮持久），种子供建档时压制已完成会话
                idb_state: &mut HashMap::new(),
            };
            let mut round: u64 = 0;
            let mut fail_count: u32 = 0;
            loop {
                round += 1;
                match poll_round(&stop, &tx, kind, &mut ctx, &mut trackers, round) {
                    Ok(()) => fail_count = 0,
                    Err(e) => {
                        fail_count += 1;
                        if fail_count.is_multiple_of(WARN_EVERY_ROUNDS) {
                            tracing::warn!(
                                "豆包工作会话目录轮询已连续失败 {fail_count} 次（最近错误：{e:#}）。\
                                 若持续出现，说明数据布局已变，监控实际处于失效状态。"
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

/// 一轮轮询的扫描上下文（会话根、IndexedDB 游标与状态表）
struct ScanCtx<'a> {
    cancel_log: &'a mut CancelLog,
    roots: &'a [PathBuf],
    idb_tails: &'a [IdbTails],
    /// IndexedDB 会话状态表（跨轮持久）：首轮扫描先于建档，种子必须留到
    /// tracker 建档时用——否则重启后已完成会话会被 trajectory 引导点亮，
    /// 10 分钟后被判死误亮失败色
    idb_state: &'a mut HashMap<String, (bool, String, Vec<String>)>,
}

/// 一轮轮询：IndexedDB 状态增量 → trajectory 增量 → 终态/心跳 → 闭环 → 淘汰。
/// 单文件的失败不影响其它会话（只计入整体失败计数）。
fn poll_round(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    ctx: &mut ScanCtx<'_>,
    trackers: &mut HashMap<String, SessionTracker>,
    round: u64,
) -> anyhow::Result<()> {
    // 1) 扫 IndexedDB 增量并合并进状态表
    let mut updates: Vec<IdbUpdate> = Vec::new();
    for tail in ctx.idb_tails {
        if let Ok(mut t) = tail.lock() {
            updates.extend(t.scan());
        }
    }
    for u in updates {
        ctx.idb_state.insert(u.conv.clone(), (u.running, u.workspace, u.message_ids));
    }
    // 状态表防无界增长：只保留有 tracker 的会话（状态只在跃迁时写入，
    // 轮转重扫可能带回大量历史会话条目）
    if ctx.idb_state.len() > TRACKER_MAX * 4 {
        ctx.idb_state.retain(|conv, _| trackers.values().any(|t| t.sid == *conv));
    }
    // 2) 状态跃迁（完成信号的主源）——只对已建档的会话生效
    ctx.cancel_log.poll();
    apply_idb_state(stop, tx, kind, trackers, ctx.idb_state, ctx.cancel_log, now_millis());

    // 2) 枚举全部主 agent 的 trajectory.jsonl，消费增量
    let mut wires: Vec<(String, PathBuf)> = Vec::new();
    for root in ctx.roots {
        let Ok(sessions) = std::fs::read_dir(root) else {
            continue;
        };
        for session in sessions.flatten() {
            let agents = session.path().join("agents");
            let Ok(agent_dirs) = std::fs::read_dir(&agents) else {
                continue;
            };
            for agent in agent_dirs.flatten() {
                // 子智能体（Role: 非 main）整路跳过：它们的「完成」不是任务的完成，
                // 且没有后续事件能收档，建档必然走到 10 分钟判死亮红光
                if !is_main_agent(&agent.path()) {
                    continue;
                }
                let wire = agent.path().join("system").join("trajectory.jsonl");
                let sid = session.file_name().to_string_lossy().into_owned();
                let awaiting_trajectory = ctx.idb_state.contains_key(&sid)
                    && agent.path().join("system/assignment.md").is_file()
                    && trackers.get(&wire.display().to_string()).is_none_or(|t| t.offset == 0);
                if wire.is_file() || awaiting_trajectory {
                    wires.push((sid, wire));
                }
            }
        }
    }

    let now = now_millis();
    let mut seen_keys: Vec<String> = Vec::new();
    for (sid, wire_path) in wires {
        seen_keys.push(wire_path.display().to_string());
        let (size, mtime_age_ms) = match std::fs::metadata(&wire_path) {
            Ok(m) => (
                m.len(),
                m.modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_millis() as i64),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (0, None),
            Err(_) => continue,
        };
        let key = wire_path.display().to_string();
        let initial = if !trackers.contains_key(&key) {
            match if size == 0 { Ok((String::new(), 0)) } else { read_bootstrap(&wire_path, size) } {
                Ok(snapshot) => Some(snapshot),
                Err(e) => {
                    tracing::debug!("读取豆包工作初始日志失败（{}）：{e}", wire_path.display());
                    continue;
                }
            }
        } else { None };
        let tracker = trackers.entry(key).or_insert_with(|| {
            // 首见：回看尾部判断是否正有回合在跑/等回答，偏移只推进到完整行末尾
            // （历史终态不重放，避免启动风暴；运行中的会话立即点亮）
            let (full, offset) = initial.expect("新会话必须先成功读取初始快照");
            let (mut events, mut running, waiting, completed) =
                bootstrap_tail(bootstrap_sample(&full, BOOTSTRAP_TAIL as usize));
            // IndexedDB 种子：会话已结束（条目空）时不发引导点亮——否则重启后
            // 全部历史会话被点亮一轮、10 分钟后被判死误亮失败色。等回答态同理
            // （条目保留才是在等；条目已清说明早不是活会话）。
            let seeded = ctx.idb_state.get(&sid).cloned();
            let idb_running = seeded.as_ref().map(|(r, _, _)| *r);
            // 云端任务可能直到结束才落 trajectory；运行信号不应等待第一条 user。
            if idb_running == Some(true) && events.is_empty() && !completed {
                events.push((EventKind::Activity, None, String::new()));
                running = true;
            }
            let last_emit_ms = if events.is_empty() { 0 } else { now };
            // 数据库不可读或尚未记录此会话时，按轨迹新鲜度兜底，避免历史消息重放。
            let stale_idle = seeded.is_none()
                && mtime_age_ms.is_some_and(|age| age > BOOTSTRAP_IDLE_AFTER_MS);
            let suppressed = idb_running == Some(false) || stale_idle;
            let last_prompt = events.first().map(|(_, _, msg)| msg.clone()).unwrap_or_default();
            if !suppressed {
                for (ev, tool, msg) in events {
                    emit(stop, tx, kind, ev, &sid, "", &msg, tool.as_deref());
                }
            }
            SessionTracker {
                aborted: false,
                sid: sid.clone(),
                message_ids: seeded.as_ref().map(|(_, _, ids)| ids.clone()).unwrap_or_default(),
                offset,
                settled: suppressed || completed,
                running: running && !suppressed,
                waiting_ask: waiting && !suppressed,
                // 停摆判定只压点亮，不写 idb_running（保持 None = 降级心跳口径，
                // 后续任何真实事件照常接管，不产生「卡在已结束」的错误状态）
                idb_running: if stale_idle { None } else { idb_running },
                pending_complete_at: None,
                last_emit_ms: if suppressed { 0 } else { last_emit_ms },
                last_growth_ms: now,
                last_prompt,
                workspace: seeded.map(|(_, w, _)| w).unwrap_or_default(),
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
            tracker.last_growth_ms = now;
            match read_increment(&wire_path, tracker.offset) {
                Ok(chunk) => {
                    let (lines, consumed) = split_complete_lines(&chunk, &mut tracker.pending);
                    for line in lines {
                        let Ok(r) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let role = r.get("role").and_then(|v| v.as_str());
                        if role == Some("user") {
                            let text = content_text(&r);
                            if !text.trim().is_empty() {
                                tracker.last_prompt = truncate_chars(text.trim(), 200);
                            }
                        }
                        // 数据库已确认空闲时，轨迹可能只是迟到/重写的旧消息。
                        // 仍记住新文案，待数据库 false→true 再开启下一轮。
                        // 提问清除的宽限期例外：真实回答必须仍能解除等待。
                        if (tracker.idb_running == Some(false) && (tracker.settled || (tracker.pending_complete_at.is_some() && !tracker.waiting_ask)))
                            || (tracker.idb_running.is_some() && tracker.aborted) {
                            continue;
                        }
                        if tracker.settled && role != Some("user") {
                            continue;
                        }
                        if let Some((ev, tool, msg)) = classify(&r) {
                            if ev == EventKind::RunCompleted {
                                tracker.settled = true;
                                tracker.waiting_ask = false;
                                tracker.pending_complete_at = None;
                            }
                            // 终态正文用本回合的任务文案（「任务完成」下面直接看到是哪件事）
                            let msg = if ev == EventKind::RunCompleted {
                                tracker.last_prompt.clone()
                            } else {
                                msg
                            };
                            emit(stop, tx, kind, ev, &sid, &tracker.workspace, &msg, tool.as_deref());
                            tracker.last_emit_ms = now;
                        }
                        // trajectory 视角状态推进（与 classify 同源判定，见模块头映射表）
                        match r.get("role").and_then(|v| v.as_str()) {
                            Some("user") => {
                                tracker.settled = false;
                                tracker.running = true;
                                tracker.waiting_ask = false;
                                tracker.pending_complete_at = None;
                            }
                            Some("tool") => {
                                tracker.running = true;
                                // 提问被回答：解除等待、撤销「清除即完成」的延迟判定
                                tracker.waiting_ask = false;
                                tracker.pending_complete_at = None;
                            }
                            Some("assistant") => {
                                let calls = r
                                    .get("tool_calls")
                                    .and_then(|t| t.as_array())
                                    .filter(|c| !c.is_empty());
                                match calls {
                                    None => tracker.running = false,
                                    Some(calls) => {
                                        let is_ask =
                                            calls.iter().any(|c| tool_name(c) == Some("interaction.ask"));
                                        if is_ask {
                                            tracker.waiting_ask = true;
                                        }
                                        // 提问复位 running（等待期心跳会冲掉等待色）；其余工具运行保持
                                        tracker.running = !is_ask;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    tracker.offset += consumed as u64;
                }
                Err(e) => {
                    tracing::debug!("读取豆包工作会话流失败（{}）：{e}", wire_path.display());
                }
            }
        }
        // 残行异常膨胀：单条消息不应这么大，放弃缓冲（下轮从新行继续）
        if tracker.pending.len() > PENDING_CAP {
            tracker.pending.clear();
            tracker.offset = size;
        }

        // 「提问挂起时条目被清除」的宽限到期：仍无回答 → 判完成
        // （模型：条目清除 = 回合结束；只有「回答即清除」需要这层宽限区分）
        if let Some(deadline) = tracker.pending_complete_at {
            if now >= deadline {
                tracker.pending_complete_at = None;
                tracker.settled = true;
                tracker.running = false;
                tracker.waiting_ask = false;
                emit(
                    stop,
                    tx,
                    kind,
                    EventKind::RunCompleted,
                    &sid,
                    &tracker.workspace,
                    &tracker.last_prompt,
                    None,
                );
                tracker.last_emit_ms = now;
            }
        }

        // 运行态心跳：云端生图 / 长思考期间没有任何本地事件，靠它保活到判死线之外。
        // IndexedDB 可用时以它为准（完成后的条目清除会顺带停掉心跳）；
        // 不可用时退回 trajectory 视角（见 STALL_ABORT_MS 的降级兜底）。
        let heartbeats = match tracker.idb_running {
            Some(r) => r && !tracker.waiting_ask,
            None => tracker.running && !tracker.waiting_ask,
        };
        if !tracker.settled && heartbeats && now - tracker.last_emit_ms > HEARTBEAT_MS {
            emit(
                stop,
                tx,
                kind,
                EventKind::Activity,
                &sid,
                &tracker.workspace,
                &tracker.last_prompt,
                None,
            );
            tracker.last_emit_ms = now;
        }

        // 降级兜底：IndexedDB 不可读 + trajectory 停摆 → 按「中止」收场
        // （不通知不亮失败色；完成信号缺失时至少不让会话永远卡在思考色）
        if tracker.idb_running.is_none()
            && tracker.running
            && !tracker.waiting_ask
            && now - tracker.last_growth_ms > STALL_ABORT_MS
        {
            tracker.running = false;
            tracker.settled = true;
            emit(
                stop,
                tx,
                kind,
                EventKind::RunAborted,
                &sid,
                &tracker.workspace,
                "轨迹停止更新且无法读取会话状态，按中止收场",
                None,
            );
            tracker.last_emit_ms = now;
        }
    }

    // 「trajectory 从会话树里消失」的闭环（§1.12a 同一口径）：运行中的会话被删除
    // 不会有终态跃迁，主动发 RunAborted 收档（不通知、不亮失败色），别等 daemon
    // 侧 10 分钟僵死判死在随机时刻亮失败色。终态/等待态的条目直接静默移除。
    trackers.retain(|key, t| {
        if seen_keys.iter().any(|k| k == key) {
            return true;
        }
        if t.running {
            emit_lost_row(stop, tx, kind, &t.sid, "豆包工作");
        }
        false
    });

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

/// 应用 IndexedDB 状态表到已建档的会话：跃迁产出终态事件，workspace 非空即回填。
/// - `true→false`（或轨迹已活动后首次读到 false）：回合完成（**主完成信号**——最终回复不落盘，会话目录里也没有
///   完成标记，条目清除是唯一的结束标志）；提问挂起时转延迟判定
/// - `false→true`：确认下一轮开始，允许重开已收场会话；trajectory 可能先于它落盘。
/// - 首次播种：空闲且未活动的会话不产事件；引导点亮在建档处按种子压制。
/// - 只作用于已建档（有主 agent 轨迹或 assignment）的会话：豆包普通聊天也有 running_messages，
///   但那不是 agent 任务
fn apply_idb_state(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    trackers: &mut HashMap<String, SessionTracker>,
    idb_state: &HashMap<String, (bool, String, Vec<String>)>,
    cancel_log: &CancelLog,
    now: i64,
) {
    for tracker in trackers.values_mut() {
        let Some(&(running, ref workspace, ref messages)) = idb_state.get(&tracker.sid) else {
            continue;
        };
        let prev = tracker.idb_running;
        tracker.idb_running = Some(running);
        if tracker.workspace.is_empty() && !workspace.is_empty() {
            tracker.workspace = workspace.clone();
        }
        let new_messages = running && !messages.is_empty() && *messages != tracker.message_ids;
        if running && (new_messages || prev == Some(false) || (prev.is_none() && tracker.settled)) {
            tracker.message_ids = messages.clone();
            tracker.aborted = false;
            tracker.pending_complete_at = None;
            tracker.settled = false;
            tracker.running = !tracker.waiting_ask;
            if !tracker.waiting_ask {
                emit(stop, tx, kind, EventKind::Activity, &tracker.sid,
                    &tracker.workspace, &tracker.last_prompt, None);
                tracker.last_emit_ms = now;
            }
        }
        if !tracker.settled && cancel_log.matches(&tracker.sid, &tracker.message_ids) {
            tracker.settled = true;
            tracker.aborted = true;
            tracker.running = false;
            tracker.waiting_ask = false;
            tracker.pending_complete_at = None;
            emit(stop, tx, kind, EventKind::RunAborted, &tracker.sid, &tracker.workspace, "回合被手动终止", None);
            tracker.last_emit_ms = now;
        }
        let became_idle = !running && prev != Some(false)
            && (prev == Some(true) || tracker.running || tracker.waiting_ask);
        if became_idle && !tracker.settled && tracker.pending_complete_at.is_none() {
            if tracker.waiting_ask {
                // 提问挂起时条目被清除：可能是「回答即清除」的另一种实现，
                // 等 trajectory 的 tool 结果行（宽限内到达则撤销）
                tracker.pending_complete_at = Some(now + ASK_CLEAR_GRACE_MS);
            } else {
                tracker.running = false;
                tracker.pending_complete_at = Some(now + COMPLETE_GRACE_MS);
            }
        }
    }
}

/// 从指定字节偏移读到 EOF；未写完的 UTF-8 字符留到下轮重试。
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

    struct PollFixture {
        dir: PathBuf,
        wire: PathBuf,
        trackers: HashMap<String, SessionTracker>,
        idb_state: HashMap<String, (bool, String, Vec<String>)>,
        idb_tails: Vec<IdbTails>,
        tx: mpsc::Sender<NormalizedEvent>,
        rx: mpsc::Receiver<NormalizedEvent>,
        round: u64,
        cancel_log: CancelLog,
    }

    impl PollFixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("bark-doubao-poll-{}", uuid::Uuid::new_v4()));
            let wire = dir.join("sessions/1001/agents/main/system/trajectory.jsonl");
            std::fs::create_dir_all(wire.parent().unwrap()).unwrap();
            std::fs::write(&wire, "").unwrap();
            let (tx, rx) = mpsc::channel(64);
            Self { cancel_log: CancelLog::new(dir.join("logs")), dir, wire, trackers: HashMap::new(), idb_state: HashMap::new(), idb_tails: Vec::new(), tx, rx, round: 0 }
        }

        fn append(&self, record: Value) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&self.wire).unwrap();
            writeln!(file, "{record}").unwrap();
        }

        fn idb(&mut self, running: bool) {
            self.idb_state.insert("1001".into(), (running, String::new(), if running { vec!["123".into()] } else { Vec::new() }));
        }

        fn cancel(&self, message: &str) {
            use std::io::Write;
            let root = self.dir.join("logs");
            std::fs::create_dir_all(&root).unwrap();
            let mut file = std::fs::OpenOptions::new().create(true).append(true)
                .open(root.join("saman_2026.1014.0.log")).unwrap();
            file.write_all(crate::doubaowork_log::tests::line("1001", message).as_bytes()).unwrap();
        }

        fn poll(&mut self) -> Vec<EventKind> {
            self.round += 1;
            let mut ctx = ScanCtx {
                cancel_log: &mut self.cancel_log, roots: &[self.dir.join("sessions")], idb_tails: &self.idb_tails, idb_state: &mut self.idb_state,
            };
            poll_round(&AtomicBool::new(false), &self.tx, AgentKind::DoubaoWork,
                &mut ctx, &mut self.trackers, self.round).unwrap();
            let mut events = Vec::new();
            while let Ok(event) = self.rx.try_recv() { events.push(event.kind); }
            events
        }

        fn finish(&mut self) -> Vec<EventKind> {
            let events = self.poll();
            if !events.is_empty() { return events; }
            assert!(self.trackers.values().any(|t| t.pending_complete_at.is_some()));
            for t in self.trackers.values_mut() {
                if t.pending_complete_at.is_some() { t.pending_complete_at = Some(0); }
            }
            self.poll()
        }

        fn make_heartbeat_due(&mut self) {
            for tracker in self.trackers.values_mut() {
                tracker.last_emit_ms = now_millis() - HEARTBEAT_MS - 1;
            }
        }
    }

    impl Drop for PollFixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); }
    }

    #[test]
    fn user_cancel_precedes_idle_and_blocks_late_trajectory_and_heartbeats() {
        let mut f = PollFixture::new();
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.cancel("123");
        assert_eq!(f.poll(), vec![EventKind::RunAborted]);
        f.append(json!({"role":"user", "content":"late prompt"}));
        f.append(json!({"role":"tool", "content":"late tool result"}));
        f.cancel("123");
        f.make_heartbeat_due();
        assert!(f.poll().is_empty(), "重复取消/晚到轨迹/缓存运行状态都不能复活任务");
        f.idb(false);
        assert!(f.poll().is_empty(), "随后的数据库清空不能再报完成");
        f.idb_state.insert("1001".into(), (true, String::new(), vec!["456".into()]));
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.cancel("123");
        assert!(f.poll().is_empty(), "上一轮取消不影响新消息");
        f.cancel("456");
        assert_eq!(f.poll(), vec![EventKind::RunAborted], "连续两轮取消都有效");
    }

    #[test]
    fn idle_before_cancel_waits_for_log_and_new_messages_can_skip_idle_snapshot() {
        let mut f = PollFixture::new();
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.idb(false);
        assert!(f.poll().is_empty(), "空闲先到，暂不报完成");
        f.cancel("123");
        assert_eq!(f.poll(), vec![EventKind::RunAborted]);
        f.idb_state.insert("1001".into(), (true, String::new(), vec!["456".into()]));
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.cancel("456");
        assert_eq!(f.poll(), vec![EventKind::RunAborted]);
        // 快速追问时两次轮询之间可能错过 false；真实消息 ID 的变化仍能开启下一轮。
        f.idb_state.insert("1001".into(), (true, String::new(), vec!["789".into()]));
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.cancel("456");
        assert!(f.poll().is_empty());
        f.idb(false);
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
    }

    #[test]
    fn cancellation_ends_waiting_for_input_without_waiting_for_ask_grace() {
        let mut f = PollFixture::new();
        f.idb(true);
        f.poll();
        f.append(json!({"role":"assistant", "tool_calls":[{"function":{"name":"interaction.ask"}}]}));
        assert_eq!(f.poll(), vec![EventKind::InputRequired]);
        f.idb(false);
        assert!(f.poll().is_empty());
        f.cancel("123");
        assert_eq!(f.poll(), vec![EventKind::RunAborted]);
        assert!(f.trackers.values().all(|t| !t.waiting_ask && t.pending_complete_at.is_none()));
    }

    #[test]
    fn poll_real_storage_format_starts_before_trajectory_and_stays_finished() {
        use crate::doubaowork_idb::fixtures::{table, value, wal};
        let mut f = PollFixture::new();
        std::fs::remove_file(&f.wire).unwrap();
        std::fs::write(f.wire.with_file_name("assignment.md"), "- Role: main\n").unwrap();
        let db = f.dir.join("db");
        std::fs::create_dir_all(&db).unwrap();
        f.idb_tails.push(IdbTails::new(IdbLogTail::new(db.clone())));
        assert!(f.poll().is_empty());

        // 对应实测顺序：19:51:09 数据库开始运行，trajectory 此时还没写入。
        let running = value("1001", true, true, true);
        std::fs::write(db.join("000001.log"), wal(10, &[(b"session", Some(running.clone()))])).unwrap();
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.make_heartbeat_due();
        assert_eq!(f.poll(), vec![EventKind::Activity]);

        // 到完成时才追加 user，数据库完成必须压过这条迟到的“开始”。
        std::fs::write(&f.wire, "").unwrap();
        f.append(json!({"role":"user", "content":"late task prompt"}));
        let done = value("1001", false, true, true);
        std::fs::write(db.join("000002.log"), wal(20, &[(b"session", Some(done.clone()))])).unwrap();
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());

        // compaction 留下包含旧运行状态的表，也不能复活任务。
        std::fs::write(db.join("000003.ldb"), table(&[(10, b"session".to_vec(), Some(running))], true)).unwrap();
        std::fs::write(db.join("000004.ldb"), table(&[(20, b"session".to_vec(), Some(done))], true)).unwrap();
        std::fs::remove_file(db.join("000001.log")).unwrap();
        std::fs::remove_file(db.join("000002.log")).unwrap();
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());

        // 模拟重启监控，完成种子只剩 SST，不能按 user 尾行重放活动。
        f.trackers.clear(); f.idb_state.clear();
        f.idb_tails = vec![IdbTails::new(IdbLogTail::new(db.clone()))];
        assert!(f.poll().is_empty());
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());

        std::fs::write(db.join("000005.log"), wal(30, &[(b"session", Some(value("1001", true, true, true)))])).unwrap();
        assert_eq!(f.poll(), vec![EventKind::Activity], "下一轮真实运行仍可重新点亮");
        std::fs::write(db.join("000006.log"), wal(40, &[(b"session", Some(value("1001", false, true, true)))])).unwrap();
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
    }

    #[test]
    fn poll_completion_wins_over_late_trajectory_and_allows_next_turn() {
        let mut f = PollFixture::new();
        f.append(json!({"role": "user", "content": "old task"}));
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);

        // 同一轮先读到数据库完成，再读到迟落盘的 user，最终必须保持完成。
        f.idb(false);
        f.append(json!({"role": "user", "content": "old task"}));
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.append(json!({"role": "tool", "content": "late result"}));
        assert!(f.poll().is_empty());
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());

        // 新任务的轨迹可能先于数据库到达；等运行信号到达后仍能恢复，不吞下一轮。
        f.append(json!({"role": "user", "content": "next task"}));
        assert!(f.poll().is_empty());
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        assert_eq!(f.trackers.values().next().unwrap().last_prompt, "next task");
        f.idb(false);
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
    }

    #[test]
    fn poll_trajectory_terminal_blocks_cached_running_heartbeat() {
        let mut f = PollFixture::new();
        f.append(json!({"role": "user", "content": "task"}));
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.append(json!({"role": "assistant", "content": "done"}));
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.make_heartbeat_due();
        assert!(f.poll().is_empty(), "缓存的 running=true 不能复活已完成回合");
        f.append(json!({"role": "tool", "content": "late result"}));
        assert!(f.poll().is_empty());
        // 数据库尚未清除旧状态，但用户确实发起下一轮时仍可恢复。
        f.append(json!({"role": "user", "content": "new task"}));
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.append(json!({"role": "assistant", "content": "new task done"}));
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.idb(false);
        assert!(f.poll().is_empty(), "两个来源的完成信号只收场一次");
    }

    #[test]
    fn poll_idle_seed_blocks_late_records_but_ask_answer_still_resumes() {
        let mut f = PollFixture::new();
        f.append(json!({"role": "user", "content": "old"}));
        f.idb(false);
        assert!(f.poll().is_empty());
        f.append(json!({"role": "tool", "content": "old result"}));
        assert!(f.poll().is_empty());
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.append(json!({"role": "assistant", "tool_calls": [
            {"function": {"name": "interaction.ask", "arguments": {"question": "choose"}}}
        ]}));
        assert_eq!(f.poll(), vec![EventKind::InputRequired]);
        f.idb(false);
        f.append(json!({"role": "tool", "content": "answer"}));
        assert_eq!(f.poll(), vec![EventKind::ToolFinished]);
        assert!(f.trackers.values().next().unwrap().pending_complete_at.is_none());
        assert!(!f.trackers.values().next().unwrap().waiting_ask);
        f.append(json!({"role": "assistant", "tool_calls": [
            {"function": {"name": "shell.run", "arguments": {}}}
        ]}));
        assert_eq!(f.poll(), vec![EventKind::Activity], "回答后的工具执行仍属于当前回合");
    }

    #[test]
    fn poll_bootstrap_terminal_does_not_heartbeat_and_log_only_turns_reopen() {
        for seeded in [true, false] {
            let mut f = PollFixture::new();
            f.append(json!({"role": "user", "content": "old task"}));
            f.append(json!({"role": "assistant", "content": "done"}));
            if seeded { f.idb(true); }
            assert!(f.poll().is_empty());
            f.make_heartbeat_due();
            assert!(f.poll().is_empty());
            f.append(json!({"role": "user", "content": "next task"}));
            assert_eq!(f.poll(), vec![EventKind::Activity]);
            f.append(json!({"role": "assistant", "content": "done"}));
            assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
            f.make_heartbeat_due();
            assert!(f.poll().is_empty());
        }
    }

    #[test]
    fn poll_cloud_task_with_empty_trajectory_can_run_and_finish() {
        let mut f = PollFixture::new();
        f.idb(true);
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.make_heartbeat_due();
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.idb(false);
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());
    }

    #[test]
    fn poll_first_idle_observation_closes_log_only_activity() {
        let mut f = PollFixture::new();
        f.append(json!({"role": "user", "content": "task"}));
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        f.idb(false);
        f.append(json!({"role": "user", "content": "late task record"}));
        assert_eq!(f.finish(), vec![EventKind::RunCompleted]);
        f.make_heartbeat_due();
        assert!(f.poll().is_empty());
    }

    #[test]
    fn poll_bootstrap_during_partial_utf8_record_recovers() {
        use std::io::Write;
        let mut f = PollFixture::new();
        let record = format!("{}\n", json!({"role": "user", "content": "中文任务"}));
        let split = record.find('中').unwrap() + 1;
        std::fs::write(&f.wire, &record.as_bytes()[..split]).unwrap();
        assert!(f.poll().is_empty());
        std::fs::OpenOptions::new().append(true).open(&f.wire).unwrap()
            .write_all(&record.as_bytes()[split..]).unwrap();
        assert_eq!(f.poll(), vec![EventKind::Activity]);
        assert!(f.poll().is_empty());
    }

    /// 真实样本的映射钉死（记录形状取自本机 trajectory.jsonl，2026-10-10）
    #[test]
    fn classify_maps_real_records() {
        let user = json!({ "role": "user", "content": "看一下design.md，把图片替换成严肃一点的" });
        let (ev, tool, msg) = classify(&user).expect("user 行必须映射");
        assert_eq!(ev, EventKind::Activity);
        assert_eq!(tool, None);
        assert_eq!(msg, "看一下design.md，把图片替换成严肃一点的");

        let ask = json!({
            "role": "assistant",
            "content": "先给你一份现状盘点和替换建议。",
            "tool_calls": [{
                "id": "e3b46acd",
                "type": "function",
                "function": {
                    "name": "interaction.ask",
                    "arguments": { "questions": [
                        { "display_message": "新 Logo 的图形图标你倾向哪个方向？", "display_option": [] }
                    ]}
                }
            }]
        });
        let (ev, tool, msg) = classify(&ask).expect("interaction.ask 必须映射");
        assert_eq!(ev, EventKind::InputRequired, "向用户提问 = 等待输入");
        assert_eq!(tool, None);
        assert_eq!(msg, "新 Logo 的图形图标你倾向哪个方向？", "正文带问题文案");

        let gen = json!({
            "role": "assistant",
            "content": "直接用生图工具出三张概念图",
            "tool_calls": [{
                "id": "5acae77d",
                "type": "function",
                "function": { "name": "image_gen", "arguments": { "request_list": [] } }
            }]
        });
        let (ev, tool, _) = classify(&gen).expect("普通工具行必须映射");
        assert_eq!(ev, EventKind::Activity);
        assert_eq!(tool.as_deref(), Some("image_gen"), "工具名驱动「执行工具」相位");

        let result = json!({ "role": "tool", "tool_call_id": "5acae77d", "content": "image_gen 执行失败" });
        let (ev, tool, _) = classify(&result).expect("tool 行必须映射");
        assert_eq!(ev, EventKind::ToolFinished, "工具收尾（含提问被回答）");
        assert_eq!(tool, None);

        let final_msg = json!({ "role": "assistant", "content": "全套可替换文件已生成完毕。" });
        let (ev, tool, _) = classify(&final_msg).expect("最终回答必须映射");
        assert_eq!(ev, EventKind::RunCompleted, "无 tool_calls 的 assistant 行是回合完成");
        assert_eq!(tool, None);
    }

    /// 噪声与畸形行一律忽略 / 不误报完成
    #[test]
    fn classify_ignores_noise_and_malformed() {
        assert!(classify(&json!({ "role": "system", "content": "You are..." })).is_none());
        assert!(classify(&json!({ "role": "assistant" })).is_none());
        assert!(classify(&json!({ "role": "assistant", "content": "  " })).is_none());
        let (ev, _, _) = classify(&json!({ "role": "assistant", "content": "done", "tool_calls": [] }))
            .expect("空 tool_calls + 非空 content 是最终回答");
        assert_eq!(ev, EventKind::RunCompleted);
        let parts = json!({ "role": "user", "content": [
            { "type": "text", "text": "改一下" },
            { "type": "image_url", "image_url": { "url": "https://x" } }
        ]});
        let (_, _, msg) = classify(&parts).expect("分片数组要能映射");
        assert_eq!(msg, "改一下");
    }

    /// 长文案截断（与其它 adapter 的 200 字口径一致；truncate_chars 截后补 …）
    #[test]
    fn classify_truncates_long_prompts() {
        let long = "啊".repeat(500);
        let (_, _, msg) = classify(&json!({ "role": "user", "content": long })).unwrap();
        assert_eq!(msg.chars().count(), 201, "200 字 + 省略号");
        assert!(msg.ends_with('…'));
    }

    /// 完成跃迁（主信号）：IndexedDB 条目 true→false → RunCompleted（正文=任务文案，
    /// cwd=workspace）；启动播种 None→false 不产事件；普通聊天（无 trajectory）不产事件
    #[test]
    fn idb_clear_transition_emits_run_completed() {
        let (tx, mut rx) = mpsc::channel(8);
        let stop = AtomicBool::new(false);
        let mut trackers: HashMap<String, SessionTracker> = HashMap::new();
        trackers.insert(
            "w1".into(),
            SessionTracker {
                aborted: false,
                sid: "1001".into(),
                message_ids: Vec::new(),
                offset: 0,
                settled: false,
                running: true,
                waiting_ask: false,
                idb_running: None,
                pending_complete_at: None,
                last_emit_ms: 0,
                last_growth_ms: 0,
                last_prompt: "改 Logo 的任务".into(),
                workspace: String::new(),
                pending: String::new(),
                last_seen_round: 1,
            },
        );
        // 播种：None→true（回合在跑）不产事件；顺带回填 workspace
        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        idb_state.insert("1001".into(), (true, r"D:\proj".into(), Vec::new()));
        apply_idb_state(&stop, &tx, AgentKind::DoubaoWork, &mut trackers, &idb_state, &CancelLog::default(), 1000);
        assert!(rx.try_recv().is_err());
        assert_eq!(trackers.get("w1").unwrap().workspace, r"D:\proj");

        // true→false：完成（正文=最近任务文案，cwd=workspace）
        idb_state.insert("1001".into(), (false, r"D:\proj".into(), Vec::new()));
        apply_idb_state(&stop, &tx, AgentKind::DoubaoWork, &mut trackers, &idb_state, &CancelLog::default(), 2000);
        assert!(rx.try_recv().is_err(), "等待取消日志补齐后再报完成");
        assert_eq!(trackers["w1"].pending_complete_at, Some(2000 + COMPLETE_GRACE_MS));
        assert!(!trackers.get("w1").unwrap().running);
        // 重复 false：不再发
        idb_state.insert("1001".into(), (false, String::new(), Vec::new()));
        apply_idb_state(&stop, &tx, AgentKind::DoubaoWork, &mut trackers, &idb_state, &CancelLog::default(), 3000);
        assert!(rx.try_recv().is_err());
    }

    /// 提问挂起时条目被清除 → 延迟判定：回答（tool 行）撤销，宽限超时才完成
    #[test]
    fn ask_pending_clear_is_deferred_and_cancellable() {
        let (tx, mut rx) = mpsc::channel(8);
        let stop = AtomicBool::new(false);
        let mut trackers: HashMap<String, SessionTracker> = HashMap::new();
        trackers.insert(
            "w1".into(),
            SessionTracker {
                aborted: false,
                sid: "1001".into(),
                message_ids: Vec::new(),
                offset: 0,
                settled: false,
                running: false,
                waiting_ask: true,
                idb_running: Some(true),
                pending_complete_at: None,
                last_emit_ms: 0,
                last_growth_ms: 0,
                last_prompt: "做海报".into(),
                workspace: String::new(),
                pending: String::new(),
                last_seen_round: 1,
            },
        );
        // 提问挂起时清除 → 不立即完成，转延迟
        let idb_state = HashMap::from([("1001".to_string(), (false, String::new(), Vec::new()))]);
        apply_idb_state(&stop, &tx, AgentKind::DoubaoWork, &mut trackers, &idb_state, &CancelLog::default(), 1000);
        assert!(rx.try_recv().is_err(), "宽限期内不发完成");
        assert_eq!(
            trackers.get("w1").unwrap().pending_complete_at,
            Some(1000 + ASK_CLEAR_GRACE_MS)
        );

        // 宽限到期（poll_round 的到期分支以纯时间判定，这里直接模拟该分支的行为）
        let tracker = trackers.get_mut("w1").unwrap();
        let deadline = tracker.pending_complete_at.take().unwrap();
        assert!(deadline <= 1000 + ASK_CLEAR_GRACE_MS);
        // —— 到期后仍未回答 → 完成事件由 poll_round 发出（见 lost/clear 集成测试）
    }

    /// 无 IndexedDB 种子时按 trajectory 文件停摆判定：老会话（文件久无动静，
    /// 尾行停在 user 消息上）首见不点亮——否则数据库不可读时，每次重启都会把
    /// 历史会话重新点亮一遍；新鲜文件照常点亮（现有 grace 测试即此路径）。
    #[test]
    fn poll_round_suppresses_stale_idle_session_without_seed() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-stale-{}", uuid::Uuid::new_v4().simple()));
        let sessions = dir
            .join("User Data")
            .join("Default")
            .join(".doubaowork")
            .join("agent_mode")
            .join("workspace")
            .join(".sessions");
        let sys = sessions.join("2001").join("agents").join("m_main").join("system");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("assignment.md"), "# Assignment\n- Role: main\n").unwrap();
        let wire = sys.join("trajectory.jsonl");
        std::fs::write(&wire, format!("{}\n", json!({ "role": "user", "content": "早已完成的任务" }))).unwrap();
        // 把 trajectory 的 mtime 拨到 16 分钟前（超过 BOOTSTRAP_IDLE_AFTER_MS）
        {
            let f = std::fs::File::options().write(true).open(&wire).unwrap();
            f.set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(960)),
            )
            .unwrap();
        }

        let roots = vec![sessions.clone()];
        let (tx, mut rx) = mpsc::channel(8);
        let stop = Arc::new(AtomicBool::new(false));
        let idb: Vec<IdbTails> = Vec::new();
        let mut trackers: HashMap<String, SessionTracker> = HashMap::new();

        let mut idb_state = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 1).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "停摆老会话首见不得点亮（否则重启后历史会话全回来）"
        );
        let tracker = trackers.values().next().unwrap();
        assert!(!tracker.running, "老会话按空闲建档");
        assert_eq!(tracker.idb_running, None, "停摆判定不写 idb_running，保持降级口径");

        // 老会话来了新消息（文件追加）→ 照常点亮，不因先前的停摆判定卡死
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap()
            .write_all(format!("{}\n", json!({ "role": "user", "content": "新任务" })).as_bytes())
            .unwrap();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 2).unwrap();
        let ev = rx.try_recv().expect("新消息必须点亮");
        assert_eq!(ev.kind, EventKind::Activity);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「提问挂起 + 条目清除」宽限到期的完成路径（poll_round 级集成）
    #[test]
    fn poll_round_emits_completion_after_ask_clear_grace() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-grace-{}", uuid::Uuid::new_v4().simple()));
        let sessions = dir
            .join("User Data")
            .join("Default")
            .join(".doubaowork")
            .join("agent_mode")
            .join("workspace")
            .join(".sessions");
        std::fs::create_dir_all(sessions.join("1001").join("agents").join("m_main").join("system")).unwrap();
        std::fs::write(
            sessions.join("1001").join("agents").join("m_main").join("system").join("assignment.md"),
            "# Assignment\n- Role: main\n",
        )
        .unwrap();
        std::fs::write(
            sessions.join("1001").join("agents").join("m_main").join("system").join("trajectory.jsonl"),
            format!("{}\n", json!({ "role": "user", "content": "做海报" })),
        )
        .unwrap();
        let roots = vec![sessions.clone()];
        let (tx, mut rx) = mpsc::channel(8);
        let stop = Arc::new(AtomicBool::new(false));
        let idb: Vec<std::sync::Mutex<IdbLogTail>> = Vec::new();
        let mut trackers: HashMap<String, SessionTracker> = HashMap::new();

        // 首见：user 行点亮
        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 1).unwrap();
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.kind, EventKind::Activity);

        // 提问挂起 + 条目清除 → 转延迟
        {
            let tracker = trackers.values_mut().next().unwrap();
            tracker.waiting_ask = true;
            tracker.idb_running = Some(true);
        }
        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        idb_state.insert("1001".into(), (false, String::new(), Vec::new()));
        apply_idb_state(&stop, &tx, AgentKind::DoubaoWork, &mut trackers, &idb_state, &CancelLog::default(), now_millis());
        assert!(rx.try_recv().is_err());

        // 把宽限期拨到已过期，再跑一轮 → 完成事件
        {
            let tracker = trackers.values_mut().next().unwrap();
            tracker.pending_complete_at = Some(now_millis() - 1);
        }
        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 2).unwrap();
        let ev = rx.try_recv().expect("宽限到期必须发完成");
        assert_eq!(ev.kind, EventKind::RunCompleted);
        assert_eq!(ev.session_id, "1001");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 主 agent 过滤：Role: main 收，其余跳过；读不到按主处理
    #[test]
    fn is_main_agent_parses_role_line() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-{}", uuid::Uuid::new_v4().simple()));
        let sys = dir.join("system");
        std::fs::create_dir_all(&sys).unwrap();

        assert!(is_main_agent(&dir), "无 assignment.md 按主智能体处理");

        std::fs::write(
            sys.join("assignment.md"),
            "# Assignment\n- Agent ID: m_0cwR7yGmIWS\n- Role: main\n- Assigned by: user\n\n## [t] 需求\nx\n",
        )
        .unwrap();
        assert!(is_main_agent(&dir), "Role: main 是主智能体");

        std::fs::write(sys.join("assignment.md"), "# Assignment\n- Agent ID: w_1\n- Role: worker\n").unwrap();
        assert!(!is_main_agent(&dir), "子智能体要被跳过");

        std::fs::write(sys.join("assignment.md"), "# Assignment\n- Agent ID: w_2\n").unwrap();
        assert!(is_main_agent(&dir), "没写 Role 行的按主智能体处理");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 启动回看：尾部停在工具运行后（回合在跑）→ 发一条 Activity 心跳；
    /// 停在提问后 → 也点亮但不置 running（等待态不心跳，避免冲掉等待色）；
    /// 尾部是最终回答（已结束）→ 不发历史终态（避免启动风暴）
    #[test]
    fn bootstrap_tail_only_reports_active_sessions() {
        let running = format!(
            "{}\n{}\n{}\n",
            json!({ "role": "user", "content": "长任务" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "function": { "name": "shell.run", "arguments": {} } }
            ]}),
            json!({ "role": "tool", "content": "ok" })
        );
        let (events, running_flag, waiting, _) = bootstrap_tail(&running);
        assert!(running_flag, "尾部无终态信号 → 回合在跑");
        assert!(!waiting);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, EventKind::Activity);
        assert_eq!(events[0].2, "长任务", "心跳正文带任务文案");

        let ask_pending = format!(
            "{}\n{}\n",
            json!({ "role": "user", "content": "帮我做海报" }),
            json!({ "role": "assistant", "content": "", "tool_calls": [
                { "function": { "name": "interaction.ask", "arguments": { "questions": [] } } }
            ]})
        );
        let (events, running_flag, waiting, _) = bootstrap_tail(&ask_pending);
        assert!(!running_flag, "提问后回合挂起等回答");
        assert!(waiting);
        assert_eq!(events.len(), 1, "等待态也要点亮一次（蓝色，不重放等待通知）");
        assert_eq!(events[0].2, "帮我做海报");

        let finished = format!(
            "{}\n{}\n",
            json!({ "role": "user", "content": "已完成的事" }),
            json!({ "role": "assistant", "content": "做完了" })
        );
        let (events, running_flag, waiting, completed) = bootstrap_tail(&finished);
        assert!(completed);
        assert!(!running_flag && !waiting);
        assert!(events.is_empty(), "历史完成不得重放");
    }

    /// 跨轮残行：无换行结尾的半行要等下一个 chunk 拼完才解析
    #[test]
    fn split_complete_lines_buffers_partial_tail() {
        let mut pending = String::new();
        let (lines, consumed) = split_complete_lines("{\"role\":\"use", &mut pending);
        assert!(lines.is_empty());
        assert_eq!(consumed, pending.len(), "残行已读入缓冲区，文件偏移也必须推进");

        let (lines, consumed) = split_complete_lines(
            "r\",\"content\":\"hi\"}\n{\"role\":\"tool\"}\n{partial",
            &mut pending,
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(consumed, "r\",\"content\":\"hi\"}\n{\"role\":\"tool\"}\n{partial".len());
        assert_eq!(pending, "{partial", "尾部残行暂存");
    }

    /// profile 枚举：只收存在 .doubaowork 会话目录的 profile
    #[test]
    fn sessions_roots_enumerate_profiles() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-root-{}", uuid::Uuid::new_v4().simple()));
        let mk = |profile: &str| {
            let p = dir.join("User Data").join(profile);
            std::fs::create_dir_all(p.join(".doubaowork").join("agent_mode").join("workspace").join(".sessions"))
                .unwrap();
            p.join(".doubaowork").join("agent_mode").join("workspace").join(".sessions")
        };
        let default = mk("Default");
        mk("Profile 2");
        std::fs::create_dir_all(dir.join("User Data").join("Empty")).unwrap();
        let roots = sessions_roots_under(&dir.join("User Data"));
        assert_eq!(roots.len(), 2, "Empty profile 不该收录");
        assert!(roots.contains(&default));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// IndexedDB 库目录枚举：*.indexeddb.leveldb 全收
    #[test]
    fn indexeddb_dirs_enumerate_all_databases() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-idbdir-{}", uuid::Uuid::new_v4().simple()));
        let ud = dir.join("User Data").join("Default");
        for name in [
            "IndexedDB/chrome_doubaowork-chat_0.indexeddb.leveldb",
            "IndexedDB/chrome_doubaowork-launcher_0.indexeddb.leveldb",
            "IndexedDB/chrome_doubaowork-chat_0.indexeddb.blob",
            "IndexedDB/NotALevelDb",
        ] {
            std::fs::create_dir_all(ud.join(name)).unwrap();
        }
        let dirs = indexeddb_dirs_under(&dir.join("User Data"));
        assert_eq!(dirs.len(), 2, "blob 目录与非 leveldb 目录不收");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「运行中会话的 trajectory 被删除」→ 主动 RunAborted 收档（不亮失败色）；
    /// 已终态/空闲会话的文件被删则静默移除。与 WorkBuddy / TraeWork 同口径（§1.12a）。
    #[test]
    fn poll_round_closes_lost_running_session_as_aborted() {
        let dir = std::env::temp_dir().join(format!("ab-dbw-lost-{}", uuid::Uuid::new_v4().simple()));
        let sys = dir
            .join("User Data")
            .join("Default")
            .join(".doubaowork")
            .join("agent_mode")
            .join("workspace")
            .join(".sessions")
            .join("1001")
            .join("agents")
            .join("m_main")
            .join("system");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(sys.join("assignment.md"), "# Assignment\n- Role: main\n").unwrap();
        let wire = sys.join("trajectory.jsonl");
        std::fs::write(
            &wire,
            format!(
                "{}\n{}\n",
                json!({ "role": "user", "content": "会被删掉的任务" }),
                json!({ "role": "assistant", "content": "", "tool_calls": [
                    { "function": { "name": "shell.run", "arguments": {} } }
                ]})
            ),
        )
        .unwrap();

        let roots = vec![dir.join("User Data").join("Default").join(".doubaowork").join("agent_mode").join("workspace").join(".sessions")];
        let (tx, mut rx) = mpsc::channel(8);
        let stop = Arc::new(AtomicBool::new(false));
        let idb: Vec<std::sync::Mutex<IdbLogTail>> = Vec::new();
        let mut trackers: HashMap<String, SessionTracker> = HashMap::new();

        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 1).unwrap();
        assert_eq!(trackers.len(), 1);
        assert!(trackers.values().next().unwrap().running, "尾部停在工具运行 → 运行中");
        let ev = rx.try_recv().expect("首见运行中会话应发引导心跳");
        assert_eq!(ev.kind, EventKind::Activity);

        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 2).unwrap();
        assert!(rx.try_recv().is_err());

        std::fs::remove_file(&wire).unwrap();
        let mut idb_state: HashMap<String, (bool, String, Vec<String>)> = HashMap::new();
        let mut ctx = ScanCtx { cancel_log: &mut CancelLog::default(), roots: &roots, idb_tails: &idb, idb_state: &mut idb_state };
        poll_round(&stop, &tx, AgentKind::DoubaoWork, &mut ctx, &mut trackers, 3).unwrap();
        assert!(trackers.is_empty(), "消失的会话不应继续占用跟踪表");
        let ev = rx.try_recv().expect("运行中会话被删必须闭环");
        assert_eq!(ev.kind, EventKind::RunAborted, "删除按中止收场（不通知不亮失败色）");
        assert!(!ev.kind.should_notify(), "不该弹通知");
        assert_eq!(ev.session_id, "1001");
        assert!(ev.message.contains("豆包工作"), "正文要说明来源: {}", ev.message);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 增量消费端到端（纯函数拼装）：user 行点亮并记住文案，
    /// 随后的增量按序产出事件、终态正文回填任务文案
    #[test]
    fn steady_state_event_order() {
        let (events, running, _, _) = bootstrap_tail(&format!(
            "{}\n{}\n",
            json!({ "role": "user", "content": "改 Logo" }),
            json!({ "role": "tool", "content": "ok" })
        ));
        assert!(running && events.len() == 1);
        let mut last_prompt = "改 Logo".to_string();

        let batch = format!(
            "{}\n{}\n{}\n",
            json!({ "role": "user", "content": "那不要 logo 了，只保留文字" }),
            json!({ "role": "tool", "content": "ok" }),
            json!({ "role": "assistant", "content": "已生成纯文字版。" })
        );
        let mut evs: Vec<(EventKind, String)> = Vec::new();
        for line in batch.lines() {
            let r: Value = serde_json::from_str(line).unwrap();
            if let Some((ev, _, msg)) = classify(&r) {
                let role = r.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if role == "user" && !msg.is_empty() {
                    last_prompt = msg.clone();
                }
                let msg = if ev == EventKind::RunCompleted { last_prompt.clone() } else { msg };
                evs.push((ev, msg));
            }
        }
        assert_eq!(
            evs,
            vec![
                (EventKind::Activity, "那不要 logo 了，只保留文字".to_string()),
                (EventKind::ToolFinished, String::new()),
                (EventKind::RunCompleted, "那不要 logo 了，只保留文字".to_string()),
            ],
            "完成通知正文 = 最近一次任务文案"
        );
    }

    /// 端到端冒烟：真启动 watcher 跑几秒，打印产出的事件（验证引导心跳 / 增量消费 /
    /// IndexedDB 扫描 / 心跳节流的实际行为）。默认跳过，本机装了豆包工作时手动跑：
    /// `cargo test -p bark-adapters --lib doubao_work_live_events -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn doubao_work_live_events() {
        use std::sync::atomic::Ordering;
        let watch = DoubaoWorkWatch;
        eprintln!(
            "installed={} available={} reason={:?}",
            watch.is_installed(),
            watch.is_available(),
            watch.unavailable_reason()
        );
        let (tx, mut rx) = mpsc::channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = watch.spawn(stop.clone(), tx).expect("watcher 应能启动");
        let mut count = 0;
        for sec in 0..10 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            while let Ok(ev) = rx.try_recv() {
                count += 1;
                eprintln!("[{:>2}s] {:?} session={}", sec + 1, ev.kind, ev.session_id);
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        eprintln!("10 秒只读监控共收到 {count} 个事件");
    }

    /// 端到端冒烟：读**真实**豆包工作会话目录并解析（本机装了豆包工作时手动跑：
    /// `cargo test -p bark-adapters --lib doubao_work_live_smoke -- --ignored --nocapture`）
    #[test]
    #[ignore]
    fn doubao_work_live_smoke() {
        let roots = sessions_roots();
        assert!(!roots.is_empty(), "本机没有豆包工作会话目录");
        let mut found = 0;
        for root in &roots {
            for session in std::fs::read_dir(root).unwrap().flatten() {
                let agents = session.path().join("agents");
                for agent in std::fs::read_dir(&agents).unwrap().flatten() {
                    let wire = agent.path().join("system").join("trajectory.jsonl");
                    if !wire.is_file() || !is_main_agent(&agent.path()) {
                        continue;
                    }
                    found += 1;
                    let full = std::fs::read_to_string(&wire).unwrap();
                    let mut classified = 0;
                    for line in full.lines() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let r: Value = serde_json::from_str(line).expect("每行都是合法 JSON");
                        if classify(&r).is_some() {
                            classified += 1;
                        }
                    }
                    eprintln!("{}: {classified} 条可归类消息", wire.display());
                }
            }
        }
        assert!(found > 0, "应能找到至少一个主 agent 的 trajectory");

        // IndexedDB 实测：合并 WAL 与 SST，应能配出会话状态（运行中或已结束）
        let mut updates = Vec::new();
        for dir in indexeddb_dirs_under(&user_data_root().unwrap()) {
            updates.extend(IdbLogTail::new(dir).scan());
        }
        assert!(!updates.is_empty(), "实际数据库必须能解出状态");
        for update in updates { eprintln!("会话 {}: running={}", update.conv, update.running); }
    }
}
