//! ZCode 回合终态看门狗（**非官方机制**：中断 + 致命失败）。
//!
//! 背景（2026-09-10 实测，ZCode 3.11.2.6792 / Windows）：ZCode 手动终止回合时
//! **一个 hook 都不发**——
//! - 思考中中断：连 hook 都不尝试；
//! - 工具执行中中断：它**试图**调 `PostToolUseFailure`，但 1ms 内就把 hook 进程
//!   取消了（日志里留下 `hook.run.failed`），我们什么都收不到。
//!
//! 于是「按了停止但流光一直停在思考色」只能等下一条事件或判死超时（默认 10 分钟）。
//! 唯一能拿到秒级信号的痕迹在 ZCode 自己的日志里：中断后它会写两条记录
//! （相隔约 25ms，见 `bark_adapters::zcode::LogSignal` 的文档）。
//!
//! 2026-09-19 补充了第二类终态：**回合致命失败**（实测 Captcha 超时、订阅权限 1311）。
//! 模型请求不可重试地失败时 ZCode 写一条 `turn.failed`，但**进程不退出、不发任何
//! hook、会话停在输入框等用户手动继续**——会话相位永远卡在「思考中」，同样只能等
//! 判死超时。它与中断在日志里可机械区分（`status: "failed"` vs `"cancelled"`，
//! 详见 `parse_log_signal`），按 `RunFailed` 合成：失败色全屏特效（多会话并行时
//! 也照放一次，见 `glow::burst_only` 的双通道）+「任务失败」通知 + failed 音效。
//!
//! 这里每秒增量读一次那份日志（按字节偏移，不重读历史；首次启动从末尾开始），
//! 发现信号就**合成对应的终态事件**塞进事件管道。
//!
//! 顺带的第二职责是**存活心跳**：任何带会话 id 的日志行都说明该会话还活着
//! （子代理行经 `parentSessionId` 向父会话传播），据此刷新状态表的
//! `last_activity`（[`state::touch_session`]）——长工具 / 长子代理 / 长思考期间
//! hook 心跳静默是常态（实测 `Agent` 工具单次可跑 27 分钟），没有这层正向信号，
//! 「心跳静止超时判死」会把还在跑的回合误判成意外终止（红光 + 失败音）。
//!
//! 第三职责是**子代理显示**（2026-10-21 实测：新代 ZCode 子代理**不触发任何
//! hook**，hook 通道对子代理全盲）：从子代理日志行（turn.started /
//! tool.call.started / turn.completed，都带 `parentSessionId` 血缘）合成
//! Activity / RunCompleted 事件塞进管道——建档、挂树（前端 ↳ + 「子代理」标记）、
//! 向父会话传播存活，全部复用既有机制（与 DSH 插件带血缘的事件同一套状态机）。
//! 合成事件带 `is_subagent`：通知、音效、流光特效三道门都拦得住，纯显示不扰人。
//!
//! 四道保险，避免误报与刷屏：
//! 1. **终态只对「我们自己认为还在跑」的会话生效**：正常回合的 `Stop` 已经把
//!    会话清掉了，所以日志里那些记录不会再触发一次通知；启动时读不到历史
//!    （会话表是空的）。子代理活动合成不受此门约束——子代理没有 hook 通道，
//!    这条日志通道就是它们的「第一条心跳」，门住它就永远建档不了。
//! 2. **按信号去重 30 秒**：中断的三条痕迹相隔毫秒级，按会话去重合并成一次；
//!    失败按会话+回合去重（不同回合的连续失败不得互相吞），回合 id 缺失时
//!    干脆不去重——无标识时宁可重复也不静默吞信号。子代理活动不另设去重：
//!    tail 是增量读取，同一行日志不会被读两次。
//! 3. **子代理终态照常合成**（RunFailed / RunAborted / RunCompleted）：
//!    通知与音效有 is_subagent 门，流光特效的同款门也已补齐（`burst_for_event`
//!    与 `edge_kind`，见 glow 模块）——早年「子代理失败不合成」的拒绝是因为
//!    特效通道还没有门，如今合成只是为了把子代理的会话条目收掉（否则失败的
//!    子代理行要挂满 10 分钟等判死巡检清场）。
//! 4. **合成事件带 is_subagent**：与 normalize 同一份判定，防御式对齐——
//!    标记不齐会让上游过滤各走各的口径。
//!
//! 代价与边界：这是**非官方**做法，依赖 ZCode 的日志格式（应用升级可能失效）；
//! 失效时的表现是「退回到判死超时」，不会误报。接入开关关闭时整个看门狗不工作。

use crate::state::{lock, read, AppState};
use bark_adapters::zcode::{AbortLogTail, LogSignal, SubagentEvent};
use bark_core::{EventKind, NormalizedEvent};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// 轮询间隔：终态信号要求「秒级」可见，1 秒足够，成本只是读一个文件增量
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// 同一信号的去重窗口
const DEDUP_WINDOW: Duration = Duration::from_secs(30);

/// 启动看门狗（daemon 生命周期内常驻；未开启 ZCode 接入时不干活）
pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    tauri::async_runtime::spawn(async move {
        let dir = bark_adapters::zcode::log_dir_default();
        let mut tail = AbortLogTail::default();
        // 去重键 → 上次上报时刻（中断按会话、失败按会话+回合，见 LogSignal::dedup_key；
        // 键为 None 的信号不去重、直接放行）
        let mut reported: HashMap<String, Instant> = HashMap::new();

        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // 接入关闭 → 不读日志、不上报（与管道门保持一致）
            if !read(&state.config).agent_enabled("zcode") {
                continue;
            }
            let scan = tail.poll(&dir);
            // 存活心跳先行：日志活动 = 会话还活着（子代理行向父会话传播，见
            // `LogLine::parent_session_id`），刷新 last_activity 防长静默被
            // 判死巡检误判成意外终止。只刷已有条目、不建档（陈年会话不留痕）。
            for sid in &scan.alive_sessions {
                crate::state::touch_session(&state, "zcode", sid);
            }
            // 子代理显示（第三职责）：合成子代理生命周期事件塞进管道。放在终态
            // 信号之前——同一批里「turn.started 后紧跟 turn.failed」时，先建档
            // 才能让失败信号过下面的会话门、把刚建的条目收掉。
            for act in &scan.subagent_events {
                let (kind, tool_name) = match act {
                    SubagentEvent::TurnStarted { .. } => (EventKind::Activity, None),
                    SubagentEvent::ToolCall { tool_name, .. } => {
                        (EventKind::Activity, Some(tool_name.as_str()))
                    }
                    SubagentEvent::TurnCompleted { .. } => (EventKind::RunCompleted, None),
                };
                // cwd 取父会话表里的值：子代理与父同工作区、项目名一致；表里没有
                // （daemon 晚启动、父会话还没建档）就空着——挂树靠 parent id，不靠 cwd
                let parent_key = crate::state::session_key("zcode", act.parent_session_id());
                let cwd = lock(&state.active_sessions)
                    .get(&parent_key)
                    .map(|s| s.cwd.clone())
                    .unwrap_or_default();
                let mut raw = json!({
                    "session_id": act.session_id(),
                    "cwd": cwd,
                });
                if let Some(t) = tool_name {
                    raw["tool_name"] = json!(t);
                }
                let mut ev = NormalizedEvent::from_raw("zcode", kind, &raw);
                ev.is_subagent = true;
                ev.parent_session_id = Some(act.parent_session_id().to_string());
                tracing::debug!(
                    session = %ev.session_id,
                    kind = ?ev.kind,
                    "ZCode 看门狗：由本地日志合成子代理活动"
                );
                // 投递带超时（与终态信号同口径）：管道满时丢弃本条并留痕——
                // 子代理显示是 best-effort，下一条活动行会补上状态
                match tokio::time::timeout(Duration::from_secs(1), tx.send(ev)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return, // 管道已关闭（应用退出）
                    Err(_) => {
                        tracing::warn!("ZCode 子代理活动投递超时（管道背压），丢弃本条");
                    }
                }
            }
            for sig in scan.signals {
                // 只处理「我们正在跟」的会话：既挡掉启动前的历史记录，
                // 也挡掉正常回合（Stop 已清表）之后的残留日志。子代理会话的
                // 条目由上面的活动合成通道建档，同样受这扇门保护。
                // **会话检查先于去重登记**（§2.18）：被跳过的信号不该占掉 30s 去重槽，
                // 否则「先到一条我们没在跟的会话的日志、30s 内该会话才建立」时，
                // 真信号会被自己人吞掉
                let session_id = sig.session_id().to_string();
                let key = crate::state::session_key("zcode", &session_id);
                let Some(session) = lock(&state.active_sessions).get(&key).cloned() else {
                    continue;
                };
                if let Some(dedup_key) = sig.dedup_key() {
                    if reported
                        .get(&dedup_key)
                        .is_some_and(|t| t.elapsed() < DEDUP_WINDOW)
                    {
                        continue;
                    }
                    reported.insert(dedup_key, Instant::now());
                }

                let (kind, message) = match &sig {
                    LogSignal::Aborted { .. } => (EventKind::RunAborted, "回合被中断".to_string()),
                    LogSignal::Failed { message, .. } => (EventKind::RunFailed, message.clone()),
                };
                // 合成事件：cwd 取会话表里的值 → project 与通知正文里的项目名保持一致
                let raw = json!({
                    "session_id": session_id,
                    "cwd": session.cwd,
                    "message": message,
                });
                let mut ev = NormalizedEvent::from_raw("zcode", kind, &raw);
                // 日志行没有 hook 载荷的布尔字段，子代理判定在解析时按会话名完成
                ev.is_subagent = sig.is_subagent();
                tracing::info!(
                    session = %ev.session_id,
                    kind = ?ev.kind,
                    "ZCode 回合终态看门狗：由本地日志判定，合成 {:?}",
                    ev.kind
                );
                // 投递带超时（§2.18）：管道满（事件管道短暂背压）时不能让看门狗
                // 停摆——1s 投不出去就丢弃本条并留痕（合成信号是 best-effort，
                // 兜底是心跳判死巡检）。管道关闭（应用退出）才结束循环。
                match tokio::time::timeout(Duration::from_secs(1), tx.send(ev)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return, // 管道已关闭（应用退出）
                    Err(_) => {
                        tracing::warn!("ZCode 终态信号投递超时（管道背压），丢弃本条，判死巡检兜底");
                    }
                }
            }
            // 去重表不会无限增长：清掉过期的
            reported.retain(|_, t| t.elapsed() < DEDUP_WINDOW);
        }
    });
}
