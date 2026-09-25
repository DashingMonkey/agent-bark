//! ZCode 适配器（Z.ai / 智谱 GLM 的编程 Agent，桌面应用 `ZCode.exe`，含 CLI 入口）。
//!
//! ZCode 的 hook 协议是 Claude Code 风格的本地子进程协议，但**配置形态有三处不同**，
//! 所以没有复用 `claude_style` 引擎（复用会把 5 个已在跑的 agent 一起卷进来）：
//!
//! 1. 事件**多嵌一层**：用户级配置写 `hooks.events.<Event>`，而插件的 `hooks/hooks.json`
//!    才是顶层 `hooks.<Event>`——两者混淆会让整份 config.json 加载失败；
//! 2. 有**文件级总开关** `hooks.enabled`（默认 false），不开我们一条事件都收不到；
//! 3. 条目是 `type: "process"`（`command` = argv[0]，其余走 `args[]`，**不经过 shell**），
//!    而 claude_style 写的是「整串交给 shell」的 `type: "command"`。
//!
//! 写入原则与 claude_style 一致（外科手术式）：
//! 1. 只添加/修复/移除 `args` 带 `--agent zcode` 身份标记的**单个条目**，同组内用户自己的
//!    hook、同事件下其他工具（clawd / OpenViking / Hindsight 等）的组一律原样保留；
//! 2. 配置文件存在但解析失败时**拒绝写入**（不能把用户配置替换成空对象）；
//! 3. 首次写入前备份为 `<file>.agent-bark.bak`；
//! 4. 目标配置目录不存在（agent 未安装/未初始化）时跳过，绝不凭空创建。
//!
//! 两条安全边界：
//! - **绝不输出决定**：`PermissionRequest` 在 ZCode 里是阻塞型、hook 可以返回 Allow/Deny，
//!   且**同事件 hook 串行执行、后写的决定覆盖先写的**——一个放行 hook 能悄悄盖掉别人
//!   写的 deny。我们只通知，hook 永远退出 0、stdout 为空（bark-cli 全路径只写 stderr），
//!   任何异常都 fail-closed 回 ZCode 自己的权限流程。
//! - **`hooks.enabled` 不覆盖用户选择**：只在该键**缺失**时补 true；用户显式写了 false
//!   就保留并拒绝注册、由调用方把原因展示给用户（同类实现也是这个策略）。
//!
//! 事实来源：官方文档 <https://zcode.z.ai/cn/docs/hooks>，以及本机安装版
//! `resources/glm/zcode.cjs` 内的 schema（事件表恰好 7 个、`IRn`/`TRn`/`Oca` 定义、
//! 用户配置路径 `join(homedir, ".zcode", "cli", "config.json")`）。

use crate::jsonio;
use crate::{HookAdapter, InstallCtx, InstallEnv, RegisterCtx, VerifyReport};
use bark_core::{AgentKind, EventKind, NormalizedEvent};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 我们订阅的 ZCode 事件。
///
/// **只能写 ZCode 确认支持的事件名**：本版恰好 7 个
/// （SessionStart / UserPromptSubmit / PreToolUse / PermissionRequest / PostToolUse /
/// PostToolUseFailure / Stop），没有 Notification、SessionEnd、子代理生命周期事件。
/// 我们全部注册。其中 `PostToolUse` → `ToolFinished`（工具收尾）：
/// 它是**等待状态解除的唯一信号**——答完 `AskUserQuestion` / 批完权限后，agent 要先
/// 思考一段时间才可能调下一个工具，这期间既没有 `PreToolUse` 也没有
/// `UserPromptSubmit`，不收它的话会话相位会一直卡在「等待中」、警告色一直亮到
/// 下一个工具开始（实测复现）。曾经以「进程开销翻倍」为由不注册它，是误判：
/// 每轮多 3 次起进程的代价很小，而「答完问题卡橙色几十秒」的代价是用户每天可见的。
///
/// `PostToolUseFailure` 归一成 `ToolFailed` 而不是 `RunFailed`：它是**工具级失败**
/// （如编辑前没先读文件），agent 下一步就会重试——按回合失败处理会让一次瞬时
/// 失败亮终止色全屏特效 + 弹「任务失败」（实测纯噪音）。真回合失败不发任何 hook
/// （实测 2026-09-19：模型请求致命失败时 7 个事件一个都不触发），由看门狗读日志的
/// `turn.failed` 合成，见下方「中断与回合失败推断」一节；心跳判死仍是最后兜底，
/// 见 `EventKind::ToolFailed` 的文档。
pub const EVENTS: &[(&str, EventKind)] = &[
    ("SessionStart", EventKind::SessionStart),
    ("UserPromptSubmit", EventKind::Activity),
    ("PreToolUse", EventKind::Activity),
    ("PostToolUse", EventKind::ToolFinished),
    ("PermissionRequest", EventKind::PermissionRequired),
    ("PostToolUseFailure", EventKind::ToolFailed),
    ("Stop", EventKind::RunCompleted),
];

/// 条目超时（毫秒）。
///
/// ZCode 的根默认是 60000ms；`PermissionRequest` 是阻塞型，同类实现会给它配到 600000ms
/// ——那是**要替用户做决定**的用法。我们只通知，所以统一给一个小值：既覆盖 bark-cli 的
/// 最坏耗时（读 stdin 2s 超时 + POST 100ms），也让「hook 挂住」最坏只拖住 agent 5 秒。
const HOOK_TIMEOUT_MS: u64 = 5000;

/// 身份标记：`args` 的前 3 项。同一个 config.json 里可能还有别的工具的条目，
/// 只认自己的标记才能精确修复/回收（同类实现都这么做）。
const ARGS_MARKER: [&str; 3] = ["hook", "--agent", "zcode"];

/// 配置文件路径的来源（与 claude_style 同样式：生产按 home 推导，测试注入固定路径）
pub enum ConfigPaths {
    /// 按当前用户 home 推导（生产路径）
    FromHome(fn(&InstallEnv) -> Vec<PathBuf>),
    /// 固定列表（测试用）
    #[cfg(test)]
    Fixed(Vec<PathBuf>),
}

pub struct ZcodeAdapter {
    paths: ConfigPaths,
}

impl ZcodeAdapter {
    pub fn new() -> Self {
        Self {
            paths: ConfigPaths::FromHome(|env| vec![user_config_path(&env.home)]),
        }
    }

    /// 供测试构造（可指定 config 路径）
    #[cfg(test)]
    pub fn with_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            paths: ConfigPaths::Fixed(paths),
        }
    }

    /// 该 adapter 实际要读写的配置文件列表
    fn config_paths_for(&self) -> Vec<PathBuf> {
        match &self.paths {
            ConfigPaths::FromHome(f) => f(&InstallEnv::detect()),
            #[cfg(test)]
            ConfigPaths::Fixed(v) => v.clone(),
        }
    }
}

impl Default for ZcodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// 用户级配置：`~/.zcode/cli/config.json`
/// （与模型配置 `~/.zcode/v2/config.json` 分开，我们永不触碰后者）。
fn user_config_path(home: &std::path::Path) -> PathBuf {
    home.join(".zcode").join("cli").join("config.json")
}

/// 安装标记目录：`~/.zcode`
fn install_marker() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".zcode")
}

/// 去掉可能存在的成对引号。
///
/// 合法的 process 条目**不该**带引号（`command` 会被当作单个 argv 元素、带引号就执行不了），
/// 但历史上出现过这种写法：认出来并就地修好，比把它当用户条目留在配置里更对。
fn unquote(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// 归一化路径：去引号、斜杠统一、去尾斜杠、小写（与 `jsonio::command_exe_matches` 同口径）
fn norm_path(s: &str) -> String {
    unquote(s)
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// 取路径最后一段的文件名
fn file_name_of(path: &str) -> &str {
    unquote(path)
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

/// 这个可执行路径的文件名是不是我们的（含历史重命名）
fn exe_name_is_ours(path: &str) -> bool {
    let name = file_name_of(path);
    jsonio::KNOWN_EXE_NAMES
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
}

/// `args` 是否带我们的身份标记（`hook --agent zcode ...`）。
///
/// 按**原索引**逐位 `as_str()` 精确比较（全等，长度也必须相等）：旧实现先
/// `filter_map(as_str)` 把非字符串元素压缩掉再按位置比，`[null, "hook", "--agent",
/// "zcode"]` 这种用户条目会被位置错位误判成我们的、进而被覆写/删除。
fn has_args_marker(entry: &Value) -> bool {
    let Some(args) = entry.get("args").and_then(|a| a.as_array()) else {
        return false;
    };
    args.len() >= ARGS_MARKER.len()
        && ARGS_MARKER
            .iter()
            .enumerate()
            .all(|(i, want)| args[i].as_str() == Some(*want))
}

/// shell 形态命令串里是否带 `--agent <id>` 身份标记（token 边界匹配，§2.18a）。
///
/// 不能用子串包含：`--agent zcode-canary` 含有 `--agent zcode` 子串，用户的 canary
/// 条目会被认领、进而被覆写/删除。只认两种精确形态：空白分隔后**相邻**的
/// `--agent` + `<id>` 两个 token，或单 token `--agent=<id>` 精确等值。
fn cmd_has_agent_flag(cmd: &str, agent_id: &str) -> bool {
    let mut prev: Option<&str> = None;
    for tok in cmd.split_whitespace() {
        if tok == agent_id && prev == Some("--agent") {
            return true;
        }
        if let Some(v) = tok.strip_prefix("--agent=") {
            if v == agent_id {
                return true;
            }
        }
        prev = Some(tok);
    }
    false
}

/// 单个 hook 条目是否我们写的。
///
/// **不能用 `jsonio::is_our_entry`**：它取命令的「第一个 token」当可执行路径，而 process
/// 条目的 `command` 是单个 argv 元素、不带引号，路径含空格时（`C:\Program Files\...`）
/// 会被切成 `C:\Program`，于是我们自己的条目会被判成用户条目——既不能就地修复，
/// 也不能回收。这里按**全串**比较可执行路径，另加 args 身份标记。
pub(crate) fn is_our_entry(entry: &Value) -> bool {
    let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    if entry.get("type").and_then(|t| t.as_str()) == Some("process") {
        return exe_name_is_ours(cmd) && has_args_marker(entry);
    }
    // 兼容「整串交给 shell」的形态（claude_style 风格）：首 token 文件名是我们的
    // + 串内带身份标记（token 边界匹配——`--agent zcode-canary` 是用户条目）。
    // 将来若换了条目写法，老条目仍能被识别并回收。
    jsonio::is_our_entry(entry) && cmd_has_agent_flag(cmd, "zcode")
}

/// 我们条目里指向的可执行文件是否就是当前 exe
fn entry_matches_current_exe(entry: &Value, exe_path: &str) -> bool {
    let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    if entry.get("type").and_then(|t| t.as_str()) == Some("process") {
        return norm_path(cmd) == norm_path(exe_path);
    }
    jsonio::command_exe_matches(cmd, exe_path)
}

/// 我们写入的条目。
///
/// `matcher` 键**整个省略**：ZCode 文档说缺省/空串/`*` 都是「匹配全部」，但同类实现警告
/// 严格解析器可能把空串当非法、进而丢弃整份来源，所以不写这个键。
fn our_entry(ctx: &RegisterCtx, event: &str) -> Value {
    json!({
        "type": "process",
        "command": ctx.exe_path,
        "args": [
            ARGS_MARKER[0], ARGS_MARKER[1], ARGS_MARKER[2],
            "--event", event
        ],
        "enabled": true,
        "timeoutMs": HOOK_TIMEOUT_MS,
    })
}

/// 就地刷新我们的条目（程序被移动 → 路径漂移；args 与事件不符；被手工关掉）。
/// 返回是否有变更。
///
/// **只覆写我们管的字段**（`command` / `args` / `enabled` / `type`），用户在条目上
/// 加的 `timeoutMs`、`env` 等定制一律保留——曾经是整体替换，用户的调优每次启动
/// 对齐都被静默还原。`timeoutMs` 的默认值只在缺失时补。
fn refresh_our_entry(entry: &mut Value, ctx: &RegisterCtx, event: &str) -> bool {
    let want = our_entry(ctx, event);
    let Some(obj) = entry.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for (k, v) in want.as_object().expect("our_entry 是对象") {
        // 我们的默认超时只在缺失时补，用户改过的值不还原
        if k == "timeoutMs" {
            continue;
        }
        if obj.get(k) != Some(v) {
            obj.insert(k.clone(), v.clone());
            changed = true;
        }
    }
    if !obj.contains_key("timeoutMs") {
        obj.insert("timeoutMs".into(), json!(HOOK_TIMEOUT_MS));
        changed = true;
    }
    changed
}

/// 文档里是否存在「已接入」的我们的条目。
///
/// 总开关不是 `true`（含用户显式关掉）就算没接入：UI 会显示成「hook 已丢失」，
/// 用户点开关重写；若是用户显式关的总开关，register 会拒绝并说明原因。
fn doc_has_active_entry(doc: &Value) -> bool {
    if doc
        .get("hooks")
        .and_then(|h| h.get("enabled"))
        .and_then(|v| v.as_bool())
        != Some(true)
    {
        return false;
    }
    doc.get("hooks")
        .and_then(|h| h.get("events"))
        .and_then(|e| e.as_object())
        .is_some_and(|events| {
            events.values().any(|groups| {
                groups.as_array().is_some_and(|gs| {
                    gs.iter().any(|g| {
                        g.get("hooks")
                            .and_then(|h| h.as_array())
                            .is_some_and(|hs| hs.iter().any(is_our_entry))
                    })
                })
            })
        })
}

/// 这个事件是不是「提问」（AskUserQuestion）。
///
/// `AskUserQuestion` 已确认是真实工具名（本机 bundle 的工具表里就有），语义是「等你回答」，
/// 不是「等你授权」：按 PermissionRequired 归一的话，通知标题会写「需要确认」、
/// 会话相位记成 WaitingPermission，与真实情况不符（同 claude_style::normalize 对 Qoder
/// `permission_prompt` 的特例处理，方向相反）。
///
/// 注意：ZCode 里这类交互**不能**通过 PermissionRequest 的决定通道回答（同类实现的结论），
/// 所以对我们只意味着「通知该怎么说」，不意味着可以替用户答。
fn is_ask_user_question(raw: &Value) -> bool {
    raw.get("tool_name").and_then(|v| v.as_str()) == Some("AskUserQuestion")
}

/// ZCode 的子代理判定（覆盖 bark-core 的通用启发式）。
///
/// 通用启发式把 `agent_type` / `agent_id` 非空即判子代理，而 ZCode 的 `agent_type` 是
/// `SessionStart` 的**可选**普通字段（主会话也可能带）；一旦主会话带上它，子代理事件
/// 不通知的规则会把**所有**完成/授权通知静默吞掉——误判的代价远大于噪音。所以这里
/// 只认显式布尔字段，以及本机实测到的子代理会话命名
/// （`~/.zcode/cli/artifacts/sess_subagent_*`）。
///
/// 只服务 hook 载荷的 normalize 路径；日志看门狗的合成事件走
/// [`LogSignal`] 的 `is_subagent` 字段（解析时按同一份命名约定判定）。
pub(crate) fn is_subagent(raw: &Value) -> bool {
    for key in ["is_subagent", "subagent"] {
        if let Some(b) = raw.get(key).and_then(|v| v.as_bool()) {
            return b;
        }
    }
    // session id 两种拼写都认（§4.16）：bark-core 的 from_raw 就是两者兼容，
    // 只认 snake_case 会在 camelCase 载荷下漏判子代理
    raw.get("session_id")
        .or_else(|| raw.get("sessionId"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.starts_with("sess_subagent"))
}

impl HookAdapter for ZcodeAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Zcode
    }

    fn display_name(&self) -> &'static str {
        AgentKind::Zcode.display_name()
    }

    fn is_installed(&self) -> bool {
        install_marker().exists()
    }

    fn is_registered(&self) -> bool {
        self.config_paths_for()
            .iter()
            .filter(|p| p.exists())
            .any(|p| {
                jsonio::read_doc(p)
                    .map(|doc| doc_has_active_entry(&doc))
                    .unwrap_or(false)
            })
    }

    fn config_paths(&self) -> Vec<PathBuf> {
        self.config_paths_for()
    }

    fn hook_events(&self) -> Vec<&'static str> {
        EVENTS.iter().map(|(name, _)| *name).collect()
    }

    fn event_kind(&self, event_name: &str) -> Option<EventKind> {
        EVENTS
            .iter()
            .find(|(name, _)| *name == event_name)
            .map(|(_, kind)| *kind)
    }

    fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()> {
        let mut wrote_any = false;
        let mut last_err: Option<anyhow::Error> = None;

        for path in self.config_paths_for() {
            // 只写入其父目录已存在的配置（agent 已安装/已初始化）
            if !path.parent().is_some_and(|p| p.exists()) {
                continue;
            }
            // 解析失败 → 跳过该路径继续写其余路径，绝不写回损坏文件
            let mut doc = match jsonio::Doc::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            let existed = path.exists();
            let mut changed = false;

            // 顶层 hooks：缺失则建；存在但不是对象 → 结构损坏，跳过（绝不覆盖）
            match doc.value.get("hooks") {
                None => {
                    doc.value
                        .as_object_mut()
                        .expect("read_doc 保证顶层是对象")
                        .insert("hooks".into(), json!({}));
                    changed = true;
                }
                Some(h) if h.is_object() => {}
                Some(_) => {
                    last_err = Some(anyhow::anyhow!("{} 的 hooks 字段不是对象，已跳过", path.display()));
                    continue;
                }
            }
            let hooks = doc
                .value
                .get_mut("hooks")
                .and_then(|h| h.as_object_mut())
                .expect("已确认是对象");

            // 总开关：**只补缺失，绝不覆盖用户显式选择**。
            // 先校验后写：这里拒绝时一个字节都不会落盘。
            match hooks.get("enabled") {
                None => {
                    hooks.insert("enabled".into(), json!(true));
                    changed = true;
                }
                Some(Value::Bool(true)) => {}
                Some(Value::Bool(false)) => {
                    last_err = Some(anyhow::anyhow!(
                        "ZCode 的 hooks 总开关被显式关掉了（{} 里的 hooks.enabled = false）。\
                         agent-bark 不会覆盖你的选择：请在 ZCode 的 设置 > Hooks 里打开，\
                         或把该键改成 true，再重新开启本项接入。",
                        path.display()
                    ));
                    continue;
                }
                Some(other) => {
                    last_err = Some(anyhow::anyhow!(
                        "{} 的 hooks.enabled 不是布尔值（{}），已跳过",
                        path.display(),
                        other
                    ));
                    continue;
                }
            }

            // hooks.events：缺失则建；存在但不是对象 → 结构损坏，跳过
            match hooks.get("events") {
                None => {
                    hooks.insert("events".into(), json!({}));
                }
                Some(e) if e.is_object() => {}
                Some(_) => {
                    last_err = Some(anyhow::anyhow!("{} 的 hooks.events 不是对象，已跳过", path.display()));
                    continue;
                }
            }
            let events = hooks
                .get_mut("events")
                .and_then(|e| e.as_object_mut())
                .expect("已确认是对象");

            for event in self.hook_events() {
                let groups = events.entry(event.to_string()).or_insert_with(|| json!([]));
                if !groups.is_array() {
                    last_err = Some(anyhow::anyhow!(
                        "{} 的 hooks.events.{} 不是数组，已跳过",
                        path.display(),
                        event
                    ));
                    continue;
                }
                let arr = groups.as_array_mut().expect("已确认是数组");

                // 1) 优先就地更新我们的条目（修复 exe 路径漂移，且不碰用户条目）。
                //    去重是**事件级**（跨组，§1.9）：用户手工复制/旧版本并组写入形成
                //    「多个组各有一个我们的条目」时，第二个起删除——否则每个事件起
                //    N 个 hook 进程、N 条不同 uuid 的事件绕过 daemon 的 id 去重，
                //    同一事件双触发、双通知。
                let mut updated = false;
                let mut seen_ours = false;
                for group in arr.iter_mut() {
                    let Some(entries) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                        continue;
                    };
                    // 自己造的 null 哨兵按索引记账：只删哨兵，不误删用户配置里的
                    // 合法 null 元素（§4.15——旧实现 retain(!is_null) 会顺手删掉它们）
                    let mut sentinel: Vec<usize> = Vec::new();
                    for (i, entry) in entries.iter_mut().enumerate() {
                        if !is_our_entry(entry) {
                            continue;
                        }
                        if seen_ours {
                            // 历史遗留的重复条目：标记为删除（按索引回收，见下）
                            *entry = json!(null);
                            sentinel.push(i);
                            changed = true;
                            continue;
                        }
                        seen_ours = true;
                        if refresh_our_entry(entry, ctx, event) {
                            changed = true;
                        }
                        updated = true;
                    }
                    for i in sentinel.into_iter().rev() {
                        entries.remove(i);
                    }
                }

                // 2) 没有我们的条目 → 在数组末尾新增一个独立组（不并入、不重排用户的组）
                if !updated {
                    arr.push(json!({ "hooks": [ our_entry(ctx, event) ] }));
                    changed = true;
                }
            }

            if changed {
                if existed {
                    jsonio::backup_once(&path);
                }
                // Doc::write 自带 CAS：文件被并发修改（如 ZCode 自身重写配置）时报错而非覆盖。
                // 与 unregister 一致的「记 last_err 继续」语义（§2.18d）：单路径写失败
                // 不中断其余路径的注册——旧实现 `?` 直接中断多路径循环，与「部分成功
                // 只 warn」的整体语义矛盾
                match jsonio::Doc::write(doc, &path) {
                    Ok(()) => wrote_any = true,
                    Err(e) => last_err = Some(e),
                }
            }
        }

        if let Some(e) = last_err {
            if wrote_any {
                // 部分配置写入成功、部分因结构性损坏或用户选择被跳过：整体算 Ok，但必须留痕
                tracing::warn!("{} 部分配置未写入：{e:#}", self.display_name());
            } else {
                return Err(e);
            }
        }
        if !wrote_any && !self.is_registered() {
            anyhow::bail!(
                "未找到可写入的配置（{} 可能未安装或未初始化）",
                self.display_name()
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
            // 解析失败就跳过，宁可留着自己的条目，也不破坏用户文件——但必须留痕
            // （§2.18c）：静默跳过会让损坏配置里的 hook 永久残留且继续触发，而
            // 用户永远不知道「卸载没卸干净」
            let mut doc = match jsonio::Doc::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        "{} 解析失败，跳过清理（其中的 agent-bark hook 会残留并继续触发）：{e:#}",
                        path.display()
                    );
                    continue;
                }
            };
            let Some(events) = doc
                .value
                .get_mut("hooks")
                .and_then(|h| h.get_mut("events"))
                .and_then(|e| e.as_object_mut())
            else {
                continue;
            };
            let mut changed = false;
            let event_names: Vec<String> = events.keys().cloned().collect();
            for event in event_names {
                let Some(groups) = events.get_mut(&event).and_then(|g| g.as_array_mut()) else {
                    continue;
                };
                // 条目级删除：只摘掉我们的 hook，组内用户条目保留
                let mut removed_here = false;
                for group in groups.iter_mut() {
                    if let Some(entries) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                        let before = entries.len();
                        entries.retain(|e| !is_our_entry(e));
                        if entries.len() != before {
                            changed = true;
                            removed_here = true;
                        }
                    }
                }
                // 只有这一轮真的删过我们的条目才顺手清空壳：
                // 用户（或第三方工具）预存的空组 / 空事件键不属于我们，不碰
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
                        events.remove(&event);
                        changed = true;
                    }
                }
            }
            // 尊重 ctx 语义：dry_run 只计算不落盘；backup=false 时不备份。
            // hooks.enabled 一律不动：那是用户的总开关，也管着他自己的 hook。
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
        let mut seen_any = false;
        for path in self.config_paths_for() {
            if !path.exists() {
                continue;
            }
            let doc = match jsonio::read_doc(&path) {
                Ok(d) => d,
                Err(e) => {
                    return VerifyReport::ConfigUnreadable {
                        path: path.clone(),
                        reason: e.to_string(),
                    }
                }
            };
            // 总开关不是 true（含缺失/用户显式关掉）：等同「没接入」——UI 显示为关，
            // 点一下开关重写；显式 false 时 register 会拒绝并把原因交给用户
            if doc
                .get("hooks")
                .and_then(|h| h.get("enabled"))
                .and_then(|v| v.as_bool())
                != Some(true)
            {
                continue;
            }
            let Some(events) = doc
                .get("hooks")
                .and_then(|h| h.get("events"))
                .and_then(|e| e.as_object())
            else {
                continue;
            };
            for groups in events.values() {
                let Some(groups) = groups.as_array() else {
                    continue;
                };
                for group in groups {
                    let Some(entries) = group.get("hooks").and_then(|h| h.as_array()) else {
                        continue;
                    };
                    for entry in entries {
                        if !is_our_entry(entry) {
                            continue;
                        }
                        seen_any = true;
                        if !entry_matches_current_exe(entry, &ctx.exe_path) {
                            return VerifyReport::StalePath { path: path.clone() };
                        }
                    }
                }
            }
        }
        if seen_any {
            VerifyReport::Ok
        } else {
            VerifyReport::NotRegistered
        }
    }

    fn normalize(&self, event_name: &str, raw: &Value) -> Option<NormalizedEvent> {
        let mut kind = self.event_kind(event_name)?;
        // 提问不算「授权」：PreToolUse / PermissionRequest 上都升级成「等待输入」
        // （计划模式批准 ExitPlanMode 走 PermissionRequest，默认就是 PermissionRequired，
        //   不需要特例）。PostToolUse 刻意**不**升级：它是「用户已答完」的收尾信号
        // （ToolFinished），正要把相位从等待中打回思考中——升回 InputRequired 等于
        // 把卡住的警告色又钉回去。
        if is_ask_user_question(raw)
            && matches!(kind, EventKind::Activity | EventKind::PermissionRequired)
        {
            kind = EventKind::InputRequired;
        }
        // 工具失败若是**用户打断**引起的（is_interrupt），按「中止」归一：
        // 不通知、不亮终止色。注：实测（见 doc/agent-integration.md §6）当前版本
        // 的中断路径上这个字段从未出现——ZCode 中断时 1ms 内就取消了 hook 进程，
        // 真中断的秒级信号由 abort_watch 读日志合成；这里是按字段名的防御式处理。
        if kind == EventKind::ToolFailed && raw.get("is_interrupt").is_some_and(|v| v.as_bool() == Some(true)) {
            kind = EventKind::RunAborted;
        }
        let mut event = NormalizedEvent::from_raw(self.kind().id(), kind, raw);
        // 见 is_subagent：ZCode 的 agent_type 会误伤主会话，必须覆盖通用启发式
        event.is_subagent = is_subagent(raw);
        if event.message.is_empty() {
            // Stop 的助手回复正文：官方文档写 last_assistant_message（from_raw 已取），
            // 第三方逆向说是 responseText / responsePreview —— 两者都试，不赌哪份准
            for key in ["last_assistant_message", "responseText", "responsePreview"] {
                if let Some(s) = raw.get(key).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        event.message = s.to_string();
                        break;
                    }
                }
            }
        }
        Some(event)
    }

    /// 用户级 hooks 不需要任何信任/审核动作；唯一门槛是**新建会话**
    /// （ZCode 在 session 启动时对 hook 配置拍快照）。
    /// 另外如实告知：手动中断要读它自己的日志才能发现（非官方机制）。
    fn trust_hint(&self) -> Option<&'static str> {
        Some("ZCode 在会话启动时快照 Hook 配置：写入后请新建会话（或重启 ZCode）才生效；手动中断与回合失败由读取其本地日志推断（非官方方案，应用升级可能失效）")
    }

    /// 关闭后已打开的会话仍持有旧配置快照，下一条 hook 还会被调起
    /// （daemon 侧按「已关闭」忽略其事件）。
    fn unregister_hint(&self) -> Option<&'static str> {
        Some("已停止接入；ZCode 已打开的会话要新建会话后才彻底停止触发（其间事件已被忽略）")
    }
}

// ---------------------------------------------------------------------------
// 中断与回合失败推断（非官方机制）：两类回合终态都不发 hook，只在日志里留痕
// ---------------------------------------------------------------------------

/// 看门狗从 ZCode 日志行解析出的「回合终态」信号。
///
/// ZCode 有两类回合终态**不发任何 hook**、只在本地日志留痕：
///
/// 1. **用户中断**（2026-09-10 实测）：7 个事件全埋探针、零事件。痕迹是本地日志的
///    两条记录（相隔约 25ms）：`v4.stop.foreground_execution_inspected` 且
///    `context.runtimeStopKind == "stopped"`；一条**没有 `event` 字段**的 warn 记录、
///    `context.error == "Turn was cancelled."`。
///
/// 2. **回合致命失败**（2026-09-19 实测 Captcha 超时 / 订阅权限 1311）：模型请求
///    不可重试地失败时写一条 `turn.failed`，**进程不退出、不发 hook、会话停在
///    输入框等用户手动继续**——对 agent-bark 来说会话相位永远卡在「思考中」，
///    只能等心跳判死。日志里它与中断可机械区分（两天 11 条样本实测）：
///    中断的 `error.message` 是 "Turn was cancelled."（顶层 `status: "cancelled"`，
///    cause.code = `model_request_cancelled` / "v4 session stopped"），
///    致命失败是 "Turn execution failed"（`status: "failed"`，cause.code =
///    `model_request_failed`，cause.message 带真实错误原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogSignal {
    /// 回合被用户中断 → 看门狗合成 `RunAborted`
    Aborted {
        session_id: String,
        /// 按会话名判定（`sess_subagent_*`）：日志行没有 hook 载荷的布尔字段，
        /// 只能靠命名。合成事件必须带上——通知与音效有 is_subagent 门
        is_subagent: bool,
    },
    /// 回合致命失败（非取消）→ 看门狗合成 `RunFailed`
    Failed {
        session_id: String,
        /// 回合 id：同一回合只写一条 turn.failed，按会话+回合去重，
        /// 不同回合的连续失败（Captcha 超时后手动重试又超时）不得互相吞。
        /// 缺失时看门狗**不去重**：无标识就无法区分「同一条读两次」与「两次真失败」，
        /// 按项目口径宁可重复也不静默吞信号
        turn_id: Option<String>,
        /// `error.cause.message`：给用户看的真实错误（通知正文 / 事件流）
        message: String,
        is_subagent: bool,
    },
}

impl LogSignal {
    /// 涉及的会话 id
    pub fn session_id(&self) -> &str {
        match self {
            LogSignal::Aborted { session_id, .. } => session_id,
            LogSignal::Failed { session_id, .. } => session_id,
        }
    }

    /// 是否子代理会话（与 [`is_subagent`] 同一份命名约定）
    pub fn is_subagent(&self) -> bool {
        match self {
            LogSignal::Aborted { is_subagent, .. } | LogSignal::Failed { is_subagent, .. } => {
                *is_subagent
            }
        }
    }

    /// 看门狗的去重键。`None` = 不去重（宁可重复，不吞信号）。
    ///
    /// 中断按**会话**：三条痕迹相隔毫秒级，同一秒内必须只合成一次；
    /// 失败按**会话 + 回合**：不同回合的连续失败是两个真实事件，按会话去重会把
    /// 第二次吞掉；回合 id 缺失时无法可靠区分「同一条读两次」与「两次真失败」，
    /// 返回 None 让调用方放行（见字段文档）。
    pub fn dedup_key(&self) -> Option<String> {
        match self {
            LogSignal::Aborted { session_id, .. } => Some(session_id.clone()),
            LogSignal::Failed { session_id, turn_id, .. } => {
                turn_id.as_ref().map(|t| format!("{session_id}|{t}"))
            }
        }
    }
}

/// 从一行 ZCode 日志里解析回合终态信号（无关记录返回 None）。
pub fn parse_log_signal(line: &str) -> Option<LogSignal> {
    // 行首 BOM 剥离（§4.16）：与 jsonio 的 BOM 口径一致——部分编辑器/轮转工具
    // 会给文件或行补 BOM，不剥的话整行解析失败、信号被静默丢弃
    let v: Value = serde_json::from_str(line.trim().trim_start_matches('\u{feff}')).ok()?;
    // 只认会话级记录：hook 载荷里的 session_id 与它同源
    let session_id = v.get("sessionId")?.as_str()?;
    if !session_id.starts_with("sess_") {
        return None;
    }
    let ctx = v.get("context");
    // 子代理判定只有会话名可用（日志行没有 hook 载荷的布尔字段）
    let is_subagent = session_id.starts_with("sess_subagent");
    let event_name = v.get("event").and_then(|e| e.as_str());
    // 中断判定**收窄到两条实测形态**（§2.9）：旧实现对任意事件的 `context.error`
    // 含 "cancel" 就合成 RunAborted，"cancelled by API error, retrying" 这类
    // 非终态记录会把运行中会话误摘表、真实完成通知漏发。
    // (i) 用户按停止的第一条痕迹：`v4.stop.foreground_execution_inspected`
    //     且 `context.runtimeStopKind == "stopped"`（2026-09-10 实测）；
    // (ii) 紧随其后的 warn 记录：**没有 event 字段**、message 含
    //     "background turn failed"、`context.error` 含 cancel（实测原文
    //     "Turn was cancelled."，contains 容忍文案微调）。
    let runtime_stopped = event_name == Some("v4.stop.foreground_execution_inspected")
        && ctx
            .and_then(|c| c.get("runtimeStopKind"))
            .and_then(|k| k.as_str())
            == Some("stopped");
    let cancel_warn = event_name.is_none()
        && v.get("message")
            .and_then(|m| m.as_str())
            .is_some_and(|m| m.contains("background turn failed"))
        && ctx
            .and_then(|c| c.get("error"))
            .and_then(|e| e.as_str())
            .is_some_and(|e| e.to_ascii_lowercase().contains("cancel"));
    if runtime_stopped || cancel_warn {
        return Some(LogSignal::Aborted { session_id: session_id.to_string(), is_subagent });
    }
    // `turn.failed`：回合级失败记录，中断与致命失败都会写。中断形态已由上面的
    // abort 记录合成过 Aborted，这里归并成同一信号、靠去重键合并；只有**非取消**
    // 形态才需要补信号——那是「进程还活着、不发任何 hook、等用户手动继续」的
    // 致命失败（2026-09-19 实测：Captcha 超时 / 订阅权限 1311）。
    if v.get("event").and_then(|e| e.as_str()) != Some("turn.failed") {
        return None;
    }
    let error = v.get("error");
    let err_msg = error
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    // 结构化字段三档判定：status 是枚举值、不随文案改版漂移——明确是 "cancelled"
    // 才算中断，明确不是（"failed" 及未来新增的终态）**不再看文案**，否则
    // "status": "failed" 但文案恰好含 cancel 的失败会被静默吞成中断（不通知、
    // 不亮终止色，真实失败无声消失）。只有 status 缺失才退回 cancel 文案兜底。
    let cancelled_shape = match v.get("status").and_then(|s| s.as_str()) {
        Some("cancelled") => true,
        Some(_) => false,
        None => err_msg.to_ascii_lowercase().contains("cancel"),
    };
    if cancelled_shape {
        return Some(LogSignal::Aborted { session_id: session_id.to_string(), is_subagent });
    }
    // 真实错误在 cause 链上（error.cause.message，如 "Captcha instance timed out
    // after 10000ms."），逐层兜底，字段缺失时给一句通用文案
    let message = error
        .and_then(|e| e.get("cause"))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            error
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("回合执行失败")
        .to_string();
    Some(LogSignal::Failed {
        session_id: session_id.to_string(),
        turn_id: v.get("turnId").and_then(|t| t.as_str()).map(str::to_string),
        message,
        is_subagent,
    })
}

/// ZCode 日志目录：`~/.zcode/cli/log`
pub fn log_dir(home: &Path) -> PathBuf {
    home.join(".zcode").join("cli").join("log")
}

/// 按当前用户 home 推导的日志目录（生产路径）
pub fn log_dir_default() -> PathBuf {
    log_dir(&InstallEnv::detect().home)
}

/// 最新的一份日志：`zcode-<yyyy-mm-dd>.jsonl` 按文件名取最大（日期零填充，
/// 字典序即时间序）。不解析日期、不依赖时区与 chrono，跨天自然切换。
///
/// **严格匹配日期名**（§2.8c）：`zcode-foo.jsonl` 这类非日期名在字典序上
/// 'f' > '2'，纯 `starts_with`/`ends_with` 的旧实现会让它永久劫持 tail——
/// 跟错文件后新日志里的终态信号全部漏读。大小写不敏感（Windows 文件系统
/// 不区分大小写，`ZCODE-2026-09-11.JSONL` 也可能是真实的）。
fn newest_log_file(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        if !is_dated_log_name(&lower) {
            continue;
        }
        if best.as_ref().is_none_or(|(n, _)| lower > *n) {
            best = Some((lower, entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// `zcode-<yyyy-mm-dd>.jsonl` 形态判定（入参应为小写）：
/// 日期段恰好 10 位、分隔符在第 4/7 位、其余全是数字。
fn is_dated_log_name(lower: &str) -> bool {
    let Some(mid) = lower.strip_prefix("zcode-").and_then(|s| s.strip_suffix(".jsonl")) else {
        return false;
    };
    let b = mid.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                true
            } else {
                c.is_ascii_digit()
            }
        })
}

/// 单次最多读取的字节数：日志可能很大（实测单日 13MB），一次读满是浪费；
/// 读不完就留给下一轮，**不跳字节**（跳了会漏掉终态信号）。
const TAIL_MAX_READ: u64 = 256 * 1024;

/// carry 的内存上限：正常 JSON 行远小于此值。超过说明遇到了无换行的异常输出
/// （截断的日志、非文本写入等），丢最旧的部分——宁可丢信号也不无界增长。
const CARRY_MAX: usize = 4 * 1024 * 1024;

/// 按字节偏移增量读取 ZCode 日志的「尾巴」，跨天自动换文件。
#[derive(Default)]
pub struct AbortLogTail {
    path: Option<PathBuf>,
    /// 当前文件的创建时间（`metadata.created()` 可用时）：同名文件被删除重建后
    /// created 会变——只看长度无法识别「重建后比旧 offset 更长」的场景（§2.8b）
    created: Option<std::time::SystemTime>,
    offset: u64,
    /// 最后一行尚未写完的部分（日志是边写边刷的，必须容忍半行）
    carry: Vec<u8>,
}

impl AbortLogTail {
    /// 读一次新增内容，返回本次新发现的回合终态信号。
    ///
    /// 首次见到某个日志文件时**从文件末尾开始**：启动之前发生的中断/失败属于陈年旧事，
    /// 不该补一拨通知（调用方那边会话表里也没有它们）。
    pub fn poll(&mut self, dir: &Path) -> Vec<LogSignal> {
        let Some(path) = newest_log_file(dir) else {
            return Vec::new();
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return Vec::new();
        };
        let len = meta.len();
        let created = meta.created().ok();

        // 换文件（跨天/轮转）：**从新文件的末尾开始**、旧文件的残留半行丢弃。
        // （不能「从头读」：历史行里可能积累几十条 turn.failed，每条去重键
        //   互不相同、30 秒去重窗拦不住，从头重放会把正在跑的会话批量误杀）
        if self.path.as_ref() != Some(&path) {
            self.path = Some(path.clone());
            self.created = created;
            self.offset = len;
            self.carry.clear();
            return Vec::new();
        }
        // 同名文件被删除重建：视同「换了一份新文件」，从当前末尾开始。
        // 识别手段（§2.8b）：`created` 变了就是重建——旧实现只看 `len < offset`，
        // 重建后**超过旧 offset** 的新文件不被识别，从旧 offset 读进去会把新文件
        // 拦腰切成半行垃圾/错位行；created 不可用的平台退回 len < offset 兜底。
        // **不能**从头重放：同上，历史行的陈年失败会批量误杀正在跑的会话。
        // 代价是「重建与新追加落在同一次写入里」的新事件会被跳过，交给判死兜底
        // （与跨天换文件「从末尾开始」同一口径）。
        let recreated = matches!((self.created, created), (Some(old), Some(now)) if old != now);
        if recreated || len < self.offset {
            self.created = created;
            self.offset = len;
            self.carry.clear();
        }
        if len == self.offset {
            return Vec::new();
        }

        let want = (len - self.offset).min(TAIL_MAX_READ);
        let mut buf = vec![0u8; want as usize];
        let read = {
            use std::io::{Read, Seek, SeekFrom};
            let Ok(mut f) = std::fs::File::open(&path) else {
                return Vec::new();
            };
            if f.seek(SeekFrom::Start(self.offset)).is_err() {
                return Vec::new();
            }
            match f.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return Vec::new(),
            }
        };
        if read == 0 {
            return Vec::new();
        }
        self.offset += read as u64;
        buf.truncate(read);
        self.carry.extend_from_slice(&buf);
        if self.carry.len() > CARRY_MAX {
            // 对齐到被丢弃区间内最后一个换行之后：避免把半行垃圾拼进后面的完整行
            let drop = self.carry.len() - CARRY_MAX;
            let cut = self.carry[..drop]
                .iter()
                .rposition(|b| *b == b'\n')
                .map_or(drop, |p| p + 1);
            self.carry.drain(..cut);
        }

        let mut out = Vec::new();
        // 只处理**完整行**：最后一个换行符之后的部分留在 carry 里等下一轮
        let last_newline = self.carry.iter().rposition(|b| *b == b'\n');
        let Some(cut) = last_newline else {
            return out;
        };
        let complete: Vec<u8> = self.carry.drain(..=cut).collect();
        for line in complete.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(sig) = parse_log_signal(&String::from_utf8_lossy(line)) {
                out.push(sig);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    fn ctx() -> RegisterCtx {
        RegisterCtx {
            exe_path: r"C:\Users\u\AppData\Local\Programs\AgentBark\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-bark-zcode-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 造一个 adapter：路径是 `<dir>/config.json`（父目录已存在 → 可写）
    fn adapter(dir: &Path) -> ZcodeAdapter {
        ZcodeAdapter::with_paths(vec![dir.join("config.json")])
    }

    fn cfg(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    fn write_json(path: &Path, doc: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(doc).unwrap()).unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// 统计文档里「非我们」的 hook 条目数（用户自己的，必须一条不少）
    fn count_foreign(doc: &Value) -> usize {
        doc.get("hooks")
            .and_then(|h| h.get("events"))
            .and_then(|e| e.as_object())
            .map(|events| {
                events
                    .values()
                    .filter_map(|g| g.as_array())
                    .flat_map(|gs| gs.iter())
                    .filter_map(|g| g.get("hooks").and_then(|h| h.as_array()))
                    .flat_map(|hs| hs.iter())
                    .filter(|e| !is_our_entry(e))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn register_creates_gate_and_seven_events_without_matcher() {
        let dir = tmpdir();
        let a = adapter(&dir);
        a.register(&ctx()).unwrap();

        let doc = read(&cfg(&dir));
        assert_eq!(doc["hooks"]["enabled"], json!(true), "缺失的总开关要补 true");
        let events = doc["hooks"]["events"].as_object().unwrap();
        assert_eq!(events.len(), EVENTS.len(), "7 个事件全部注册");
        for (name, _) in EVENTS {
            let groups = events.get(*name).unwrap().as_array().unwrap();
            assert_eq!(groups.len(), 1, "{name} 应恰好一个我们的组");
            let group = &groups[0];
            assert!(
                group.get("matcher").is_none(),
                "{name} 的组不得写 matcher 键（空串有被严格解析器丢弃整份来源的风险）"
            );
            let entry = &group["hooks"][0];
            assert_eq!(entry["type"], json!("process"), "{name} 必须是 process 条目（不走 shell）");
            assert_eq!(entry["command"], json!(ctx().exe_path));
            assert_eq!(
                entry["args"],
                json!(["hook", "--agent", "zcode", "--event", name]),
                "{name} 的 args 身份标记"
            );
            assert_eq!(entry["enabled"], json!(true));
            assert_eq!(entry["timeoutMs"], json!(HOOK_TIMEOUT_MS));
        }
        assert!(!jsonio::backup_path(&cfg(&dir)).exists(), "首次创建不产生备份");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_preserves_foreign_keys_and_hooks() {
        let dir = tmpdir();
        let path = cfg(&dir);
        // 用户文件：MCP 服务、权限默认值、hooks 根参数、用户自己的组（含同组的混合组）
        write_json(
            &path,
            &json!({
                "mcp": { "servers": { "x": { "command": "npx" } } },
                "permissions": { "defaultMode": "ask" },
                "hooks": {
                    "enabled": true,
                    "timeoutMs": 60000,
                    "events": {
                        "Stop": [
                            { "hooks": [ { "type": "command", "command": "node mine.mjs" } ] },
                            { "hooks": [
                                { "type": "command", "command": "node mine2.mjs" },
                                { "type": "process", "command": "C:/old/AgentBark.exe",
                                  "args": ["hook", "--agent", "zcode", "--event", "Stop"] }
                            ] }
                        ]
                    }
                }
            }),
        );

        let a = adapter(&dir);
        a.register(&ctx()).unwrap();

        let doc = read(&path);
        assert_eq!(doc["mcp"]["servers"]["x"]["command"], json!("npx"), "MCP 配置必须原样保留");
        assert_eq!(doc["permissions"]["defaultMode"], json!("ask"));
        assert_eq!(doc["hooks"]["timeoutMs"], json!(60000), "我们不动 hooks 根参数");
        assert_eq!(count_foreign(&doc), 2, "用户的两条 hook 都要保留");
        // 混合组里我们的旧条目就地修复，而不是新加一组
        assert_eq!(doc["hooks"]["events"]["Stop"].as_array().unwrap().len(), 2);
        assert!(doc_has_active_entry(&doc));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn register_is_idempotent_and_repairs_stale_exe_path() {
        let dir = tmpdir();
        let path = cfg(&dir);
        let a = adapter(&dir);
        a.register(&ctx()).unwrap();
        a.register(&ctx()).unwrap();
        assert_eq!(
            read(&path)["hooks"]["events"]["Stop"].as_array().unwrap().len(),
            1,
            "重复 register 不得新增组"
        );

        // 程序被移动到新位置 → verify 报失效，register 就地修复
        let moved = RegisterCtx {
            exe_path: r"D:\new path\AgentBark.exe".into(),
            port: 1,
            token: "t".into(),
        };
        assert!(matches!(a.verify(&moved), VerifyReport::StalePath { .. }));
        a.register(&moved).unwrap();
        let doc = read(&path);
        assert_eq!(doc["hooks"]["events"]["Stop"].as_array().unwrap().len(), 1, "就地修复不新增组");
        assert_eq!(doc["hooks"]["events"]["Stop"][0]["hooks"][0]["command"], json!(moved.exe_path));
        assert!(matches!(a.verify(&moved), VerifyReport::Ok));
        // 旧路径的重复条目不得残留
        assert!(!std::fs::read_to_string(&path).unwrap().contains("AppData"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unregister_removes_only_ours_and_keeps_gate() {
        let dir = tmpdir();
        let path = cfg(&dir);
        write_json(
            &path,
            &json!({
                "mcp": { "servers": {} },
                "hooks": {
                    "enabled": true,
                    "events": {
                        "Stop": [
                            { "hooks": [ { "type": "command", "command": "node mine.mjs" } ] },
                            { "hooks": [
                                { "type": "command", "command": "node mine2.mjs" },
                                { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent zcode --event Stop" }
                            ] }
                        ],
                        "PostToolUse": [
                            { "hooks": [ { "type": "command", "command": "node keep-me.mjs" } ] }
                        ]
                    }
                }
            }),
        );

        let a = adapter(&dir);
        a.register(&ctx()).unwrap();
        assert!(read(&path)["hooks"]["events"].get("PostToolUse").is_some(), "register 不碰别人的事件");

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: true };
        a.unregister(&mut ictx).unwrap();

        let doc = read(&path);
        assert_eq!(count_foreign(&doc), 3, "用户条目一条不少");
        assert_eq!(doc["mcp"]["servers"], json!({}));
        assert_eq!(doc["hooks"]["enabled"], json!(true), "总开关是用户的，unregister 不动它");
        assert!(!doc_has_active_entry(&doc));
        // 只被我们占用的 6 个事件键应被删掉；用户自己那两条事件保留
        // （键序不比较：preserve_order 之后 Map 保持写入序，而不再按字母序重排）
        let mut kept: Vec<String> = doc["hooks"]["events"].as_object().unwrap().keys().cloned().collect();
        kept.sort();
        assert_eq!(kept, vec!["PostToolUse", "Stop"]);
        // 两个组里都还有用户自己的条目（`node mine.mjs` / `node mine2.mjs`），所以都保留；
        // 只剩空组才会被删掉
        assert_eq!(doc["hooks"]["events"]["Stop"].as_array().unwrap().len(), 2);
        let stop = serde_json::to_string(&doc["hooks"]["events"]["Stop"]).unwrap();
        assert!(!stop.contains("AgentBark"), "我们的条目必须被摘干净: {stop}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unregister_dry_run_writes_nothing() {
        let dir = tmpdir();
        let path = cfg(&dir);
        write_json(&path, &json!({ "hooks": { "enabled": true, "events": {} } }));
        let a = adapter(&dir);
        a.register(&ctx()).unwrap();
        let bak = jsonio::backup_path(&path);
        let _ = std::fs::remove_file(&bak);
        let before = std::fs::read_to_string(&path).unwrap();

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: true, backup: true };
        a.unregister(&mut ictx).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "dry_run 不得改文件");
        assert!(!bak.exists(), "dry_run 不得创建备份");

        let mut ictx = InstallCtx { exe_path: ctx().exe_path, dry_run: false, backup: false };
        a.unregister(&mut ictx).unwrap();
        assert!(!bak.exists(), "backup=false 不应备份");
        assert!(!doc_has_active_entry(&read(&path)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_config_aborts_without_overwriting() {
        let dir = tmpdir();
        let path = cfg(&dir);
        let original = "{ \"hooks\": { \"enabled\": true, }, }"; // 尾随逗号 → 非法
        std::fs::write(&path, original).unwrap();

        let a = adapter(&dir);
        let err = a.register(&ctx()).expect_err("非法 JSON 必须中止");
        assert!(err.to_string().contains("不是合法 JSON"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "文件不能被改写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_disabled_gate_is_preserved_and_register_refuses() {
        let dir = tmpdir();
        let path = cfg(&dir);
        // 用户显式关掉总开关：必须保留，且注册要拒绝（不能覆盖用户选择）
        write_json(&path, &json!({ "hooks": { "enabled": false, "events": {} } }));
        let before = std::fs::read_to_string(&path).unwrap();

        let a = adapter(&dir);
        let err = a.register(&ctx()).expect_err("显式 false 必须拒绝注册");
        assert!(err.to_string().contains("hooks.enabled"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before, "一个字节都不能改");
        assert!(matches!(a.verify(&ctx()), VerifyReport::NotRegistered));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_is_only_filled_when_missing() {
        let dir = tmpdir();
        let path = cfg(&dir);
        let a = adapter(&dir);

        // 缺失 → 补 true
        write_json(&path, &json!({ "hooks": { "events": {} } }));
        a.register(&ctx()).unwrap();
        assert_eq!(read(&path)["hooks"]["enabled"], json!(true));

        // 已是 true → 保持（并保留同文件其他键）
        write_json(&path, &json!({ "hooks": { "enabled": true, "maxOutputBytes": 4096, "events": {} } }));
        a.register(&ctx()).unwrap();
        let doc = read(&path);
        assert_eq!(doc["hooks"]["enabled"], json!(true));
        assert_eq!(doc["hooks"]["maxOutputBytes"], json!(4096));

        // 非布尔 → 报错且不改文件
        write_json(&path, &json!({ "hooks": { "enabled": "yes", "events": {} } }));
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(a.register(&ctx()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_reports_not_registered_on_empty_or_missing_config() {
        let dir = tmpdir();
        let a = adapter(&dir);
        // 文件不存在
        assert!(matches!(a.verify(&ctx()), VerifyReport::NotRegistered));
        // 空文件 / 没有我们的条目
        write_json(&cfg(&dir), &json!({}));
        assert!(matches!(a.verify(&ctx()), VerifyReport::NotRegistered));
        assert!(!a.is_registered());
        // 有 total 开关但没条目
        write_json(&cfg(&dir), &json!({ "hooks": { "enabled": true, "events": {} } }));
        assert!(matches!(a.verify(&ctx()), VerifyReport::NotRegistered));
        // 损坏的 JSON
        std::fs::write(cfg(&dir), "{ nope").unwrap();
        assert!(matches!(a.verify(&ctx()), VerifyReport::ConfigUnreadable { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownership_detection_handles_spaces_and_foreign_commands() {
        // process 条目：command 是单个 argv 元素、可能含空格 → 必须按全串比较
        for exe in [
            r"C:\Program Files\AgentBark\AgentBark.exe",
            r"C:\Users\81928\AppData\Local\Programs\AgentBark\AgentBark.exe",
            r"D:\工具\AgentBark 目录\AgentBark.exe",
        ] {
            let entry = json!({
                "type": "process",
                "command": exe,
                "args": ["hook", "--agent", "zcode", "--event", "Stop"],
            });
            assert!(is_our_entry(&entry), "应判为我们条目: {exe}");
            assert!(entry_matches_current_exe(&entry, exe));
        }
        // 用户自己的条目：绝不是我们的
        for entry in [
            json!({ "type": "process", "command": "node", "args": ["mine.mjs"] }),
            // 路径里含 agent-bark 子串，但可执行名不是我们的
            json!({ "type": "process", "command": "/home/u/agent-bark-notes/run.sh", "args": [] }),
            // 是 python 在跑我们的脚本（不是我们的 hook 条目）
            json!({ "type": "command", "command": "python agent-bark-helper.py" }),
            // 我们的 exe，但 args 没带身份标记（例如别的工具复用同一可执行名）
            json!({ "type": "process", "command": "C:/x/AgentBark.exe", "args": ["serve"] }),
        ] {
            assert!(!is_our_entry(&entry), "应判为用户条目: {entry}");
        }
        // 兼容历史「整串交给 shell」的写法
        let legacy = json!({
            "type": "command",
            "command": "\"C:/Program Files/AgentBark/AgentBark.exe\" hook --agent zcode --event Stop",
        });
        assert!(is_our_entry(&legacy));
        assert!(entry_matches_current_exe(&legacy, r"C:\Program Files\AgentBark\AgentBark.exe"));

        // process 条目不该带引号（带引号的 argv[0] 执行不了），但这种写法历史上出现过：
        // 要能认出来并就地修好，而不是当用户条目留下
        let quoted = json!({
            "type": "process",
            "command": "\"C:/Program Files/AgentBark/AgentBark.exe\"",
            "args": ["hook", "--agent", "zcode", "--event", "Stop"],
        });
        assert!(is_our_entry(&quoted), "带引号的 process 条目要能认出来");
        assert!(
            entry_matches_current_exe(&quoted, r"C:\Program Files\AgentBark\AgentBark.exe"),
            "带引号的路径归一化后应等于当前 exe（从而就地修复而不是报失效）"
        );
    }

    #[test]
    fn normalize_maps_events_and_ask_user_question() {
        let dir = tmpdir();
        let a = adapter(&dir);

        // 会话开始 / 心跳
        let ev = a.normalize("SessionStart", &json!({ "session_id": "s", "cwd": r"D:\proj" })).unwrap();
        assert_eq!(ev.kind, EventKind::SessionStart);
        assert_eq!(ev.agent, "zcode");
        assert_eq!(ev.project.as_deref(), Some("proj"));

        let ev = a.normalize("UserPromptSubmit", &json!({ "session_id": "s", "prompt": "修个 bug" })).unwrap();
        assert_eq!(ev.kind, EventKind::Activity);
        assert_eq!(ev.message, "修个 bug");
        assert_eq!(ev.tool_name, None, "无 tool_name → 状态机判「思考中」");

        let ev = a.normalize("PreToolUse", &json!({ "session_id": "s", "tool_name": "Bash" })).unwrap();
        assert_eq!(ev.kind, EventKind::Activity);
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"), "带 tool_name → 判「执行工具」");

        // 提问：PreToolUse 与 PermissionRequest 都升级成「等待输入」
        for event in ["PreToolUse", "PermissionRequest"] {
            let ev = a
                .normalize(event, &json!({ "session_id": "s", "tool_name": "AskUserQuestion" }))
                .unwrap();
            assert_eq!(ev.kind, EventKind::InputRequired, "{event} 上的提问不是「需要确认」");
            assert_eq!(ev.kind.default_title(), "等待输入");
        }

        // 工具收尾：PostToolUse 一律 ToolFinished（相位回落思考中）。
        // 答完 AskUserQuestion 的收尾也必须是它——升回「等待输入」等于把
        // 卡住的警告色又钉回去（回归：实测答完问题后警告色一直亮到下一个工具）
        for tool in ["Bash", "AskUserQuestion"] {
            let ev = a
                .normalize("PostToolUse", &json!({ "session_id": "s", "tool_name": tool }))
                .unwrap();
            assert_eq!(ev.kind, EventKind::ToolFinished, "PostToolUse({tool}) 是工具收尾");
            assert!(!ev.kind.should_notify(), "工具收尾是纯状态信号，不通知");
        }
        // 真实授权等待
        let ev = a
            .normalize("PermissionRequest", &json!({ "session_id": "s", "tool_name": "Bash" }))
            .unwrap();
        assert_eq!(ev.kind, EventKind::PermissionRequired);
        assert_eq!(ev.kind.default_title(), "需要确认");

        // 工具失败 / 回合完成
        // 工具失败是「工具级」失败（agent 会自行重试），归一成 ToolFailed：
        // 不通知、不亮终止色，但事件仍入历史与事件流（中性色可排查）
        let ev = a
            .normalize("PostToolUseFailure", &json!({ "session_id": "s", "error": "boom", "is_interrupt": false }))
            .unwrap();
        assert_eq!(ev.kind, EventKind::ToolFailed);
        assert_eq!(ev.kind.default_title(), "工具失败");
        assert_eq!(ev.message, "boom");
        assert!(!ev.kind.should_notify(), "工具失败 agent 会自行重试，不该弹「任务失败」");

        // 打断型工具失败（is_interrupt）按「中止」归一：不亮终止色（防御式分支，
        // 实测当前版本中断路径上该字段从未出现）
        let ev = a
            .normalize("PostToolUseFailure", &json!({ "session_id": "s", "error": "用户取消", "is_interrupt": true }))
            .unwrap();
        assert_eq!(ev.kind, EventKind::RunAborted);
        assert!(!ev.kind.should_notify());

        let ev = a
            .normalize("Stop", &json!({ "session_id": "s", "stop_hook_active": false, "last_assistant_message": "做完了" }))
            .unwrap();
        assert_eq!(ev.kind, EventKind::RunCompleted);
        assert_eq!(ev.message, "做完了");

        // Stop 正文的防御式回退：第三方逆向说实际字段是 responseText / responsePreview
        for key in ["responseText", "responsePreview"] {
            let raw = json!({ "session_id": "s", key: "另一种字段名" });
            let ev = a.normalize("Stop", &raw).unwrap();
            assert_eq!(ev.message, "另一种字段名", "{key} 应作为 Stop 正文回退");
        }

        // 未注册的事件不产生事件（ZCode 没有 SessionEnd；7 个事件现已全部注册）
        assert!(a.normalize("SessionEnd", &json!({})).is_none(), "ZCode 没有 SessionEnd");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_detection_ignores_agent_type() {
        let dir = tmpdir();
        let a = adapter(&dir);

        // 主会话带 agent_type（ZCode 的 SessionStart 可选字段）→ 绝不能判成子代理，
        // 否则默认配置会把所有完成/授权通知静默吞掉
        let ev = a
            .normalize("Stop", &json!({ "session_id": "s", "agent_type": "glm", "last_assistant_message": "done" }))
            .unwrap();
        assert!(!ev.is_subagent, "agent_type 是 SessionStart 的普通字段，不能当子代理标记");

        // 显式布尔字段优先
        let ev = a.normalize("Stop", &json!({ "session_id": "s", "is_subagent": true })).unwrap();
        assert!(ev.is_subagent);
        let ev = a
            .normalize("Stop", &json!({ "session_id": "s", "is_subagent": false, "agent_type": "coder" }))
            .unwrap();
        assert!(!ev.is_subagent);

        // 本机实测的子代理会话命名
        let ev = a
            .normalize("Stop", &json!({ "session_id": "sess_subagent_agent_005ac652" }))
            .unwrap();
        assert!(ev.is_subagent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_when_config_dir_missing() {
        // 父目录不存在（ZCode 未安装/未初始化）→ 跳过，绝不凭空创建目录树
        let dir = std::env::temp_dir()
            .join(format!("agent-bark-zcode-missing-{}", uuid::Uuid::new_v4().simple()))
            .join(".zcode")
            .join("cli");
        let a = ZcodeAdapter::with_paths(vec![dir.join("config.json")]);
        let err = a.register(&ctx()).expect_err("没有可写配置时应报错");
        assert!(err.to_string().contains("未找到可写入的配置"), "{err}");
        assert!(!dir.exists(), "不得创建目录");
    }

    #[test]
    fn default_paths_are_the_user_config_and_marker() {
        let a = ZcodeAdapter::new();
        let paths = a.config_paths();
        assert_eq!(paths.len(), 1);
        let p = paths[0].to_string_lossy().replace('\\', "/");
        assert!(p.ends_with("/.zcode/cli/config.json"), "{p}");
        assert!(!p.contains("/v2/"), "模型配置在 v2/config.json，绝不能指向它: {p}");
    }

    // ---- 中断推断（非官方机制）---------------------------------------------

    /// 实测原样：用户按停止后的第一条记录
    const REAL_STOP_LINE: &str = r#"{"timestamp":"2026-09-10T16:28:00.959Z","level":"info","event":"v4.stop.foreground_execution_inspected","module":"core.runtime","message":"v4 stop foreground execution inspected","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"activeForegroundExecutionId":"runtime_command_1","expectedForegroundExecutionId":"runtime_command_1","runtimeStopKind":"stopped"}}"#;
    /// 实测原样：紧随其后的取消记录（**顶层没有 event 字段**）
    const REAL_CANCEL_LINE: &str = r#"{"timestamp":"2026-09-10T16:28:00.984Z","level":"warn","module":"core.runtime","message":"v4 background turn failed","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"error":"Turn was cancelled.","inputId":"01a08c25-72ff-7399-b777-702fdc678785"}}"#;
    /// 实测原样（2026-09-19）：Captcha 超时导致的回合致命失败——进程不退、不发任何
    /// hook，status = "failed"，真实错误在 error.cause.message
    const REAL_TURN_FAILED_LINE: &str = r#"{"timestamp":"2026-09-19T13:26:03.740Z","level":"error","event":"turn.failed","module":"core.runtime","message":"Turn failed","traceId":"f909d120-72f5-4f31-9fe8-40f62c173eda","spanId":"29e1649a-5412-42","parentSpanId":"3d4d2568-b191-4f","sessionId":"sess_87eab534-f233-4d4b-b2fa-fce4caf3c45c","turnId":"turn_f032c4f6-a76c-42d6-97fd-3267cccadeed","status":"failed","context":{"queryId":"01a0b9cc-215d-700a-a1a1-221096c82786","turnNumber":11,"turnPhase":"processing_input"},"error":{"name":"Error","message":"Turn execution failed","code":"UNKNOWN_ERROR","type":"unknown_error","context":{"errorPayloadRole":"wrapper"},"cause":{"name":"AiSdkModelAdapterError","message":"Captcha instance timed out after 10000ms.","code":"model_request_failed","context":{"attempt":1,"maxAttempts":11,"modelId":"GLM-5.3-Flash","errorPhase":"prepare","exceptionKind":"generic","providerId":"account:bigmodel-start-plan","providerKind":"anthropic","reason":"unknown","requestId":"3a090f18-e2cc-45fe-a51f-70ce63ca8505","retryable":false,"source":"runtime","traceId":"f909d120-72f5-4f31-9fe8-40f62c173eda","transport":"sse"},"cause":{"name":"RuntimeHeadersRefreshError","message":"Captcha instance timed out after 10000ms.","cause":{"name":"ProtocolRequestError","message":"Captcha instance timed out after 10000ms."}}}}}"#;
    /// 实测原样（2026-09-18）：用户中断的 turn.failed——status = "cancelled"，
    /// error.message = "Turn was cancelled."（中断的第三条痕迹，须归并为 Aborted）
    const REAL_TURN_CANCELLED_LINE: &str = r#"{"timestamp":"2026-09-18T08:37:31.751Z","level":"error","event":"turn.failed","module":"core.runtime","message":"Turn failed","traceId":"5af90b5c-f74c-485d-8ce2-7f4cc59660d0","spanId":"a65374e6-4cc6-4f","parentSpanId":"67e43b88-b528-4b","sessionId":"sess_dfa58fb4-0119-46da-8c09-a1dd64be80d7","turnId":"turn_4f23348c-b59d-4a30-aaf5-5d509c13ee3a","status":"cancelled","context":{"queryId":"01a0b3a8-2dec-7fd3-9083-da8c3f924c79","turnNumber":0,"turnPhase":"processing_input"},"error":{"name":"Error","message":"Turn was cancelled.","code":"TURN_CANCELLED","type":"turn_cancelled","cause":{"name":"AiSdkModelAdapterError","message":"Model request was cancelled.","code":"model_request_cancelled","context":{"attempt":1,"maxAttempts":11,"modelId":"GLM-5.3-Flash","errorPhase":"stream","exceptionKind":"generic","providerId":"bigmodel-api","providerKind":"anthropic","reason":"cancelled","requestId":"279a2a57-68f9-4584-91b3-dda2c4542ceb","retryable":false,"source":"runtime","traceId":"5af90b5c-f74c-485d-8ce2-7f4cc59660d0","transport":"sse"},"cause":{"name":"Error","message":"v4 session stopped"}}}}"#;
    const SID: &str = "sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719";

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn log_signal_parsing_matches_real_records() {
        // 两条中断痕迹 + 中断的 turn.failed（第三条痕迹）都归并为中断信号
        // （turn.failed 样本来自另一个会话，各自断言各自的 session_id）
        for (line, sid) in [
            (REAL_STOP_LINE, SID),
            (REAL_CANCEL_LINE, SID),
            (REAL_TURN_CANCELLED_LINE, "sess_dfa58fb4-0119-46da-8c09-a1dd64be80d7"),
        ] {
            let sig = parse_log_signal(line).expect("真实中断记录必须被识别");
            assert!(matches!(sig, LogSignal::Aborted { .. }), "应判为中断: {line}");
            assert_eq!(sig.session_id(), sid);
        }
        // 致命失败（Captcha 超时）：status=failed、错误取 cause.message、带 turnId
        let sig = parse_log_signal(REAL_TURN_FAILED_LINE).expect("致命失败必须被识别");
        match &sig {
            LogSignal::Failed { session_id, turn_id, message, .. } => {
                assert_eq!(session_id, "sess_87eab534-f233-4d4b-b2fa-fce4caf3c45c");
                assert_eq!(turn_id.as_deref(), Some("turn_f032c4f6-a76c-42d6-97fd-3267cccadeed"));
                assert_eq!(message, "Captcha instance timed out after 10000ms.");
            }
            other => panic!("应判为致命失败: {other:?}"),
        }
        assert!(!sig.is_subagent(), "主会话的失败不得标成子代理");
        // 去重键：中断按会话；失败按会话+回合——不同回合的连续失败
        // （超时→手动重试→再超时）不得互相吞
        let aborted = parse_log_signal(REAL_STOP_LINE).unwrap();
        let failed_1 = parse_log_signal(REAL_TURN_FAILED_LINE).unwrap();
        let mut failed_2 = failed_1.clone();
        if let LogSignal::Failed { turn_id, .. } = &mut failed_2 {
            *turn_id = Some("turn_next".into());
        }
        assert_eq!(aborted.dedup_key().as_deref(), Some(SID));
        assert_ne!(failed_1.dedup_key(), failed_2.dedup_key(), "不同回合的失败不得共享去重键");
        assert_ne!(aborted.dedup_key(), failed_1.dedup_key());

        // turn.failed 没有 error 载荷（异常记录）：回合确实失败了，用兜底文案合成
        let sig = parse_log_signal(
            r#"{"timestamp":"2026-09-10T16:28:00.982Z","level":"error","event":"turn.failed","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"queryId":"q","turnNumber":0,"turnPhase":"processing_input"}}"#,
        )
        .expect("无 error 载荷的 turn.failed 也是回合终态");
        assert!(matches!(sig, LogSignal::Failed { message, .. } if !message.is_empty()));

        // 负样本：请求层失败（会重试、事件名不是 turn.failed）、hook 失败、正常工具完成
        for line in [
            r#"{"timestamp":"2026-09-19T13:26:03.718Z","level":"warn","event":"model.request.failed","sessionId":"sess_87eab534-f233-4d4b-b2fa-fce4caf3c45c","status":"failed","context":{"attempt":1,"maxAttempts":11,"retryable":false,"statusMessage":"Captcha instance timed out after 10000ms."}}"#,
            r#"{"timestamp":"2026-09-10T16:28:00.960Z","level":"warn","event":"hook.run.failed","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"hookEventName":"PostToolUseFailure","hookIndex":0,"source":"config.PostToolUseFailure.0.0"}}"#,
            r#"{"timestamp":"2026-09-10T16:29:54.481Z","level":"info","event":"tool.call.completed","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"toolName":"Skill"}}"#,
            // 同类记录但停顿原因不是用户停止
            r#"{"timestamp":"2026-09-10T16:28:00.959Z","level":"info","event":"v4.stop.foreground_execution_inspected","sessionId":"sess_672cbc8c-d92e-4cde-ae86-2bd0448d2719","context":{"runtimeStopKind":"completed"}}"#,
            // 非会话级记录
            r#"{"timestamp":"2026-09-10T16:28:00.959Z","level":"info","event":"v4.stop.foreground_execution_inspected","sessionId":"other","context":{"runtimeStopKind":"stopped"}}"#,
            "not json at all",
            "",
        ] {
            assert!(parse_log_signal(line).is_none(), "误判为回合终态: {line}");
        }
    }

    #[test]
    fn turn_failed_status_tiers_subagent_and_dedup_fallback() {
        // status = "failed" 但文案含 cancel：**不**得吞成中断——结构化字段明确时
        // 文案只作参考，否则真实失败会静默消失（不通知、不亮终止色）
        let sig = parse_log_signal(
            r#"{"event":"turn.failed","status":"failed","sessionId":"sess_abc12345","turnId":"turn_a","error":{"message":"Request was cancelled by upstream","cause":{"message":"upstream reset"}}}"#,
        )
        .expect("status=failed 必须判为失败");
        assert!(matches!(&sig, LogSignal::Failed { message, .. } if message == "upstream reset"));

        // status 缺失：cancel 文案兜底仍然生效（容忍措辞微调的宽匹配口径）
        let sig = parse_log_signal(
            r#"{"event":"turn.failed","sessionId":"sess_abc12345","error":{"message":"Turn was cancelled."}}"#,
        )
        .expect("status 缺失时按文案判中断");
        assert!(matches!(sig, LogSignal::Aborted { .. }));

        // 子代理会话的失败：is_subagent 按会话名判定（看门狗据此拒合成）
        let sig = parse_log_signal(
            r#"{"event":"turn.failed","status":"failed","sessionId":"sess_subagent_agent_005ac652","turnId":"turn_b","error":{"message":"Turn execution failed","cause":{"message":"boom"}}}"#,
        )
        .expect("子代理失败必须被识别");
        assert!(sig.is_subagent(), "sess_subagent_* 必须判为子代理");
        assert!(matches!(sig, LogSignal::Failed { .. }));

        // 无 turnId 的失败：不去重（None）——无标识时宁可重复也不静默吞信号
        let sig = parse_log_signal(
            r#"{"event":"turn.failed","status":"failed","sessionId":"sess_abc12345","error":{"message":"Turn execution failed","cause":{"message":"boom"}}}"#,
        )
        .unwrap();
        assert!(matches!(&sig, LogSignal::Failed { turn_id: None, .. }));
        assert_eq!(sig.dedup_key(), None, "无 turnId 不得退化为会话级去重键");
    }

    #[test]
    fn tail_skips_existing_content_then_reads_appends() {
        let dir = tmpdir();
        let log = dir.join("zcode-2026-09-11.jsonl");
        // 启动前就存在的中断（陈年旧事）不该被补报
        std::fs::write(&log, format!("{REAL_STOP_LINE}\n")).unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty(), "首见文件应从末尾开始，不补旧信号");

        append(&log, &format!("{REAL_CANCEL_LINE}\n"));
        let got = tail.poll(&dir);
        assert_eq!(got.len(), 1, "新增的中断记录要被读到");
        assert_eq!(got[0].session_id(), SID);
        assert!(tail.poll(&dir).is_empty(), "同一行不该被读第二次");

        // 一次追加两条（去重由调用方做，这里两条都要返回）
        append(&log, &format!("{REAL_STOP_LINE}\n{REAL_CANCEL_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 2);

        // 致命失败的 turn.failed：同样要被增量读到
        append(&log, &format!("{REAL_TURN_FAILED_LINE}\n"));
        let got = tail.poll(&dir);
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], LogSignal::Failed { .. }), "应读到失败信号");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_handles_partial_lines() {
        let dir = tmpdir();
        let log = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&log, "").unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());

        // 半行（日志边写边刷）：不能当成完整行解析，也不能丢
        let cut = 40;
        append(&log, &REAL_CANCEL_LINE[..cut]);
        assert!(tail.poll(&dir).is_empty(), "半行不该产生信号");
        append(&log, &format!("{}\n", &REAL_CANCEL_LINE[cut..]));
        let got = tail.poll(&dir);
        assert_eq!(got.len(), 1, "补齐后的行必须被解析出来");
        assert_eq!(got[0].session_id(), SID);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_follows_newest_log_across_days() {
        let dir = tmpdir();
        let yesterday = dir.join("zcode-2026-09-10.jsonl");
        std::fs::write(&yesterday, format!("{REAL_STOP_LINE}\n")).unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());

        // 跨天：新文件出现后应切过去，并从它的末尾开始
        let today = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&today, format!("{REAL_CANCEL_LINE}\n")).unwrap();
        assert!(tail.poll(&dir).is_empty(), "换文件后同样不补旧信号");
        append(&today, &format!("{REAL_STOP_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 1);

        // 旧文件继续增长也不该再被读（它已经不是最新那份）
        append(&yesterday, &format!("{REAL_CANCEL_LINE}\n"));
        assert!(tail.poll(&dir).is_empty(), "只跟随最新日志");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_recovers_from_truncation_without_replaying_history() {
        let dir = tmpdir();
        let log = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&log, format!("{REAL_STOP_LINE}\n")).unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());
        append(&log, &format!("{REAL_CANCEL_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 1);

        // 清理/重建导致文件变短：视同换新文件、从当前末尾开始——**不得重放历史**。
        // 历史行里的失败信号去重键互不相同（会话|回合）、30 秒窗口拦不住，
        // 重放会把正在跑的会话批量误杀（假「任务失败」通知 + 误亮终止色 + 摘表）
        std::fs::write(&log, format!("{REAL_STOP_LINE}\n{REAL_CANCEL_LINE}\n")).unwrap();
        assert!(tail.poll(&dir).is_empty(), "重建后的旧内容属于历史，不得重放");

        // 重建之后的新增照常读到（不 panic、不死循环）
        append(&log, &format!("{REAL_TURN_FAILED_LINE}\n"));
        let got = tail.poll(&dir);
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], LogSignal::Failed { .. }));
        assert!(tail.poll(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- §2.9 中断宽匹配收窄回归 --------------------------------------------

    /// (i) 形态：`runtimeStopKind == "stopped"` 只在
    /// `v4.stop.foreground_execution_inspected` 事件上算中断
    #[test]
    fn abort_shape_stopped_requires_exact_event_name() {
        // 正样本（实测原样）已在 log_signal_parsing_matches_real_records 覆盖；
        // 这里锁反面：同款 context 挂在别的事件名上**不再**合成 RunAborted
        for event in ["tool.call.completed", "turn.retrying", "model.request.failed"] {
            let line = format!(
                r#"{{"event":"{event}","sessionId":"sess_abc12345","context":{{"runtimeStopKind":"stopped","error":"Turn was cancelled."}}}}"#
            );
            assert!(
                parse_log_signal(&line).is_none(),
                "{event} 不是中断痕迹，不得合成 RunAborted（§2.9 收窄）: {line}"
            );
        }
    }

    /// (ii) 形态：无 event 字段 + message 含 "background turn failed" + error 含 cancel
    #[test]
    fn abort_shape_background_turn_failed_warn_record() {
        let ok = r#"{"timestamp":"t","level":"warn","module":"core.runtime","message":"v4 background turn failed","sessionId":"sess_abc12345","context":{"error":"Turn was cancelled."}}"#;
        assert!(matches!(parse_log_signal(ok), Some(LogSignal::Aborted { .. })));
    }

    /// (ii) 的 message 短语是必要条件：换措辞的记录不算中断痕迹
    #[test]
    fn abort_shape_cancel_warn_needs_message_phrase() {
        let no_phrase = r#"{"message":"background turn aborted","sessionId":"sess_abc12345","context":{"error":"Turn was cancelled."}}"#;
        assert!(parse_log_signal(no_phrase).is_none(), "message 不含 background turn failed 不算");
    }

    /// (ii) 的 error 含 cancel 是必要条件
    #[test]
    fn abort_shape_cancel_warn_needs_cancel_in_error() {
        let no_cancel = r#"{"message":"v4 background turn failed","sessionId":"sess_abc12345","context":{"error":"model blew up"}}"#;
        assert!(parse_log_signal(no_cancel).is_none(), "error 不含 cancel 不算");
    }

    /// (ii) 只认**没有 event 字段**的 warn 记录；带 event 的记录走各自分支
    #[test]
    fn abort_shape_cancel_warn_requires_missing_event_field() {
        let has_event = r#"{"event":"turn.failed","message":"v4 background turn failed","sessionId":"sess_abc12345","context":{"error":"Turn was cancelled."}}"#;
        // turn.failed 走三档 status 判定（status 缺失 → cancel 文案兜底 → 中断），
        // 但那是 turn.failed 分支，不是 (ii) ——这里断言它不从 (ii) 漏出去即可
        assert!(parse_log_signal(has_event).is_some(), "turn.failed 有自己的判定分支");
        let other_event = r#"{"event":"model.request.failed","message":"v4 background turn failed","sessionId":"sess_abc12345","context":{"error":"Turn was cancelled."}}"#;
        assert!(
            parse_log_signal(other_event).is_none(),
            "带 event 的记录不得走 (ii) 的兜底匹配"
        );
    }

    /// §2.9 主回归：**带 cancel 文案的非终态记录不再误判**——旧宽匹配对任意事件的
    /// `context.error` 含 "cancel" 就合成 RunAborted，把运行中会话误摘表、
    /// 真实完成通知漏发
    #[test]
    fn cancel_text_on_non_terminal_records_is_not_aborted() {
        for line in [
            // 「取消后重试」类文案：会话还在跑
            r#"{"event":"model.request.failed","sessionId":"sess_abc12345","context":{"error":"cancelled by API error, retrying","attempt":1}}"#,
            r#"{"event":"model.request.retrying","sessionId":"sess_abc12345","context":{"error":"request cancelled due to timeout, retrying"}}"#,
            // 纯工具级取消（工具被跳过，回合继续）
            r#"{"event":"tool.call.cancelled","sessionId":"sess_abc12345","context":{"toolName":"Bash","error":"tool call cancelled by user filter"}}"#,
            // 有 event 字段的任意记录：error 含 cancel 不再兜底命中
            r#"{"event":"hook.run.failed","sessionId":"sess_abc12345","context":{"error":"hook cancelled by timeout"}}"#,
            // 无 event 但 message 不是 background turn failed
            r#"{"message":"turn progress","sessionId":"sess_abc12345","context":{"error":"cancelled by API error, retrying"}}"#,
        ] {
            assert!(
                parse_log_signal(line).is_none(),
                "带 cancel 文案的非终态记录不得合成 RunAborted: {line}"
            );
        }
    }

    // ---- §2.8 日志 tail 三连回归 --------------------------------------------

    /// §2.8c：`newest_log_file` 只认 `zcode-<yyyy-mm-dd>.jsonl`，
    /// `zcode-foo.jsonl` 等非日期名（字典序 'f' > '2'）不得劫持 tail
    #[test]
    fn newest_log_ignores_non_date_names() {
        let dir = tmpdir();
        // 只有非日期名：找不到日志（宁可不读，也不能跟错文件）
        std::fs::write(dir.join("zcode-foo.jsonl"), "").unwrap();
        std::fs::write(dir.join("zcode-9999.jsonl"), "").unwrap();
        std::fs::write(dir.join("zcode-2026-09.jsonl"), "").unwrap();
        std::fs::write(dir.join("zcode-.jsonl"), "").unwrap();
        std::fs::write(dir.join("notes.jsonl"), "").unwrap();
        assert!(newest_log_file(&dir).is_none(), "非日期名一律忽略");

        // 有日期文件时：字典序更大的 zcode-foo.jsonl 不得压过真实日期
        std::fs::write(dir.join("zcode-2026-09-11.jsonl"), "").unwrap();
        let got = newest_log_file(&dir).unwrap();
        assert!(got.ends_with("zcode-2026-09-11.jsonl"), "got={}", got.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.8c：日期名取最大；大小写不敏感（Windows 文件系统不区分大小写）
    #[test]
    fn newest_log_picks_latest_date_case_insensitively() {
        let dir = tmpdir();
        std::fs::write(dir.join("zcode-2026-09-10.jsonl"), "").unwrap();
        std::fs::write(dir.join("zcode-2026-09-11.jsonl"), "").unwrap();
        let got = newest_log_file(&dir).unwrap();
        assert!(got.ends_with("zcode-2026-09-11.jsonl"), "got={}", got.display());

        std::fs::write(dir.join("ZCODE-2026-09-12.JSONL"), "").unwrap();
        let got = newest_log_file(&dir).unwrap();
        assert!(
            got.ends_with("ZCODE-2026-09-12.JSONL"),
            "大小写变体也应被认作日期日志: {}",
            got.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.8c：日期段的形状必须精确（10 位、分隔符在 4/7 位、其余数字）
    #[test]
    fn newest_log_rejects_malformed_date_shapes() {
        for name in [
            "zcode-2026-9-11.jsonl",   // 未零填充
            "zcode-2026-091-1.jsonl",  // 段长不对
            "zcode-2026_09_11.jsonl",  // 分隔符不对
            "zcode-202a-09-11.jsonl",  // 含非数字
            "zcode-2026-09-11.log",    // 后缀不对
            "zcode-2026-09-11.jsonl.bak",
        ] {
            assert!(!is_dated_log_name(name), "不应识别为日期日志: {name}");
        }
        assert!(is_dated_log_name("zcode-2026-09-11.jsonl"));
    }

    /// §2.8b：同名日志删除重建后**超过旧 offset** 也必须被识别为新文件
    /// （created 变化）——走「从末尾开始」分支，绝不从旧 offset 拦腰读新文件。
    ///
    /// 白盒构造 created 变化：本项目所在的文件系统会冻结创建时间戳（delete +
    /// recreate 后 `metadata.created()` 返回同一值），真实重建拿不到新 created；
    /// 被测逻辑是「记录的 created 与当前不一致 → 视同新文件」，这里把 tail 记录的
    /// created 伪造成重建前的旧值来钉死该分支（真实 Windows/NTFS 上 delete +
    /// recreate 自然产生新 created，效果相同）。
    #[test]
    fn tail_detects_recreated_file_larger_than_old_offset() {
        let dir = tmpdir();
        let log = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&log, format!("{REAL_STOP_LINE}\n")).unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());
        append(&log, &format!("{REAL_CANCEL_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 1);
        let old_offset = tail.offset;
        assert!(old_offset > 0);

        // 伪造「重建前」的创建时间（等价于 delete + recreate 拿到新 created）
        tail.created = tail
            .created
            .map(|t| t - std::time::Duration::from_secs(60));
        // 重建后的文件长度**超过**旧 offset，且旧 offset 之后恰好是一条完整信号行——
        // 只看 len < offset 的旧实现会从这里拦腰读进去并合成信号
        std::fs::remove_file(&log).unwrap();
        let filler = "x".repeat(old_offset as usize);
        std::fs::write(&log, format!("{filler}{REAL_CANCEL_LINE}\n")).unwrap();

        assert!(
            tail.poll(&dir).is_empty(),
            "重建后的文件视同新文件、从末尾开始——旧 offset 之后的内容不得被读出信号"
        );
        // 重建后新增的行照常按行读到（行边界对齐，没有半行垃圾拼进来）
        append(&log, &format!("{REAL_TURN_FAILED_LINE}\n"));
        let got = tail.poll(&dir);
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0], LogSignal::Failed { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.8a 防回归：换文件的语义是「从末尾开始」（注释与实现一致——
    /// 从头重放历史会批量误杀正在跑的会话）
    #[test]
    fn tail_new_file_starts_from_end_not_head() {
        let dir = tmpdir();
        let today = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&today, "").unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());
        // 换到新的一天：新文件里已有的历史信号不得补报
        let next = dir.join("zcode-2026-09-12.jsonl");
        std::fs::write(&next, format!("{REAL_TURN_FAILED_LINE}\n{REAL_STOP_LINE}\n")).unwrap();
        assert!(tail.poll(&dir).is_empty(), "换文件后从末尾开始，不补历史信号");
        append(&next, &format!("{REAL_CANCEL_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 1, "换文件后的新增照常读到");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- §2.18 杂项回归 ------------------------------------------------------

    /// §2.18b：has_args_marker 按**原索引**逐位精确比较——非字符串元素不得
    /// 被压缩后错位误判（旧 filter_map 实现会把用户条目认领成我们的）
    #[test]
    fn has_args_marker_compares_by_original_index() {
        let ours = json!({
            "type": "process", "command": "C:/x/AgentBark.exe",
            "args": ["hook", "--agent", "zcode", "--event", "Stop"],
        });
        assert!(has_args_marker(&ours));
        // 非字符串元素把标记挤离前 3 位 → 不得命中（旧 filter_map 压缩会错位误判）
        for args in [
            json!([null, "hook", "--agent", "zcode"]),
            json!([1, "hook", "--agent", "zcode"]),
            json!([[], "--agent", "zcode"]),
            json!([null, null, null, "hook", "--agent", "zcode"]),
        ] {
            let entry = json!({ "type": "process", "command": "C:/x/AgentBark.exe", "args": args });
            assert!(
                !has_args_marker(&entry),
                "错位的 args 不得判为我们的标记: {entry}"
            );
        }
        // 标记落在前 3 位即认领（后面的元素不看）：尾部异常元素由 refresh_our_entry
        // 就地修复成规范 args，不该被当成用户条目留下
        let trailing_null = json!({
            "type": "process", "command": "C:/x/AgentBark.exe",
            "args": ["hook", "--agent", "zcode", null],
        });
        assert!(has_args_marker(&trailing_null), "标记位正确即认领: {trailing_null}");
        // 标记必须落在 args 的前 3 位（前缀多一个元素不算）
        let prefixed = json!({
            "type": "process", "command": "C:/x/AgentBark.exe",
            "args": ["serve", "hook", "--agent", "zcode"],
        });
        assert!(!has_args_marker(&prefixed), "标记必须落在 args 的前 3 位");
    }

    /// §2.8c：空目录 / 无日志时返回 None（tail 不跟错文件）
    #[test]
    fn newest_log_returns_none_for_empty_or_missing_dir() {
        let dir = tmpdir();
        assert!(newest_log_file(&dir).is_none());
        assert!(newest_log_file(&dir.join("missing")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.18b：标记三元组必须**逐位全等**（as_str 精确比较，长度也必须相等）
    #[test]
    fn has_args_marker_requires_exact_triple() {
        let mk = |args: Value| json!({ "type": "process", "command": "C:/x/AgentBark.exe", "args": args });
        // 缺位 / 长度不等的元素不算
        assert!(!has_args_marker(&mk(json!(["hook", "--agent"]))));
        assert!(!has_args_marker(&mk(json!(["hook"]))));
        assert!(!has_args_marker(&mk(json!(["hook", "--agent", "zcodes"]))));
        assert!(!has_args_marker(&mk(json!(["hook", "--agent", "zcod"]))));
        assert!(!has_args_marker(&mk(json!(["hook", "--agents", "zcode"]))));
        // 无 args 字段 / args 不是数组
        assert!(!has_args_marker(&json!({ "type": "process", "command": "x" })));
        assert!(!has_args_marker(&json!({ "type": "process", "command": "x", "args": "hook" })));
    }

    /// §2.18a：shell 形态命令的 `--agent zcode` 认领必须是 token 边界匹配
    #[test]
    fn legacy_cmd_agent_flag_accepts_exact_forms() {
        for cmd in [
            r#""C:/Program Files/AgentBark/AgentBark.exe" hook --agent zcode --event Stop"#,
            r#""C:/x/AgentBark.exe" hook --agent=zcode"#,
        ] {
            assert!(cmd_has_agent_flag(cmd, "zcode"), "应认领: {cmd}");
            assert!(is_our_entry(&json!({ "type": "command", "command": cmd })), "应判为我们条目: {cmd}");
        }
    }

    /// §2.18a：`--agent zcode-canary` 是用户条目，不得被覆写/删除（token 边界）
    #[test]
    fn legacy_cmd_agent_flag_rejects_lookalikes() {
        for cmd in [
            r#""C:/x/AgentBark.exe" hook --agent zcode-canary --event Stop"#,
            r#""C:/x/AgentBark.exe" hook --agent=zcode-canary"#,
            r#""C:/x/AgentBark.exe" hook --agent zcodeextra"#,
            r#""C:/x/AgentBark.exe" hook --my-agent zcode"#,
            r#""C:/x/AgentBark.exe" hook --agent=zcode-extra"#,
        ] {
            assert!(!cmd_has_agent_flag(cmd, "zcode"), "不得认领: {cmd}");
            assert!(
                !is_our_entry(&json!({ "type": "command", "command": cmd })),
                "应判为用户条目（不能覆写/删除）: {cmd}"
            );
        }
    }

    /// §4.15：null 哨兵清理只删**我们自己造的**哨兵（按索引），用户配置里的
    /// 合法 null 元素原样保留
    #[test]
    fn null_sentinel_cleanup_keeps_user_null_elements() {
        let dir = tmpdir();
        let path = cfg(&dir);
        // 用户在 hooks 列表里放了合法 null 元素 + 一条我们的旧路径重复条目
        write_json(
            &path,
            &json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "Stop": [
                            { "hooks": [
                                null,
                                { "type": "command", "command": "\"C:/old/AgentBark.exe\" hook --agent zcode --event Stop" },
                                null,
                                { "type": "process", "command": "C:/old/AgentBark.exe",
                                  "args": ["hook", "--agent", "zcode", "--event", "Stop"] }
                            ] }
                        ]
                    }
                }
            }),
        );
        let a = adapter(&dir);
        a.register(&ctx()).unwrap();
        let doc = read(&path);
        let entries = doc["hooks"]["events"]["Stop"][0]["hooks"].as_array().unwrap();
        let nulls = entries.iter().filter(|e| e.is_null()).count();
        assert_eq!(nulls, 2, "用户的两个合法 null 元素必须原样保留（§4.15）");
        let ours = entries.iter().filter(|e| is_our_entry(e)).count();
        assert_eq!(ours, 1, "重复的我们的条目去重后只留一个（§1.9）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §1.9：跨组重复的我们的条目去重到只剩一个（每事件只起一个 hook 进程）
    #[test]
    fn cross_group_duplicate_ours_entries_are_deduped() {
        let dir = tmpdir();
        let path = cfg(&dir);
        write_json(
            &path,
            &json!({
                "hooks": {
                    "enabled": true,
                    "events": {
                        "Stop": [
                            { "hooks": [
                                { "type": "command", "command": "node mine.mjs" },
                                { "type": "process", "command": "C:/old/AgentBark.exe",
                                  "args": ["hook", "--agent", "zcode", "--event", "Stop"] }
                            ] },
                            { "hooks": [
                                { "type": "process", "command": "C:/old/AgentBark.exe",
                                  "args": ["hook", "--agent", "zcode", "--event", "Stop"] }
                            ] }
                        ]
                    }
                }
            }),
        );
        let a = adapter(&dir);
        a.register(&ctx()).unwrap();
        let doc = read(&path);
        let mut count = 0;
        let mut user = 0;
        for g in doc["hooks"]["events"]["Stop"].as_array().unwrap() {
            for e in g["hooks"].as_array().unwrap() {
                if is_our_entry(e) {
                    count += 1;
                } else if !e.is_null() {
                    user += 1;
                }
            }
        }
        assert_eq!(count, 1, "跨组重复的我们的条目必须去重（§1.9），实际 {count} 个");
        assert_eq!(user, 1, "用户的条目原样保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4.16：日志行首 BOM 剥离（对齐 jsonio 的 BOM 口径）
    #[test]
    fn log_line_bom_is_stripped() {
        let with_bom = format!("\u{feff}{REAL_CANCEL_LINE}");
        assert!(
            matches!(parse_log_signal(&with_bom), Some(LogSignal::Aborted { .. })),
            "行首 BOM 不得让整行解析失败（信号被静默丢弃）"
        );
        let with_bom_failed = format!("\u{feff}{REAL_TURN_FAILED_LINE}");
        assert!(matches!(parse_log_signal(&with_bom_failed), Some(LogSignal::Failed { .. })));

        // tail 路径同样生效：BOM 行照常解析出信号
        let dir = tmpdir();
        let log = dir.join("zcode-2026-09-11.jsonl");
        std::fs::write(&log, "").unwrap();
        let mut tail = AbortLogTail::default();
        assert!(tail.poll(&dir).is_empty());
        append(&log, &format!("\u{feff}{REAL_CANCEL_LINE}\n"));
        assert_eq!(tail.poll(&dir).len(), 1, "tail 读到的 BOM 行必须解析出信号");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4.16：is_subagent 兼容 `sessionId`（camelCase）字段——对齐 bark-core from_raw
    #[test]
    fn is_subagent_accepts_camel_case_session_id() {
        assert!(is_subagent(&json!({ "sessionId": "sess_subagent_agent_005ac652" })));
        assert!(is_subagent(&json!({ "session_id": "sess_subagent_agent_005ac652" })));
        assert!(!is_subagent(&json!({ "sessionId": "sess_main_123" })));
    }

    /// §4.16：显式布尔字段优先于会话名命名判定
    #[test]
    fn is_subagent_explicit_bool_wins_over_session_name() {
        assert!(!is_subagent(&json!({ "sessionId": "sess_subagent_x", "is_subagent": false })));
        assert!(is_subagent(&json!({ "session_id": "sess_main", "is_subagent": true })));
        assert!(is_subagent(&json!({ "session_id": "sess_main", "subagent": true })));
    }
}
