//! Antigravity（Google）hooks 接入 —— 「命名组」结构的专用实现。
//!
//! 配置：`~/.gemini/config/hooks.json`（全局；工作区级是 `.agents/hooks.json`，
//! 不碰它）。顶层是**命名组**：每个组名一个顶层键，组内按事件名分键；同文件
//! 可以有多个组（用户自己的 linter hooks 等），我们只拥有组名 `agent-bark`，
//! 组外的所有键与组内非事件键一律不碰——隔离边界与 opencode 的「独占插件文件」
//! 同思路，只是粒度从整个文件缩到一个键。
//!
//! 条目形态因事件而异（官方文档 antigravity.google/docs/hooks + AgentPet 实现
//! 双源一致）：`PreToolUse` / `PostToolUse` 是 matcher 事件，需要
//! `{ "matcher": "*", "hooks": [ { "type": "command", ... } ] }` 包装；
//! `PreInvocation` / `Stop` 是裸 handler 列表 `[{ "type": "command", ... }]`。
//!
//! payload 是 camelCase（`conversationId` / `workspacePaths` / `toolCall{name}`）
//! 且**没有事件名字段**——事件名由 hook 命令的 `--event` 参数提供，不受影响；
//! 字段提取靠 `NormalizedEvent::from_raw` 的别名兜底。
//!
//! **未实测**：映射按官方事件表 + AgentPet 验证过的形态写入，配合启动自愈与
//! 判死兜底。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, InstallEnv, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, EventKind};
use serde_json::{json, Value};
use std::path::PathBuf;

/// 我们独占的组名（顶层键）。其余组是用户的，一个字节都不动。
const GROUP: &str = "agent-bark";

/// agent 事件名 → 统一事件类型（官方 5 个事件里取子集）：
/// - `PreInvocation` → Activity（回合计时开始，等价 UserPromptSubmit；
///   Antigravity 没有 session-start/notification 钩子，这是最早的信号）
/// - `PreToolUse` → Activity（工具开始）；`PostToolUse` → ToolFinished
///   （等待状态解除的唯一信号，§1.11 同口径）
/// - `Stop` → RunCompleted（官方无会话级结束事件，Stop 就是释放运行态的信号）
/// - 没注册 `PostInvocation`：与 Stop 高度重合的回合收尾信号，多注册只多空转
pub(crate) const EVENT_MAP: &[(&str, EventKind)] = &[
    ("PreInvocation", EventKind::Activity),
    ("PreToolUse", EventKind::Activity),
    ("PostToolUse", EventKind::ToolFinished),
    ("Stop", EventKind::RunCompleted),
];

/// 需要 matcher 包装的事件（官方 schema：工具事件按工具匹配，`*` = 全部工具）
const MATCHER_EVENTS: [&str; 2] = ["PreToolUse", "PostToolUse"];

pub struct AntigravityAdapter {
    /// 测试注入固定配置路径（生产为 None，按 home 推导）
    pub(crate) config_override: Option<PathBuf>,
}

pub(crate) fn antigravity() -> AntigravityAdapter {
    AntigravityAdapter { config_override: None }
}

impl AntigravityAdapter {
    fn config_path(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(p) = &self.config_override {
            return p.clone();
        }
        #[cfg(not(test))]
        debug_assert!(self.config_override.is_none());
        let _ = &self.config_override;
        InstallEnv::detect().home.join(".gemini").join("config").join("hooks.json")
    }

    /// 我们的 hook 命令（路径用正斜杠 + 引号，与 claude_style 同规则）
    fn hook_command(&self, ctx: &RegisterCtx, event: &str) -> String {
        let exe = ctx.exe_path.replace('\\', "/");
        format!("\"{}\" hook --agent {} --event {}", exe, AgentKind::Antigravity.id(), event)
    }

    /// 某事件在我们组里的期望形态（matcher 事件带包装，裸事件直接条目数组）
    fn desired_entries(&self, ctx: &RegisterCtx, event: &str) -> Value {
        let command = self.hook_command(ctx, event);
        if MATCHER_EVENTS.contains(&event) {
            json!([{ "matcher": "*", "hooks": [ { "type": "command", "command": command } ] }])
        } else {
            json!([ { "type": "command", "command": command } ])
        }
    }

    /// 展平我们组里某事件下的全部 hook 条目（matcher 包装的拆开，裸的即条目）
    fn entries_of<'a>(group: &'a Value, event: &str) -> Vec<&'a Value> {
        group
            .get(event)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .flat_map(|item| match item.get("hooks").and_then(|h| h.as_array()) {
                        Some(hs) => hs.iter().collect::<Vec<_>>(),
                        None => vec![item],
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl HookAdapter for AntigravityAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Antigravity
    }

    fn display_name(&self) -> &'static str {
        self.kind().display_name()
    }

    fn is_installed(&self) -> bool {
        // 只认 config/ 子目录：~/.gemini 本身 Gemini CLI 也会创建（同家产品共享）
        self.config_path().parent().is_some_and(|p| p.exists())
    }

    fn is_registered(&self) -> bool {
        let Ok(doc) = jsonio::read_doc(self.config_path().as_path()) else {
            return false;
        };
        let Some(group) = doc.get(GROUP) else { return false };
        EVENT_MAP.iter().any(|(event, _)| {
            Self::entries_of(group, event)
                .iter()
                .any(|e| jsonio::is_our_entry(e))
        })
    }

    fn config_paths(&self) -> Vec<PathBuf> {
        vec![self.config_path()]
    }

    fn hook_events(&self) -> Vec<&'static str> {
        EVENT_MAP.iter().map(|(name, _)| *name).collect()
    }

    fn event_kind(&self, event_name: &str) -> Option<EventKind> {
        EVENT_MAP
            .iter()
            .find(|(name, _)| *name == event_name)
            .map(|(_, kind)| *kind)
    }

    fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()> {
        let path = self.config_path();
        // agent 未安装（config/ 目录不存在）时不凭空创建整棵树
        if !path.parent().is_some_and(|p| p.exists()) {
            anyhow::bail!("未找到可写入的配置（{} 可能未安装或未初始化）", self.display_name());
        }
        let mut doc = jsonio::Doc::read(&path)?;
        let existed = path.exists();

        let group = doc
            .value
            .as_object_mut()
            .expect("read_doc 保证顶层是对象")
            .entry(GROUP)
            .or_insert_with(|| json!({}));
        if !group.is_object() {
            anyhow::bail!("{} 的 \"{}\" 组不是对象，已中止写入", path.display(), GROUP);
        }

        let mut changed = false;
        for event in self.hook_events() {
            let desired = self.desired_entries(ctx, event);
            if group.get(event) != Some(&desired) {
                group[event] = desired;
                changed = true;
            }
        }

        if changed {
            if existed {
                jsonio::backup_once(&path);
            }
            // Doc::write 自带 CAS：宿主整份重写时拒绝覆盖
            jsonio::Doc::write(doc, &path)?;
        }
        Ok(())
    }

    fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()> {
        let path = self.config_path();
        if !path.exists() {
            return Ok(());
        }
        let Ok(doc) = jsonio::Doc::read(&path) else {
            // 解析失败就跳过：宁可留下我们的组，也不破坏用户文件
            return Ok(());
        };
        let mut doc = doc;
        let mut changed = false;
        if let Some(obj) = doc.value.as_object_mut() {
            if obj.remove(GROUP).is_some() {
                changed = true;
            }
        }
        if changed && !ctx.dry_run {
            if ctx.backup {
                jsonio::backup_once(&path);
            }
            jsonio::Doc::write(doc, &path)?;
        }
        Ok(())
    }

    fn verify(&self, ctx: &RegisterCtx) -> VerifyReport {
        let path = self.config_path();
        if !path.exists() {
            return VerifyReport::NotRegistered;
        }
        let doc = match jsonio::read_doc(&path) {
            Ok(d) => d,
            Err(e) => return VerifyReport::ConfigUnreadable { path, reason: e.to_string() },
        };
        let Some(group) = doc.get(GROUP) else {
            return VerifyReport::NotRegistered;
        };
        let mut current_any = false;
        let mut stale_any = false;
        for (event, _) in EVENT_MAP {
            for entry in Self::entries_of(group, event) {
                if !jsonio::is_our_entry(entry) {
                    continue;
                }
                let current = entry
                    .get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| jsonio::command_exe_matches(c, &ctx.exe_path));
                if current {
                    current_any = true;
                } else {
                    stale_any = true;
                }
            }
        }
        if current_any {
            VerifyReport::Ok
        } else if stale_any {
            VerifyReport::StalePath { path }
        } else {
            VerifyReport::NotRegistered
        }
    }

    fn trust_hint(&self) -> Option<&'static str> {
        Some("需在 Antigravity 设置中开启 Hooks 后生效")
    }
}

/// 测试辅助：注入固定配置路径
#[cfg(test)]
impl AntigravityAdapter {
    fn with_config(path: PathBuf) -> Self {
        Self { config_override: Some(path) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf as PB;

    fn ctx() -> RegisterCtx {
        RegisterCtx {
            exe_path: r"C:\Program Files\AgentBark\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        }
    }

    fn tmpdir() -> PB {
        let d = std::env::temp_dir().join(format!("agent-bark-antigravity-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// register 写入命名组：组内四个事件形态正确（matcher 包装 vs 裸列表），
    /// 其他组与其他顶层键原样保留
    #[test]
    fn register_writes_named_group_and_preserves_others() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "my-linter": {
                    "enabled": true,
                    "PreToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo hi" } ] } ]
                },
                "unrelated": "keep-me"
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = AntigravityAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["unrelated"], "keep-me", "无关顶层键必须保留");
        assert_eq!(doc["my-linter"]["enabled"], true, "用户组必须原样保留");
        assert_eq!(doc["my-linter"]["PreToolUse"][0]["matcher"], "Bash", "用户组的事件不得被改");

        let group = &doc[GROUP];
        assert_eq!(group["PreToolUse"][0]["matcher"], "*", "工具事件必须 matcher 包装");
        let ptu_cmd = group["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(ptu_cmd.contains("--event PreToolUse"), "matcher 包装内的条目必须指向我们的命令");
        assert!(group["Stop"][0]["command"].as_str().unwrap().contains("--event Stop"));
        assert!(group["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("--event PostToolUse"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等：重复 register 不产生重复条目，内容无变化时 CAS 也不刷新备份
    #[test]
    fn register_is_idempotent() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = AntigravityAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();
        let once = std::fs::read_to_string(&cfg).unwrap();
        adapter.register(&ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), once, "重复注册不得改变文件内容");
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc[GROUP]["Stop"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 卸载只摘我们的组：用户组与无关键原样保留；dry_run 不落盘
    #[test]
    fn unregister_removes_only_our_group() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        let adapter = AntigravityAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: true, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        assert!(jsonio::read_doc(&cfg).unwrap().get(GROUP).is_some(), "dry_run 不得改文件");

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert!(doc.get(GROUP).is_none(), "我们的组必须被整体移除");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// exe 漂移：verify 报 StalePath，register 就地修复
    #[test]
    fn verify_reports_stale_path_and_register_repairs() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = AntigravityAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::Ok));

        let moved = RegisterCtx { exe_path: r"D:\new\AgentBark.exe".into(), port: 1, token: "t".into() };
        assert!(matches!(adapter.verify(&moved), VerifyReport::StalePath { .. }));
        adapter.register(&moved).unwrap();
        assert!(matches!(adapter.verify(&moved), VerifyReport::Ok));
        // 旧路径条目被就地改写，没有新增组
        let doc = jsonio::read_doc(&cfg).unwrap();
        let text = serde_json::to_string(&doc).unwrap();
        assert!(!text.contains("C:/Program Files/AgentBark"), "旧路径必须被改写: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非法 JSON 拒绝写入（不覆盖用户配置）
    #[test]
    fn malformed_config_aborts_without_overwriting() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        let original = "{ not json";
        std::fs::write(&cfg, original).unwrap();
        let adapter = AntigravityAdapter::with_config(cfg.clone());
        assert!(adapter.register(&ctx()).is_err());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "文件不能被改写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未安装（config/ 目录不存在）时报错而非凭空创建
    #[test]
    fn register_without_config_dir_fails_without_creating_tree() {
        let dir = tmpdir();
        let cfg = dir.join("gemini").join("config").join("hooks.json");
        let adapter = AntigravityAdapter::with_config(cfg.clone());
        assert!(adapter.register(&ctx()).is_err());
        assert!(!dir.join("gemini").exists(), "不得凭空创建配置树");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 事件归一化：事件名映射 + payload 别名（camelCase、无事件名字段）
    #[test]
    fn normalize_maps_events_and_payload_aliases() {
        let adapter = antigravity();
        let ev = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "workspacePaths": [r"D:\Workspace\proj"],
                    "toolCall": { "name": "edit_file", "args": {} }
                }),
            )
            .expect("PreToolUse 已注册");
        assert_eq!(ev.kind, EventKind::Activity);
        assert_eq!(ev.session_id, "conv-1", "conversationId 别名必须生效");
        assert_eq!(ev.cwd, r"D:\Workspace\proj", "workspacePaths[0] 别名必须生效");
        assert_eq!(ev.tool_name.as_deref(), Some("edit_file"), "toolCall.name 别名必须生效");

        // Stop → 完成；未注册的事件名不产生事件
        assert_eq!(adapter.event_kind("Stop"), Some(EventKind::RunCompleted));
        assert_eq!(adapter.event_kind("PostInvocation"), None);
        assert_eq!(adapter.event_kind("SessionStart"), None);
    }

    /// Path 传递路径检查：config_path 在生产模式下指向 ~/.gemini/config/hooks.json
    #[test]
    fn production_config_path_layout() {
        let adapter = antigravity();
        let p = adapter.config_path();
        assert!(p.to_string_lossy().replace('\\', "/").ends_with("/.gemini/config/hooks.json"), "{p:?}");
    }
}
