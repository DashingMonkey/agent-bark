//! 「本地证据」终态源的统一接入点。
//!
//! 有些 agent 的关键终态**不发 hook**，只能读它自己的 transcript / 日志推断：
//! - Codex：`task_complete.error`（额度用尽等）只在 rollout JSONL 里；
//! - Qoder：额度/鉴权被拒时引擎只在自己的 transcript 里写一条合成 API 错误。
//!
//! 这类补偿各有一个 watcher 模块（`codex_watch` / `qoder_watch`），本模块只做**注册表**：
//! 状态机入口问一次「这条事件还能不能落到当前回合」，具体判据留在各 watcher 里。
//! 这样 `run_pipeline` / `is_stale_replay` 不需要认识任何 agent 名字；新增一个源 =
//! 新 watcher 模块 + 这里登记一行。

use crate::state::{lock, session_key, AppState, SessionStatus};
use bark_core::NormalizedEvent;

struct Source {
    /// 事件所属 agent（与 id 前缀一起匹配，防别的 agent 伪造同前缀 id）
    agent: &'static str,
    /// 该 watcher 生成的事件 id 前缀
    prefix: &'static str,
    /// 该 agent **所有**事件进入状态机前的过滤（如 Codex 的回合身份 / 内部建议）。
    /// 不是本地证据的普通 hook 事件也要过它——这是既有行为，不能只在终态事件上生效。
    accept: Option<fn(&AppState, &NormalizedEvent) -> bool>,
    /// 带本前缀的终态事件是否仍属于该会话的当前回合
    current: fn(Option<&SessionStatus>, &NormalizedEvent) -> bool,
    /// 应用事件前的额外相位修正（Codex 异步提问：等待回答期间保留等待相位）
    preserve_wait: Option<fn(&AppState, &NormalizedEvent) -> bool>,
}

static SOURCES: &[Source] = &[
    Source {
        agent: "codex",
        prefix: crate::codex_watch::EVENT_PREFIX,
        accept: Some(crate::codex_watch::accept),
        current: crate::codex_watch::still_current,
        preserve_wait: Some(crate::codex_watch::preserve_wait),
    },
    Source {
        agent: "qoder",
        prefix: crate::qoder_watch::EVENT_PREFIX,
        accept: None,
        current: crate::qoder_watch::still_current,
        preserve_wait: None,
    },
    Source {
        agent: "codebuddy",
        prefix: crate::codebuddy_watch::EVENT_PREFIX,
        accept: None,
        current: crate::codebuddy_watch::still_current,
        preserve_wait: None,
    },
];

fn source_for_agent(agent: &str) -> Option<&'static Source> {
    SOURCES.iter().find(|source| source.agent == agent)
}

/// 状态机入口的统一门：先跑该 agent 的入口过滤；本地证据终态再校验回合身份。
/// 其余事件（普通 hook 心跳、监控型事件）直接放行，行为与没有本模块时一致。
pub fn admit(state: &AppState, event: &NormalizedEvent) -> bool {
    let Some(source) = source_for_agent(&event.agent) else {
        return true;
    };
    if let Some(accept) = source.accept {
        if !accept(state, event) {
            return false;
        }
    }
    if !event.id.starts_with(source.prefix) {
        return true;
    }
    let current = lock(&state.active_sessions)
        .get(&session_key(&event.agent, &event.session_id))
        .cloned();
    (source.current)(current.as_ref(), event)
}

/// 应用事件前的额外相位修正；该 agent 没声明就返回 false（保持通用语义）。
/// 目前只有 Codex 的异步提问需要：等待回答期间不能被工具心跳解除等待。
pub fn preserve_wait(state: &AppState, event: &NormalizedEvent) -> bool {
    source_for_agent(&event.agent)
        .and_then(|source| source.preserve_wait)
        .is_some_and(|preserve| preserve(state, event))
}

/// 是不是某个 watcher 补出来的「本地证据」事件（只豁免这类事件的补投有效期；
/// 它们的回合身份已由 [`admit`] 校验过）。
pub fn is_local_evidence(event: &NormalizedEvent) -> bool {
    source_for_agent(&event.agent)
        .is_some_and(|source| event.id.starts_with(source.prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_core::EventKind;

    fn event(agent: &str, id: &str) -> NormalizedEvent {
        let mut event = NormalizedEvent::from_raw(
            agent,
            EventKind::RunFailed,
            &serde_json::json!({ "session_id": "s1" }),
        );
        event.id = id.into();
        event
    }

    #[test]
    fn only_registered_watcher_ids_are_local_evidence() {
        let hook = event("qoder", "hook-id");
        assert!(!is_local_evidence(&hook), "普通 hook 事件不是本地证据");

        let qoder = event("qoder", &format!("{}s1:1", crate::qoder_watch::EVENT_PREFIX));
        assert!(is_local_evidence(&qoder));

        let codex = event("codex", &format!("{}s1:1", crate::codex_watch::EVENT_PREFIX));
        assert!(is_local_evidence(&codex));

        let codebuddy = event(
            "codebuddy",
            &format!("{}s1:1", crate::codebuddy_watch::EVENT_PREFIX),
        );
        assert!(is_local_evidence(&codebuddy));

        // 前缀对但 agent 不对：不认，避免别的 agent 的事件混进豁免/回合校验。
        let forged = event("claude-code", &format!("{}s1:1", crate::qoder_watch::EVENT_PREFIX));
        assert!(!is_local_evidence(&forged));
    }

    #[test]
    fn registered_sources_keep_their_own_judgements() {
        let qoder = source_for_agent("qoder").expect("qoder 必须登记");
        assert_eq!(qoder.prefix, crate::qoder_watch::EVENT_PREFIX);
        assert!(qoder.accept.is_none(), "Qoder 的普通 hook 事件不需要入口过滤");
        assert!(qoder.preserve_wait.is_none());
        let codex = source_for_agent("codex").expect("codex 必须登记");
        assert_eq!(codex.prefix, crate::codex_watch::EVENT_PREFIX);
        assert!(codex.accept.is_some(), "Codex 的回合身份过滤不能丢");
        assert!(codex.preserve_wait.is_some(), "Codex 的异步提问等待修正不能丢");
        let codebuddy = source_for_agent("codebuddy").expect("codebuddy 必须登记");
        assert_eq!(codebuddy.prefix, crate::codebuddy_watch::EVENT_PREFIX);
        assert!(codebuddy.accept.is_none(), "CodeBuddy 的普通 hook 事件不需要入口过滤");
        assert!(source_for_agent("trae-code").is_none());
    }
}
