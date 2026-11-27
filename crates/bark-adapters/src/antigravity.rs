//! Antigravity（Google）hooks 接入 —— 「命名组」结构的专用实现。
//!
//! 配置：`~/.gemini/config/hooks.json`（全局；工作区级是 `.agents/hooks.json`，
//! 不碰它）。顶层是**命名组**：每个组名一个顶层键，组内按事件名分键；同文件
//! 可以有多个组（用户自己的 linter hooks 等），我们只拥有组名 `agent-bark`，
//! 组外的所有键与组内非事件键一律不碰——隔离边界与 opencode 的「独占插件文件」
//! 同思路，只是粒度从整个文件缩到一个键。
//!
//! 条目形态因事件而异（官方文档 antigravity.google/docs/hooks + 2026-11-27 本机实测
//! Antigravity 2.35.2）：`PreToolUse` / `PostToolUse` 是 matcher 事件，需要
//! `{ "matcher": "*", "hooks": [ { "type": "command", ... } ] }` 包装；
//! `PreInvocation` / `Stop` 是裸 handler 列表 `[{ "type": "command", ... }]`。
//!
//! payload 是 camelCase（`conversationId` / `workspacePaths` / `toolCall{name,args}` /
//! `lastUserInput` / `terminationReason` / `error`），事件名由 hook 命令的 `--event`
//! 参数提供，字段提取靠 `NormalizedEvent::from_raw` 与本模块的 `normalize` 补齐。
//!
//! 实测要点（2026-11-27，Antigravity 2.35.2 / macOS）：
//! - Antigravity 没有独立的 `Notification` / `PermissionRequest` hook：向用户展示选项
//!   或请求额外权限走 `PreToolUse`（`ask_question` / `ask_permission` / 带 `BlockedOnUser`
//!   的 `notify_user`），用户回答/确认后触发 `PostToolUse`（`ToolFinished` 解除等待）；
//! - `Stop` 载荷带 `terminationReason`（`NO_TOOL_CALL` / `USER_CANCELED` / `ERROR` 等）与
//!   `error`，需区分正常完成、用户取消与失败；
//! - 思考中手动点击停止时，宿主记录 `CancelCascadeInvocation` / `Cancel requested for cascade`
//!   但**不会执行 `Stop` hook**，由 daemon 侧的 `antigravity_watch` 增量尾读日志补出
//!   `RunAborted`。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, InstallEnv, RegisterCtx, VerifyReport};
use bark_core::event::{truncate_chars, MAX_MESSAGE_CHARS};
use bark_core::{AgentKind, EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::path::PathBuf;

/// 我们独占的组名（顶层键）。其余组是用户的，一个字节都不动。
const GROUP: &str = "agent-bark";

/// agent 事件名 → 默认统一事件类型（官方 6 个事件里取子集，`normalize` 再按载荷细化）：
/// - `PreInvocation` → Activity（回合/推理开始，带 `lastUserInput` 作为任务提示）
/// - `PreToolUse` → Activity（交互提问/权限工具升级为 InputRequired / PermissionRequired）
/// - `PostToolUse` → ToolFinished（带非空 `error` 时为 ToolFailed；两者均解除等待状态）
/// - `Stop` → RunCompleted（按 `terminationReason` / `error` 细分为 RunAborted / RunFailed）
/// - 没注册 `PostInvocation`：与 Stop 高度重合，多注册只多空转
pub(crate) const EVENT_MAP: &[(&str, EventKind)] = &[
    ("PreInvocation", EventKind::Activity),
    ("PreToolUse", EventKind::Activity),
    ("PostToolUse", EventKind::ToolFinished),
    ("Stop", EventKind::RunCompleted),
];

/// 提取工具的末段名（兼容 `default_api:ask_question` / `functions.ask_question` 等命名空间前缀）。
pub fn base_tool_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name).trim()
}

/// 载荷里的工具名（Antigravity 在 `toolCall.name`，同时兜底常见别名）。
fn raw_tool_name(raw: &Value) -> Option<&str> {
    raw.pointer("/toolCall/name")
        .or_else(|| raw.get("tool_name"))
        .or_else(|| raw.get("toolName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 载荷里的工具参数对象（Antigravity 在 `toolCall.args`，同时兜底 `tool_input`）。
fn raw_tool_args(raw: &Value) -> Option<&Value> {
    raw.pointer("/toolCall/args")
        .or_else(|| raw.get("tool_input"))
        .or_else(|| raw.get("toolInput"))
}

/// 布尔字段宽松读取（兼容真实 bool 与 proto/JSON 序列化出的 `"true"`）。
fn bool_flag(v: &Value) -> bool {
    v.as_bool() == Some(true)
        || v.as_str()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true"))
}

/// Antigravity 交互式等待工具判定：`PreToolUse` 命中时立即切入对应等待相位。
fn waiting_kind_for_tool(name: &str, args: Option<&Value>) -> Option<EventKind> {
    match base_tool_name(name).to_ascii_lowercase().as_str() {
        "ask_question" | "ask_user_question" => Some(EventKind::InputRequired),
        "ask_permission" => Some(EventKind::PermissionRequired),
        "notify_user" => {
            let blocked = args.is_some_and(|a| {
                a.get("BlockedOnUser")
                    .or_else(|| a.get("blockedOnUser"))
                    .or_else(|| a.get("blocked_on_user"))
                    .is_some_and(bool_flag)
            });
            blocked.then_some(EventKind::InputRequired)
        }
        _ => None,
    }
}

/// 从 `ask_question` 参数提取问题文案：同时兼容 `questions` 为 JSON 数组或 JSON 字符串
/// （Antigravity transcript 实测存在 JSON 字符串化形态）。
fn extract_ask_question_message(args: Option<&Value>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    let parsed_str;
    let questions = match args.get("questions") {
        Some(Value::Array(items)) => Some(items.as_slice()),
        Some(Value::String(text)) => {
            parsed_str = serde_json::from_str::<Value>(text).ok();
            parsed_str.as_ref().and_then(Value::as_array).map(Vec::as_slice)
        }
        _ => None,
    };
    if let Some(items) = questions {
        let joined = items
            .iter()
            .filter_map(|q| {
                q.get("question")
                    .or_else(|| q.get("title"))
                    .or_else(|| q.get("header"))
                    .or_else(|| q.get("prompt"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return truncate_chars(&joined, MAX_MESSAGE_CHARS);
        }
    }
    for key in ["question", "prompt", "message", "Message"] {
        if let Some(s) = args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            return truncate_chars(s, MAX_MESSAGE_CHARS);
        }
    }
    String::new()
}

/// 从 `ask_permission` 参数提取权限说明（优先 `Reason`，否则拼 `Action` + `Target`）。
fn extract_ask_permission_message(args: Option<&Value>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    if let Some(reason) = args
        .get("Reason")
        .or_else(|| args.get("reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return truncate_chars(reason, MAX_MESSAGE_CHARS);
    }
    let action = args
        .get("Action")
        .or_else(|| args.get("action"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target = args
        .get("Target")
        .or_else(|| args.get("target"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let summary = match (action, target) {
        (Some(a), Some(t)) => format!("{a}: {t}"),
        (Some(a), None) => a.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => String::new(),
    };
    truncate_chars(&summary, MAX_MESSAGE_CHARS)
}

/// 从 `notify_user` 参数提取展示消息。
fn extract_notify_user_message(args: Option<&Value>) -> String {
    let Some(args) = args else {
        return String::new();
    };
    args.get("Message")
        .or_else(|| args.get("message"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_chars(s, MAX_MESSAGE_CHARS))
        .unwrap_or_default()
}

/// 非空错误文本（`Stop` / `PostToolUse` 的 `error` 字段）。
fn non_empty_error(raw: &Value) -> Option<&str> {
    raw.get("error")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 根据 `Stop` 载荷里的 `terminationReason` 与 `error` 细分终态类型。
/// 官方枚举（hooks.md）：`NO_TOOL_CALL` / `MAX_INVOCATIONS` / `MAX_FORCED_INVOCATIONS` /
/// `MAX_TOKEN_BUDGET_EXCEEDED` / `ERROR` / `USER_CANCELED` / `TERMINAL_STEP_TYPE` /
/// `TERMINAL_CUSTOM_HOOK` / `INJECTED_RESPONSE`（以及在线文档示例里的 `model_stop` / `error`）。
fn stop_event_kind(raw: &Value) -> EventKind {
    let reason = raw
        .get("terminationReason")
        .or_else(|| raw.get("termination_reason"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(reason) = reason {
        let norm = reason.to_ascii_uppercase();
        let norm = norm.strip_prefix("EXECUTOR_TERMINATION_REASON_").unwrap_or(&norm);
        return match norm {
            "USER_CANCELED" | "USER_CANCELLED" | "CANCELED" | "CANCELLED" | "ABORTED" => {
                EventKind::RunAborted
            }
            "ERROR" => EventKind::RunFailed,
            _ if non_empty_error(raw).is_some() => EventKind::RunFailed,
            _ => EventKind::RunCompleted,
        };
    }
    if non_empty_error(raw).is_some() {
        EventKind::RunFailed
    } else {
        EventKind::RunCompleted
    }
}

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

    fn normalize(&self, event_name: &str, raw: &Value) -> Option<NormalizedEvent> {
        let base_kind = self.event_kind(event_name)?;
        let tool = raw_tool_name(raw);
        let args = raw_tool_args(raw);
        let kind = match event_name {
            "PreToolUse" => tool
                .and_then(|name| waiting_kind_for_tool(name, args))
                .unwrap_or(base_kind),
            "PostToolUse" => {
                if non_empty_error(raw).is_some() {
                    EventKind::ToolFailed
                } else {
                    EventKind::ToolFinished
                }
            }
            "Stop" => stop_event_kind(raw),
            _ => base_kind,
        };
        let mut event = NormalizedEvent::from_raw(self.kind().id(), kind, raw);
        if event_name == "PreToolUse" && event.message.is_empty() {
            if let Some(name) = tool {
                let extracted = match base_tool_name(name).to_ascii_lowercase().as_str() {
                    "ask_question" | "ask_user_question" => extract_ask_question_message(args),
                    "ask_permission" => extract_ask_permission_message(args),
                    "notify_user" if kind == EventKind::InputRequired => {
                        extract_notify_user_message(args)
                    }
                    _ => String::new(),
                };
                if !extracted.is_empty() {
                    event.message = extracted;
                }
            }
        } else if event_name == "PreInvocation" && event.message.is_empty() {
            if let Some(prompt) = raw
                .get("lastUserInput")
                .or_else(|| raw.get("last_user_input"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                event.message = truncate_chars(prompt, MAX_MESSAGE_CHARS);
            }
        }
        Some(event)
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

    fn trust_hint(&self, lang: bark_core::Lang) -> Option<&'static str> {
        let (zh, en) = (
            "需在 Antigravity 设置中开启 Hooks 后生效",
            "Enable Hooks in the Antigravity settings for this to take effect",
        );
        Some(match lang {
            bark_core::Lang::Zh => zh,
            bark_core::Lang::En => en,
        })
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
            lang: bark_core::Lang::Zh,
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

        let moved = RegisterCtx { exe_path: r"D:\new\AgentBark.exe".into(), port: 1, token: "t".into(), lang: bark_core::Lang::Zh };
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

        // PreInvocation 提取 lastUserInput 作为回合 prompt
        let pre_inv = adapter
            .normalize(
                "PreInvocation",
                &json!({
                    "conversationId": "conv-1",
                    "workspacePaths": [r"D:\Workspace\proj"],
                    "lastUserInput": "帮我排查 hook 问题"
                }),
            )
            .unwrap();
        assert_eq!(pre_inv.kind, EventKind::Activity);
        assert_eq!(pre_inv.tool_name, None);
        assert_eq!(pre_inv.message, "帮我排查 hook 问题");
    }

    /// 交互式选项 / 权限工具：PreToolUse 切等待态，PostToolUse 解除等待
    #[test]
    fn normalize_maps_interactive_tools_to_waiting_and_post_tool_releases() {
        let adapter = antigravity();

        // 1) ask_question（questions 为 JSON 数组）
        let ev = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": {
                        "name": "ask_question",
                        "args": {
                            "questions": [
                                { "question": "选择方案 A 还是 B？", "options": ["A", "B"] },
                                { "question": "是否同步更新文档？", "options": ["是", "否"] }
                            ]
                        }
                    }
                }),
            )
            .unwrap();
        assert_eq!(ev.kind, EventKind::InputRequired);
        assert_eq!(ev.tool_name.as_deref(), Some("ask_question"));
        assert_eq!(ev.message, "选择方案 A 还是 B？\n是否同步更新文档？");

        // 2) ask_question（questions 为 JSON 字符串 + 带命名空间前缀）
        let ev_str = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": {
                        "name": "default_api:ask_question",
                        "args": {
                            "questions": "[{\"question\":\"模型结构选哪种？\",\"options\":[\"A\",\"B\"]}]"
                        }
                    }
                }),
            )
            .unwrap();
        assert_eq!(ev_str.kind, EventKind::InputRequired);
        assert_eq!(ev_str.message, "模型结构选哪种？");

        // 3) ask_permission → PermissionRequired
        let perm = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": {
                        "name": "ask_permission",
                        "args": { "Action": "run_command", "Target": "npm test", "Reason": "需要运行测试" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(perm.kind, EventKind::PermissionRequired);
        assert_eq!(perm.message, "需要运行测试");

        // 4) notify_user：仅 BlockedOnUser=true 才进等待态
        let blocked = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": {
                        "name": "notify_user",
                        "args": { "BlockedOnUser": true, "Message": "请确认实现计划" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(blocked.kind, EventKind::InputRequired);
        assert_eq!(blocked.message, "请确认实现计划");

        let non_blocking = adapter
            .normalize(
                "PreToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": {
                        "name": "notify_user",
                        "args": { "BlockedOnUser": false, "Message": "进度同步" }
                    }
                }),
            )
            .unwrap();
        assert_eq!(non_blocking.kind, EventKind::Activity);

        // 5) PostToolUse：正常收尾 → ToolFinished；带 error → ToolFailed
        let post_ok = adapter
            .normalize(
                "PostToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": { "name": "ask_question" }
                }),
            )
            .unwrap();
        assert_eq!(post_ok.kind, EventKind::ToolFinished);
        assert_eq!(post_ok.tool_name.as_deref(), Some("ask_question"));

        let post_err = adapter
            .normalize(
                "PostToolUse",
                &json!({
                    "conversationId": "conv-1",
                    "toolCall": { "name": "edit_file" },
                    "error": "target not found"
                }),
            )
            .unwrap();
        assert_eq!(post_err.kind, EventKind::ToolFailed);
        assert_eq!(post_err.message, "target not found");
    }

    /// Stop 载荷按 terminationReason / error 区分完成、用户取消与失败
    #[test]
    fn normalize_classifies_stop_termination_reasons() {
        let adapter = antigravity();

        for reason in ["USER_CANCELED", "EXECUTOR_TERMINATION_REASON_USER_CANCELED", "user_canceled", "aborted"] {
            let ev = adapter
                .normalize("Stop", &json!({ "conversationId": "c1", "terminationReason": reason }))
                .unwrap();
            assert_eq!(ev.kind, EventKind::RunAborted, "reason={reason}");
        }

        let err_reason = adapter
            .normalize("Stop", &json!({ "conversationId": "c1", "terminationReason": "ERROR", "error": "quota exceeded" }))
            .unwrap();
        assert_eq!(err_reason.kind, EventKind::RunFailed);
        assert_eq!(err_reason.message, "quota exceeded");

        let err_only = adapter
            .normalize("Stop", &json!({ "conversationId": "c1", "error": "upstream 500" }))
            .unwrap();
        assert_eq!(err_only.kind, EventKind::RunFailed);

        for reason in ["NO_TOOL_CALL", "model_stop", "MAX_INVOCATIONS", "TERMINAL_STEP_TYPE"] {
            let ev = adapter
                .normalize("Stop", &json!({ "conversationId": "c1", "terminationReason": reason, "error": "" }))
                .unwrap();
            assert_eq!(ev.kind, EventKind::RunCompleted, "reason={reason}");
        }
    }

    /// Path 传递路径检查：config_path 在生产模式下指向 ~/.gemini/config/hooks.json
    #[test]
    fn production_config_path_layout() {
        let adapter = antigravity();
        let p = adapter.config_path();
        assert!(p.to_string_lossy().replace('\\', "/").ends_with("/.gemini/config/hooks.json"), "{p:?}");
    }
}
