//! Kimi Code CLI（Moonshot）hooks 接入 —— 唯一的 TOML 宿主。
//!
//! 配置：`~/.kimi-code/config.toml` 的 `[[hooks]]` 数组（官方文档
//! moonshotai.github.io/kimi-code customization/hooks，20 个事件，fail-open）：
//!
//! ```toml
//! [[hooks]]
//! event = "Stop"
//! command = "..."
//! ```
//!
//! config.toml 里同时是用户的 provider 凭据与手工注释，**必须格式保留编辑**
//! （`toml::DocumentMut`，toml_edit 语义）：只增删改我们自己的 `[[hooks]]`
//! 条目，注释、空行、键序原样保留；解析失败拒绝写入。写回前做内容级 CAS
//! （与 jsonio::Doc 同口径：读取后被并发修改就拒绝覆盖），原子落盘复用
//! [`jsonio::write_text_atomic`]。
//!
//! payload 为 snake_case（`hook_event_name` / `session_id` / `cwd`），与 Claude
//! 系同构；事件名走 `--event` 参数。**未实测**，配合启动自愈与判死兜底。
//!
//! 注：kimi-cli（旧仓）已归档，继任者是 kimi-code；两者的会话目录不同
//! （`~/.kimi` vs `~/.kimi-code`），这里只认后者。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, InstallEnv, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, EventKind};
use std::path::PathBuf;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// agent 事件名 → 统一事件类型（官方 20 个事件里取子集）：
/// - `UserPromptSubmit` / `PreToolUse` → Activity 心跳（回合开始 / 工具开始）
/// - `PostToolUse` → ToolFinished（等待状态解除的唯一信号，§1.11 同口径）
/// - `PostToolUseFailure` → ToolFailed（工具级失败，agent 自行重试）
/// - `PermissionRequest` → PermissionRequired；`Notification` → InputRequired
/// - `Stop` → RunCompleted；`StopFailure` → RunFailed
/// - `Interrupt` → RunAborted：**显式的用户中断信号**（多数宿主没有它），
///   不用等判死；`SessionEnd` → RunAborted（会话级释放，同口径不通知）
/// - 没注册：SessionStart（本仓库空操作）、SessionHeartbeat（未知频率，
///   避免空转）、SubagentStart / SubagentStop（子代理噪音）、TaskStarted /
///   UserPromptQueued / PermissionResult / PreCompact / PostCompact（无通知语义）
pub(crate) const EVENT_MAP: &[(&str, EventKind)] = &[
    ("UserPromptSubmit", EventKind::Activity),
    ("PreToolUse", EventKind::Activity),
    ("PostToolUse", EventKind::ToolFinished),
    ("PostToolUseFailure", EventKind::ToolFailed),
    ("PermissionRequest", EventKind::PermissionRequired),
    ("Notification", EventKind::InputRequired),
    ("Stop", EventKind::RunCompleted),
    ("StopFailure", EventKind::RunFailed),
    ("Interrupt", EventKind::RunAborted),
    ("SessionEnd", EventKind::RunAborted),
];

pub struct KimiCodeAdapter {
    /// 测试注入固定配置路径（生产为 None，按 home 推导）
    pub(crate) config_override: Option<PathBuf>,
}

pub(crate) fn kimi_code() -> KimiCodeAdapter {
    KimiCodeAdapter { config_override: None }
}

/// TOML hook 条目是否是我们写入的（command 首个 token 是已知可执行名）。
/// TOML 条目没有 JSON Value 外壳，不能用 jsonio::is_our_entry，但判定口径一致。
fn entry_is_ours(item: &Table) -> bool {
    item.get("command")
        .and_then(|i| i.as_str())
        .is_some_and(|cmd| {
            let exe = jsonio::first_token(cmd);
            let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
            jsonio::KNOWN_EXE_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
        })
}

/// 条目里登记的事件名（缺 event 字段视为未知，去重时按 command 兜底）
fn entry_event(item: &Table) -> Option<&str> {
    item.get("event").and_then(|i| i.as_str())
}

impl KimiCodeAdapter {
    fn config_path(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(p) = &self.config_override {
            return p.clone();
        }
        #[cfg(not(test))]
        debug_assert!(self.config_override.is_none());
        let _ = &self.config_override;
        InstallEnv::detect().home.join(".kimi-code").join("config.toml")
    }

    fn hook_command(&self, ctx: &RegisterCtx, event: &str) -> String {
        let exe = ctx.exe_path.replace('\\', "/");
        format!("\"{}\" hook --agent {} --event {}", exe, AgentKind::KimiCode.id(), event)
    }
}

impl HookAdapter for KimiCodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::KimiCode
    }

    fn display_name(&self) -> &'static str {
        self.kind().display_name()
    }

    fn is_installed(&self) -> bool {
        // CLI 与桌面客户端（2026-09-17 发布）共用 ~/.kimi-code（官方文档明示），
        // hooks 对桌面端同样生效；只装桌面端时补认其应用数据目录
        // %APPDATA%\kimi-code-app（官方「数据路径」文档）
        self.config_path().parent().is_some_and(|p| p.exists())
            || dirs::data_dir().is_some_and(|d| d.join("kimi-code-app").is_dir())
    }

    fn is_registered(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.config_path()) else {
            return false;
        };
        let Ok(doc) = text.parse::<DocumentMut>() else {
            return false;
        };
        doc.get("hooks")
            .and_then(|i| i.as_array_of_tables())
            .is_some_and(|aot| aot.iter().any(|t| entry_is_ours(t)))
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
        if !path.parent().is_some_and(|p| p.exists()) {
            anyhow::bail!("未找到可写入的配置（{} 可能未安装或未初始化）", self.display_name());
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        // 解析失败拒绝写入：config.toml 里有用户的 provider 凭据，绝不能整份替换
        let mut doc: DocumentMut = raw
            .parse()
            .map_err(|e| anyhow::anyhow!("{} 不是合法 TOML（{e}）。为避免覆盖你的配置，已中止写入。", path.display()))?;

        // hooks 数组缺失时补一个空的 [[hooks]] 数组（DocumentMut 索引默认建的是
        // 隐式 Table，[[hooks]] 必须显式用 ArrayOfTables）
        if doc.get("hooks").is_none() {
            doc.insert("hooks", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let Some(aot) = doc.get_mut("hooks").and_then(|i| i.as_array_of_tables_mut()) else {
            anyhow::bail!("{} 的 hooks 不是 [[hooks]] 数组表，已中止写入", path.display());
        };

        // 每个事件保留一个我们的条目（就位修复），多余的移除，缺的追加——
        // 保证重复注册幂等（内容无变化就不写盘）
        let mut changed = false;
        let mut kept: Vec<bool> = vec![false; EVENT_MAP.len()];
        let mut removed = true;
        while removed {
            removed = false;
            for idx in (0..aot.len()).rev() {
                let ours = aot.get(idx).is_some_and(|t| entry_is_ours(t));
                if !ours {
                    continue;
                }
                let ev_name = aot.get(idx).and_then(entry_event).map(str::to_string);
                let pos = ev_name
                    .as_deref()
                    .and_then(|e| EVENT_MAP.iter().position(|(name, _)| *name == e));
                match pos {
                    Some(p) if !kept[p] => {
                        // 保留的第一个条目：就地修 command（exe 漂移自愈）
                        let command = self.hook_command(ctx, EVENT_MAP[p].0);
                        let item = aot.get_mut(idx).expect("idx 有效");
                        if item.get("command").and_then(|i| i.as_str()) != Some(command.as_str()) {
                            item.insert("command", value(command));
                            changed = true;
                        }
                        kept[p] = true;
                    }
                    _ => {
                        aot.remove(idx);
                        changed = true;
                        removed = true;
                    }
                }
            }
        }
        for (p, (_, _)) in EVENT_MAP.iter().enumerate() {
            if kept[p] {
                continue;
            }
            let mut t = Table::new();
            t.insert("event", value(EVENT_MAP[p].0));
            t.insert("command", value(self.hook_command(ctx, EVENT_MAP[p].0)));
            aot.push(t);
            changed = true;
        }

        if !changed {
            return Ok(());
        }
        // 内容级 CAS：读取后被并发修改就拒绝覆盖（与 jsonio::Doc 同口径）
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != raw {
            anyhow::bail!(
                "{} 在 agent-bark 读取后被其他程序修改过。为避免覆盖刚发生的改动，本次写入已中止——请重试一次。",
                path.display()
            );
        }
        if !raw.is_empty() {
            jsonio::backup_once(&path);
        }
        jsonio::write_text_atomic(&path, &doc.to_string())
    }

    fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()> {
        let path = self.config_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(());
        };
        let Ok(mut doc) = raw.parse::<DocumentMut>() else {
            // 解析失败就跳过：宁可留下我们的条目，也不破坏用户文件
            return Ok(());
        };
        let Some(aot) = doc.get_mut("hooks").and_then(|i| i.as_array_of_tables_mut()) else {
            return Ok(());
        };
        let before = aot.len();
        for idx in (0..aot.len()).rev() {
            if aot.get(idx).is_some_and(|t| entry_is_ours(t)) {
                aot.remove(idx);
            }
        }
        let changed = aot.len() != before;
        // 我们清空了整个数组（用户本来没有自己的 hooks）就把键也摘掉，
        // 还原成接入前的形态
        if changed && aot.is_empty() {
            doc.remove("hooks");
        }
        if changed && !ctx.dry_run {
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            if current != raw {
                anyhow::bail!(
                    "{} 在 agent-bark 读取后被其他程序修改过。为避免覆盖刚发生的改动，本次写入已中止——请重试一次。",
                    path.display()
                );
            }
            if ctx.backup {
                jsonio::backup_once(&path);
            }
            jsonio::write_text_atomic(&path, &doc.to_string())?;
        }
        Ok(())
    }

    fn verify(&self, ctx: &RegisterCtx) -> VerifyReport {
        let path = self.config_path();
        if !path.exists() {
            return VerifyReport::NotRegistered;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => return VerifyReport::ConfigUnreadable { path, reason: e.to_string() },
        };
        let doc: DocumentMut = match raw.parse() {
            Ok(d) => d,
            Err(e) => return VerifyReport::ConfigUnreadable { path, reason: e.to_string() },
        };
        let Some(aot) = doc.get("hooks").and_then(|i| i.as_array_of_tables()) else {
            return VerifyReport::NotRegistered;
        };
        let mut current_any = false;
        let mut stale_any = false;
        for t in aot.iter() {
            if !entry_is_ours(t) {
                continue;
            }
            let current = t
                .get("command")
                .and_then(|i| i.as_str())
                .is_some_and(|c| jsonio::command_exe_matches(c, &ctx.exe_path));
            if current {
                current_any = true;
            } else {
                stale_any = true;
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
        None
    }
}

#[cfg(test)]
impl KimiCodeAdapter {
    fn with_config(path: PathBuf) -> Self {
        Self { config_override: Some(path) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf as PB;

    fn ctx() -> RegisterCtx {
        RegisterCtx {
            exe_path: r"C:\Program Files\AgentBark\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        }
    }

    fn tmpdir() -> PB {
        let d = std::env::temp_dir().join(format!("agent-bark-kimi-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 写入 [[hooks]] 条目且**格式保留**：用户原有的凭据键、注释、空行原样不动
    #[test]
    fn register_preserves_user_format_and_comments() {
        let dir = tmpdir();
        let cfg = dir.join("config.toml");
        let original = r#"# 我的 Kimi 配置
default_profile = "managed:kimi-code"

# 注意：token 是假的
[providers."managed:kimi-code"]
api_key = "sk-test"
"#;
        std::fs::write(&cfg, original).unwrap();

        let adapter = KimiCodeAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();

        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("# 我的 Kimi 配置"), "用户注释必须保留");
        assert!(text.contains("api_key = \"sk-test\""), "用户凭据必须保留");
        assert!(text.contains("[[hooks]]"), "必须出现 [[hooks]] 数组表");
        assert!(text.contains("--agent kimi-code --event Stop"), "Stop 条目必须就位");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 幂等：重复注册内容不变、每个事件恰好一条我们的条目
    #[test]
    fn register_is_idempotent_and_one_entry_per_event() {
        let dir = tmpdir();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "").unwrap();
        let adapter = KimiCodeAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();
        let once = std::fs::read_to_string(&cfg).unwrap();
        adapter.register(&ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), once, "重复注册不得改变文件");

        let raw = std::fs::read_to_string(&cfg).unwrap();
        let ours = raw.lines().filter(|l| l.contains("--agent kimi-code")).count();
        assert_eq!(ours, EVENT_MAP.len(), "每个注册事件恰好一条条目");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// exe 漂移：verify 报 StalePath，register 就地修复且不新增条目
    #[test]
    fn verify_reports_stale_path_and_register_repairs_in_place() {
        let dir = tmpdir();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "").unwrap();
        let adapter = KimiCodeAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();

        let moved = RegisterCtx { exe_path: r"D:\new\AgentBark.exe".into(), port: 1, token: "t".into() };
        assert!(matches!(adapter.verify(&moved), VerifyReport::StalePath { .. }));
        adapter.register(&moved).unwrap();
        assert!(matches!(adapter.verify(&moved), VerifyReport::Ok));
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("C:/Program Files/AgentBark"), "旧路径必须被改写: {text}");
        assert_eq!(
            // 注意别用裸子串：`--event Stop` 会连 `--event StopFailure` 一起数进去；
            // 结尾引号兼容单引号（toml_edit 的 literal string 偏好）与双引号两种序列化
            text.matches("--event Stop'").count() + text.matches("--event Stop\"").count(),
            1,
            "就地修复不得新增条目"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 卸载：我们的条目全部摘除；清空后 hooks 键也摘掉；用户的键原样保留
    #[test]
    fn unregister_removes_ours_and_restores_shape() {
        let dir = tmpdir();
        let cfg = dir.join("config.toml");
        let original = "[providers.x]\napi_key = \"k\"\n";
        std::fs::write(&cfg, original).unwrap();
        let adapter = KimiCodeAdapter::with_config(cfg.clone());
        adapter.register(&ctx()).unwrap();

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: true, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("--agent kimi-code"), "dry_run 不得改文件");

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("agent-bark") && !text.contains("AgentBark"), "我们的条目必须清空: {text}");
        assert!(!text.contains("[[hooks]]"), "清空后 hooks 键应摘掉");
        assert_eq!(text, original, "应还原成接入前的形态");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非法 TOML 拒绝写入
    #[test]
    fn malformed_config_aborts_without_overwriting() {
        let dir = tmpdir();
        let cfg = dir.join("config.toml");
        let original = "not [valid toml";
        std::fs::write(&cfg, original).unwrap();
        let adapter = KimiCodeAdapter::with_config(cfg.clone());
        assert!(adapter.register(&ctx()).is_err());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "文件不能被改写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 事件归一化：Interrupt → 中止（多数宿主没有的显式取消信号）
    #[test]
    fn normalize_maps_interrupt_to_aborted() {
        let adapter = kimi_code();
        let ev = adapter
            .normalize("Interrupt", &serde_json::json!({ "session_id": "s", "cwd": r"D:\p" }))
            .expect("Interrupt 已注册");
        assert_eq!(ev.kind, EventKind::RunAborted);
        assert_eq!(ev.session_id, "s");
        // 未注册的事件名不产生事件
        assert_eq!(adapter.event_kind("SessionHeartbeat"), None);
        assert_eq!(adapter.event_kind("UserPromptQueued"), None);
    }

    /// 事件表 ⊆ 官方 20 事件（未知事件名有让宿主拒绝整份配置的风险，钉住）
    #[test]
    fn event_map_stays_within_official_event_list() {
        const OFFICIAL: [&str; 20] = [
            "UserPromptSubmit", "UserPromptQueued", "PreToolUse", "Stop", "TurnStarted",
            "PostToolUse", "PostToolUseFailure", "PermissionRequest", "PermissionResult",
            "SessionStart", "SessionEnd", "SessionHeartbeat", "SubagentStart", "SubagentStop",
            "TaskStarted", "StopFailure", "Interrupt", "PreCompact", "PostCompact", "Notification",
        ];
        for (event, _) in EVENT_MAP {
            assert!(OFFICIAL.contains(event), "注册了官方之外的事件: {event}");
        }
    }
}
