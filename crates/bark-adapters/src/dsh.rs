//! DeepSeek Harness（DSH）适配器：原生 Cordis 插件接入。
//!
//! DSH 是「一切皆插件」的 Agent 运行框架（deepseek-ai/deepseek-harness，
//! Developer Preview）。接入方式与 OpenCode 类似：生成一个原生 Cordis 插件
//! （导出 `name` + `apply(ctx)` 的 ESM 模块，无外部 import），再把其**绝对路径**
//! 登记到用户级 patch `$DSH_HOME/cordis.patch.yml`（默认 ~/.dsh，所有 profile
//! 共享）。插件内部监听 DSH 的类型化事件（`ctx.on(...)`），归一化后直接 POST
//! 本地 daemon，不经过 hook 子命令。
//!
//! 为什么不走官方 `@deepseek-ai/dsh-hooks-claude-code` 桥接：桥接虽能把现成
//! Claude Code hooks.json 翻译成 DSH 钩子，但它是刻意 opt-in 的——用户需先在
//! profile 里 `dsh plugin add @deepseek-ai/dsh-hooks-claude-code` 安装桥接包
//! （网络 + pnpm 依赖）才能挂载；原生插件方案只写两个本地文件，勾选即生效。
//! 两者的事件覆盖面相同（turn 结束≈Stop），社区同类插件（dsh-notifier 等）
//! 也均走原生插件路线。
//!
//! 写入原则（与 claude_style / opencode 一致）：
//! 1. patch 文件解析失败时拒绝写入，绝不覆盖用户配置；
//! 2. 只增删我们自己的条目（insert 的条目 id 固定为 `agent-bark`，name 指向
//!    我们的插件文件），用户条目原样保留；重复条目就地修复 / 删除；
//! 3. 首次修改前备份为 `<file>.agent-bark.bak`；
//! 4. DSH_HOME 不存在（未安装）时拒绝注册，绝不凭空创建。
//!
//! **patch 语义（实测踩坑）**：cordis.patch.yml 是「补丁层」而非「条目清单」——
//! 顶层 `{id, name}` 的语义是**覆盖同名 id 的既有条目**，id 在基础树里不存在时
//! DSH 只在 stderr 打 `patch: entry "agent-bark" not found` 并**跳过**，插件根本
//! 不会被挂载（见 dsh-app-boot 的 applyEntryPatches）。新增插件必须写成 insert
//! 形式：
//! ```yaml
//! - insert:
//!   - id: agent-bark
//!     name: C:/Users/<u>/.dsh/agent-bark/plugin.js
//! ```
//! 外层不带 id 表示追加到根组；name 写绝对路径即可，DSH 加载时会经
//! `pathToFileURL` 转成 file:// URL（Windows 盘符路径也支持）。
//!
//! 事件面（全部由生成的插件自己归一化后 POST，不经过 hook 子命令）：
//! - `agent/turn-stopping`（serial）→ run_completed，正文取会话日志里最后一条
//!   `assistant/message` 的 text 块；
//! - `agent/status`：running → activity 心跳（只发根会话）；running→idle 且本回合
//!   `agent/turn-stopping` 没报过 → 兜底补一条终态（外部中止 / 回合被拦等收尾路径）；
//! - `agent/error` → run_failed；`approval/request`（waterfall，观察完必须 `next()`）
//!   → permission_required；`user-questions/request` → input_required；
//! - `session/event` 的 `tool/call` → activity（带 tool_name，面板显示「执行工具」），
//!   并记下 `turn/end` 的 reason 供 idle 兜底分类。
//!
//! 这套选型与社区通知插件一致（dsh-plugin-notify / dsh-notify / dsh-notification）：
//! 只挂 `agent/turn-stopping` 会漏掉「停下来等你确认/回答」这类会话终态，
//! 而 DSH 的会话状态与结束原因要分别从 `agent/status` 与 `session/event` 取。
//!
//! 已知局限（DSH Developer Preview 所致）：
//! - 事件 payload 与 session 日志结构未稳定：插件侧对字段名做多重兜底，取不到摘要
//!   时退回「回合 N 结束」，绝不让通知本身失败；
//! - patch 经 serde_yaml 重写会丢失用户注释——仅在内容确有变化时才写回；
//! - **改完 patch 必须重启 dsh**：实测在 `dsh web` 运行中往
//!   `$DSH_HOME/cordis.patch.yml` 追加 insert 条目不会被挂载（HMR 只重载已挂载的
//!   条目），社区插件文档同样写「重启 dsh 生效」——所以接入后提示用户重启，
//!   而不是声称即时生效。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, EventKind, NormalizedEvent};
use serde_yaml::Value as Yaml;
use std::path::{Path, PathBuf};

pub struct DshAdapter;

/// patch 条目固定 id：cordis.patch.yml 按 id 寻址，我们的条目永远用它
const ENTRY_ID: &str = "agent-bark";

/// 插件文件名（DSH_HOME/agent-bark/ 下）
const PLUGIN_FILE: &str = "plugin.js";

impl DshAdapter {
    /// DSH 数据根目录：尊重 DSH_HOME 环境变量，默认 ~/.dsh
    fn home() -> PathBuf {
        std::env::var_os("DSH_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".dsh")))
            .unwrap_or_else(|| PathBuf::from(".dsh"))
    }

    fn plugin_file(home: &Path) -> PathBuf {
        home.join("agent-bark").join(PLUGIN_FILE)
    }

    fn patch_file(home: &Path) -> PathBuf {
        home.join("cordis.patch.yml")
    }

    /// 当前插件路径的正斜杠形式（写入 patch 的 name 字段，Windows 反斜杠
    /// 在 YAML 引号串里虽合法，但与 Node 路径解析习惯不一致，统一正斜杠）
    fn plugin_path_str(home: &Path) -> String {
        Self::plugin_file(home).to_string_lossy().replace('\\', "/")
    }

    // -- patch 文件的原语 -----------------------------------------------------

    /// 读取 patch 文件为条目数组。
    /// - 文件不存在 / 空白 → 空数组（官方文档说 patch 初始为空是常态）
    /// - 解析失败 / 顶层不是数组 → `Err`（调用方必须中止，不可写回）
    fn read_patch(path: &Path) -> anyhow::Result<Vec<Yaml>> {
        Self::read_patch_raw(path).map(|(e, _)| e)
    }

    /// 同 [`Self::read_patch`]，额外返回原文底稿（CAS 写回的比对基准）
    fn read_patch_raw(path: &Path) -> anyhow::Result<(Vec<Yaml>, String)> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), String::new())),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("读取 {} 失败", path.display()))),
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.trim().is_empty() {
            // 空文件视为空数组；底稿保留原样（含空白），CAS 才不会把
            // 「仍是这份空白文件」误判成「被并发修改」
            return Ok((Vec::new(), text.to_string()));
        }
        let entries: Vec<Yaml> = serde_yaml::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "{} 不是合法的 patch YAML（{}）。为避免覆盖你的配置，agent-bark 已中止写入，请先修复该文件。",
                path.display(),
                e
            )
        })?;
        Ok((entries, text.to_string()))
    }

    /// CAS 写回 patch：重读比对底稿，被并发修改则报错而非覆盖；
    /// 内容确有变化时刷新 `.bak.latest`。
    fn write_patch_checked(path: &Path, entries: &[Yaml], expected_raw: &str) -> anyhow::Result<()> {
        let text = serde_yaml::to_string(entries)?;
        match std::fs::read_to_string(path) {
            Ok(current) => {
                let current = current.strip_prefix('\u{feff}').unwrap_or(&current);
                if current != expected_raw {
                    anyhow::bail!(
                        "{} 在 agent-bark 读取后被其他程序修改过。为避免覆盖刚发生的改动，\
                         本次写入已中止——请重试一次。",
                        path.display()
                    );
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !expected_raw.is_empty() {
                    anyhow::bail!(
                        "{} 在 agent-bark 读取后被删除。为避免误判，本次写入已中止——请重试一次。",
                        path.display()
                    );
                }
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("读取 {} 失败", path.display()))),
        }
        if !expected_raw.is_empty() && expected_raw != text {
            jsonio::backup_latest(path, expected_raw);
        }
        jsonio::write_text_atomic(path, &text)
    }

    /// 条目里取字符串字段
    fn entry_str(entry: &Yaml, key: &str) -> Option<String> {
        entry
            .as_mapping()
            .and_then(|m| m.get(Yaml::String(key.to_string())))
            .and_then(Yaml::as_str)
            .map(str::to_string)
    }

    /// 条目的 insert 列表（新增插件的合法补丁形式）
    fn entry_insert_seq(entry: &Yaml) -> Option<&Vec<Yaml>> {
        entry
            .as_mapping()
            .and_then(|m| m.get(Yaml::String("insert".to_string())))
            .and_then(Yaml::as_sequence)
    }

    /// 条目是否是我们写入的（寻址用）。两种都算我们的：
    /// - 现行格式：`insert` 列表里含 id 为 `agent-bark` 的条目；
    /// - 旧版错误写法：顶层 `id: agent-bark`（无 insert）——DSH 把它当覆盖补丁、
    ///   找不到目标直接跳过，插件从未挂载；归我们所有，register 时就地修复。
    /// 不看 name——旧路径遗留条目也算我们的，register 时就地修复。
    ///
    /// 注意「整条归我们」只用于**寻址**：insert 形式的条目可能是用户把多个插件
    /// 合并写进同一 insert 列表的混合条目，增删时只动我们的**子项**（见
    /// [`Self::surgery_insert_entry`]），整条覆盖/删除会把用户的插件一起灭掉（§1.8）。
    fn is_our_entry(entry: &Yaml) -> bool {
        if let Some(seq) = Self::entry_insert_seq(entry) {
            return seq
                .iter()
                .any(|e| Self::entry_str(e, "id").as_deref() == Some(ENTRY_ID));
        }
        Self::entry_str(entry, "id").as_deref() == Some(ENTRY_ID)
    }

    /// 对 insert 形式条目做**子项级手术**（§1.8）：
    /// - `keep_first` 为真时，insert 序列里**首个** id==agent-bark 的子项就地修复
    ///   name（只动 `name` 键，用户在子项上加的其它键原样保留），其余我们的子项
    ///   删除；为假时（unregister / 已有主条目）删除全部我们的子项；
    /// - 用户的子项（其它 id）与外层键一律原样保留——用户把多个插件合并进同一
    ///   insert 列表时，整条覆盖/删除会把他们的配置静默灭掉；
    /// - insert 序列删空才把整条置 Null（外层随之删除）。
    /// 返回是否有变更。
    fn surgery_insert_entry(entry: &mut Yaml, home: Option<&Path>, keep_first: bool) -> bool {
        let mut changed = false;
        let mut emptied = false;
        if let Some(seq) = entry
            .as_mapping_mut()
            .and_then(|m| m.get_mut(Yaml::String("insert".to_string())))
            .and_then(Yaml::as_sequence_mut)
        {
            let mut kept = false;
            let mut remove: Vec<usize> = Vec::new();
            for (i, sub) in seq.iter_mut().enumerate() {
                if Self::entry_str(sub, "id").as_deref() != Some(ENTRY_ID) {
                    continue; // 用户的子项，原样保留
                }
                if keep_first && !kept {
                    kept = true;
                    if let Some(home) = home {
                        let want = Self::plugin_path_str(home);
                        let cur = Self::entry_str(sub, "name").unwrap_or_default();
                        if cur.replace('\\', "/") != want {
                            if let Some(m) = sub.as_mapping_mut() {
                                // 只动 name 键：子项上的其它键是用户/宿主的定制
                                m.insert(Yaml::String("name".to_string()), Yaml::String(want));
                                changed = true;
                            }
                        }
                    }
                } else {
                    // 重复的我们的子项（或 unregister 模式下的全部）：删除
                    remove.push(i);
                }
            }
            for i in remove.into_iter().rev() {
                seq.remove(i);
                changed = true;
            }
            emptied = seq.is_empty();
        }
        if emptied {
            *entry = Yaml::Null;
            changed = true;
        }
        changed
    }

    /// 条目是否是**有效的现行接入**（verify 用）：必须是 insert 形式、且 name
    /// 恰好指向当前插件路径。旧格式或旧路径都算漂移，重新 register 就地修复。
    fn entry_is_current(entry: &Yaml, home: &Path) -> bool {
        let Some(seq) = Self::entry_insert_seq(entry) else { return false };
        let want = Self::plugin_path_str(home);
        seq.iter().any(|e| {
            Self::entry_str(e, "id").as_deref() == Some(ENTRY_ID)
                && Self::entry_str(e, "name").is_some_and(|n| n.replace('\\', "/") == want)
        })
    }

    /// 构造我们的条目：insert 语义（外层不带 id = 追加到根组）。
    /// 顶层 `{id, name}` 是覆盖语义，基础树里没有同名 id 时 DSH 告警并跳过。
    fn make_entry(home: &Path) -> Yaml {
        let mut inner = serde_yaml::Mapping::new();
        inner.insert(Yaml::String("id".into()), Yaml::String(ENTRY_ID.into()));
        inner.insert(
            Yaml::String("name".into()),
            Yaml::String(Self::plugin_path_str(home)),
        );
        let mut outer = serde_yaml::Mapping::new();
        outer.insert(
            Yaml::String("insert".into()),
            Yaml::Sequence(vec![Yaml::Mapping(inner)]),
        );
        Yaml::Mapping(outer)
    }

    // -- 核心：在指定 home 下执行（便于测试注入临时目录） -------------------

    fn register_at(home: &Path, ctx: &RegisterCtx) -> anyhow::Result<()> {
        if !home.exists() {
            anyhow::bail!(
                "未检测到 DeepSeek Harness（{} 不存在）。请先安装 dsh（npm i -g @deepseek-ai/dsh）并至少启动一次。",
                home.display()
            );
        }
        let patch = Self::patch_file(home);
        // 1. 先读并校验 patch：解析失败必须中止（否则会把用户的 patch 清空）
        let existed = patch.exists();
        let (mut entries, raw) = Self::read_patch_raw(&patch)?;

        // 2. 写插件文件（含端口与 token）。内容一致就不写：启动时的例行刷新不该
        //    每次都换 mtime（也让「文件确实没变」这件事可观察），不一致才原子写。
        let plugin = Self::plugin_file(home);
        let js = plugin_js(ctx.port, &ctx.token);
        if std::fs::read_to_string(&plugin).map(|current| current != js).unwrap_or(true) {
            jsonio::write_text_atomic(&plugin, &js)?;
        }

        // 3. 外科手术式更新 patch：首个条目就地修复 name（旧路径不能留着，
        //    否则新路径永不生效），重复项删除；一个都没有才追加。
        //    insert 形式的混合条目只对**子项**做手术（§1.8）：用户把多个插件合并
        //    进同一 insert 列表时，整条覆盖会灭掉他们的子条目；旧版顶层条目
        //    （无 insert，DSH 本就不挂载）仍整条处理。
        let mut found = false;
        let mut changed = false;
        // 手术前的 null 位置快照：清理时只删**我们造的** null 哨兵（标记删除 /
        // insert 删空整条置 Null），用户 patch 数组里合法的 null 元素原样保留
        let pre_nulls: Vec<bool> = entries.iter().map(|e| e.is_null()).collect();
        for e in entries.iter_mut() {
            if !Self::is_our_entry(e) {
                continue;
            }
            if Self::entry_insert_seq(e).is_some() {
                let keep = !found;
                if Self::surgery_insert_entry(e, Some(home), keep) {
                    changed = true;
                }
                if keep && Self::entry_is_current(e, home) {
                    found = true;
                }
            } else if found {
                *e = Yaml::Null; // 重复项，标记删除
                changed = true;
            } else {
                found = true;
                if !Self::entry_is_current(e, home) {
                    *e = Self::make_entry(home);
                    changed = true;
                }
            }
        }
        let mut i = 0usize;
        entries.retain(|e| {
            let ours = e.is_null() && !pre_nulls.get(i).copied().unwrap_or(true);
            i += 1;
            !ours
        });
        if !found {
            entries.push(Self::make_entry(home));
            changed = true;
        }

        // 4. 幂等：内容一致则不写回（serde_yaml 重写会丢用户注释，能不写就不写）
        if changed {
            if existed {
                jsonio::backup_once(&patch);
            }
            Self::write_patch_checked(&patch, &entries, &raw)?;
        }
        Ok(())
    }

    fn unregister_at(home: &Path, ctx: &InstallCtx) -> anyhow::Result<()> {
        let patch = Self::patch_file(home);
        // 解析失败就跳过，宁可留着自己的条目，也不破坏用户文件
        if let Ok((mut entries, raw)) = Self::read_patch_raw(&patch) {
            // insert 形式的混合条目只摘**子项**（§1.8，与 register 同款手术）：
            // 用户的子项与外层键原样保留，insert 列表删空才删整条；
            // 旧版顶层条目整条删除
            let mut removed_any = false;
            // 只删我们造的 null 哨兵；用户 patch 数组里合法的 null 元素原样保留
            let pre_nulls: Vec<bool> = entries.iter().map(|e| e.is_null()).collect();
            for e in entries.iter_mut() {
                if !Self::is_our_entry(e) {
                    continue;
                }
                if Self::entry_insert_seq(e).is_some() {
                    if Self::surgery_insert_entry(e, None, false) {
                        removed_any = true;
                    }
                } else {
                    *e = Yaml::Null;
                    removed_any = true;
                }
            }
            let mut i = 0usize;
            entries.retain(|e| {
                let ours = e.is_null() && !pre_nulls.get(i).copied().unwrap_or(true);
                i += 1;
                !ours
            });
            if removed_any && !ctx.dry_run {
                if ctx.backup {
                    jsonio::backup_once(&patch);
                }
                // 空数组也写回（"[]"），不删文件：patch 可能是用户创建的
                Self::write_patch_checked(&patch, &entries, &raw)?;
            }
        }
        if !ctx.dry_run {
            let plugin = Self::plugin_file(home);
            let _ = std::fs::remove_file(&plugin);
            // 目录是我们创建的，空了就顺手回收（失败忽略）
            if let Some(dir) = plugin.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        Ok(())
    }

    fn verify_at(home: &Path, _ctx: &RegisterCtx) -> VerifyReport {
        let patch = Self::patch_file(home);
        if !patch.exists() {
            return VerifyReport::NotRegistered;
        }
        let entries = match Self::read_patch(&patch) {
            Ok(e) => e,
            Err(e) => {
                return VerifyReport::ConfigUnreadable { path: patch, reason: e.to_string() }
            }
        };
        let ours: Vec<&Yaml> = entries.iter().filter(|e| Self::is_our_entry(e)).collect();
        match ours.as_slice() {
            [] => VerifyReport::NotRegistered,
            [only] => {
                if !Self::entry_is_current(only, home) {
                    // 只有旧路径条目 → 路径漂移（程序被移动/DSH_HOME 迁移），重新 register 可就地修复
                    return VerifyReport::StalePath { path: patch };
                }
                if Self::plugin_file(home).exists() {
                    VerifyReport::Ok
                } else {
                    // 登记了但插件文件丢了
                    VerifyReport::NotRegistered
                }
            }
            // 多条同 id 条目（异常状态）：重新 register 会去重修复
            _ => VerifyReport::StalePath { path: patch },
        }
    }
}

impl HookAdapter for DshAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Dsh
    }

    fn display_name(&self) -> &'static str {
        AgentKind::Dsh.display_name()
    }

    fn is_installed(&self) -> bool {
        Self::home().exists()
    }

    fn is_registered(&self) -> bool {
        let home = Self::home();
        if !Self::plugin_file(&home).exists() {
            return false;
        }
        Self::read_patch(&Self::patch_file(&home))
            .map(|entries| entries.iter().any(|e| Self::is_our_entry(e)))
            .unwrap_or(false)
    }

    fn config_paths(&self) -> Vec<PathBuf> {
        let home = Self::home();
        vec![Self::patch_file(&home), Self::plugin_file(&home)]
    }

    /// 插件监听的 DSH 事件名（UI 展示与文档用；事件由插件直接归一化 POST，
    /// 不经过 hook 子命令，event_kind 恒为 None）
    fn hook_events(&self) -> Vec<&'static str> {
        vec![
            "agent/turn-stopping",
            "agent/status",
            "agent/error",
            "approval/request",
            "user-questions/request",
            "session/event",
        ]
    }

    fn event_kind(&self, _event_name: &str) -> Option<EventKind> {
        None
    }

    fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()> {
        Self::register_at(&Self::home(), ctx)
    }

    fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()> {
        Self::unregister_at(&Self::home(), ctx)
    }

    fn verify(&self, ctx: &RegisterCtx) -> VerifyReport {
        Self::verify_at(&Self::home(), ctx)
    }

    fn normalize(&self, _event_name: &str, _raw: &serde_json::Value) -> Option<NormalizedEvent> {
        None
    }

    fn trust_hint(&self) -> Option<&'static str> {
        Some("接入或重新勾选后需要**重启 dsh**（实测运行中的 dsh 不会挂载 patch 新增条目，社区插件同样要求重启）。若重启后仍无通知：运行 dsh web --dump-config 确认 agent-bark 条目在列（不在列说明 patch 没挂上）。DSH 处于 Developer Preview，事件结构可能随版本变动")
    }

    fn unregister_hint(&self) -> Option<&'static str> {
        Some("已移除 patch 条目与插件文件；正在运行的 dsh 进程里插件仍在内存中，会继续发送事件直到重启 dsh。重启前 daemon 已按开关忽略 DeepSeek Harness 的通知")
    }
}

/// 生成的 Cordis 插件源码。
///
/// 模板用 `__PORT__` / `__TOKEN__` 占位符加 `replace` 注入，而不是 `format!`：
/// 插件 JS 里大量 `{}`（对象字面量、模板字符串）不必逐个写成 `{{`/`}}`，
/// 少一类容易写错、且出错后只在 DSH 侧静默失败的转义。
///
/// 纵深防御：token 经 serde_json 序列化为带转义的 JS 双引号字符串字面量，
/// 即使 token 含引号/反斜杠/控制字符也不会破坏生成的 JS
/// （serde_json 的字符串转义规则是 JS 字符串字面量的安全子集，与 opencode 插件同款处理）。
/// 先替换端口再替换 token，token 内容里恰好出现占位符时不会被二次替换。
fn plugin_js(port: u16, token: &str) -> String {
    let token_js = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".into());
    let template = r##"// Generated by agent-bark. Do not edit; re-register from agent-bark UI to refresh.
// 该文件含本地事件服务的 token（用于向 127.0.0.1 的 daemon 鉴权），请勿提交到版本库。
const ENDPOINT = "http://127.0.0.1:__PORT__/event";
const TOKEN = __TOKEN__;

/** 上报摘要的上限；daemon 侧还会再截到 200 字。 */
const SUMMARY_MAX = 300;

/** 正在运行的会话（status running 记入、idle 取出、disposed 清理）。 */
const running = new Set();
/**
 * 本回合的终态**已确认送达**的会话。一个回合结束会有两个信号（turn-stopping 与随后的
 * status idle），按 running→idle 周期去重、只报一次；不用时间窗口，避免把紧接着
 * 的下一个短回合的完成通知一起吞掉。
 *
 * 关键：它记的是「送达」而不是「发过」。终态 POST 失败/超时/非 2xx 时**不置位**，
 * 这样随后的 status idle 兜底还会再报一次——否则一次瞬时失败就让 daemon 的
 * 「运行中」条目挂到 10 分钟判死（用户实测：回合早已结束，流光仍是思考蓝）。
 */
const settledSessions = new Set();
/** 正在重投的终态通知：sessionId → { type, body, attempt, timer }。 */
const pendingTerminal = new Map();

/**
 * 终态投递的重试节奏：本地 daemon 正常毫秒级返回，失败只可能是瞬时的
 * （事件通道打满返回 503、daemon 正在重启、连接被拒）。1s→2s→4s→8s→16s
 * 共 5 次重投，覆盖约 31 秒——远小于 daemon 侧 10 分钟判死线，也远大于任何
 * 一次 GC / 建窗 / 主线程卡顿。全部失败后放弃：状态表最终由判死巡检收场。
 */
const TERMINAL_RETRY_BASE_MS = 1000;
const TERMINAL_RETRY_MAX_MS = 16000;
const TERMINAL_RETRY_ATTEMPTS = 5;
/**
 * 会话最近一次 turn/end 的结束原因。DSH 的取值集合（TurnEndReasonMap）是
 * `completed | aborted | blocked | error | max-tokens | interrupted`：
 * `interrupted` 只在崩溃修复/冷读时合成、live 不会出现；该类型可被插件扩展（merge-extensible），
 * 所以下面按「认识的显式处理、其余走兜底」写，不假设集合封闭。
 */
const lastReason = new Map();

function asText(value) {
  return typeof value === "string" && value !== "" ? value : "";
}

function sessionIdOf(agent, session) {
  const s = session || (agent && agent.session);
  return asText(agent && agent.id) || asText(s && s.id);
}

function cwdOf(agent, session) {
  const s = session || (agent && agent.session);
  const header = s && s.header;
  // 唯一来源是 session.header.cwd（Agent 上没有 cwd 字段，别再写那种兜底）
  return asText(header && header.cwd);
}

/**
 * 会话是否属于子代理：daemon 默认不为子代理发通知。
 *
 * 判据取 `header.origin === "subagent"` 或 `delegationDepth > 0`：DSH 权威口径是
 * `delegationDepthOf(agent) = max(header.delegationDepth ?? 0, agent.options.subagentDepth ?? 0)`
 * ——运行时深度「只能加深、不能降低」，只读 header 在冷 resume / 策略覆盖时可能偏低；
 * `origin` 是派生子代理时恒写的粗粒度标记，两者取或最稳（插件无依赖，不引入 dsh-subagent 包）。
 */
function isSubagentOf(agent, session) {
  const s = session || (agent && agent.session);
  const header = s && s.header;
  if (!header) return false;
  if (header.origin === "subagent") return true;
  const depth = header.delegationDepth;
  return typeof depth === "number" && depth > 0;
}

function isSettledOf(keys) {
  return keys.some((k) => settledSessions.has(k));
}

function markSettled(keys) {
  for (const k of keys) settledSessions.add(k);
}

/**
 * 会话记忆表的**双键**（§2.12 键分裂防御）：DSH 的 `agent.id` 与 `session.id`
 * 不保证相等——`sessionIdOf` 优先 `agent.id`，而 `session/event` 回调只拿得到
 * `session.id`。若按单键记忆，`turn/end` 记下的 reason 会与 idle 兜底查询用的键
 * 对不上 → idle 读不到 lastReason，max-tokens / aborted / error / blocked 分类
 * 全部失效，中止/出错回合被报成「任务完成」。所以 running / settledSessions /
 * lastReason / pendingTerminal 一律「两个 id 都登记、读取两键都查」。
 * （待确认 #4：拿到真实会话 dump 确认两 id 恒等后可简化回单键。）
 */
function idKeys(agent, session, primary) {
  const s = session || (agent && agent.session);
  const keys = [];
  for (const v of [primary, agent && agent.id, s && s.id]) {
    const t = asText(v);
    if (t && !keys.includes(t)) keys.push(t);
  }
  return keys;
}

/** lastReason 的两键查询（§2.12）：任一键上记过就用它 */
function reasonOf(keys) {
  for (const k of keys) {
    const v = asText(lastReason.get(k));
    if (v) return v;
  }
  return "";
}

/** 一条会话日志事件里的 assistant 文本（assistant/message 的 message.content text 块）。 */
function textOfEvent(event) {
  if (!event || event.type !== "assistant/message") return "";
  const blocks = (event.data && event.data.message && event.data.message.content) || [];
  if (!Array.isArray(blocks)) return "";
  const parts = [];
  for (const block of blocks) {
    if (block && block.type === "text" && typeof block.text === "string") parts.push(block.text);
  }
  return parts.join("\n").trim().slice(0, SUMMARY_MAX);
}

/**
 * `agent/error` 的 payload.error 在 DSH 里声明为 `unknown`（抛出的值原样透传），
 * 不是 Error 类型：字符串直接用、对象取 message、其余才 String()，
 * 避免普通对象被 String() 成一句没信息量的 "[object Object]"。
 */
function errorText(error) {
  if (error === undefined || error === null) return "未知错误";
  if (typeof error === "string") return error.slice(0, 200);
  if (typeof error === "object") {
    const message = asText(error.message);
    if (message) return message.slice(0, 200);
    try {
      const json = JSON.stringify(error);
      if (json && json !== "{}") return json.slice(0, 200);
    } catch {
      // 循环引用等：退回 String()
    }
  }
  return String(error).slice(0, 200);
}

/**
 * 最后一条 assistant 文本，作为「任务完成」的正文。
 * 优先按 seq 倒着读最近 60 条（不物化整份日志），再退回全量快照倒扫。
 * 注意 DSH 的 Session 没有 events 字段，只有 eventAt()/snapshotEvents()/ownEvents()。
 */
function lastAssistantText(session) {
  try {
    if (!session) return "";
    if (typeof session.eventAt === "function" && typeof session.seq === "number") {
      const floor = Math.max(0, session.seq - 60);
      for (let i = session.seq - 1; i >= floor; i -= 1) {
        const text = textOfEvent(session.eventAt(i));
        if (text) return text;
      }
    }
    let events = [];
    if (typeof session.snapshotEvents === "function") events = session.snapshotEvents() || [];
    else if (typeof session.ownEvents === "function") events = session.ownEvents() || [];
    for (let i = events.length - 1; i >= 0; i -= 1) {
      const text = textOfEvent(events[i]);
      if (text) return text;
    }
  } catch {
    // 日志结构随版本变动：取不到摘要不影响通知本身
  }
  return "";
}

/** 单次 POST 的超时（ms）：daemon 是本地进程，正常情况下毫秒级返回。
 * 没有超时的话，端口被某个「只接受连接不响应」的进程占用时，turn-stopping 这个
 * serial 监听会把 DSH 的回合收尾一起拖住（下面 await 它是为了让终态不丢）。 */
const POST_TIMEOUT_MS = 2000;

/** 事件体的公共字段（终态与心跳共用）。 */
function eventBody(f) {
  const o = f && typeof f === "object" ? f : {};
  const body = {
    // id 在**构造时**生成一次，重投（retryTerminal）复用同一个 id：首投「daemon
    // 已收下但响应丢了」时，重投会被 daemon 按 id 去重——那正是想要的（宁可被
    // 去重，也不能产生第二条通知）。反过来「每次投递都新生成 id」才会绕过 daemon
    // 去重、把一条终态变成两条通知。不同事件各自构造 body，id 互不相同。
    id: globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`,
    agent: "dsh",
    type: asText(o.type),
    session_id: asText(o.sessionId),
    cwd: asText(o.cwd),
    message: asText(o.message),
    timestamp: Date.now(),
    is_subagent: o.isSubagent === true,
  };
  const toolName = asText(o.toolName);
  if (toolName) body.tool_name = toolName;
  return body;
}

/**
 * 投递一条事件，返回是否**确认送达**（2xx）。
 *
 * 必须看响应状态：daemon 的事件通道打满时 axum 返回 503（见 bark-server 的
 * try_send 分支），此时请求「成功」但事件已被丢弃——不看状态码就会把一次
 * 丢失当成送达。
 */
async function deliver(body) {
  const options = {
    method: "POST",
    headers: { "content-type": "application/json", "x-bark-token": TOKEN },
    body: JSON.stringify(body),
  };
  // AbortSignal.timeout 需要 Node 17.3+ / 现代浏览器；取不到就退回无超时（老版本行为）
  if (typeof AbortSignal !== "undefined" && typeof AbortSignal.timeout === "function") {
    options.signal = AbortSignal.timeout(POST_TIMEOUT_MS);
  }
  try {
    const res = await fetch(ENDPOINT, options);
    return !!res && res.status >= 200 && res.status < 300;
  } catch {
    // daemon 未运行时静默失败，绝不阻塞 DSH
    return false;
  }
}

/** 重投延迟（第 n 次失败后）：1s、2s、4s、8s、16s。 */
function retryDelay(attempt) {
  return Math.min(TERMINAL_RETRY_MAX_MS, TERMINAL_RETRY_BASE_MS * 2 ** Math.max(0, attempt - 1));
}

/** 终态投递成功：置「本回合已终态」——随后的 status idle 兜底据此去重。 */
function finishTerminal(sessionId) {
  const job = pendingTerminal.get(sessionId);
  const keys = job ? job.keys : [sessionId];
  for (const k of keys) pendingTerminal.delete(k);
  markSettled(keys);
}

/** 重投一次待发终态；仍失败则排下一次，重试次数用尽就放弃（交给 daemon 判死收场）。 */
async function retryTerminal(sessionId) {
  const job = pendingTerminal.get(sessionId);
  if (!job) return; // 已被新回合取消 / 已送达
  job.timer = null;
  if (await deliver(job.body)) {
    if (pendingTerminal.get(sessionId) === job) finishTerminal(sessionId);
    return;
  }
  if (job.attempt >= TERMINAL_RETRY_ATTEMPTS) {
    pendingTerminal.delete(sessionId);
    return;
  }
  job.attempt += 1;
  job.timer = setTimeout(() => {
    void retryTerminal(sessionId);
  }, retryDelay(job.attempt));
}

/**
 * 投递一条终态（run_completed / run_failed / run_aborted）并等它有着落。
 *
 * 与心跳的本质区别：心跳丢了下一跳会补上，终态**只报一次**——丢了 daemon 就再也
 * 不知道这个回合结束了（「运行中」条目挂到判死、流光停在思考蓝，用户实测过）。
 * 因此这里必须等结果：首次投递失败就进重投队列，成功了才置「已终态」。
 * 返回 Promise 是为了让 turn-stopping 这个 serial 监听把首投等完（不阻塞 DSH 收尾，
 * 只保证「报过没有」这件事在回合收尾前有确定结论）。
 */
async function settle(type, agent, session, fields) {
  const f = fields && typeof fields === "object" ? fields : {};
  const sessionId = asText(f.sessionId) || sessionIdOf(agent, session);
  // 双键（§2.12）：settled / pending 的查、记都覆盖 agent.id 与 session.id
  const keys = idKeys(agent, session, sessionId);
  if (!keys.length || isSettledOf(keys) || keys.some((k) => pendingTerminal.has(k))) return;
  // 先算完 cwd / 子代理标记再入队：取字段抛异常时（结构随版本变动）
  // 不该把本回合标记成已上报——那样随后的 idle 兜底也会被跳过，一条通知都不剩。
  const body = eventBody({
    type,
    sessionId,
    cwd: cwdOf(agent, session),
    message: asText(f.message),
    isSubagent: isSubagentOf(agent, session),
  });
  const job = { body, attempt: 0, timer: null, keys };
  for (const k of keys) pendingTerminal.set(k, job);
  if (await deliver(body)) {
    finishTerminal(sessionId);
    return;
  }
  const pending = pendingTerminal.get(sessionId);
  if (!pending) return; // 首次投递在途时被新回合取消
  pending.attempt = 1;
  pending.timer = setTimeout(() => {
    void retryTerminal(sessionId);
  }, retryDelay(1));
}

/**
 * 新回合开始：作废上一回合的终态状态。
 *
 * 两件事：
 * 1. 清掉「本回合已报过终态」——去重按回合（不是按时间窗），同一会话紧接着的
 *    下一个短回合必须能照常报完成；
 * 2. 取消还挂着的重投——它属于上一个回合，此时重投只会把刚开始的新回合从
 *    daemon 状态表里误删。
 *
 * **不能**在这里顺手置「已终态」来堵住 idle 兜底：那条兜底正是本回合终态丢失时
 * 的补救路径，堵住它就等于把「POST 失败 → 永久挂运行中」这个 bug 原样搬回来
 * （本文件的 Node 回归测试 case 5 就是照这条教训写的）。
 *
 * 代价（有意为之）：上一回合那次投递在重投窗口内彻底失败、并且用户立刻又发了
 * 一轮时，那一条完成通知会丢——此时 daemon 侧一轮都没见过「运行中 → 结束」的
 * 闭环，比收到一条把新回合说成已完成的假通知要好。
 */
function beginTurn(keys) {
  for (const k of keys) settledSessions.delete(k);
  const job = keys.map((k) => pendingTerminal.get(k)).find(Boolean);
  if (job) {
    if (job.timer !== null) clearTimeout(job.timer);
    for (const k of job.keys) pendingTerminal.delete(k);
  }
}

/** 心跳（Activity）：丢了不影响正确性（下一跳会补），投一次、失败补一次就够。 */
function postActivity(fields) {
  const f = fields && typeof fields === "object" ? fields : {};
  const body = eventBody({
    type: "activity",
    sessionId: asText(f.sessionId),
    cwd: asText(f.cwd),
    toolName: asText(f.toolName),
  });
  void deliver(body).then((ok) => {
    if (ok) return;
    setTimeout(() => {
      void deliver(body);
    }, 500);
  });
}

export const name = "agent-bark";

/** 监听器统一兜底：agent-bark 只是观察者，任何异常都不能打断 DSH 的回合/审批/提问。 */
function observe(run) {
  try {
    return run();
  } catch {
    return undefined;
  }
}

export function apply(ctx) {
  // 1) 回合自然结束（serial 事件）：返回 Promise 让 DSH 等**首投**走完——收尾前就把
  //    「这一回合报过终态没有」定下来（首投失败会进重投队列，不阻塞收尾）。
  ctx.on("agent/turn-stopping", (payload) => observe(() => {
    const agent = payload && payload.agent;
    const session = agent && agent.session;
    const turn = payload && payload.turn;
    return settle("run_completed", agent, session, {
      message: lastAssistantText(session) || (typeof turn === "number" ? `回合 ${turn} 结束` : ""),
    });
  }));

  // 2) 运行状态：running → 心跳（只发根会话），让 agent-bark 的会话面板/流光看得到 DSH；
  //    running→idle 且本回合没被 turn-stopping 报过 → 兜底补一条终态（外部中止 / 被拦等）。
  ctx.on("agent/status", (payload) => observe(() => {
    const agent = payload && payload.agent;
    const session = agent && agent.session;
    const sessionId = sessionIdOf(agent, session);
    // 双键（§2.12）：本回调拿的是 agent.id 优先的键，session/event 记 reason 用的
    // 是 session.id——两键都登记/都查才不会键分裂
    const keys = idKeys(agent, session, sessionId);
    if (!keys.length) return;
    if (payload.status === "running") {
      for (const k of keys) running.add(k);
      // 新回合开始：允许本回合再报一次终态，并作废上一回合还挂着的待投终态
      beginTurn(keys);
      if (!isSubagentOf(agent, session)) {
        postActivity({ sessionId, cwd: cwdOf(agent, session) });
      }
      return;
    }
    if (payload.status !== "idle") return;
    let wasRunning = false;
    for (const k of keys) {
      if (running.delete(k)) wasRunning = true;
    }
    if (!wasRunning || isSettledOf(keys)) return;
    const reason = reasonOf(keys);
    if (reason === "aborted" || reason === "interrupted") {
      // 用户主动中止：既不是完成（不亮完成色、不谎报跑完）也不是失败（不亮失败色、不弹「任务失败」）
      void settle("run_aborted", agent, session, { sessionId, message: "回合已中止" });
      return;
    }
    if (reason === "error" || reason === "blocked") {
      void settle("run_failed", agent, session, {
        sessionId,
        message: reason === "blocked" ? "回合被拦截" : "回合出错",
      });
      return;
    }
    // max-tokens：至少一个 step 撞到输出 token 上限，回合结束了但任务是被截断的，
    // 不能报「任务完成」（否则用户以为结果完整）。其余（completed 及未来新增类型）按完成。
    if (reason === "max-tokens") {
      const text = lastAssistantText(session);
      // 标记放**前缀**：daemon 侧正文只截前 200 字，放后缀会被截掉
      void settle("run_completed", agent, session, {
        sessionId,
        message: text ? `〔达到输出上限〕${text}` : "回合达到输出上限结束",
      });
      return;
    }
    void settle("run_completed", agent, session, { sessionId, message: lastAssistantText(session) });
  }));

  // 3) 回合/步骤出错（emit）：报失败（走终态投递，失败会重投），
  //    并让随后的 idle 不再报一条「完成」。
  ctx.on("agent/error", (payload) => observe(() => {
    const agent = payload && payload.agent;
    const session = agent && agent.session;
    void settle("run_failed", agent, session, {
      sessionId: sessionIdOf(agent, session),
      message: errorText(payload && payload.error),
    });
  }));

  // 4) 等待用户授权（waterfall：观察完必须 next()，否则会把请求吞掉）。
  ctx.on("approval/request", (request, next) => {
    try {
      const agent = request && request.agent;
      const session = agent && agent.session;
      const tool = asText(request && request.toolName) || "工具";
      const reason = asText(request && request.reason);
      void post("permission_required", {
        sessionId: sessionIdOf(agent, session),
        cwd: cwdOf(agent, session),
        message: reason ? `${tool}：${reason}` : `${tool} 等待授权`,
        isSubagent: isSubagentOf(agent, session),
      });
    } catch {
      // 观察者绝不改变审批结果
    }
    return next();
  });

  // 5) 等待用户回答（waterfall：同上，观察完必须 next()）。
  ctx.on("user-questions/request", (request, next) => {
    try {
      const agent = request && request.agent;
      const session = agent && agent.session;
      const questions = (request && request.questions) || [];
      const first = (Array.isArray(questions) ? questions[0] : undefined) || {};
      const header = asText(first.header);
      const question = asText(first.question);
      void post("input_required", {
        sessionId: sessionIdOf(agent, session),
        cwd: cwdOf(agent, session),
        message: header && question ? `${header}：${question}` : question || header,
        isSubagent: isSubagentOf(agent, session),
      });
    } catch {
      // 观察者绝不改变提问结果
    }
    return next();
  });

  // 6) 会话日志（session/event(session, event)，emit）：tool/call 发心跳（带 tool_name，
  //    面板显示「执行工具」），并记下 turn/end 的 reason 供 idle 兜底分类。
  ctx.on("session/event", (session, event) => {
    try {
      const type = event && event.type;
      if (type === "turn/end") {
        const sessionId = sessionIdOf(undefined, session);
        // 双键登记（§2.12）：这里只有 session.id，而 idle 兜底可能按 agent.id 查
        const keys = idKeys(undefined, session, sessionId);
        if (keys.length) {
          const reason = event.data && event.data.reason;
          const kind = asText(reason && reason.kind);
          for (const k of keys) lastReason.set(k, kind);
        }
        return;
      }
      if (type !== "tool/call" || isSubagentOf(undefined, session)) return;
      const sessionId = sessionIdOf(undefined, session);
      if (!sessionId) return;
      postActivity({
        sessionId,
        cwd: cwdOf(undefined, session),
        toolName: asText(event.data && event.data.name),
      });
    } catch {
      // 日志结构随版本变动，观察者绝不打断会话
    }
  });

  // 7) 会话销毁：清掉四张表，避免长驻 dsh 进程里无界增长。
  //    正在重投的终态一并取消——会话都没了，重投只会往 daemon 塞一条无主事件。
  ctx.on("agent/disposed", (payload) => observe(() => {
    const agent = payload && payload.agent;
    const sessionId = sessionIdOf(agent, agent && agent.session);
    const keys = idKeys(agent, agent && agent.session, sessionId);
    if (!keys.length) return;
    // 四张表都按双键清（§2.12）：只清一个键会留下孤儿记忆
    for (const k of keys) {
      running.delete(k);
      settledSessions.delete(k);
      lastReason.delete(k);
    }
    const job = keys.map((k) => pendingTerminal.get(k)).find(Boolean);
    if (job) {
      if (job.timer !== null) clearTimeout(job.timer);
      for (const k of job.keys) pendingTerminal.delete(k);
    }
  }));
}
"##;
    template
        .replace("__PORT__", &port.to_string())
        .replace("__TOKEN__", &token_js)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> RegisterCtx {
        RegisterCtx { exe_path: String::new(), port: 1234, token: "tok".into() }
    }

    fn tmp_home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-bark-dsh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn generated_plugin_shape() {
        let js = plugin_js(1234, "tok");
        assert!(js.contains("const ENDPOINT = \"http://127.0.0.1:1234/event\";"));
        assert!(js.contains("const TOKEN = \"tok\";"));
        assert!(js.contains("x-bark-token"));
        assert!(js.contains("agent: \"dsh\""));
        assert!(js.contains("export const name = \"agent-bark\""));
        // 模板占位符必须全部被替换（漏掉一个会让插件在 DSH 侧静默失效）
        assert!(!js.contains("__PORT__") && !js.contains("__TOKEN__"));
        // 占位符替换不得再触发 `{{`/`}}` 那类 format! 转义（改用 replace 后花括号是字面量）
        assert!(
            js.contains("const body = {") && js.contains("JSON.stringify(body)"),
            "花括号必须是字面量: {js}"
        );
        assert!(!js.contains("{{"), "生成物里不应出现转义后的双花括号");

        // 事件面：终态 + 兜底 + 出错 + 两类等待 + 心跳/日志
        for event in [
            "agent/turn-stopping",
            "agent/status",
            "agent/error",
            "approval/request",
            "user-questions/request",
            "session/event",
            "agent/disposed",
        ] {
            assert!(
                js.contains(&format!("ctx.on(\"{event}\"")),
                "缺少 {event} 监听: {js}"
            );
        }
        // 归一化后的 type 必须都在 bark-core 的 EventKind 里（snake_case）
        for kind in [
            "run_completed",
            "run_failed",
            "activity",
            "permission_required",
            "input_required",
        ] {
            assert!(js.contains(&format!("\"{kind}\"")), "缺少事件类型 {kind}");
        }
        // 正文取真实字段：session.header.cwd + assistant/message 的 message.content
        assert!(
            js.contains("header && header.cwd"),
            "cwd 必须取 session.header.cwd"
        );
        assert!(
            js.contains("event.data.message.content")
                || js.contains("event.data && event.data.message")
        );
        assert!(js.contains("assistant/message"));
        // 子代理判定与心跳字段：origin 与 delegationDepth 取或（后者在冷 resume 时可能偏低）
        assert!(
            js.contains("header.origin === \"subagent\"") && js.contains("delegationDepth"),
            "子代理判定要同时看 origin 与 delegationDepth"
        );
        assert!(js.contains("is_subagent"));
        assert!(js.contains("tool_name"));
        // Agent 上没有 cwd 字段：不得再出现 agent.cwd 这类无效兜底
        assert!(!js.contains("agent.cwd"), "cwd 只能取 session.header.cwd");
        // turn/end 的 reason 分类：completed 显式算成功、aborted/error/blocked/max-tokens 各有分支
        assert!(
            js.contains("reason === \"max-tokens\""),
            "max-tokens 必须单独处理（否则会被报成「任务完成」）"
        );
        // 用户主动中止必须是独立的「中止」语义：报完成会亮完成色（谎报跑完），
        // 报失败会亮失败色并弹「任务失败」（用户自己按的停止）
        assert!(
            js.contains("void settle(\"run_aborted\""),
            "aborted/interrupted 要归一成 run_aborted"
        );
        // 真实的 TurnEndReason 集合必须在注释里留档（含只出现在修复路径的 interrupted）
        for kind in ["completed", "aborted", "blocked", "max-tokens", "interrupted"] {
            assert!(js.contains(kind), "缺少 TurnEndReason {kind}");
        }
        assert!(!js.contains("finished"), "TurnEndReason 里没有 finished");
        // POST 超时：serial 监听里 await 的 fetch 不能无限期挂住 DSH 的回合收尾
        assert!(js.contains("AbortSignal.timeout(POST_TIMEOUT_MS)"));
        // error 是 unknown：普通对象不能退化成 "[object Object]"
        assert!(js.contains("function errorText(error)") && js.contains("JSON.stringify(error)"));
        // waterfall 观察者必须把请求交还下去，否则会吞掉审批/提问
        assert_eq!(
            js.matches("return next();").count(),
            2,
            "两个 waterfall 监听都要 next()"
        );
        // 终态去重：turn-stopping 与 status idle 前后脚到，按 running→idle 周期只报一次
        assert!(js.contains("settledSessions") && js.contains("isSettledOf("));
    }

    /// 终态投递可靠性的结构约束。
    ///
    /// 回归的是用户实测的「回合早已结束、流光仍是思考蓝、面板卡在『执行工具』」：
    /// 旧的 `settle()` 先 `markSettled()` 再 fire-and-forget POST，一次瞬时失败
    /// （事件通道打满 503 / daemon 重启 / 连接被拒）就把这条终态永久丢掉，idle 兜底
    /// 也被「已终态」挡住，daemon 的「运行中」条目只能等 10 分钟判死。
    /// 这里锁住修复后的三条不变量：看状态码、失败重投、送达才置位。
    #[test]
    fn generated_plugin_terminal_delivery_is_acknowledged_and_retried() {
        let js = plugin_js(1234, "tok");

        // 1) 必须看响应状态：daemon 事件通道打满时返回 503，请求「成功」但事件已丢
        assert!(
            js.contains("res.status >= 200 && res.status < 300"),
            "投递必须按 2xx 判定成功（503 不能当送达）: {js}"
        );
        // 2) 终态失败必须重投，且重投次数有上限（不能无限占着定时器）
        assert!(
            js.contains("TERMINAL_RETRY_ATTEMPTS") && js.contains("retryTerminal(sessionId)"),
            "终态失败必须进重投队列: {js}"
        );
        assert!(
            js.contains("TERMINAL_RETRY_BASE_MS") && js.contains("setTimeout("),
            "重投必须退避延时，不能忙等: {js}"
        );
        // 3) 只有确认送达才置「本回合已终态」——否则 idle 兜底会被误挡
        let finish = js
            .split("function finishTerminal(sessionId)")
            .nth(1)
            .expect("缺少 finishTerminal");
        let finish_body = finish.split("}").next().unwrap_or("");
        assert!(
            finish_body.contains("markSettled(keys)"),
            "markSettled 只能在送达路径上（双键登记）: {finish_body}"
        );
        let settle_body = js
            .split("async function settle(")
            .nth(1)
            .expect("缺少 settle");
        let settle_head = settle_body.split("pendingTerminal.set").next().unwrap_or("");
        assert!(
            !settle_head.contains("markSettled"),
            "settle 不得在投递前就标记已终态: {settle_head}"
        );
        // 4) 心跳与终态走两条路：心跳丢了下一跳会补，不该占重投队列
        assert!(
            js.contains("function postActivity(fields)"),
            "心跳要独立成 postActivity（best-effort）: {js}"
        );
        assert!(
            !js.contains("post(\"activity\""),
            "旧的 fire-and-forget post 调用必须全部替换掉"
        );
        // 5) 重投复用事件 id（retryTerminal 重投的是同一个 body）：daemon 按 id
        //    去重，「首投已收下但响应丢了」的重投会被去重掉，绝不产生第二条通知。
        //    投递中途重新生成 id 才会绕过去重、双发通知。
        assert!(
            !js.contains("body.id = ") && js.contains("id: globalThis.crypto?.randomUUID?.()"),
            "事件 id 只能在构造 body 时生成一次、重投复用: {js}"
        );
        // 6) 新回合开始要作废上一回合的待投终态，否则迟到的终态会把新回合从状态表里误删
        assert!(
            js.contains("function beginTurn(keys)") && js.contains("clearTimeout(job.timer)"),
            "新回合必须取消上一回合的待投终态: {js}"
        );
    }

    /// 生成插件的**行为**回归：用 Node 直接跑产物，把 daemon 换成假 fetch，
    /// 覆盖「终态首投失败 → 必须重投 / idle 兜底必须补报 / 新回合必须取消迟到终态」。
    ///
    /// 为什么真跑而不是只断言字符串：投递可靠性是**时序**问题（fail → 排定时器 →
    /// 重投成功 → 置已终态），字符串断言锁不住「先 markSettled 再发」这类顺序错误，
    /// 而那正是线上丢终态的成因。
    ///
    /// Node 缺失时跳过：这个包本身不依赖 Node，缺了它只少一层保险，不该让 CI 红。
    #[test]
    fn generated_plugin_terminal_delivery_survives_daemon_failures() {
        let dir = std::env::temp_dir().join(format!("agent-bark-plugin-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let plugin = dir.join("plugin.js");
        std::fs::write(&plugin, plugin_js(1, "tok")).unwrap();
        let sim = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("plugin_sim.mjs");
        assert!(sim.exists(), "缺少插件事务级测试脚本: {}", sim.display());

        let output = match std::process::Command::new("node").arg(&sim).arg(&plugin).output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("跳过：找不到 node（{e}），无法运行插件投递行为测试");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 9009（Windows）/ 127（POSIX）= node 不存在，同上跳过
        if matches!(output.status.code(), Some(9009) | Some(127)) {
            eprintln!("跳过：node 不可执行，无法运行插件投递行为测试");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            output.status.success(),
            "插件投递行为测试失败：\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
    }

    #[test]
    fn placeholder_replacement_order_keeps_token_intact() {
        // token 里恰好含 __PORT__ 时，先替换端口再注入 token，token 不会被二次替换
        let js = plugin_js(7, "__PORT__");
        assert!(js.contains("const ENDPOINT = \"http://127.0.0.1:7/event\";"));
        assert!(js.contains("const TOKEN = \"__PORT__\";"), "{js}");
    }

    #[test]
    fn token_is_js_escaped() {
        // token 含引号/反斜杠时，生成的 JS 字符串字面量必须仍然合法
        let js = plugin_js(1, "a\"b\\c");
        assert!(
            js.contains("const TOKEN = \"a\\\"b\\\\c\";"),
            "token 必须转义: {js}"
        );
    }

    #[test]
    fn register_restores_a_stale_or_tampered_plugin_file() {
        // 启动对齐会重跑 register：插件文件被改坏 / 停留在旧版本模板时都必须恢复。
        // （内容一致时 register_at 不写盘，这条由内容比较保证，见第 2 步注释）
        let home = tmp_home();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let plugin = DshAdapter::plugin_file(&home);
        let current = std::fs::read_to_string(&plugin).unwrap();

        std::fs::write(&plugin, "// 旧版本生成的插件\n").unwrap();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), current, "内容不一致必须重写");

        // 再跑一次：登记与内容都稳定，不产生重复条目
        DshAdapter::register_at(&home, &ctx()).unwrap();
        assert_eq!(std::fs::read_to_string(&plugin).unwrap(), current);
        let entries = DshAdapter::read_patch(&DshAdapter::patch_file(&home)).unwrap();
        assert_eq!(entries.len(), 1, "刷新不得追加条目");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn null_sentinel_cleanup_keeps_user_null_elements() {
        // 重复项清理只删我们造的 null 哨兵（§4.15）：用户 patch 里合法的 null 元素
        // 原样保留（blanket retain 会把它们一并误删）
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::create_dir_all(patch.parent().unwrap()).unwrap();
        let body = serde_yaml::to_string(&Yaml::Sequence(vec![
            Yaml::Null,
            DshAdapter::make_entry(&home),
            DshAdapter::make_entry(&home), // 重复项：register 应把它收敛掉
            Yaml::Null,
        ]))
        .unwrap();
        std::fs::write(&patch, body).unwrap();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let (entries, _) = DshAdapter::read_patch_raw(&patch).unwrap();
        assert_eq!(entries.len(), 3, "只删 1 条重复哨兵: {entries:?}");
        assert!(entries[0].is_null() && entries[2].is_null(), "用户 null 原样保留: {entries:?}");
        assert_eq!(
            entries.iter().filter(|e| DshAdapter::is_our_entry(e)).count(),
            1,
            "重复的我们的条目收敛为一条: {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn register_writes_plugin_and_patch_then_idempotent() {
        let home = tmp_home();
        DshAdapter::register_at(&home, &ctx()).unwrap();

        let plugin = DshAdapter::plugin_file(&home);
        assert!(plugin.exists(), "插件文件必须生成");
        let entries = DshAdapter::read_patch(&DshAdapter::patch_file(&home)).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(DshAdapter::entry_is_current(&entries[0], &home));

        // 幂等：重复注册不新增条目（且不应再改动文件）
        let before = std::fs::read_to_string(DshAdapter::patch_file(&home)).unwrap();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let after = std::fs::read_to_string(DshAdapter::patch_file(&home)).unwrap();
        assert_eq!(before, after, "幂等注册不应重写 patch");
        let entries = DshAdapter::read_patch(&DshAdapter::patch_file(&home)).unwrap();
        assert_eq!(entries.len(), 1, "不得重复追加条目");

        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::Ok));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn register_preserves_user_entries() {
        let home = tmp_home();
        // 用户已有自己的 patch 条目（含注释会丢，但条目本身必须保留）
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(&patch, "- id: user-plugin\n  name: '@scope/dsh-user-plugin'\n").unwrap();

        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 2, "用户条目 + 我们的条目");
        assert!(DshAdapter::entry_str(&entries[0], "id").as_deref() == Some("user-plugin"));
        assert!(DshAdapter::is_our_entry(&entries[1]));

        // 卸载后用户条目原样保留，我们的条目消失
        let mut ictx = InstallCtx { exe_path: String::new(), dry_run: false, backup: true };
        DshAdapter::unregister_at(&home, &mut ictx).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(DshAdapter::entry_str(&entries[0], "id").as_deref() == Some("user-plugin"));
        assert!(!DshAdapter::plugin_file(&home).exists(), "插件文件应被删除");
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::NotRegistered));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn malformed_patch_aborts_without_overwriting() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        let original = "{ 这不是合法 yaml";
        std::fs::write(&patch, original).unwrap();

        let err = DshAdapter::register_at(&home, &ctx()).expect_err("非法 YAML 必须中止");
        assert!(format!("{err:#}").contains("不是合法的 patch YAML"));
        assert_eq!(std::fs::read_to_string(&patch).unwrap(), original, "文件不能被改写");
        assert!(!DshAdapter::plugin_file(&home).exists(), "中止时不得留下插件文件");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_home_is_an_error() {
        let home = std::env::temp_dir().join(format!("agent-bark-dsh-none-{}", uuid::Uuid::new_v4().simple()));
        let err = DshAdapter::register_at(&home, &ctx()).expect_err("DSH_HOME 不存在必须报错");
        assert!(format!("{err:#}").contains("未检测到 DeepSeek Harness"));
        assert!(!home.exists(), "不得凭空创建 DSH_HOME");
    }

    #[test]
    fn empty_patch_file_is_treated_as_empty_list() {
        // 官方文档：patch 初始为空文件是常态
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(&patch, "   \n").unwrap();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stale_path_is_reported_and_repaired_in_place() {
        let home = tmp_home();
        DshAdapter::register_at(&home, &ctx()).unwrap();

        // 模拟 DSH_HOME 迁移（或用户手改）导致 insert 条目的 name 指向旧路径
        let patch = DshAdapter::patch_file(&home);
        let stale = format!("- insert:\n  - id: {ENTRY_ID}\n    name: 'C:/old/location/agent-bark/plugin.js'\n");
        std::fs::write(&patch, &stale).unwrap();
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::StalePath { .. }));

        // 重新 register 就地修复，不新增条目
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::Ok));

        // 旧版错误写法（顶层 id、无 insert）：同样报漂移并就地升级为 insert 形式
        let legacy = format!("- id: {ENTRY_ID}\n  name: 'C:/old/location/agent-bark/plugin.js'\n");
        std::fs::write(&patch, &legacy).unwrap();
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::StalePath { .. }));
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(DshAdapter::entry_insert_seq(&entries[0]).is_some(), "旧格式必须被升级为 insert 形式");
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::Ok));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn registered_but_plugin_missing_is_not_registered() {
        // 登记了但插件文件丢了：修复路径是重新 register（重写插件文件）
        let home = tmp_home();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        std::fs::remove_file(DshAdapter::plugin_file(&home)).unwrap();
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::NotRegistered));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unregister_dry_run_writes_nothing() {
        let home = tmp_home();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let patch = DshAdapter::patch_file(&home);
        let bak = jsonio::backup_path(&patch);
        let _ = std::fs::remove_file(&bak); // 清掉 register 留下的备份便于观察
        let before = std::fs::read_to_string(&patch).unwrap();

        let mut ictx = InstallCtx { exe_path: String::new(), dry_run: true, backup: true };
        DshAdapter::unregister_at(&home, &mut ictx).unwrap();
        assert_eq!(std::fs::read_to_string(&patch).unwrap(), before, "dry_run 不得改文件");
        assert!(!bak.exists(), "dry_run 不得创建备份");
        assert!(DshAdapter::plugin_file(&home).exists(), "dry_run 不得删插件文件");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn generated_entry_uses_insert_semantics() {
        // DSH patch 语义：顶层 {id, name} 是覆盖既有条目，id 不存在时跳过；
        // 新增插件必须是 insert 形式，否则插件永远不会被挂载
        let home = tmp_home();
        let entry = DshAdapter::make_entry(&home);
        let text = serde_yaml::to_string(&vec![entry.clone()]).unwrap();
        assert!(text.contains("insert:"), "必须是 insert 补丁: {text}");
        assert!(text.contains(&format!("id: {ENTRY_ID}")), "{text}");
        assert!(
            DshAdapter::entry_str(&entry, "id").is_none(),
            "外层不得带 id（外层 id 会把 insert 定向到某个 group）"
        );
        assert!(DshAdapter::entry_is_current(&entry, &home));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn our_entry_detection_by_id() {
        // 现行 insert 格式：insert 列表内含我们的 id 即归我们（寻址）
        let insert_entry = |id: &str, name: &str| {
            serde_yaml::from_str::<Yaml>(&format!("insert:\n  - id: {id}\n    name: {name}\n")).unwrap()
        };
        assert!(DshAdapter::is_our_entry(&insert_entry("agent-bark", "'/any/path/plugin.js'")));
        assert!(!DshAdapter::is_our_entry(&insert_entry("user-plugin", "'/x/agent-bark/plugin.js'")));

        // 旧版错误写法（顶层 id、无 insert）也算我们的：register 时就地修复
        let legacy = |id: &str, name: &str| {
            serde_yaml::from_str::<Yaml>(&format!("id: {id}\nname: {name}\n")).unwrap()
        };
        let home = tmp_home();
        let legacy_ours = legacy("agent-bark", "'/any/path/plugin.js'");
        assert!(DshAdapter::is_our_entry(&legacy_ours));
        assert!(
            !DshAdapter::entry_is_current(&legacy_ours, &home),
            "旧格式即使路径碰巧一致也不算有效接入（DSH 根本不会挂载它）"
        );
        assert!(!DshAdapter::is_our_entry(&legacy("user-plugin", "'/x/agent-bark/plugin.js'")));

        // 非映射条目（如字符串）不算
        assert!(!DshAdapter::is_our_entry(&Yaml::String("agent-bark".into())));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn json_payload_fields_match_normalization_contract() {
        // 插件 POST 的 body 必须能被反序列化回 NormalizedEvent（bark-server 的契约）
        let body = json!({
            "id": "x", "agent": "dsh", "type": "run_completed",
            "session_id": "s", "cwd": "C:/p", "message": "done", "timestamp": 1,
        });
        let ev: NormalizedEvent = serde_json::from_value(body).unwrap();
        assert_eq!(ev.agent, "dsh");
        assert_eq!(ev.kind, EventKind::RunCompleted);
    }

    // ---- §1.8 混合 insert 条目：子项级手术（用户配置保护） --------------------

    /// 用户把多个插件合并进同一 insert 列表：register / unregister 后
    /// `insert: [{id: agent-bark,...},{id: user-plugin,...}]` 的用户子项**原样保留**
    #[test]
    fn mixed_insert_keeps_user_subitems_through_register_and_unregister() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        let mixed = format!(
            "- insert:\n  - id: {ENTRY_ID}\n    name: 'C:/old/location/agent-bark/plugin.js'\n  - id: user-plugin\n    name: '@scope/dsh-user-plugin'\n"
        );
        std::fs::write(&patch, &mixed).unwrap();

        // register：只修我们的子项的 name，用户的子项与外层键原样保留
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1, "混合条目仍是一条（不得整条替换/新增）");
        let seq = DshAdapter::entry_insert_seq(&entries[0]).expect("insert 形式保留");
        assert_eq!(seq.len(), 2, "用户的子项必须原样保留（§1.8），实际 {}", seq.len());
        assert_eq!(DshAdapter::entry_str(&seq[1], "id").as_deref(), Some("user-plugin"));
        assert_eq!(
            DshAdapter::entry_str(&seq[1], "name").as_deref(),
            Some("@scope/dsh-user-plugin"),
            "用户子项内容逐字不动"
        );
        assert_eq!(DshAdapter::entry_str(&seq[0], "id").as_deref(), Some(ENTRY_ID));
        assert!(DshAdapter::entry_is_current(&entries[0], &home), "我们的子项就地修复到当前路径");
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::Ok));

        // unregister：只摘我们的子项，insert 列表还有用户子项 → 整条保留
        let mut ictx = InstallCtx { exe_path: String::new(), dry_run: false, backup: true };
        DshAdapter::unregister_at(&home, &mut ictx).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1, "用户子项还在 → 外层条目保留");
        let seq = DshAdapter::entry_insert_seq(&entries[0]).expect("insert 形式保留");
        assert_eq!(seq.len(), 1, "只剩用户的子项");
        assert_eq!(DshAdapter::entry_str(&seq[0], "id").as_deref(), Some("user-plugin"));
        assert_eq!(
            DshAdapter::entry_str(&seq[0], "name").as_deref(),
            Some("@scope/dsh-user-plugin")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 混合条目里我们的子项就地修复 name 时，子项上的**其它键**（用户的定制）
    /// 与外层键必须原样保留
    #[test]
    fn mixed_insert_repairs_only_the_name_key_of_our_subitem() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(
            &patch,
            "- insert:\n  - id: agent-bark\n    name: 'C:/old/agent-bark/plugin.js'\n    enabled: false\n    note: user-tuned\n",
        )
        .unwrap();

        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        let seq = DshAdapter::entry_insert_seq(&entries[0]).unwrap();
        assert_eq!(seq.len(), 1);
        let sub = &seq[0];
        assert_eq!(
            DshAdapter::entry_str(sub, "name").map(|n| n.replace('\\', "/")),
            Some(DshAdapter::plugin_path_str(&home)),
            "name 修到当前路径"
        );
        // 只动 name：用户加的其它键原样保留
        let text = serde_yaml::to_string(sub).unwrap();
        assert!(text.contains("enabled: false"), "用户的键不得被抹掉: {text}");
        assert!(text.contains("note: user-tuned"), "用户的键不得被抹掉: {text}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// insert 列表**只剩我们的子项**时，unregister 摘掉子项后整条删除（不留空壳）
    #[test]
    fn mixed_insert_drops_entry_only_when_insert_seq_empties() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(
            &patch,
            "- id: user-plugin\n  name: '@u/p'\n- insert:\n  - id: agent-bark\n    name: 'C:/old/agent-bark/plugin.js'\n",
        )
        .unwrap();

        let mut ictx = InstallCtx { exe_path: String::new(), dry_run: false, backup: true };
        DshAdapter::unregister_at(&home, &mut ictx).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1, "只剩用户条目（空 insert 的外壳已删）");
        assert_eq!(DshAdapter::entry_str(&entries[0], "id").as_deref(), Some("user-plugin"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 多个混合条目都含我们的子项：register 去重只留首个，用户子项两边都保留
    #[test]
    fn mixed_insert_duplicates_collapse_to_one_our_subitem() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(
            &patch,
            "- insert:\n  - id: agent-bark\n    name: 'C:/old/a/plugin.js'\n  - id: user-a\n    name: '@u/a'\n- insert:\n  - id: agent-bark\n    name: 'C:/old/b/plugin.js'\n  - id: user-b\n    name: '@u/b'\n",
        )
        .unwrap();

        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 2, "两条混合条目都保留（各自带着用户子项）");
        let seq_a = DshAdapter::entry_insert_seq(&entries[0]).unwrap();
        let seq_b = DshAdapter::entry_insert_seq(&entries[1]).unwrap();
        assert_eq!(seq_a.len(), 2, "首个条目：我们的 + 用户的");
        assert_eq!(
            seq_b.len(),
            1,
            "第二个条目里重复的我们的子项被去重，用户的保留（§1.8/§1.9）"
        );
        assert_eq!(DshAdapter::entry_str(&seq_b[0], "id").as_deref(), Some("user-b"));
        assert!(matches!(DshAdapter::verify_at(&home, &ctx()), VerifyReport::Ok));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 旧版顶层条目（无 insert）仍整条处理：register 换成 insert 形式、unregister 整条删
    #[test]
    fn legacy_top_level_entry_is_still_handled_whole() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(&patch, format!("- id: {ENTRY_ID}\n  name: 'C:/old/agent-bark/plugin.js'\n")).unwrap();

        DshAdapter::register_at(&home, &ctx()).unwrap();
        let entries = DshAdapter::read_patch(&patch).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(DshAdapter::entry_insert_seq(&entries[0]).is_some(), "升级为 insert 形式");

        let mut ictx = InstallCtx { exe_path: String::new(), dry_run: false, backup: true };
        DshAdapter::unregister_at(&home, &mut ictx).unwrap();
        assert!(DshAdapter::read_patch(&patch).unwrap().is_empty(), "旧版顶层条目整条删除");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// surgery_insert_entry 的直接单测：keep_first 只修首个我们的子项的 name
    #[test]
    fn surgery_keeps_first_our_subitem_and_repairs_name() {
        let home = tmp_home();
        let mut entry: Yaml = serde_yaml::from_str(
            "insert:\n  - id: agent-bark\n    name: 'C:/old/a.js'\n  - id: user-x\n    name: '@u/x'\n  - id: agent-bark\n    name: 'C:/old/b.js'\n",
        )
        .unwrap();
        let changed = DshAdapter::surgery_insert_entry(&mut entry, Some(&home), true);
        assert!(changed);
        let seq = DshAdapter::entry_insert_seq(&entry).unwrap();
        assert_eq!(seq.len(), 2, "首个我们的子项留下 + 用户子项留下，重复的删掉");
        assert_eq!(
            DshAdapter::entry_str(&seq[0], "name").map(|n| n.replace('\\', "/")),
            Some(DshAdapter::plugin_path_str(&home))
        );
        assert_eq!(DshAdapter::entry_str(&seq[1], "id").as_deref(), Some("user-x"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// surgery_insert_entry 的直接单测：keep_first=false（unregister）删光我们的子项，
    /// 序列删空 → 整条置 Null
    #[test]
    fn surgery_removes_all_our_subitems_and_nulls_when_emptied() {
        let home = tmp_home();
        let mut entry: Yaml =
            serde_yaml::from_str("insert:\n  - id: agent-bark\n    name: '/a.js'\n").unwrap();
        assert!(DshAdapter::surgery_insert_entry(&mut entry, None, false));
        assert!(entry.is_null(), "insert 删空后整条置 Null（外层随之删除）");

        let mut mixed: Yaml =
            serde_yaml::from_str("insert:\n  - id: agent-bark\n    name: '/a.js'\n  - id: user-x\n    name: '@u/x'\n")
                .unwrap();
        assert!(DshAdapter::surgery_insert_entry(&mut mixed, None, false));
        let seq = DshAdapter::entry_insert_seq(&mixed).unwrap();
        assert_eq!(seq.len(), 1);
        assert_eq!(DshAdapter::entry_str(&seq[0], "id").as_deref(), Some("user-x"));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// register 幂等：混合条目反复 register 不改变内容（不重复修复/不新增）
    #[test]
    fn mixed_insert_register_is_idempotent() {
        let home = tmp_home();
        let patch = DshAdapter::patch_file(&home);
        std::fs::write(
            &patch,
            "- insert:\n  - id: agent-bark\n    name: 'C:/old/agent-bark/plugin.js'\n  - id: user-plugin\n    name: '@u/p'\n",
        )
        .unwrap();

        DshAdapter::register_at(&home, &ctx()).unwrap();
        let first = std::fs::read_to_string(&patch).unwrap();
        DshAdapter::register_at(&home, &ctx()).unwrap();
        let second = std::fs::read_to_string(&patch).unwrap();
        assert_eq!(first, second, "幂等注册不得反复改写 patch");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// surgery_insert_entry：name 已是当前路径时无变更（幂等基础）
    #[test]
    fn surgery_is_noop_when_name_already_current() {
        let home = tmp_home();
        let want = DshAdapter::plugin_path_str(&home);
        let mut entry: Yaml = serde_yaml::from_str(&format!(
            "insert:\n  - id: agent-bark\n    name: '{want}'\n  - id: user-x\n    name: '@u/x'\n"
        ))
        .unwrap();
        assert!(
            !DshAdapter::surgery_insert_entry(&mut entry, Some(&home), true),
            "无需修复时不得报变更"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// §2.12：生成插件的会话记忆表（lastReason / settledSessions / running /
    /// pendingTerminal）必须双键登记（agent.id 与 session.id）、读取两键都查
    #[test]
    fn generated_plugin_dual_key_session_memory() {
        let js = plugin_js(1234, "tok");
        assert!(js.contains("function idKeys(agent, session, primary)"), "{js}");
        // lastReason：双键登记 + 两键查询
        assert!(js.contains("for (const k of keys) lastReason.set(k, kind);"), "lastReason 双键登记");
        assert!(js.contains("function reasonOf(keys)"), "lastReason 两键都查");
        // settled / pending / running 同款
        assert!(js.contains("function markSettled(keys)"), "settledSessions 双键登记");
        assert!(js.contains("isSettledOf(keys)"), "settledSessions 两键都查");
        assert!(js.contains("for (const k of keys) pendingTerminal.set(k, job);"), "pendingTerminal 双键登记");
        assert!(js.contains("for (const k of keys) running.add(k);"), "running 双键登记");
        // 注释里必须写明为什么（键分裂会让中止/出错回合被报成「任务完成」）
        assert!(js.contains("§2.12"), "双键的原因要留档: {js}");
    }

    /// §2.12：清理路径也要按双键——只清一个键会留下孤儿记忆，
    /// 随后的 settle 被 stale 记忆挡住、一条通知都不发
    #[test]
    fn generated_plugin_cleanup_covers_all_keys() {
        let js = plugin_js(1234, "tok");
        // finishTerminal / beginTurn / agent/disposed 都按 job.keys 清 pendingTerminal
        let finish = js.split("function finishTerminal(sessionId)").nth(1).unwrap();
        let finish_body = finish.split("}").next().unwrap();
        assert!(finish_body.contains("job.keys"), "finishTerminal 要按双键清理: {finish_body}");
        let begin = js.split("function beginTurn(keys)").nth(1).unwrap();
        assert!(begin.contains("job.keys"), "beginTurn 要按双键清理");
        // disposed 清四张表都走 keys 循环
        let disposed = js.split("ctx.on(\"agent/disposed\"").nth(1).unwrap();
        assert!(disposed.contains("for (const k of keys)"), "disposed 双键清理: {disposed}");
        assert!(disposed.contains("lastReason.delete(k)"), "lastReason 也要清");
    }
}
