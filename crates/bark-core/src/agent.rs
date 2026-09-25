use serde::{Deserialize, Serialize};

/// 已下线的 agent id：不再注册 adapter、设置页里也看不到。
///
/// 名单保留下来，是为了让 adapter 注册时能顺手清掉用户配置里指向它们的死 hook
/// （那些条目现在只会拉起一个立刻 `exit 0` 的 bark-cli 进程，纯属噪音）：
/// - `qoder-work`：QoderWork，官方已从产品线除名
/// - `qoder-cn-cli`：Qoder CN CLI，几乎无人使用
/// - `qoder-cn`：Qoder CN 国内版已与国际版**合并**成一条 adapter（id 仍是 `qoder`，
///   同时写 `~/.qoder/settings.json` 与 `~/.qoder-cn/settings.json`，与 TraeCode 同样式）。
///   老配置里的 `qoder-cn` 开关会在加载时并到 `qoder` 上（见 `BarkConfig::load`）
pub const RETIRED_AGENT_IDS: [&str; 3] = ["qoder-work", "qoder-cn-cli", "qoder-cn"];

/// 已知的 agent 种类（hook 型 + 监控型统一枚举）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    ClaudeCode,
    TraeCode,
    CodeBuddy,
    Qoder,
    Codex,
    Zcode,
    OpenCode,
    Dsh,
    TraeWork,
    WorkBuddy,
}

/// adapter 的工作模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterMode {
    /// 事件监听挂载点：各 agent 差异见各适配器模块头注释
    Hook,
    /// 轮询/监听 agent 本地数据文件，推断状态跃迁
    Watch,
}

impl AgentKind {
    /// 全部已支持的 agent（QoderWork / Qoder CN CLI / Qoder CN 独立条目已下线：
    /// 前两者没人用或被官方除名，后者并入了 Qoder。都不再注册 adapter，用户配置里
    /// 残留的旧 id 字符串无害——agents 段存的是 String，bark-cli 遇到未知 id 也只
    /// 打印一行并 exit 0）
    pub const ALL: [AgentKind; 10] = [
        AgentKind::ClaudeCode,
        AgentKind::TraeCode,
        AgentKind::CodeBuddy,
        AgentKind::Qoder,
        AgentKind::Codex,
        AgentKind::Zcode,
        AgentKind::OpenCode,
        AgentKind::Dsh,
        AgentKind::TraeWork,
        AgentKind::WorkBuddy,
    ];

    pub fn id(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::TraeCode => "trae-code",
            AgentKind::CodeBuddy => "codebuddy",
            AgentKind::Qoder => "qoder",
            AgentKind::Codex => "codex",
            AgentKind::Zcode => "zcode",
            AgentKind::OpenCode => "opencode",
            AgentKind::Dsh => "dsh",
            AgentKind::TraeWork => "trae-work",
            AgentKind::WorkBuddy => "workbuddy",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::TraeCode => "TraeCode",
            AgentKind::CodeBuddy => "CodeBuddy",
            // 国际版与国内版（Qoder CN）共用这一条：一般人不会同时装两个版本，
            // 装了也会一起写（与 TraeCode 的「国际版 / 国内版」同样式）
            AgentKind::Qoder => "Qoder",
            AgentKind::Codex => "Codex",
            // Z.ai（智谱 GLM）的编程 Agent，国内 bigmodel / 国际 z.ai 同一份配置
            AgentKind::Zcode => "ZCode",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Dsh => "DeepSeek Harness",
            AgentKind::TraeWork => "TraeWork",
            AgentKind::WorkBuddy => "WorkBuddy",
        }
    }

    pub fn mode(&self) -> AdapterMode {
        match self {
            AgentKind::TraeWork | AgentKind::WorkBuddy => AdapterMode::Watch,
            // DSH：生成原生 Cordis 插件并挂载（见 bark-adapters/src/dsh.rs）
            _ => AdapterMode::Hook,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.id() == s)
    }
}
