//! 公共父子任务完成判定。只修改现有会话表，不持有第二份运行状态。
//!
//! 回合结束与任务完成分开：后代未收尾时等待后代；后代结束后等待父代理
//! 恢复并完成新的回合。没有恢复证据时不能靠定时器补发旧完成。
use std::collections::{HashMap, HashSet};

use bark_core::{EventKind, NormalizedEvent, RunBoundary};

use crate::state::{session_key, SessionPhase, SessionStatus};

#[derive(Debug, Clone, Default)]
pub struct RunState {
    pub turn_id: Option<String>,
    pub started_at: i64,
    pub held: Option<Hold>,
    pub candidate_message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    Candidate,
    Descendants,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Apply,
    Ignore,
    Defer,
}

/// 仅根据明确的血缘查后代，跨 agent 隔离，坏数据中的环不会卡住事件管道。
pub fn descendant_count(sessions: &HashMap<String, SessionStatus>, root: &str) -> usize {
    let mut seen = HashSet::from([root.to_owned()]);
    let mut pending = vec![root.to_owned()];
    while let Some(parent) = pending.pop() {
        for (key, session) in sessions {
            if session.parent_key.as_deref() == Some(&parent) && seen.insert(key.clone()) {
                pending.push(key.clone());
            }
        }
    }
    seen.len() - 1
}

/// 在普通相位转换前判断边界。Defer 同时拦住通知、光效、音效和完成驻留。
pub fn prepare(sessions: &mut HashMap<String, SessionStatus>, ev: &NormalizedEvent) -> Decision {
    let key = session_key(&ev.agent, &ev.session_id);
    let count = descendant_count(sessions, &key);
    let Some(session) = sessions.get_mut(&key) else {
        // Stop 先于日志到达时不能造一个已完成的虚假任务；日志才是边界真源。
        return if ev
            .run
            .as_ref()
            .is_some_and(|r| r.boundary == RunBoundary::Candidate)
        {
            Decision::Ignore
        } else {
            Decision::Apply
        };
    };
    if session.lifecycle.turn_id.is_some() && ev.timestamp < session.lifecycle.started_at {
        return Decision::Ignore;
    }
    if let Some(run) = &ev.run {
        if run.boundary == RunBoundary::Started {
            if session.lifecycle.turn_id.as_deref() == Some(&run.turn_id)
                || ev.timestamp < session.lifecycle.started_at
            {
                return Decision::Ignore;
            }
            session.lifecycle = RunState {
                turn_id: Some(run.turn_id.clone()),
                started_at: ev.timestamp,
                ..Default::default()
            };
            return Decision::Apply;
        }
        if !run.turn_id.is_empty()
            && session
                .lifecycle
                .turn_id
                .as_ref()
                .is_some_and(|id| id != &run.turn_id)
        {
            return Decision::Ignore;
        }
        if session.lifecycle.turn_id.is_none() && !run.turn_id.is_empty() {
            session.lifecycle.turn_id = Some(run.turn_id.clone());
            session.lifecycle.started_at = ev.timestamp;
        }
    }
    if matches!(ev.kind, EventKind::RunFailed | EventKind::RunAborted) {
        return Decision::Apply;
    }
    if ev.kind == EventKind::RunCompleted
        && (session.lifecycle.turn_id.is_some() || ev.run.is_some())
    {
        let candidate = ev
            .run
            .as_ref()
            .is_some_and(|r| r.boundary == RunBoundary::Candidate);
        let children_active = ev
            .run
            .as_ref()
            .and_then(|r| r.children_active)
            .unwrap_or(count > 0);
        if candidate && !ev.message.is_empty() {
            session.lifecycle.candidate_message = ev.message.clone();
        }
        if candidate || children_active || session.lifecycle.held == Some(Hold::Descendants) {
            // Candidate 后的 Finished 可以确认普通回合；一旦因后代扣住过，
            // 同一回合的重复 Finished 不能在子代理刚结束时放行。
            if !candidate {
                session.lifecycle.held = Some(Hold::Descendants);
            } else if session.lifecycle.held.is_none() {
                session.lifecycle.held = Some(Hold::Candidate);
            }
            session.phase = if children_active {
                SessionPhase::WaitingChildren
            } else {
                SessionPhase::Settling
            };
            session.active_descendants = count;
            session.last_activity = session.last_activity.max(ev.timestamp);
            return Decision::Defer;
        }
        return Decision::Apply;
    }
    if session.lifecycle.held == Some(Hold::Candidate)
        && !ev.keepalive
        && matches!(
            ev.kind,
            EventKind::Activity | EventKind::ToolFinished | EventKind::ToolFailed
        )
    {
        session.lifecycle.held = None;
        session.lifecycle.candidate_message.clear();
    }
    // 已结束回合的保活/工具回声不能把等待后代伪装成主代理重新思考。
    // 提供边界的适配器必须用下一回合 Started 解除；审批仍可正常呈现。
    if session.lifecycle.held.is_some()
        && matches!(
            ev.kind,
            EventKind::Activity | EventKind::ToolFinished | EventKind::ToolFailed
        )
        && !ev.keepalive
    {
        return Decision::Ignore;
    }
    Decision::Apply
}

/// 子树变化只更新快照；最后一个后代消失绝不等价于父任务成功。
pub fn refresh(sessions: &mut HashMap<String, SessionStatus>) -> bool {
    let updates: Vec<_> = sessions
        .iter()
        .filter(|(_, s)| s.lifecycle.held.is_some())
        .map(|(key, _)| (key.clone(), descendant_count(sessions, key)))
        .collect();
    let mut changed = false;
    for (key, count) in updates {
        let s = sessions.get_mut(&key).expect("snapshot key");
        if matches!(
            s.phase,
            SessionPhase::WaitingPermission | SessionPhase::WaitingInput
        ) {
            continue;
        }
        let phase = if count > 0 {
            SessionPhase::WaitingChildren
        } else {
            SessionPhase::Settling
        };
        changed |= s.phase != phase || s.active_descendants != count;
        s.phase = phase;
        s.active_descendants = count;
    }
    changed
}
