//! adapter 注册表：集中管理所有 hook 型与监控型适配器。

use crate::claude_style;
use crate::dsh::DshAdapter;
use crate::opencode::OpenCodeAdapter;
use crate::watch::{TraeWorkWatch, WatchAdapter, WorkBuddyWatch};
use crate::zcode::ZcodeAdapter;
use crate::{AgentStatus, HookAdapter, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, BarkConfig};
use std::collections::HashMap;

pub fn hook_adapters() -> Vec<Box<dyn HookAdapter>> {
    vec![
        Box::new(claude_style::claude_code()),
        Box::new(claude_style::trae_code()),
        Box::new(claude_style::codebuddy()),
        Box::new(claude_style::qoder()),
        Box::new(claude_style::codex()),
        Box::new(ZcodeAdapter::new()),
        Box::new(OpenCodeAdapter),
        Box::new(DshAdapter),
    ]
}

pub fn watch_adapters() -> Vec<Box<dyn WatchAdapter>> {
    vec![Box::new(WorkBuddyWatch), Box::new(TraeWorkWatch)]
}

/// 按种类查找 hook 型 adapter（hook 子命令归一化用）
pub fn find_hook_adapter(kind: AgentKind) -> Option<Box<dyn HookAdapter>> {
    hook_adapters().into_iter().find(|a| a.kind() == kind)
}

/// 按种类查找监控型 adapter（GUI 开关启停的可用性预检用）
pub fn find_watch_adapter(kind: AgentKind) -> Option<Box<dyn WatchAdapter>> {
    watch_adapters().into_iter().find(|a| a.kind() == kind)
}

/// 监控型 adapter 的运行时状态（由 GUI 侧维护后回传，用于状态展示）
#[derive(Debug, Clone, Default)]
pub struct WatchRuntime {
    /// 已成功启动的监控：kind_id → 是否正在运行
    pub running: HashMap<String, bool>,
    /// 最近一次启动失败原因
    pub errors: HashMap<String, String>,
}

/// 汇总所有 agent 的状态（UI 面板数据源）
pub fn statuses(cfg: &BarkConfig, exe_path: &str, watch: &WatchRuntime) -> Vec<AgentStatus> {
    let mut out = Vec::new();
    let ctx = RegisterCtx {
        exe_path: exe_path.to_string(),
        port: 0,
        token: String::new(),
    };

    for adapter in hook_adapters() {
        let id = adapter.kind().id();
        let verify = if adapter.is_installed() {
            Some(adapter.verify(&ctx))
        } else {
            None
        };
        let registered = matches!(verify, Some(VerifyReport::Ok));
        out.push(AgentStatus {
            kind: id,
            display_name: adapter.display_name(),
            mode: adapter.kind().mode(),
            installed: adapter.is_installed(),
            registered,
            enabled: cfg.agent_enabled(id),
            trust_hint: adapter.trust_hint(),
            // 只列**实际会写入**的配置（父目录不存在时 register 会跳过）：
            // 合并型 adapter（如 Qoder 的国际版 + 国内版两个路径）只装了一个版本时，
            // 卡片上不该出现另一个不存在的路径
            config_paths: adapter
                .config_paths()
                .iter()
                .filter(|p| p.parent().is_some_and(|d| d.exists()))
                .map(|p| p.display().to_string())
                .collect(),
            verify,
            last_error: None,
        });
    }

    for adapter in watch_adapters() {
        let id = adapter.kind().id();
        let available = adapter.is_available();
        let running = watch.running.get(id).copied().unwrap_or(false);
        out.push(AgentStatus {
            kind: id,
            display_name: adapter.kind().display_name(),
            mode: adapter.kind().mode(),
            installed: adapter.is_installed(),
            // 只有真的跑起来了才算「已接入」，避免显示「监控中」却什么都不产出
            registered: running,
            enabled: cfg.agent_enabled(id),
            trust_hint: Some("非官方方案：轮询本地会话库，应用升级可能导致失效"),
            config_paths: Vec::new(),
            verify: None,
            last_error: if !available {
                adapter.unavailable_reason()
            } else {
                watch.errors.get(id).cloned()
            },
        });
    }

    // DSH 在 hook_adapters() 中，不再需要 Planned 占位块
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个已注册的 hook adapter 都要能被 `AgentKind::from_id` 解析回来。
    ///
    /// 这是本仓最容易漏的一处坑：`AgentKind` 加了变体却忘了同步 `ALL`（显式长度数组）
    /// **仍然能编译**，但 `from_id()` 会返回 None——hook 子命令打印 "unknown agent"
    /// 后 exit 0，事件静默丢失；GUI 勾选则报「未知 agent」。这条断言就是这个坑的网。
    #[test]
    fn every_hook_adapter_resolves_to_a_known_agent_kind() {
        for adapter in hook_adapters() {
            let kind = adapter.kind();
            assert_eq!(
                AgentKind::from_id(kind.id()),
                Some(kind),
                "{} 不在 AgentKind::ALL 里：hook 会静默失效",
                kind.id()
            );
            assert!(
                find_hook_adapter(kind).is_some(),
                "{} 在 hook_adapters() 里找不到 adapter",
                kind.id()
            );
            assert!(!adapter.display_name().is_empty(), "{} 缺少展示名", kind.id());
        }
    }

    /// `ALL` 自洽：id 唯一、可 round-trip（防止复制粘贴写错 id）
    #[test]
    fn agent_kind_all_is_self_consistent() {
        let mut ids: Vec<&str> = AgentKind::ALL.iter().map(|k| k.id()).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "AgentKind::ALL 里有重复 id: {ids:?}");
        for kind in AgentKind::ALL {
            assert_eq!(AgentKind::from_id(kind.id()), Some(kind), "id 漂移: {}", kind.id());
        }
    }

    /// ZCode 接入的回归网（新增 adapter 时最容易漏的就是上面那条 ALL）
    #[test]
    fn zcode_is_wired_up() {
        assert_eq!(AgentKind::from_id("zcode"), Some(AgentKind::Zcode));
        assert_eq!(AgentKind::Zcode.display_name(), "ZCode");
        assert_eq!(AgentKind::Zcode.mode(), bark_core::AdapterMode::Hook);
        let adapter = find_hook_adapter(AgentKind::Zcode).expect("zcode 必须注册在 hook_adapters()");
        assert!(
            adapter.config_paths()[0]
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("/.zcode/cli/config.json"),
            "ZCode 只写用户级配置"
        );
        // 事件表必须限定在 ZCode 确认支持的 7 个事件内（未知事件名有让整份配置加载失败的风险）
        const SUPPORTED: [&str; 7] = [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "PostToolUseFailure",
            "Stop",
        ];
        for event in adapter.hook_events() {
            assert!(SUPPORTED.contains(&event), "注册了 ZCode 不支持的事件: {event}");
        }
    }
}
