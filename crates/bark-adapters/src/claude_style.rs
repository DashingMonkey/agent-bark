//! Claude Code 风格配置的通用 hook 引擎。
//! Claude Code / TraeCode / CodeBuddy / Qoder（含国内版 Qoder CN）/ Codex 的 hooks
//! 均为 `{ "hooks": { "<Event>": [ { "hooks": [ { "type": "command", "command": "..." } ] } ] } }`
//! 的变体，仅事件名集合、配置路径、信任机制不同，全部复用本引擎。
//!
//! 写入原则（外科手术式）：
//! 1. 只添加/移除 command 中含 `agent-bark` 标记的**单个条目**，同组内用户自己的
//!    hook 必须原样保留，绝不整组删除；
//! 2. 配置文件存在但解析失败时**拒绝写入**（不能把用户配置替换成空对象）；
//! 3. 首次写入前备份为 `<file>.agent-bark.bak`；
//! 4. 目标配置目录不存在（agent 未安装/未初始化）时跳过，绝不凭空创建。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, InstallEnv, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::path::PathBuf;

/// 配置文件路径的来源。
/// 用枚举而不是 `fn` 指针，既支持按 home 推导，也支持测试里注入固定路径。
pub enum ConfigPaths {
    /// 按当前用户 home 推导（生产路径）
    FromHome(fn(&InstallEnv) -> Vec<PathBuf>),
    /// 固定列表（测试用）
    #[cfg(test)]
    Fixed(Vec<PathBuf>),
}

/// hook 条目在配置文件里的组织形态。
///
/// 两类宿主并存（官方文档逐字核对，见各 adapter 注释）：
/// - **嵌套**（Claude 系）：`hooks.<Event>[] = { hooks: [entry] }`，
///   entry 外还包一层组，组上可带 matcher 等字段；
/// - **扁平**（Cursor / Copilot CLI / Windsurf）：`hooks.<Event>[] = entry`，
///   数组元素就是 hook 条目本身。
#[derive(Clone, Copy)]
pub enum HookShape {
    Nested,
    /// `version = Some(true)` 时保证顶层有 `"version": 1` 键（Cursor / Copilot
    /// 缺它整份 hooks 不生效）；Windsurf 无 version 键，传 `false`。
    Flat { version: bool },
}

/// 标准命令条目：`{ "type": "command", "command": <cmd> }`（嵌套与 Cursor / Copilot 通用）
fn command_entry(command: &str) -> Value {
    json!({ "type": "command", "command": command })
}

/// Windsurf 条目：`{ "command": <cmd>, "show_output": false }`（无 type 字段；
/// show_output=false 不在 Cascade 面板里刷屏，与 AgentPet 实现一致）
fn windsurf_entry(command: &str) -> Value {
    json!({ "command": command, "show_output": false })
}

pub struct ClaudeStyleSpec {
    pub kind: AgentKind,
    /// 任一存在即视为 agent 已安装（通常是 agent 的配置目录）
    pub install_markers: Vec<PathBuf>,
    /// 要写入 hook 的配置文件（可能多个，如 TraeCode 国内/国际版）
    pub config_paths: ConfigPaths,
    /// agent 事件名 → 统一事件类型。**只能包含该 agent 确认支持的事件名**：
    /// 未知事件名可能导致 agent 丢弃整份 hooks 配置。
    pub event_map: &'static [(&'static str, EventKind)],
    pub trust_hint: Option<&'static str>,
    /// 条目组织形态（嵌套 / 扁平）
    pub shape: HookShape,
    /// 追加新条目时的构造函数（各扁平宿主的字段习惯不同）
    pub entry: fn(&str) -> Value,
    /// 配置文件所在目录不存在时，是否创建**最后一层**目录。
    ///
    /// 专用文件（`~/.copilot/hooks/agent-bark.json`、`~/.grok/hooks/agent-bark.json`）
    /// 的 `hooks/` 子目录宿主自己不会创建，不建就无法注册；
    /// 前提是 agent 根目录已存在（即 `install_markers` 命中的那个目录），
    /// 根目录都没有说明未安装，绝不凭空造整棵树。
    pub create_parent_dir: bool,
}

pub struct ClaudeStyleAdapter {
    pub spec: ClaudeStyleSpec,
}

impl ClaudeStyleAdapter {
    pub fn new(spec: ClaudeStyleSpec) -> Self {
        Self { spec }
    }

    /// hook 命令：token/port 不入命令，由子命令从配置文件读取，
    /// 避免 token 散落在各 agent 配置文件中。
    /// 路径用正斜杠 + 引号：cmd / PowerShell / Git Bash 都能解析。
    fn hook_command(&self, ctx: &RegisterCtx, event: &str) -> String {
        let exe = ctx.exe_path.replace('\\', "/");
        format!("\"{}\" hook --agent {} --event {}", exe, self.spec.kind.id(), event)
    }

    /// 该 adapter 实际要写的配置文件列表
    fn config_paths_for(&self) -> Vec<PathBuf> {
        match &self.spec.config_paths {
            ConfigPaths::FromHome(f) => f(&InstallEnv::detect()),
            #[cfg(test)]
            ConfigPaths::Fixed(v) => v.clone(),
        }
    }
}

/// 该组里我们的条目是否与当前 exe 一致。
/// 用与 is_our_entry 相同的精确解析：比较命令第一个 token 指向的可执行文件路径
/// （旧实现用 contains(exe_path)，`AgentBark.exe.old` 这类同前缀路径会被误判为一致）
fn group_has_current(group: &Value, ctx: &RegisterCtx) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| {
            hs.iter().any(|e| {
                jsonio::is_our_entry(e)
                    && e.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| jsonio::command_exe_matches(c, &ctx.exe_path))
            })
        })
}

/// 扁平条目是否指向当前可执行文件（嵌套版是 [`group_has_current`]）
fn entry_is_current(entry: &Value, ctx: &RegisterCtx) -> bool {
    entry
        .get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| jsonio::command_exe_matches(c, &ctx.exe_path))
}

/// 扁平形态（Cursor / Copilot CLI / Windsurf）的写入逻辑：`hooks.<Event>[]` 的
/// 数组元素就是条目本身，没有组包装。语义与嵌套路径一致：优先就地更新我们的
/// 条目（修 exe 漂移）、事件级去重（§1.9 同款）、用户条目原样保留、没有则追加。
/// 顶层 version 键由 `register` 主流程处理（它不在 hooks 子对象里）。
fn register_flat(
    adapter: &ClaudeStyleAdapter,
    hooks: &mut Value,
    ctx: &RegisterCtx,
    path: &std::path::Path,
) -> (bool, Option<anyhow::Error>) {
    let mut changed = false;

    let mut err: Option<anyhow::Error> = None;
    changed |= strip_retired_entries_flat(hooks);
    for event in adapter.hook_events() {
        let command = adapter.hook_command(ctx, event);
        let slot = hooks
            .as_object_mut()
            .expect("hooks 是对象")
            .entry(event)
            .or_insert_with(|| json!([]));
        if !slot.is_array() {
            err = Some(anyhow::anyhow!(
                "{} 的 hooks.{} 不是数组，已跳过",
                path.display(),
                event
            ));
            continue;
        }
        let arr = slot.as_array_mut().expect("已确认是数组");

        let pre_nulls: Vec<bool> = arr.iter().map(|e| e.is_null()).collect();
        let mut updated = false;
        let mut seen_ours = false;
        for entry in arr.iter_mut() {
            if !jsonio::is_our_entry(entry) {
                continue;
            }
            if seen_ours {
                // 历史遗留的重复条目：标记为删除（事件级去重，嵌套路径 §1.9 同款）
                *entry = json!(null);
                changed = true;
                continue;
            }
            seen_ours = true;
            if entry.get("command").and_then(|c| c.as_str()) != Some(command.as_str()) {
                entry["command"] = json!(command);
                changed = true;
            }
            updated = true;
        }
        // 只清我们造的 null 哨兵；用户数组里合法的 null 元素原样保留
        let mut i = 0usize;
        arr.retain(|e| {
            let ours = e.is_null() && !pre_nulls.get(i).copied().unwrap_or(true);
            i += 1;
            !ours
        });
        if !updated {
            arr.push((adapter.spec.entry)(&command));
            changed = true;
        }
    }
    (changed, err)
}

impl HookAdapter for ClaudeStyleAdapter {
    fn kind(&self) -> AgentKind {
        self.spec.kind
    }

    fn display_name(&self) -> &'static str {
        self.spec.kind.display_name()
    }

    fn is_installed(&self) -> bool {
        self.spec.install_markers.iter().any(|p| p.exists())
    }

    fn is_registered(&self) -> bool {
        self.config_paths_for()
            .iter()
            .filter(|p| p.exists())
            .any(|p| {
                jsonio::read_doc(p)
                    .map(|doc| match self.spec.shape {
                        HookShape::Nested => doc_has_active_entry(&doc),
                        HookShape::Flat { .. } => doc_has_active_entry_flat(&doc),
                    })
                    .unwrap_or(false)
            })
    }

    fn config_paths(&self) -> Vec<PathBuf> {
        self.config_paths_for()
    }

    fn hook_events(&self) -> Vec<&'static str> {
        self.spec.event_map.iter().map(|(name, _)| *name).collect()
    }

    fn event_kind(&self, event_name: &str) -> Option<EventKind> {
        self.spec
            .event_map
            .iter()
            .find(|(name, _)| *name == event_name)
            .map(|(_, kind)| *kind)
    }

    fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()> {
        let mut wrote_any = false;
        let mut last_err: Option<anyhow::Error> = None;

        for path in self.config_paths_for() {
            // 只写入其父目录已存在的配置（agent 已安装）。专用文件（Copilot / Grok 的
            // hooks/ 子目录）宿主自己不会创建，按 spec 允许补建**最后一层**——
            // 前提是 agent 根目录已存在，根目录都没有就是未安装，绝不凭空造整棵树。
            match path.parent() {
                Some(p) if p.exists() => {}
                Some(p) => {
                    let root_ok = p.parent().is_some_and(|r| r.exists());
                    if !(self.spec.create_parent_dir && root_ok) {
                        continue;
                    }
                    if let Err(e) = std::fs::create_dir_all(p) {
                        last_err =
                            Some(anyhow::Error::new(e).context(format!("创建 {} 失败", p.display())));
                        continue;
                    }
                }
                None => continue,
            }
            // 解析失败 → 跳过该路径继续写其余路径（多路径 adapter 如 TraeCode/Qoder，
            // 之前 `?` 直接中止会让「第一份已改、第二份没写」还没提示就退出）
            let mut doc = match jsonio::Doc::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let existed = path.exists();

            // 扁平宿主的顶层 version 键（Cursor / Copilot 要求，缺了整份 hooks 不生效）：
            // 它在**文档顶层**而不是 hooks 子对象里，必须在 hooks 提取之前写。
            // 已有（哪怕值不对）不动——那是用户的文件；verify 会把接入失效暴露出来
            let mut changed = false;
            if matches!(self.spec.shape, HookShape::Flat { version: true }) {
                let obj = doc.value.as_object_mut().expect("read_doc 保证顶层是对象");
                if !obj.contains_key("version") {
                    obj.insert("version".into(), json!(1));
                    changed = true;
                }
            }

            let hooks = doc
                .value
                .as_object_mut()
                .expect("read_doc 保证顶层是对象")
                .entry("hooks")
                .or_insert_with(|| json!({}));
            if !hooks.is_object() {
                last_err = Some(anyhow::anyhow!("{} 的 hooks 字段不是对象，已跳过", path.display()));
                continue;
            }

            // 扁平宿主（Cursor / Copilot CLI / Windsurf）：`hooks.<Event>[]` 的元素
            // 就是条目本身，没有组包装，走独立路径——语义与嵌套一致（只增改自己的
            // 条目、用户条目原样保留、事件级去重），落盘口径也相同
            if let HookShape::Flat { .. } = self.spec.shape {
                let (flat_changed, flat_err) = register_flat(self, hooks, ctx, &path);
                let changed = changed || flat_changed;
                if changed {
                    if existed {
                        jsonio::backup_once(&path);
                    }
                    // Doc::write 自带 CAS：文件被并发修改（宿主应用整份重写）时报错而非覆盖
                    jsonio::Doc::write(doc, &path)?;
                    wrote_any = true;
                }
                match flat_err {
                    // 结构性损坏且什么都没写成 → 交给尾部按嵌套路径同口径报错
                    Some(e) if !wrote_any => last_err = Some(e),
                    // 部分写入成功也要留痕，否则用户永远不知道有配置没写上
                    Some(e) => {
                        tracing::warn!("{} 部分配置文件被跳过：{e:#}", self.spec.kind.display_name())
                    }
                    None => {}
                }
                continue;
            }
            // 顺手清掉指向已下线 agent 的死条目（如删掉 qoder-cn-cli 后配置里遗留的
            // `--agent qoder-cn-cli` hook）：它们只会拉起一个立刻 exit 0 的进程。
            // 只认「我们的条目 + 已下线 id」，用户自己的 hook 一律不碰。
            changed |= strip_retired_entries(hooks);
            for event in self.hook_events() {
                let command = self.hook_command(ctx, event);
                let groups = hooks
                    .as_object_mut()
                    .expect("hooks 是对象")
                    .entry(event)
                    .or_insert_with(|| json!([]));
                if !groups.is_array() {
                    last_err = Some(anyhow::anyhow!("{} 的 hooks.{} 不是数组，已跳过", path.display(), event));
                    continue;
                }
                let arr = groups.as_array_mut().expect("已确认是数组");

                // 1) 优先就地更新我们的条目（修复 exe 路径漂移，且不碰用户条目）。
                //    去重是**事件级**（跨组，§1.9）：用户手工复制/旧版本并组写入形成
                //    `Stop: [{hooks:[我们的+用户的]}, {hooks:[我们的]}]` 时，两个「我们的
                //    条目」都会被就地修复保留 → 每个事件起两个 hook 进程、两条不同 uuid
                //    的事件绕过 daemon 的 id 去重 → 同一事件双触发、双通知。所以
                //    seen_ours 提升到 event 级，第二个起沿用「标记 null + retain 删除」。
                let mut updated = false;
                let mut seen_ours = false;
                for group in arr.iter_mut() {
                    let Some(entries) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                        continue;
                    };
                    // 清理时只删我们造的 null 哨兵；用户 hooks 数组里合法的
                    // null 元素原样保留（blanket retain 会误删）
                    let pre_nulls: Vec<bool> = entries.iter().map(|e| e.is_null()).collect();
                    for entry in entries.iter_mut() {
                        if !jsonio::is_our_entry(entry) {
                            continue;
                        }
                        if seen_ours {
                            // 历史遗留的重复条目：标记为删除
                            *entry = json!(null);
                            changed = true;
                            continue;
                        }
                        seen_ours = true;
                        if entry.get("command").and_then(|c| c.as_str()) != Some(command.as_str()) {
                            entry["command"] = json!(command);
                            changed = true;
                        }
                        updated = true;
                    }
                    let mut i = 0usize;
                    entries.retain(|e| {
                        let ours = e.is_null() && !pre_nulls.get(i).copied().unwrap_or(true);
                        i += 1;
                        !ours
                    });
                }

                // 2) 没有我们的条目 → 新增一个独立组（不并入用户的组）
                if !updated {
                    arr.push(json!({
                        "hooks": [ { "type": "command", "command": command } ]
                    }));
                    changed = true;
                }
            }

            if changed {
                if existed {
                    jsonio::backup_once(&path);
                }
                // Doc::write 自带 CAS：文件被并发修改（宿主应用整份重写）时报错而非覆盖
                jsonio::Doc::write(doc, &path)?;
                wrote_any = true;
            }
        }

        if let Some(e) = last_err {
            if wrote_any {
                // 部分配置写入成功、部分因结构性损坏被跳过：整体算 Ok，但必须留痕，
                // 否则调用方（和用户）永远不知道有配置没写上
                tracing::warn!("{} 部分配置文件被跳过：{e:#}", self.spec.kind.display_name());
            } else {
                return Err(e);
            }
        }
        if !wrote_any && !self.is_registered() {
            anyhow::bail!(
                "未找到可写入的配置（{} 可能未安装或未初始化）",
                self.spec.kind.display_name()
            );
        }
        Ok(())
    }

    fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()> {
        let mut last_err: Option<anyhow::Error> = None;
        for path in self.config_paths_for() {
            if !path.exists() {
                continue;
            }
            // 解析失败就跳过，宁可留着自己的条目，也不破坏用户文件
            let Ok(doc) = jsonio::Doc::read(&path) else {
                continue;
            };
            let mut doc = doc;
            let Some(hooks) = doc.value.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
                continue;
            };
            let mut changed = false;
            // 扁平宿主（Cursor / Copilot CLI / Windsurf）：数组元素即条目，独立清理
            // 路径——摘掉我们的条目，因此清空的事件键一并移除（与嵌套同口径）
            if let HookShape::Flat { .. } = self.spec.shape {
                let event_names: Vec<String> = hooks.keys().cloned().collect();
                for event in event_names {
                    let Some(arr) = hooks.get_mut(&event).and_then(|g| g.as_array_mut()) else {
                        continue;
                    };
                    let before = arr.len();
                    arr.retain(|e| !jsonio::is_our_entry(e));
                    if arr.len() != before {
                        changed = true;
                        if arr.is_empty() {
                            hooks.remove(&event);
                        }
                    }
                }
                // 与嵌套路径同口径：dry_run 只计算不落盘；backup=false 时不动最初备份
                if changed && !ctx.dry_run {
                    if ctx.backup {
                        jsonio::backup_once(&path);
                    }
                    if let Err(e) = jsonio::Doc::write(doc, &path) {
                        last_err = Some(e);
                    }
                }
                continue;
            }
            let event_names: Vec<String> = hooks.keys().cloned().collect();
            for event in event_names {
                let Some(groups) = hooks.get_mut(&event).and_then(|g| g.as_array_mut()) else {
                    continue;
                };
                // 条目级删除：只摘掉含 agent-bark 的 hook，组内用户条目保留
                let mut removed_here = false;
                for group in groups.iter_mut() {
                    if let Some(entries) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        let before = entries.len();
                        entries.retain(|e| !jsonio::is_our_entry(e));
                        if entries.len() != before {
                            changed = true;
                            removed_here = true;
                        }
                    }
                }
                // 只有这一轮真的删过我们的条目才顺手清空壳：用户（或第三方工具）
                // 预存的空组 / 空事件键（如 {"Stop": []}）不属于我们，不碰
                if removed_here {
                    let before = groups.len();
                    groups.retain(|g| {
                        !g.get("hooks")
                            .and_then(|h| h.as_array())
                            .is_some_and(|hs| hs.is_empty())
                    });
                    if groups.len() != before {
                        changed = true;
                    }
                    if groups.is_empty() {
                        hooks.remove(&event);
                    }
                }
            }
            // 尊重 ctx 语义：dry_run 只计算不落盘；backup=false 时不动最初备份。
            // Doc::write 自带 CAS；单路径失败不中断其余路径的清理，最后统一报错
            if changed && !ctx.dry_run {
                if ctx.backup {
                    jsonio::backup_once(&path);
                }
                if let Err(e) = jsonio::Doc::write(doc, &path) {
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        Ok(())
    }

    fn verify(&self, ctx: &RegisterCtx) -> VerifyReport {
        let mut current_any = false;
        let mut stale_any = false;
        let mut first_stale: Option<PathBuf> = None;
        for path in self.config_paths_for() {
            if !path.exists() {
                continue;
            }
            let doc = match jsonio::read_doc(&path) {
                Ok(d) => d,
                Err(e) => return VerifyReport::ConfigUnreadable { path: path.clone(), reason: e.to_string() },
            };
            let Some(hooks) = doc.get("hooks").and_then(|h| h.as_object()) else {
                continue;
            };
            // 扁平宿主：hooks.<Event>[] 的元素本身就是条目
            if let HookShape::Flat { .. } = self.spec.shape {
                for entries in hooks.values() {
                    let Some(entries) = entries.as_array() else { continue };
                    for entry in entries {
                        // 收集完**所有事件**再判定（与嵌套路径 §2.18g 同理）：
                        // 「旧路径残留 + 当前路径并存」是有效接入，不得误报 StalePath
                        if jsonio::is_our_entry(entry) && !is_retired_entry(entry) {
                            if entry_is_current(entry, ctx) {
                                current_any = true;
                            } else {
                                stale_any = true;
                                first_stale.get_or_insert_with(|| path.clone());
                            }
                        }
                    }
                }
                continue;
            }
            for groups in hooks.values() {
                let Some(groups) = groups.as_array() else { continue };
                for group in groups {
                    // 只认有效的条目：指向已下线 agent 的旧条目（如合并前的
                    // `--agent qoder-cn`）算「没接入」，UI 会提示点开关重写。
                    // 收集完**所有组**再判定（§2.18g）：多组场景下不能在首个漂移组
                    // 提前 return——「一组旧路径残留 + 一组当前路径」是有效接入，
                    // 早退会误报 StalePath（多路径 adapter 如 TraeCode/Qoder 同理）
                    if group_has_active_ours(group) {
                        if group_has_current(group, ctx) {
                            current_any = true;
                        } else {
                            stale_any = true;
                            first_stale.get_or_insert_with(|| path.clone());
                        }
                    }
                }
            }
        }
        // 有当前路径的有效条目即算已接入（残留的旧路径条目由下一次 register 就地
        // 修复/去重）；只有全部漂移才报 StalePath
        if current_any {
            VerifyReport::Ok
        } else if stale_any {
            VerifyReport::StalePath { path: first_stale.expect("stale_any 时必有路径") }
        } else {
            VerifyReport::NotRegistered
        }
    }

    fn normalize(&self, event_name: &str, raw: &Value) -> Option<NormalizedEvent> {
        let kind = self.event_kind(event_name)?;
        // 通知类事件里显式声明「这是权限请求」时按 PermissionRequired 归一。
        // 依据 2026-09 实测：Qoder（桌面应用 v0.2.3 / IDE / CLI 三个入口一致）的
        // 「等你授权」走 Notification + notification_type="permission_prompt"
        // （载荷形如 {"notification_type":"permission_prompt",
        //            "message":"Tool Bash requires confirmation"}），
        // 而它注册的 PermissionRequest 一次都没触发过。
        // 不归一的话通知标题会是「等待输入」、会话相位记成 WaitingInput，
        // 与真实情况（等人点授权）不符。
        let kind = if kind == EventKind::InputRequired && is_permission_prompt(raw) {
            EventKind::PermissionRequired
        } else {
            kind
        };
        Some(NormalizedEvent::from_raw(self.spec.kind.id(), kind, raw))
    }

    fn trust_hint(&self) -> Option<&'static str> {
        self.spec.trust_hint
    }
}

/// 一组 hook 里是否有**有效**的我们的条目：指向已下线 agent 的旧条目不算。
///
/// 那种条目（如合并前的 `--agent qoder-cn`）对应的 id 已被 bark-cli 判为未知、
/// 只会拉起一个立刻 exit 0 的进程空转。不把它算作「已接入」才能让 UI 显示成
/// 「hook 已丢失」，用户点一下开关即重写成当前 id。
fn group_has_active_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| hs.iter().any(|e| jsonio::is_our_entry(e) && !is_retired_entry(e)))
}

/// 文档里是否存在有效的我们的条目（见 group_has_active_ours）
fn doc_has_active_entry(doc: &Value) -> bool {
    doc.get("hooks")
        .and_then(|h| h.as_object())
        .is_some_and(|events| {
            events.values().any(|groups| {
                groups
                    .as_array()
                    .is_some_and(|gs| gs.iter().any(group_has_active_ours))
            })
        })
}

/// 扁平形态版：hooks.<Event>[] 的元素本身就是条目
fn doc_has_active_entry_flat(doc: &Value) -> bool {
    doc.get("hooks")
        .and_then(|h| h.as_object())
        .is_some_and(|events| {
            events.values().any(|entries| {
                entries
                    .as_array()
                    .is_some_and(|es| es.iter().any(|e| jsonio::is_our_entry(e) && !is_retired_entry(e)))
            })
        })
}

/// 扁平形态版：清掉指向已下线 agent 的条目（含因此变空的事件键）。返回是否有改动。
fn strip_retired_entries_flat(hooks: &mut Value) -> bool {
    let Some(events) = hooks.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    let keys: Vec<String> = events.keys().cloned().collect();
    for key in keys {
        let Some(arr) = events.get_mut(&key).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let before = arr.len();
        arr.retain(|e| !is_retired_entry(e));
        if arr.len() != before {
            changed = true;
            if arr.is_empty() {
                events.remove(&key);
            }
        }
    }
    changed
}

/// 通知型事件是否在声明「这是权限请求」（各 agent 的实测取值，见 normalize）。
///
/// 大小写不敏感的 `contains("permission")`（§2.18h）：旧实现精确等值
/// `"permission_prompt"`，各版本/各 agent 的变体（`Permission_Prompt`、
/// `permission-request`……）会退化成 InputRequired——通知标题写「等待输入」、
/// 相位记成 WaitingInput，与真实情况（等人点授权）不符。
fn is_permission_prompt(raw: &Value) -> bool {
    raw.get("notification_type")
        .and_then(|v| v.as_str())
        .is_some_and(|v| v.to_ascii_lowercase().contains("permission"))
}

/// 清掉 hooks 里指向已下线 agent 的 agent-bark 条目（含因此变空的组与事件键）。
/// 返回是否有改动。
fn strip_retired_entries(hooks: &mut Value) -> bool {
    let Some(events) = hooks.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for groups in events.values_mut() {
        let Some(arr) = groups.as_array_mut() else { continue };
        for group in arr.iter_mut() {
            let Some(entries) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                continue;
            };
            let before = entries.len();
            entries.retain(|e| !is_retired_entry(e));
            if entries.len() != before {
                changed = true;
            }
        }
        // 组变空才删组（用户条目所在组保持不变）
        let before = arr.len();
        arr.retain(|g| {
            !g.get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hs| hs.is_empty())
        });
        if arr.len() != before {
            changed = true;
        }
    }
    let emptied: Vec<String> = events
        .iter()
        .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
        .map(|(k, _)| k.clone())
        .collect();
    for key in emptied {
        events.remove(&key);
        changed = true;
    }
    changed
}

/// 是否是指向已下线 agent 的旧条目（如 `... hook --agent qoder-cn-cli ...`）。
/// 空格写法（`--agent <id>`）与等值写法（`--agent=<id>`）都要认（§2.18h）：
/// 只认前者会让等值写法的死条目清不掉、永远空转触发。
fn is_retired_entry(entry: &Value) -> bool {
    if !jsonio::is_our_entry(entry) {
        return false;
    }
    let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    let mut tokens = cmd.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "--agent" {
            return tokens
                .next()
                .is_some_and(|id| bark_core::RETIRED_AGENT_IDS.contains(&id));
        }
        if let Some(id) = tok.strip_prefix("--agent=") {
            return bark_core::RETIRED_AGENT_IDS.contains(&id);
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 各 agent 的具体规格
// ---------------------------------------------------------------------------

fn home(env: &InstallEnv) -> PathBuf {
    env.home.clone()
}

/// Claude Code：~/.claude/settings.json
///
/// 事件表按官方 hooks 文档（code.claude.com/docs/en/hooks，33 个事件）取子集。
/// **本机未安装 Claude Code，以下为文档映射、未实测**；有官方文档背书的取舍：
/// - `UserPromptSubmit` / `PreToolUse` → Activity 心跳：hooks 最初版本就有的两个
///   核心事件（payload 带 `prompt` / `tool_name`）。没有它们，Claude Code 会话
///   只有 Notification / 失败时才可见，「运行中」列表与流光思考色基本失效。
/// - `Stop` → RunCompleted。官方明确：**用户中断（Esc）时不触发**——所以中断的
///   回合收不到完成信号，这正是「手动中止卡黄到判死」的根因，靠下面的
///   SessionEnd 释放。
/// - `StopFailure` → RunFailed：回合因 API 错误结束（限流/超载/鉴权失败等），
///   这是 Claude Code 唯一的「回合真失败」信号。较新版本才有（文档标注部分
///   错误类型需 v2.1.267+）；若旧版本对未知事件键严格校验会丢整份 hooks——
///   概率低（hooks 加载按已知事件查表，未知键通常 inert），真遇到就关掉开关
///   回退，事件流可观测。
/// - `SessionEnd` → RunAborted：会话级结束信号，`reason` ∈ clear / logout /
///   prompt_input_exit / other。**这是中止释放的关键**：中断回合后退出会话
///   （prompt_input_exit）就靠它把「运行中」条目释放掉；正常回合 Stop 之后
///   紧跟的 SessionEnd 落在终态回声宽限窗（10s）内被抑制，更晚的退出会记一条
///   「已中止」——会话确实结束了，语义成立（中止不通知，无噪音）。
/// - `PostToolUse` → ToolFinished（工具收尾，官方文档的事件子集内）：**等待状态
///   解除的唯一信号**——答完 AskUserQuestion / 批完权限后到下一个 PreToolUse 之间
///   没有别的事件，不收它的话相位会一直卡在「等待中/执行工具」、等待色亮到下一个
///   工具开始（§1.11，与 zcode.rs 的 EVENTS 表同口径）。
/// - `PostToolUseFailure` → ToolFailed（工具级失败，官方确认存在）。
pub fn claude_code() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("SessionStart", EventKind::SessionStart),
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("Notification", EventKind::InputRequired),
        ("PostToolUseFailure", EventKind::ToolFailed),
        ("Stop", EventKind::RunCompleted),
        ("StopFailure", EventKind::RunFailed),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::ClaudeCode,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".claude")],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".claude").join("settings.json")]),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// TraeCode：国内版 ~/.trae-cn/hooks.json，国际版 ~/.trae/hooks.json（存在则都写）
/// 事件只有 6 个，且**没有失败事件**；`Notification` 是异步的、忽略 stdout 与退出码，
/// 是最安全的接入点。
/// UserPromptSubmit / PreToolUse 映射为 Activity 心跳（官方文档确认支持），
/// 用于推导会话实时状态（思考中 / 执行工具）；PreToolUse 为阻塞型 hook，
/// 子命令永远退出 0、stdout 不输出，即恒为放行，不影响 agent。
///
/// **手动中止无信号**（官方文档 docs.trae.cn/ide_hook-configuration-reference，2026-09 核对）：
/// 官方事件表就这 6 个（SessionStart / UserPromptSubmit / PreToolUse / PostToolUse /
/// Stop / Notification），**没有 SessionEnd，也没有任何中断事件**；Stop 只在
/// 「智能体完成输出、准备结束当前查询时」触发。所以中断的回合收不到任何事件，
/// 只能等下一条事件（继续对话的 UserPromptSubmit 会刷新会话）或 10 分钟判死——
/// 这是官方事件表的能力边界，不是遗漏；若后续版本加了 SessionEnd 再补映射。
pub fn trae_code() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("SessionStart", EventKind::SessionStart),
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        // PostToolUse 在官方 6 事件表内（上面注释自列）：工具收尾是等待状态解除的
        // 唯一信号（§1.11）——答完问题/批完权限后不收它，相位卡「等待中」到下个工具
        ("PostToolUse", EventKind::ToolFinished),
        ("Stop", EventKind::RunCompleted),
        ("Notification", EventKind::InputRequired),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::TraeCode,
        install_markers: vec![
            dirs::home_dir().unwrap_or_default().join(".trae-cn"),
            dirs::home_dir().unwrap_or_default().join(".trae"),
        ],
        config_paths: ConfigPaths::FromHome(|env| {
            vec![
                home(env).join(".trae-cn").join("hooks.json"),
                home(env).join(".trae").join("hooks.json"),
            ]
        }),
        event_map: MAP,
        // TraeCode 需在 IDE 设置 > Hooks 中确认外部修改已启用（无程序化绕过）
        trust_hint: Some("TraeCode 首次使用需在 设置 > Hooks 中确认配置已启用（安全警示面板点击启用）"),
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// CodeBuddy：~/.codebuddy/settings.json
///
/// 事件表按官方 CLI 文档（codebuddy.ai/docs/zh/cli/hooks，CodeBuddy Code CLI
/// v1.16.0+，Beta）取子集——该文档是 Claude Code hooks 的近逐字镜像，27+ 事件，
/// 含 `SessionEnd`（reason: clear / logout / prompt_input_exit / other）与
/// `StopFailure`（回合因 API 错误结束）。此前这里只写 3 个事件是囿于「官方
/// 事件表未公开」；现在有文档了，按 Claude Code 同款口径补齐（**未实测**，
/// 但本机装有 CodeBuddy，可直接验证）：
/// - `Stop` 官方同样明确「用户中断时不触发」→ 中止回合靠 `SessionEnd` 释放。
/// - `UserPromptSubmit` / `PreToolUse` → Activity 心跳（此前会话只在等待/失败时可见）。
/// - `PostToolUseFailure` → ToolFailed（工具级失败）。
/// CodeBuddy 的安装标记（2026-09 收紧）。**不能**用 `~/.codebuddy` 目录本身：
/// WorkBuddy（同厂产品，内嵌 CodeBuddy CLI 内核）只装 WorkBuddy 也会创建该目录——
/// 实测 WorkBuddy-only 机器（注册表无 CodeBuddy、PATH 无 codebuddy）上，目录里只有
/// WorkBuddy 自己写的 code-ratio/、diagnostics/、logs/memwatch/（CliMemWatch 日志），
/// 且 `%LOCALAPPDATA%\CodeBuddyExtension` 与 WorkBuddy 首次运行同秒出现；官方文档
/// （codebuddy.ai/docs/zh/cli/installation）也把「与 WorkBuddy 共存」列为
/// CODEBUDDY_CONFIG_DIR 重定向的使用场景，等于承认两者共用 `~/.codebuddy`。
/// 这里只认 WorkBuddy 不会创建的真实 CodeBuddy 足迹：
/// - 配置目录内官方文档列出的三件套（CLI 至少跑过一次才生成，npm 安装亦然）：
///   `~/.codebuddy/settings.json` / `.mcp.json` / `skills/`
/// - 原生二进制安装：Windows `%LOCALAPPDATA%\codebuddy\bin`、macOS/Linux `~/.local/bin/codebuddy`
fn codebuddy_install_markers() -> Vec<PathBuf> {
    let mut markers: Vec<PathBuf> = ["settings.json", ".mcp.json", "skills"]
        .iter()
        .map(|f| dirs::home_dir().unwrap_or_default().join(".codebuddy").join(f))
        .collect();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        markers.push(PathBuf::from(local).join("codebuddy").join("bin"));
    }
    markers.push(dirs::home_dir().unwrap_or_default().join(".local/bin/codebuddy"));
    markers
}
pub fn codebuddy() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("SessionStart", EventKind::SessionStart),
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("Notification", EventKind::InputRequired),
        ("PostToolUseFailure", EventKind::ToolFailed),
        ("Stop", EventKind::RunCompleted),
        ("StopFailure", EventKind::RunFailed),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::CodeBuddy,
        install_markers: codebuddy_install_markers(),
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".codebuddy").join("settings.json")]),
        event_map: MAP,
        // CodeBuddy 外部修改 hooks 需在 /hooks 面板审核后生效
        trust_hint: Some("CodeBuddy 需在会话内运行 /hooks 面板，审核外部修改的 hooks 后生效"),
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// Qoder（国际版 + 国内版 Qoder CN，**一条 adapter 覆盖两个版本**，与 TraeCode 同样式）：
/// - 国际版：`~/.qoder/settings.json`
/// - 国内版：`~/.qoder-cn/settings.json`
///
/// 存在哪个写哪个，都存在则都写；安装标记同理看这两个目录（一般人不会同时装两个版本）。
///
/// **2026-09 探针实测**（见仓库外 qoder-hook-probe）：
/// - 国际版：桌面应用（Qoder v0.2.3）、IDE、CLI 三个入口**共用** `~/.qoder/settings.json`，
///   应用重写该文件时会保留我们写入的 hooks；官方文档
///   https://docs.qoder.com/extensions/hooks 也写「Config files are shared between the
///   IDE and the CLI」。
/// - 国内版：**实测读 `~/.qoder-cn/settings.json`**（`QODER_HOOK_SOURCE=ide`、协议 1.29.0），
///   ⚠️ 而官方帮助文档至今仍写 `~/.lingma/settings.json` —— 那是灵码时代「VS Code 插件线」
///   的家（埋在那儿的探针一次都没被调起；CN IDE 自带引擎里也搜不到任何 `.lingma` 文件路径）。
///   别照文档改回去。
/// - ⚠️ 国内版的**桌面应用**启动时会**整份重写** `~/.qoder-cn/settings.json`（连 IDE 自己的
///   插件键都一起冲掉），我们写的 hooks 会被清掉。UI 侧已把「配置开着但 hook 不在」显示成
///   开关关闭，用户点一下即重新写入（见 AgentsPage 的 isOn / actionHint）。
///
/// 事件表取**两个版本都支持的交集**，安全且无功能损失：
/// - `UserPromptSubmit` / `PreToolUse` → Activity 心跳（会话实时状态：思考中 / 执行工具）
/// - `Notification` → InputRequired；payload 带 `notification_type="permission_prompt"` 时
///   由 `normalize` 升级成 PermissionRequired → 等待色「需要确认」（两版实测一致）
/// - `PostToolUseFailure` → ToolFailed（工具级失败，agent 会自行重试，不亮失败色不通知）
/// - `Stop` → RunCompleted（payload 的 `last_assistant_message` 是助手回复全文，
///   已被 extract_message 优先取作通知正文）
/// - `SessionEnd` → **RunAborted**：**用户主动中断回合时只有它、没有 Stop**（实测），
///   不注册的话那条会话会永远留在「运行中」列表、流光一直停在思考色。按「中止」归一
///   而不是「失败」：用户自己按的停止不该亮失败色、更不该弹「任务失败」；正常回合
///   Stop 之后紧跟的那条会被状态机的「终态回声」抑制，不重复通知、也不压掉完成色。
///
/// 没注册的两个（都是交集之外、且实测无功能损失）：
/// - `SessionStart`：在本仓库是空操作（不通知、不建档，见 state.rs 的 apply_session_event）
/// - `PermissionRequest`：国际版实测**从不触发**（授权等待走 Notification），CN 文档也没列它
pub fn qoder() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("Notification", EventKind::InputRequired),
        ("PostToolUseFailure", EventKind::ToolFailed),
        ("Stop", EventKind::RunCompleted),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Qoder,
        install_markers: vec![
            dirs::home_dir().unwrap_or_default().join(".qoder"),
            dirs::home_dir().unwrap_or_default().join(".qoder-cn"),
        ],
        config_paths: ConfigPaths::FromHome(|env| {
            vec![
                home(env).join(".qoder").join("settings.json"),
                home(env).join(".qoder-cn").join("settings.json"),
            ]
        }),
        event_map: MAP,
        // 三个入口（IDE / 桌面应用 / CLI）都只在启动时加载 hooks，所以是"退出并重启"
        trust_hint: Some("Qoder 修改配置后需完全退出并重启对应应用（IDE / 桌面应用 / CLI）生效"),
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// Codex：~/.codex/hooks.json
/// 已确认的事件只有 SessionStart / Stop / PermissionRequest；写入后需在 /hooks 里信任。
///
/// **手动中止无信号**（2026-09 核对官方与社区资料）：Codex 的 hooks 仍是 beta，
/// 事件表只有 `SessionStart` / `Stop` / `agent-turn-complete` 三个（另有旧的
/// `notify` 机制），没有 SessionEnd、没有带结束原因的载荷、也没有中断事件；
/// 社区还报过 hooks 在交互式会话里不触发的问题（openai/codex#17532）。
/// 中断的回合同样只能靠下一条事件或判死；官方补事件后再接。
pub fn codex() -> ClaudeStyleAdapter {
    // TODO（探针待钉死，审查 §2.13 / 待确认 #1）：Codex 真实回调究竟是 `Stop` 还是
    // `agent-turn-complete` 尚未实机确认——上面的注释自述真实事件表是
    // SessionStart / Stop / agent-turn-complete，而社区反馈 hooks 在交互式会话里
    // 可能不触发（openai/codex#17532）。若真实回调是 `agent-turn-complete`，这里
    // 注册的 Stop 永远收不到、回合只能挂到 10 分钟判死。按「待确认不改行为」处理：
    // 本 MAP 暂不动，用 doc/agent-integration.md §13 的探针验证法拿到实机样本后
    // 钉死这张表（届时再决定是否把 Stop 换成/加上 agent-turn-complete）。
    const MAP: &[(&str, EventKind)] = &[
        ("SessionStart", EventKind::SessionStart),
        ("Stop", EventKind::RunCompleted),
        ("PermissionRequest", EventKind::PermissionRequired),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Codex,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".codex")],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".codex").join("hooks.json")]),
        event_map: MAP,
        trust_hint: Some("Codex 首次注册后需在会话内运行 /hooks 完成信任确认"),
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// Gemini CLI：~/.gemini/settings.json（Claude 嵌套变体）。
///
/// 事件表按官方 hooks 文档（google-gemini/gemini-cli docs/hooks，11 个事件）取
/// 子集，并与 AgentPet 的实现交叉验证一致（**未实测**）。取舍：
/// - `BeforeAgent` → Activity 心跳（回合开始，等价 Claude 的 UserPromptSubmit）；
///   `BeforeTool` → Activity（工具开始）。
/// - `AfterTool` → ToolFinished：等待状态解除的唯一信号（§1.11 同口径）。
/// - `Notification` → InputRequired（payload 带 notification_type/message，
///   permission 字样时由 normalize 升级为 PermissionRequired）。
/// - `AfterAgent` → RunCompleted（agent 完成回答）；`SessionEnd` → RunAborted
///   （reason: exit / clear / logout / prompt_input_exit / other，会话级释放）。
/// - 没注册：SessionStart（本仓库空操作）、BeforeModel / AfterModel /
///   BeforeToolSelection（模型层心跳，BeforeTool 已覆盖工具期，多注册只多空转
///   hook 进程）、PreCompress（压缩信号无通知语义）。
/// - 安装标记不能用 `~/.gemini` 目录本身：Antigravity（同家产品）也会创建它
///   （其配置在 `~/.gemini/config/`），只装 Antigravity 会被误判为装了 Gemini CLI。
pub fn gemini_cli() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("BeforeAgent", EventKind::Activity),
        ("BeforeTool", EventKind::Activity),
        ("AfterTool", EventKind::ToolFinished),
        ("Notification", EventKind::InputRequired),
        ("AfterAgent", EventKind::RunCompleted),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::GeminiCli,
        install_markers: vec![
            dirs::home_dir().unwrap_or_default().join(".gemini/settings.json"),
            dirs::home_dir().unwrap_or_default().join(".gemini/tmp"),
        ],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".gemini").join("settings.json")]),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// Qwen Code：~/.qwen/settings.json（Gemini CLI 的 fork，但事件名已转向 Claude 风格）。
///
/// 事件表按官方文档（qwenlm.github.io/qwen-code-docs hooks，22 个事件）取子集
/// （**未实测**）。映射与 Claude Code 同口径：
/// - `UserPromptSubmit` / `PreToolUse` → Activity 心跳；`PostToolUse` → ToolFinished。
/// - `PostToolUseFailure` → ToolFailed（工具级失败，agent 自行重试）。
/// - `Notification` → InputRequired；`PermissionRequest` → PermissionRequired
///   （官方 22 事件表内有；是否真的触发未实测，有 Notification 兜底）。
/// - `Stop` → RunCompleted；`StopFailure` → RunFailed；`SessionEnd` → RunAborted
///   （payload 带 reason，会话级释放；用户中断是否触发 Stop 未实测，判死兜底）。
/// - payload 为 snake_case 且带 `permission_mode`，与 Claude 系同构。
pub fn qwen_code() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("PostToolUseFailure", EventKind::ToolFailed),
        ("Notification", EventKind::InputRequired),
        ("PermissionRequest", EventKind::PermissionRequired),
        ("Stop", EventKind::RunCompleted),
        ("StopFailure", EventKind::RunFailed),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::QwenCode,
        install_markers: vec![
            dirs::home_dir().unwrap_or_default().join(".qwen/settings.json"),
            dirs::home_dir().unwrap_or_default().join(".qwen/projects"),
        ],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".qwen").join("settings.json")]),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// Factory Droid：~/.factory/hooks.json（官方文档 docs.factory.ai/docs/harness/hooks，
/// 9 个事件，Claude 同构嵌套 + snake_case payload；AgentPet 实现与官方一致）。
///
/// 映射（**未实测**）：`UserPromptSubmit` / `PreToolUse` → Activity；
/// `PostToolUse` → ToolFinished（等待解除的唯一信号）；`Notification` →
/// InputRequired——AgentPet 注释明确「Notification 是权限/审批请求或 60s 空闲」，
/// permission 字样时升级为 PermissionRequired；`Stop` → RunCompleted；
/// `SessionEnd` → RunAborted（会话级释放）。官方事件表里没有 StopFailure。
/// trust：Droid 启动时快照 hooks，外部修改需在 /hooks 面板审核。
pub fn droid() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("Notification", EventKind::InputRequired),
        ("Stop", EventKind::RunCompleted),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Droid,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".factory")],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".factory").join("hooks.json")]),
        event_map: MAP,
        trust_hint: Some("Droid 启动时快照 hooks，外部修改需在会话内 /hooks 面板审核确认"),
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// xAI Grok CLI（Grok Build）：~/.grok/hooks/agent-bark.json（**专用文件**，
/// 该目录下每个 *.json 都是一份 hooks，独占自己的文件名即不碰用户条目）。
///
/// 事件表按官方文档（docs.x.ai / grok-build 仓库用户手册，15 个事件）取子集
/// （**未实测**）；AgentPet 的实现与官方 payload 描述逐字一致：
/// - payload 是 **camelCase 键**（`sessionId` / `toolName`），且 `hookEventName`
///   携带 snake_case 值——会话 id / 工具名靠 from_raw 的别名兜底，事件名本身走
///   `--event` 参数，不受影响。
/// - `StopCancelled` → RunAborted：**显式的用户取消信号**，比「Stop 不触发中断」
///   的 Claude 系干净得多；`SessionEnd` → RunAborted（会话级释放）。
/// - `Stop` → RunCompleted；`StopFailure` → RunFailed；`PostToolUseFailure` →
///   ToolFailed；`Notification` → InputRequired。
/// - 没注册：SessionStart（空操作）、SubagentStart / SubagentStop（子代理噪音，
///   管道会滤掉但白起进程）、PermissionDenied（已被拒，不是等待）、PreCompact /
///   PostCompact（无通知语义）。
/// - PreToolUse 有语义风险（官方：PreToolUse hook exit 2 会拒绝该工具），本子命令
///   恒退出 0 + stdout 不输出，恒为放行。
pub fn grok() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("UserPromptSubmit", EventKind::Activity),
        ("PreToolUse", EventKind::Activity),
        ("PostToolUse", EventKind::ToolFinished),
        ("PostToolUseFailure", EventKind::ToolFailed),
        ("Notification", EventKind::InputRequired),
        ("Stop", EventKind::RunCompleted),
        ("StopFailure", EventKind::RunFailed),
        ("StopCancelled", EventKind::RunAborted),
        ("SessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Grok,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".grok")],
        config_paths: ConfigPaths::FromHome(|env| {
            vec![home(env).join(".grok").join("hooks").join("agent-bark.json")]
        }),
        event_map: MAP,
        // 官方仅项目级 hooks 需要 /hooks-trust；用户级 ~/.grok/hooks 直接加载
        trust_hint: None,
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: true,
    })
}

/// Cursor：~/.cursor/hooks.json（扁平结构 + 顶层 version:1，camelCase 事件，
/// 官方文档 cursor.com/docs/agent/hooks；AgentPet 实现与官方逐字一致）。
///
/// 官方 18 个 agent 事件里取子集（**未实测**）：
/// - `beforeSubmitPrompt` → Activity（回合开始）；`preToolUse` → Activity。
/// - `postToolUse` → ToolFinished（等待解除的唯一信号，§1.11）。
/// - `stop` → RunCompleted；`sessionEnd` → RunAborted（会话级释放，
///   stop 后紧跟的 sessionEnd 由终态回声抑制吞掉）。
/// - 没注册：sessionStart（空操作）、subagentStart / subagentStop（子代理噪音）、
///   beforeShellExecution 等细分事件（preToolUse 已覆盖，多注册只多空转进程）。
/// - payload 用 `conversation_id` + `workspace_roots`（数组），由 from_raw 别名兜底。
/// - 无等待类事件（官方没有 Notification 类钩子）——Cursor 只有运行/完成两态，
///   权限等待退化为事件静止 + 判死。
/// - 覆盖 IDE 端；Cursor CLI（cursor-agent）对 hooks 的支持官方未文档化，不承诺。
pub fn cursor() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("beforeSubmitPrompt", EventKind::Activity),
        ("preToolUse", EventKind::Activity),
        ("postToolUse", EventKind::ToolFinished),
        ("stop", EventKind::RunCompleted),
        ("sessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Cursor,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".cursor")],
        config_paths: ConfigPaths::FromHome(|env| vec![home(env).join(".cursor").join("hooks.json")]),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Flat { version: true },
        entry: command_entry,
        create_parent_dir: false,
    })
}

/// GitHub Copilot CLI：~/.copilot/hooks/agent-bark.json（**专用文件**——官方按
/// `~/.copilot/hooks/*.json` 逐文件加载，独占文件名即不碰用户条目）。
///
/// ⚠️ 事件名与 AgentPet 的实现不同：官方 CLI 文档（docs.github.com copilot
/// reference/hooks-reference）的主事件集是 **camelCase**——`agentStop`（不是
/// `Stop`）、`userPromptSubmitted`（不是 `UserPromptSubmit`）；PascalCase 集是
/// VS Code 兼容层。按官方 CLI 文档为准（**未实测**，若实机收到 PascalCase 回调
/// 再切换映射表）：
/// - `userPromptSubmitted` → Activity；`postToolUse` → ToolFinished；
///   `postToolUseFailure` → ToolFailed。
/// - `notification` → InputRequired；`permissionRequest` → PermissionRequired。
/// - `agentStop` → RunCompleted；`sessionEnd` → RunAborted（会话级释放）。
/// - **故意不注册 preToolUse**：Copilot 的 command hook 是 fail-closed（hook 出错
///   会阻塞用户工具），少一个高风险挂载点（AgentPet 同样省略，注释同因）；
///   本子命令虽恒退出 0，但纵深防御。camelCase 事件的 payload 是 camelCase
///   （`sessionId` / `toolName`），由 from_raw 别名兜底。
pub fn copilot_cli() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("userPromptSubmitted", EventKind::Activity),
        ("postToolUse", EventKind::ToolFinished),
        ("postToolUseFailure", EventKind::ToolFailed),
        ("notification", EventKind::InputRequired),
        ("permissionRequest", EventKind::PermissionRequired),
        ("agentStop", EventKind::RunCompleted),
        ("sessionEnd", EventKind::RunAborted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::CopilotCli,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".copilot")],
        config_paths: ConfigPaths::FromHome(|env| {
            vec![home(env).join(".copilot").join("hooks").join("agent-bark.json")]
        }),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Flat { version: true },
        entry: command_entry,
        create_parent_dir: true,
    })
}

/// Windsurf：~/.codeium/windsurf/hooks.json（扁平结构，**无** version 键，
/// 官方文档 docs.windsurf.com/windsurf/cascade/hooks，12 个事件）。
///
/// 只注册官方事件表里的两个会话级事件（**未实测**；AgentPet 实现一致）：
/// - `pre_user_prompt` → Activity（回合开始）。
/// - `post_cascade_response` / `post_cascade_response_with_transcript` →
///   RunCompleted（Cascade 回答结束的两个变体，官方都列；同时触发时同 kind
///   事件会被聚合窗口合并，不会双通知）。
/// - 工具级事件（pre_write_code / pre_run_command 等）全是**阻塞型** pre-hook，
///   不注册：运行中相位靠 pre_user_prompt 心跳 + 判死兜底足够。
/// - **没有等待类事件**（官方无 Notification 钩子）——Windsurf 只有运行/完成两态。
/// - payload 用 `trajectory_id` + `agent_action_name`（from_raw 别名兜底），
///   项目目录字段缺失时通知正文不带项目名，属能力边界。
pub fn windsurf() -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("pre_user_prompt", EventKind::Activity),
        ("post_cascade_response", EventKind::RunCompleted),
        ("post_cascade_response_with_transcript", EventKind::RunCompleted),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::Windsurf,
        install_markers: vec![dirs::home_dir().unwrap_or_default().join(".codeium/windsurf")],
        config_paths: ConfigPaths::FromHome(|env| {
            vec![home(env).join(".codeium").join("windsurf").join("hooks.json")]
        }),
        event_map: MAP,
        trust_hint: Some("Windsurf 工作区处于受限模式（Restricted Mode）时 hooks 不会加载"),
        shape: HookShape::Flat { version: false },
        entry: windsurf_entry,
        create_parent_dir: false,
    })
}

/// 供测试构造 adapter（可指定 config 路径）
#[cfg(test)]
pub fn test_adapter(configs: Vec<PathBuf>) -> ClaudeStyleAdapter {
    const MAP: &[(&str, EventKind)] = &[
        ("Stop", EventKind::RunCompleted),
        ("Notification", EventKind::InputRequired),
    ];
    ClaudeStyleAdapter::new(ClaudeStyleSpec {
        kind: AgentKind::ClaudeCode,
        install_markers: vec![],
        config_paths: ConfigPaths::Fixed(configs),
        event_map: MAP,
        trust_hint: None,
        shape: HookShape::Nested,
        entry: command_entry,
        create_parent_dir: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> RegisterCtx {
        RegisterCtx {
            exe_path: r"C:\Program Files\AgentBark\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-bark-claude-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn count_foreign(doc: &Value) -> usize {
        doc.get("hooks")
            .and_then(|h| h.as_object())
            .map(|events| {
                events
                    .values()
                    .filter_map(|g| g.as_array())
                    .flat_map(|gs| gs.iter())
                    .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
                    .flat_map(|hs| hs.iter())
                    .filter(|e| !jsonio::is_our_entry(e))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn codebuddy_markers_exclude_workbuddy_shared_footprint() {
        // 回归网：~/.codebuddy 目录本身不能当安装标记——WorkBuddy（内嵌 CodeBuddy
        // 内核）只装 WorkBuddy 也会创建它（实测只有 code-ratio/、diagnostics/、logs/）。
        // 标记必须全部是 WorkBuddy 不会写的具体足迹（settings.json / .mcp.json /
        // skills/ / 原生二进制路径），防止有人图省事改回目录级标记。
        let markers = codebuddy_install_markers();
        assert!(!markers.is_empty());
        for m in &markers {
            let s = m.to_string_lossy().replace('\\', "/");
            assert!(
                s.ends_with("/settings.json")
                    || s.ends_with("/.mcp.json")
                    || s.ends_with("/skills")
                    || s.ends_with("/codebuddy/bin")
                    || s.ends_with("/.local/bin/codebuddy"),
                "可疑的 CodeBuddy 安装标记 {s}：WorkBuddy 共享目录下的痕迹会被误判"
            );
        }
    }

    #[test]
    fn register_then_unregister_preserves_foreign_hooks() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        // 用户已有配置：一个纯用户组 + 一个「用户与我们的条目同组」的混合组
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "model": "opus",
                "hooks": {
                    "Stop": [
                        { "hooks": [ { "type": "command", "command": "echo user-only" } ] },
                        { "hooks": [
                            { "type": "command", "command": "echo user-mixed" },
                            { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent claude-code --event Stop" }
                        ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        // 用户的其他顶层键必须保留
        assert_eq!(doc["model"], "opus");
        assert_eq!(count_foreign(&doc), 2, "用户的两条 hook 都应保留");
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 2, "不应重复新增组");

        // 卸载：用户条目必须一条不少
        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(count_foreign(&doc), 2, "卸载后用户 hook 必须原样保留");
        assert_eq!(doc["model"], "opus");
        assert!(!doc_has_active_entry(&doc));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unregister_dry_run_writes_nothing() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = test_adapter(vec![cfg.clone()]);
        adapter.register(&ctx()).unwrap();
        // 清掉 register 留下的备份，便于观察 dry_run 是否新建文件
        let bak = jsonio::backup_path(&cfg);
        let _ = std::fs::remove_file(&bak);
        let before = std::fs::read_to_string(&cfg).unwrap();

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: true, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before, "dry_run 不得改文件");
        assert!(!bak.exists(), "dry_run 不得创建备份");

        // backup=false 时不应产生 .bak
        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: false };
        adapter.unregister(&mut ictx).unwrap();
        assert!(!bak.exists(), "backup=false 不应备份");
        assert!(!doc_has_active_entry(&jsonio::read_doc(&cfg).unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_config_aborts_without_overwriting() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        let original = "{ \"model\": \"opus\", }"; // 尾随逗号 → 非法
        std::fs::write(&cfg, original).unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        let err = adapter.register(&ctx()).expect_err("非法 JSON 必须中止");
        assert!(err.to_string().contains("不是合法 JSON"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original, "文件不能被改写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_repairs_stale_exe_path_in_place() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = test_adapter(vec![cfg.clone()]);
        adapter.register(&ctx()).unwrap();

        // 模拟程序被移动到新位置
        let moved = RegisterCtx {
            exe_path: r"D:\new\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        };
        assert!(matches!(adapter.verify(&moved), VerifyReport::StalePath { .. }));
        adapter.register(&moved).unwrap();
        assert!(matches!(adapter.verify(&moved), VerifyReport::Ok));
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1, "就地修复不应新增组");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_reports_not_registered_on_empty_config() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = test_adapter(vec![cfg.clone()]);
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::NotRegistered));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hook_command_uses_forward_slashes() {
        let dir = tmpdir();
        let adapter = test_adapter(vec![dir.join("settings.json")]);
        let cmd = adapter.hook_command(&ctx(), "Stop");
        assert!(cmd.starts_with("\"C:/Program Files/AgentBark/AgentBark.exe\" hook"));
        assert!(!cmd.contains('\\'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_failure_is_not_run_failure() {
        // PostToolUseFailure 是工具级失败（agent 会自行重试），归一成 ToolFailed：
        // 不通知、不亮失败色，事件仍入历史与事件流（中性色「工具失败」可排查）。
        // 以前映射成 RunFailed 时，一次瞬时工具失败会亮失败色全屏特效 + 弹「任务失败」
        // （实测 ZCode 编辑前没先读文件、失败后立刻重试成功，全是噪音）。
        // 用真实规格测：钉死 Claude Code 的事件映射不漂移。
        let adapter = claude_code();
        let ev = adapter
            .normalize(
                "PostToolUseFailure",
                &json!({ "session_id": "s", "tool_name": "Edit", "error": "File has not been read yet" }),
            )
            .expect("claude_code 注册了 PostToolUseFailure");
        assert_eq!(ev.kind, EventKind::ToolFailed);
        assert_eq!(ev.kind.default_title(), "工具失败");
        assert!(!ev.kind.should_notify(), "工具失败不该弹「任务失败」通知");
    }

    /// Claude Code / CodeBuddy 的官方文档均明确：
    /// - Stop 在**用户中断时不触发** → 中止的回合收不到完成信号；
    /// - SessionEnd（带 reason）是会话级结束信号 → 释放「运行中」条目的唯一途径；
    /// - StopFailure 是回合因 API 错误结束 → 唯一的「回合真失败」信号。
    /// 本机未装 Claude Code，这些映射按官方文档钉死（未实测）；
    /// CodeBuddy 本机装有，可直接验证。映射漂移 = 中止卡黄到判死复发，钉住。
    #[test]
    fn claude_style_abort_signals_are_wired() {
        for adapter in [claude_code(), codebuddy()] {
            assert_eq!(adapter.event_kind("SessionEnd"), Some(EventKind::RunAborted), "{}: SessionEnd 必须归一为中止（中止释放会话）", adapter.kind().id());
            assert_eq!(adapter.event_kind("StopFailure"), Some(EventKind::RunFailed), "{}: StopFailure 是回合真失败（API 错误）", adapter.kind().id());
            assert_eq!(adapter.event_kind("UserPromptSubmit"), Some(EventKind::Activity), "{}: 心跳缺失会让会话只有等待/失败时可见", adapter.kind().id());
            assert_eq!(adapter.event_kind("PreToolUse"), Some(EventKind::Activity), "{}: 心跳缺失会让会话只有等待/失败时可见", adapter.kind().id());
            assert_eq!(adapter.event_kind("Stop"), Some(EventKind::RunCompleted));
        }
    }

    #[test]
    fn permission_prompt_notification_maps_to_permission_required() {
        let dir = tmpdir();
        let adapter = test_adapter(vec![dir.join("settings.json")]);

        // Qoder 实测载荷：Notification 里带 notification_type=permission_prompt
        let ev = adapter
            .normalize(
                "Notification",
                &json!({
                    "session_id": "s",
                    "cwd": r"D:\Workspace\proj",
                    "notification_type": "permission_prompt",
                    "message": "Tool Bash requires confirmation"
                }),
            )
            .expect("test_adapter 注册了 Notification");
        assert_eq!(ev.kind, EventKind::PermissionRequired);
        assert_eq!(ev.kind.default_title(), "需要确认");
        assert_eq!(ev.message, "Tool Bash requires confirmation");
        assert_eq!(ev.project.as_deref(), Some("proj"));

        // 其它通知（空闲提醒等）仍按「等待输入」
        let ev = adapter
            .normalize("Notification", &json!({ "message": "waiting for your input" }))
            .unwrap();
        assert_eq!(ev.kind, EventKind::InputRequired);

        // 未注册的事件名不产生事件
        assert!(adapter.normalize("SessionEnd", &json!({})).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qoder_covers_both_intl_and_cn_config_paths() {
        // 一条 adapter 覆盖两个版本：国际版 ~/.qoder/settings.json + 国内版 ~/.qoder-cn/settings.json
        // （2026-09 实测：CN 的 IDE / 插件 / CLI 读的都是 ~/.qoder-cn；官方文档里的 ~/.lingma
        //   是灵码时代 VS Code 插件线的家，埋在那儿的探针一次都没被调起）
        let adapter = qoder();
        let paths: Vec<String> = adapter
            .config_paths()
            .iter()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(paths.len(), 2, "两个版本各一份配置：{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("/.qoder/settings.json")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("/.qoder-cn/settings.json")), "{paths:?}");
        assert!(paths.iter().all(|p| !p.contains(".lingma")), "不得再指向灵码时代的旧家目录: {paths:?}");
        // 安装标记同理看两个目录（只装任一个版本都应被检测到）
        assert_eq!(adapter.spec.install_markers.len(), 2);
    }

    #[test]
    fn qoder_covers_permission_wait_and_interrupt() {
        let adapter = qoder();
        // 实测（国际版与国内版一致）：Notification 带着 permission_prompt 来 → 等待色（需要确认）
        let ev = adapter
            .normalize(
                "Notification",
                &json!({
                    "session_id": "s",
                    "notification_type": "permission_prompt",
                    "message": "Tool Bash requires confirmation"
                }),
            )
            .expect("两版实测都支持 Notification");
        assert_eq!(ev.kind, EventKind::PermissionRequired);
        assert_eq!(ev.kind.default_title(), "需要确认");

        // 实测：用户主动中断回合只有 SessionEnd（没有 Stop）→ 必须能清掉「运行中」。
        // 归一成 RunAborted（中止）而不是 RunFailed：用户自己按的停止不该亮失败色，
        // 也不该弹「任务失败」。
        let ev = adapter
            .normalize("SessionEnd", &json!({ "session_id": "s", "reason": "other" }))
            .expect("两版实测都支持 SessionEnd");
        assert_eq!(ev.kind, EventKind::RunAborted);
        assert_eq!(ev.kind.default_title(), "已中止");
        assert!(!ev.kind.should_notify(), "自己按的停止不通知");

        // 合并后的表只留两版交集：SessionStart（空操作）与 PermissionRequest（实测不触发）不注册
        assert!(adapter.normalize("SessionStart", &json!({})).is_none());
        assert!(adapter.normalize("PermissionRequest", &json!({})).is_none());
    }

    #[test]
    fn retired_id_entries_do_not_count_as_registered() {
        // 合并前的 hooks 用的是 `--agent qoder-cn`，该 id 已下线：bark-cli 会判未知
        // 直接退出，条目等于空转。这种条目不能算「已接入」，否则用户看不到问题、
        // 也拿不到「点开关重写」的提示。
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/old/AgentBark.exe\" hook --agent qoder-cn --event Stop"
                        } ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        assert!(!adapter.is_registered(), "下线 id 的条目不算已接入");
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::NotRegistered));

        // 点开关（register）后：旧条目被清掉，换成当前 id 的条目
        adapter.register(&ctx()).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        let text = serde_json::to_string(&doc).unwrap();
        assert!(!text.contains("--agent qoder-cn"), "旧 id 条目必须被清掉: {text}");
        assert!(doc_has_active_entry(&doc), "应写入当前 id 的条目");
        assert!(adapter.is_registered());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_drops_entries_for_retired_agents() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        // 已下线 adapter 留下的死条目：一条独占事件（test_adapter 不注册 PostToolUse，
        // 所以整条事件键都该消失），一条与用户 hook 同组
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "PostToolUse": [
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/old/AgentBark.exe\" hook --agent qoder-cn-cli --event PostToolUse"
                        } ] }
                    ],
                    "Stop": [
                        { "hooks": [
                            { "type": "command", "command": "echo user-only" },
                            { "type": "command",
                              "command": "\"C:/old/AgentBark.exe\" hook --agent qoder-work --event Stop" }
                        ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        assert!(doc["hooks"].get("PostToolUse").is_none(), "只剩死条目的事件键应被删掉");
        assert_eq!(count_foreign(&doc), 1, "用户自己的 hook 必须原样保留");
        assert!(doc_has_active_entry(&doc), "当前 adapter 的条目应已就位");
        // 死条目不得残留
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(!text.contains("qoder-work") && !text.contains("qoder-cn-cli") && !text.contains("--agent qoder-cn "));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- §1.11 PostToolUse → ToolFinished（四条 MAP 逐条钉死） ----------------

    /// ToolFinished 是**等待状态解除的唯一信号**（bark-core event.rs）：答完
    /// AskUserQuestion / 批完权限后到下一个 PreToolUse 之间没有别的事件，
    /// 不注册 PostToolUse 的 MAP 会让相位卡「等待中/执行工具」、等待色一直亮到
    /// 下一个工具开始。四条 MAP 逐条钉死（§1.11）。
    #[test]
    fn claude_code_map_has_post_tooluse_tool_finished() {
        assert_eq!(claude_code().event_kind("PostToolUse"), Some(EventKind::ToolFinished));
    }

    #[test]
    fn trae_code_map_has_post_tooluse_tool_finished() {
        // TraeCode 的官方 6 事件表就含 PostToolUse（本文件注释自列）
        assert_eq!(trae_code().event_kind("PostToolUse"), Some(EventKind::ToolFinished));
    }

    /// TraeCode 的 MAP 补上 PostToolUse 后恰好回到官方 6 事件（不多不少）——
    /// 未知事件名有让整份 hooks 配置被丢弃的风险，事件面必须钉死
    #[test]
    fn trae_code_map_covers_exactly_the_documented_six_events() {
        let mut events = trae_code().hook_events();
        events.sort_unstable();
        assert_eq!(
            events,
            vec!["Notification", "PostToolUse", "PreToolUse", "SessionStart", "Stop", "UserPromptSubmit"],
            "TraeCode 事件面 = 官方 6 事件"
        );
    }

    #[test]
    fn codebuddy_map_has_post_tooluse_tool_finished() {
        assert_eq!(codebuddy().event_kind("PostToolUse"), Some(EventKind::ToolFinished));
    }

    #[test]
    fn qoder_map_has_post_tooluse_tool_finished() {
        assert_eq!(qoder().event_kind("PostToolUse"), Some(EventKind::ToolFinished));
    }

    #[test]
    fn post_tooluse_normalizes_to_tool_finished_not_waiting() {
        let ev = claude_code()
            .normalize("PostToolUse", &json!({ "session_id": "s", "tool_name": "AskUserQuestion" }))
            .expect("claude_code 注册了 PostToolUse");
        assert_eq!(ev.kind, EventKind::ToolFinished, "答完问题的收尾信号必须解除等待态");
        assert!(!ev.kind.should_notify(), "工具收尾是纯状态信号，不通知");
    }

    // ---- §1.9 跨组去重 / §2.18g verify / §2.18h 判定修正 ----------------------

    /// §1.9：`Stop: [{hooks:[我们的+用户的]}, {hooks:[我们的]}]` 只留一个我们的
    /// 条目（事件级去重）——否则每个事件起两个 hook 进程、双触发双通知
    #[test]
    fn cross_group_duplicate_ours_entries_are_deduped() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [
                            { "type": "command", "command": "echo user-mixed" },
                            { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent claude-code --event Stop" }
                        ] },
                        { "hooks": [
                            { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent claude-code --event Stop" }
                        ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        let mut ours = 0;
        let mut foreign = 0;
        for g in doc["hooks"]["Stop"].as_array().unwrap() {
            for e in g.get("hooks").and_then(|h| h.as_array()).into_iter().flatten() {
                if jsonio::is_our_entry(e) {
                    ours += 1;
                } else {
                    foreign += 1;
                }
            }
        }
        assert_eq!(ours, 1, "事件级去重后只留一个我们的条目（§1.9）");
        assert_eq!(foreign, 1, "用户条目原样保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.18g：verify 不在首个漂移组提前 return——「一组旧路径残留 + 一组当前
    /// 路径」是有效接入，收齐所有组再判定，不得误报 StalePath
    #[test]
    fn verify_collects_all_groups_current_wins_over_stale_leftover() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        // 第一个组就是漂移的旧路径条目——旧实现在这里提前 return
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/old/AgentBark/AgentBark.exe\" hook --agent claude-code --event Stop"
                        } ] },
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/Program Files/AgentBark/AgentBark.exe\" hook --agent claude-code --event Stop"
                        } ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        assert!(
            matches!(adapter.verify(&ctx()), VerifyReport::Ok),
            "存在当前路径的有效条目就是有效接入，不得被首个漂移组误报成 StalePath"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.18g 的对偶：所有组都漂移时仍报 StalePath（可由 register 就地修复）
    #[test]
    fn verify_reports_stale_path_when_all_groups_drifted() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/old/a/AgentBark.exe\" hook --agent claude-code --event Stop"
                        } ] },
                        { "hooks": [ {
                            "type": "command",
                            "command": "\"C:/old/b/AgentBark.exe\" hook --agent claude-code --event Stop"
                        } ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::StalePath { .. }));
        // register 就地修复后回到 Ok（去重 + 改写路径）
        adapter.register(&ctx()).unwrap();
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.18h：is_permission_prompt 大小写不敏感 contains("permission")，
    /// 各版本/各 agent 的变体不再退化成 InputRequired
    #[test]
    fn is_permission_prompt_matches_variants_case_insensitively() {
        for v in ["permission_prompt", "Permission_Prompt", "PERMISSION_PROMPT", "permission-request", "needs_permission"] {
            assert!(
                is_permission_prompt(&json!({ "notification_type": v })),
                "应识别为权限请求: {v}"
            );
        }
    }

    /// §2.18h：非权限通知不得被卷进 PermissionRequired
    #[test]
    fn is_permission_prompt_rejects_non_permission_notifications() {
        for v in ["idle_notice", "waiting_input", ""] {
            assert!(!is_permission_prompt(&json!({ "notification_type": v })), "非权限通知: {v}");
        }
        assert!(!is_permission_prompt(&json!({ "message": "hi" })), "无该字段不算");
    }

    /// §2.18h：is_retired_entry 识别 `--agent=<id>` 等值写法——死条目清不掉会
    /// 永远空转触发
    #[test]
    fn is_retired_entry_accepts_equals_form() {
        let entry = |cmd: &str| json!({ "type": "command", "command": cmd });
        // 等值写法
        assert!(is_retired_entry(&entry(
            r#""C:/old/AgentBark.exe" hook --agent=qoder-cn --event Stop"#
        )));
        assert!(is_retired_entry(&entry(
            r#""C:/old/AgentBark.exe" hook --agent=qoder-cn-cli --event PostToolUse"#
        )));
        // 当前 id 不算退役
        assert!(!is_retired_entry(&entry(
            r#""C:/x/AgentBark.exe" hook --agent=claude-code --event Stop"#
        )));
        // 非我们的条目永远不算退役（不碰用户 hook）
        assert!(!is_retired_entry(&entry("node hook --agent=qoder-cn")));
    }

    /// §2.18h：空格写法（原有行为）继续生效
    #[test]
    fn is_retired_entry_accepts_space_form() {
        let entry = |cmd: &str| json!({ "type": "command", "command": cmd });
        assert!(is_retired_entry(&entry(
            r#""C:/old/AgentBark.exe" hook --agent qoder-cn-cli --event Stop"#
        )));
        assert!(!is_retired_entry(&entry(
            r#""C:/x/AgentBark.exe" hook --agent claude-code --event Stop"#
        )));
    }

    /// §2.18g 变体：**同一组**里旧路径与当前路径并存时算有效接入（组内有当前条目）
    #[test]
    fn verify_mixed_group_with_current_entry_is_ok() {
        let dir = tmpdir();
        let cfg = dir.join("settings.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [
                            { "type": "command",
                              "command": "\"C:/old/AgentBark/AgentBark.exe\" hook --agent claude-code --event Stop" },
                            { "type": "command",
                              "command": "\"C:/Program Files/AgentBark/AgentBark.exe\" hook --agent claude-code --event Stop" }
                        ] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let adapter = test_adapter(vec![cfg.clone()]);
        assert!(matches!(adapter.verify(&ctx()), VerifyReport::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Codex MAP 属「待确认」（§2.13）：行为不改，映射原样钉死——
    /// 等探针拿到实机样本再决定是否换成 agent-turn-complete
    #[test]
    fn codex_map_stays_unchanged_pending_probe() {
        let adapter = codex();
        assert_eq!(adapter.event_kind("Stop"), Some(EventKind::RunCompleted));
        assert_eq!(adapter.event_kind("PermissionRequest"), Some(EventKind::PermissionRequired));
        assert_eq!(adapter.event_kind("SessionStart"), Some(EventKind::SessionStart));
        // 待确认期间不加不减：agent-turn-complete 没有实机样本，不能瞎接
        assert_eq!(adapter.event_kind("agent-turn-complete"), None);
    }

    // ---- 2026-09 广度扩展：事件表 ⊆ 官方事件表（未知事件名有让整份 hooks
    //      配置被丢弃的风险，每个新 adapter 都要钉住官方清单） ----------------

    #[test]
    fn new_agent_event_maps_stay_within_official_event_lists() {
        // 官方事件名逐字来源：各 agent hooks 官方文档（2026-09 调研，见各 adapter 注释）
        const GEMINI: [&str; 11] = [
            "SessionStart", "SessionEnd", "BeforeAgent", "AfterAgent", "BeforeModel",
            "AfterModel", "BeforeToolSelection", "BeforeTool", "AfterTool", "PreCompress", "Notification",
        ];
        const QWEN: [&str; 22] = [
            "PreToolUse", "PostToolUse", "PostToolUseFailure", "PostToolBatch", "UserPromptSubmit",
            "UserPromptExpansion", "SessionStart", "SessionEnd", "SessionDelete", "MessageDisplay",
            "Stop", "StopFailure", "SubagentStart", "SubagentStop", "PreCompact", "PostCompact",
            "Notification", "PermissionRequest", "PermissionDenied", "TodoCreated", "TodoCompleted",
            "InstructionsLoaded",
        ];
        const DROID: [&str; 9] = [
            "PreToolUse", "PostToolUse", "UserPromptSubmit", "Notification", "Stop",
            "SubagentStop", "PreCompact", "SessionStart", "SessionEnd",
        ];
        const GROK: [&str; 15] = [
            "SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure",
            "PermissionDenied", "Stop", "StopFailure", "StopCancelled", "Notification",
            "SubagentStart", "SubagentStop", "PreCompact", "PostCompact", "SessionEnd",
        ];
        const CURSOR: [&str; 21] = [
            "sessionStart", "sessionEnd", "preToolUse", "postToolUse", "postToolUseFailure",
            "subagentStart", "subagentStop", "beforeShellExecution", "afterShellExecution",
            "beforeMCPExecution", "afterMCPExecution", "beforeReadFile", "afterFileEdit",
            "beforeSubmitPrompt", "preCompact", "stop", "afterAgentResponse", "afterAgentThought",
            "beforeTabFileRead", "afterTabFileEdit", "workspaceOpen",
        ];
        const COPILOT: [&str; 14] = [
            "agentStop", "errorOccurred", "notification", "permissionRequest", "postToolUse",
            "postToolUseFailure", "preCompact", "preToolUse", "sessionEnd", "sessionStart",
            "subagentStart", "subagentStop", "userPromptSubmitted", "userPromptTransformed",
        ];
        const WINDSURF: [&str; 12] = [
            "pre_read_code", "post_read_code", "pre_write_code", "post_write_code",
            "pre_run_command", "post_run_command", "pre_mcp_tool_use", "post_mcp_tool_use",
            "pre_user_prompt", "post_cascade_response", "post_cascade_response_with_transcript",
            "post_setup_worktree",
        ];
        let pairs: Vec<(ClaudeStyleAdapter, &[&str])> = vec![
            (gemini_cli(), &GEMINI),
            (qwen_code(), &QWEN),
            (droid(), &DROID),
            (grok(), &GROK),
            (cursor(), &CURSOR),
            (copilot_cli(), &COPILOT),
            (windsurf(), &WINDSURF),
        ];
        for (adapter, official) in pairs {
            for event in adapter.hook_events() {
                assert!(
                    official.contains(&event),
                    "{} 注册了官方事件表之外的事件: {event}",
                    adapter.kind().id()
                );
            }
        }
    }

    /// 关键归一化口径钉死（防止后续重构漂移）：
    /// - Grok 的 StopCancelled 是显式用户取消 → 中止（多数宿主没有的干净信号）
    /// - Cursor / Windsurf 没有等待类事件 → 没有 Notification 映射
    /// - Copilot 用官方 camelCase 事件名（agentStop，不是 Stop）
    #[test]
    fn new_agent_key_normalizations_are_wired() {
        let grok = grok();
        assert_eq!(grok.event_kind("StopCancelled"), Some(EventKind::RunAborted), "StopCancelled 必须归一为中止");
        assert_eq!(grok.event_kind("PostToolUseFailure"), Some(EventKind::ToolFailed));
        let cursor = cursor();
        assert_eq!(cursor.event_kind("notification"), None, "Cursor 官方没有等待类钩子");
        assert_eq!(cursor.event_kind("stop"), Some(EventKind::RunCompleted));
        let windsurf = windsurf();
        assert_eq!(windsurf.event_kind("post_cascade_response"), Some(EventKind::RunCompleted));
        let copilot = copilot_cli();
        assert_eq!(copilot.event_kind("agentStop"), Some(EventKind::RunCompleted), "Copilot 官方事件是 agentStop");
        assert_eq!(copilot.event_kind("Stop"), None, "PascalCase 的 Stop 不是 CLI 官方事件名");
        assert_eq!(copilot.event_kind("permissionRequest"), Some(EventKind::PermissionRequired));
        let gemini = gemini_cli();
        assert_eq!(gemini.event_kind("AfterAgent"), Some(EventKind::RunCompleted));
        assert_eq!(gemini.event_kind("SessionEnd"), Some(EventKind::RunAborted), "会话级释放信号必须归一为中止");
    }

    // ---- 扁平形态（HookShape::Flat）的注册/去重/卸载回归网 --------------------

    #[test]
    fn flat_register_creates_file_with_version_key() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json"); // Cursor：hooks.json 可以不存在（用户没配过）
        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Cursor,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("stop", EventKind::RunCompleted), ("preToolUse", EventKind::Activity)],
            trust_hint: None,
            shape: HookShape::Flat { version: true },
            entry: command_entry,
            create_parent_dir: false,
        });
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["version"], 1, "Cursor 必须带顶层 version:1");
        assert_eq!(doc["hooks"]["stop"].as_array().unwrap().len(), 1);
        assert_eq!(doc["hooks"]["stop"][0]["type"], "command");
        assert!(doc["hooks"]["stop"][0]["command"].as_str().unwrap().contains("--event stop"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扁平卸载：只摘我们的条目，用户条目与用户已有的 version 键原样保留
    #[test]
    fn flat_register_then_unregister_preserves_foreign_entries() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "hooks": {
                    "stop": [
                        { "type": "command", "command": "echo user-only" },
                        { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent cursor --event stop" }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Cursor,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("stop", EventKind::RunCompleted)],
            trust_hint: None,
            shape: HookShape::Flat { version: true },
            entry: command_entry,
            create_parent_dir: false,
        });
        adapter.register(&ctx()).unwrap();

        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["hooks"]["stop"].as_array().unwrap().len(), 2, "用户条目在，我们不重复新增组");
        assert_eq!(doc["version"], 1);

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: true };
        adapter.unregister(&mut ictx).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        let arr = doc["hooks"]["stop"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "卸载后只剩用户条目");
        assert_eq!(arr[0]["command"], "echo user-only", "用户条目必须原样保留");
        assert_eq!(doc["version"], 1, "用户的 version 键不动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扁平跨条目去重（§1.9 扁平版）：重复的我们的条目只留一个
    #[test]
    fn flat_register_dedupes_duplicate_ours_entries() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(
            &cfg,
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "hooks": { "stop": [
                    { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent cursor --event stop" },
                    { "type": "command", "command": "echo user-mixed" },
                    { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent cursor --event stop" }
                ] }
            }))
            .unwrap(),
        )
        .unwrap();
        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Cursor,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("stop", EventKind::RunCompleted)],
            trust_hint: None,
            shape: HookShape::Flat { version: true },
            entry: command_entry,
            create_parent_dir: false,
        });
        adapter.register(&ctx()).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        let arr = doc["hooks"]["stop"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "去重后只留一个我们的条目 + 用户条目");
        let ours: Vec<&Value> = arr.iter().filter(|e| jsonio::is_our_entry(e)).collect();
        assert_eq!(ours.len(), 1);
        assert!(
            ours[0]["command"].as_str().unwrap().contains("C:/Program Files/AgentBark"),
            "保留的就地修复为当前路径"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windsurf：无 version 键、条目是 {command, show_output:false} 形态
    #[test]
    fn windsurf_flat_shape_has_no_version_and_show_output_false() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Windsurf,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("pre_user_prompt", EventKind::Activity)],
            trust_hint: None,
            shape: HookShape::Flat { version: false },
            entry: windsurf_entry,
            create_parent_dir: false,
        });
        adapter.register(&ctx()).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert!(doc.get("version").is_none(), "Windsurf 的 hooks 文件不能带 version 键");
        let entry = &doc["hooks"]["pre_user_prompt"][0];
        assert_eq!(entry["show_output"], false, "show_output=false 不在 Cascade 面板刷屏");
        assert!(entry.get("type").is_none(), "Windsurf 条目没有 type 字段");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 专用文件（Copilot / Grok）：agent 根目录存在但 hooks/ 子目录没建时，
    /// register 允许补建最后一层目录
    #[test]
    fn dedicated_file_creates_last_level_dir_only() {
        let dir = tmpdir();
        let root = dir.join(".copilot");
        std::fs::create_dir_all(&root).unwrap(); // agent 根目录存在（已安装）
        let cfg = root.join("hooks").join("agent-bark.json"); // hooks/ 子目录不存在

        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::CopilotCli,
            install_markers: vec![root.clone()],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("agentStop", EventKind::RunCompleted)],
            trust_hint: None,
            shape: HookShape::Flat { version: true },
            entry: command_entry,
            create_parent_dir: true,
        });
        adapter.register(&ctx()).unwrap();
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["hooks"]["agentStop"][0]["type"], "command");

        // 对偶：根目录都不存在（未安装）时不得凭空造整棵树
        let missing_root = dir.join(".nonexistent").join("hooks").join("agent-bark.json");
        let adapter2 = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Grok,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![missing_root.clone()]),
            event_map: &[("Stop", EventKind::RunCompleted)],
            trust_hint: None,
            shape: HookShape::Nested,
            entry: command_entry,
            create_parent_dir: true,
        });
        assert!(adapter2.register(&ctx()).is_err(), "未安装必须报错");
        assert!(!dir.join(".nonexistent").exists(), "不得凭空创建配置树");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 扁平形态的 StalePath 检测与就地修复
    #[test]
    fn flat_verify_reports_stale_path_and_repairs() {
        let dir = tmpdir();
        let cfg = dir.join("hooks.json");
        std::fs::write(&cfg, "{}").unwrap();
        let adapter = ClaudeStyleAdapter::new(ClaudeStyleSpec {
            kind: AgentKind::Cursor,
            install_markers: vec![],
            config_paths: ConfigPaths::Fixed(vec![cfg.clone()]),
            event_map: &[("stop", EventKind::RunCompleted)],
            trust_hint: None,
            shape: HookShape::Flat { version: true },
            entry: command_entry,
            create_parent_dir: false,
        });
        adapter.register(&ctx()).unwrap();
        let moved = RegisterCtx { exe_path: r"D:\new\AgentBark.exe".into(), port: 1, token: "t".into() };
        assert!(matches!(adapter.verify(&moved), VerifyReport::StalePath { .. }));
        adapter.register(&moved).unwrap();
        assert!(matches!(adapter.verify(&moved), VerifyReport::Ok));
        let doc = jsonio::read_doc(&cfg).unwrap();
        assert_eq!(doc["hooks"]["stop"].as_array().unwrap().len(), 1, "就地修复不新增条目");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
