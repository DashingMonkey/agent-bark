//! agent 适配层：
//! - HookAdapter：向 agent 的配置文件注册 hook 命令，事件由 agent 主动推送
//! - WatchAdapter：轮询 agent 本地数据文件（SQLite 等），推断状态跃迁产出事件
//!
//! 两种模式产出同一种 NormalizedEvent，下游无感。

pub mod claude_style;
pub mod dsh;
pub mod jsonio;
pub mod opencode;
pub mod registry;
pub mod traework_db;
pub mod watch;
pub mod zcode;

use bark_core::{AdapterMode, AgentKind, EventKind, NormalizedEvent};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

/// 探测/安装时依赖的环境（便于测试注入）
#[derive(Debug, Clone)]
pub struct InstallEnv {
    pub home: PathBuf,
}

impl InstallEnv {
    pub fn detect() -> Self {
        // 与 watch.rs 的 db_path 同一条兜底链：known-folder API 优先，环境变量兜底。
        // "." 只是最后的保底（几乎不可达）——它会让各 spec 去探测当前目录下的
        // 相对路径，写入的 hook 也指向无效位置。
        Self {
            home: dirs::home_dir()
                .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
                .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// 注册 hook 时所需的上下文
pub struct RegisterCtx {
    /// agent-bark 可执行文件绝对路径（hook 命令指向它）
    pub exe_path: String,
    pub port: u16,
    pub token: String,
}

/// 卸载/写入时的上下文
pub struct InstallCtx {
    pub exe_path: String,
    /// 只计算不落盘
    pub dry_run: bool,
    /// 是否备份
    pub backup: bool,
}

/// 接入状态校验结果（「已注册」不等于「仍然可用」）
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VerifyReport {
    /// 配置里没有我们的条目
    NotRegistered,
    /// 条目存在且指向当前可执行文件
    Ok,
    /// 条目存在，但命令指向的是旧路径（程序被移动/重装过）
    StalePath { path: PathBuf },
    /// 配置文件读不了（JSON 非法等），需要用户先修复
    ConfigUnreadable { path: PathBuf, reason: String },
}

/// UI 展示的 agent 状态
#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub kind: &'static str,
    pub display_name: &'static str,
    pub mode: AdapterMode,
    /// 检测到已安装（配置目录存在）
    pub installed: bool,
    /// hook 已注册 / 监控已启动
    pub registered: bool,
    /// 配置开关（config.json 中 agents[].enabled）
    pub enabled: bool,
    /// 写入后需要用户在 agent 内完成信任/审核时的引导文案
    pub trust_hint: Option<&'static str>,
    /// 我们写入的配置文件路径（诊断用）
    pub config_paths: Vec<String>,
    /// 接入状态校验结果（漂移/配置损坏会在这里体现）
    pub verify: Option<VerifyReport>,
    /// 最近一次运行时错误（如监控线程启动失败），供 UI 直接展示
    pub last_error: Option<String>,
}

/// hook 型适配器：各 agent 事件由对方配置触发并推送给我们
pub trait HookAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn display_name(&self) -> &'static str;

    /// agent 已安装（检测配置目录 / CLI）
    fn is_installed(&self) -> bool;
    /// 我们的 hook 是否已写入（不校验路径是否仍然有效，见 verify）
    fn is_registered(&self) -> bool;
    /// 我们会写入/读取的配置文件
    fn config_paths(&self) -> Vec<PathBuf>;

    /// 我们订阅的 agent 事件名列表
    fn hook_events(&self) -> Vec<&'static str>;

    /// 外科手术式写入：只添加/修复自己的条目，写前备份
    fn register(&self, ctx: &RegisterCtx) -> anyhow::Result<()>;
    /// 精确回收自己的条目（保留同组内用户自己的 hook）
    fn unregister(&self, ctx: &mut InstallCtx) -> anyhow::Result<()>;

    /// 接入状态校验：识别「程序移动导致 hook 失效」「配置损坏」
    fn verify(&self, ctx: &RegisterCtx) -> VerifyReport;

    /// agent 原始事件名 → 统一事件类型
    fn event_kind(&self, event_name: &str) -> Option<EventKind>;

    /// 把 hook stdin JSON 归一化
    fn normalize(&self, event_name: &str, raw: &Value) -> Option<NormalizedEvent> {
        let kind = self.event_kind(event_name)?;
        Some(NormalizedEvent::from_raw(self.kind().id(), kind, raw))
    }

    /// 信任引导（如 Codex 需在会话内运行 /hooks）
    fn trust_hint(&self) -> Option<&'static str> {
        None
    }

    /// 关闭接入后给用户的提示。
    ///
    /// 默认 `None`：hook 型 agent 的下一条 hook 命令就会按新配置执行，无需额外动作。
    /// 插件型（DSH / OpenCode）必须给出提示——我们删掉的是磁盘上的插件文件与登记，
    /// 而**宿主进程里已经加载的那份仍在内存里继续 POST**，只有重启宿主才真正停止。
    /// 在宿主重启前，daemon 侧会按「已关闭」忽略该 agent 的事件（见 state.rs 管道门）。
    fn unregister_hint(&self) -> Option<&'static str> {
        None
    }
}
