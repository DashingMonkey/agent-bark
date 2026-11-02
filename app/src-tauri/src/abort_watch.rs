//! ZCode 日志观察：有序读取主/子回合开始、结束、失败与中止。
//! Stop hook 仅为完成候选；日志的 turn.completed 才确认回合边界。
//! 自动汇总没有 UserPromptSubmit hook，turn.started 负责恢复父行。
//! 普通日志只为已有会话续命；首次读取/轮转从文件末尾开始，不重放历史。
//! 日志不完整时保留待确认状态并由现有失联清理兜底，绝不推断成功。

use crate::state::{lock, read, AppState};
use bark_adapters::zcode::AbortLogTail;
use bark_core::NormalizedEvent;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// 轮询间隔：终态信号要求「秒级」可见，1 秒足够，成本只是读一个文件增量
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 启动看门狗（daemon 生命周期内常驻；未开启 ZCode 接入时不干活）
pub fn spawn(state: Arc<AppState>, tx: mpsc::Sender<NormalizedEvent>) {
    tauri::async_runtime::spawn(async move {
        let dir = bark_adapters::zcode::log_dir_default();
        let mut tail = AbortLogTail::default();
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;

            // 接入关闭 → 不读日志、不上报（与管道门保持一致）
            if !read(&state.config).agent_enabled("zcode") {
                continue;
            }
            let scan = tail.poll(&dir);
            // 存活心跳先行：日志活动 = 会话还活着（子代理行向父会话传播，见
            // `LogLine::parent_session_id`），刷新 last_activity
            // 以反映真实日志进展。只刷已有条目、不建档（陈年会话不留痕）。
            for sid in &scan.alive_sessions {
                crate::state::touch_session(&state, "zcode", sid);
            }
            // 主/子回合使用同一有序流：不能先处理全部子事件再处理父终态。
            for mut ev in scan.lifecycle_events {
                let key = crate::state::session_key("zcode", &ev.session_id);
                let session = lock(&state.active_sessions).get(&key).cloned();
                let source = session.or_else(|| ev.parent_session_id.as_ref().and_then(|parent| {
                    lock(&state.active_sessions).get(&crate::state::session_key("zcode", parent)).cloned()
                }));
                if let Some(source) = source {
                    ev.cwd = source.cwd;
                    ev.project = source.project;
                }
                // 边界不能 best-effort 丢弃：已读日志不会重读，丢失子终态会永久扣住父完成。
                if tx.send(ev).await.is_err() { return; }
            }
        }
    });
}
