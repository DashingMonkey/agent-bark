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
    // ---- 2026-09 广度扩展（官方 hooks 文档 + AgentPet/lazyagent 等开源实现
    // 双源交叉验证后接入；均未实机测试，依赖各家启动自愈 + 判死兜底）----
    /// Google Gemini CLI（~/.gemini/settings.json，Claude 嵌套变体）
    GeminiCli,
    /// Qwen Code（~/.qwen/settings.json，官方 22 事件，Claude 风格）
    QwenCode,
    /// Factory Droid（~/.factory/hooks.json，Claude 同构嵌套）
    Droid,
    /// xAI Grok CLI（~/.grok/hooks/agent-bark.json 专用文件，payload 为 camelCase）
    Grok,
    /// Cursor（~/.cursor/hooks.json，扁平结构 + version:1）
    Cursor,
    /// GitHub Copilot CLI（~/.copilot/hooks/agent-bark.json 专用文件，官方事件为 camelCase）
    CopilotCli,
    /// Windsurf（~/.codeium/windsurf/hooks.json，扁平结构无 version，仅 2-3 个事件）
    Windsurf,
    /// Google Antigravity（~/.gemini/config/hooks.json 命名组，payload 无事件名字段）
    Antigravity,
    /// Kimi Code（Moonshot，~/.kimi-code/config.toml 的 [[hooks]]——唯一的 TOML 宿主；
    /// hooks 对 CLI / 桌面客户端 / VS Code 插件三个前端通用）
    KimiCode,
    /// Kimi Work（Kimi 电脑客户端 Work 模式，内嵌 daimon 守护进程跑 Kimi Code 内核，
    /// 会话以 kimi-code 格式落盘在自己的 runtime home 下——监控型轮询，见 kimiwork.rs）
    KimiWork,
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
    pub const ALL: [AgentKind; 20] = [
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
        AgentKind::GeminiCli,
        AgentKind::QwenCode,
        AgentKind::Droid,
        AgentKind::Grok,
        AgentKind::Cursor,
        AgentKind::CopilotCli,
        AgentKind::Windsurf,
        AgentKind::Antigravity,
        AgentKind::KimiCode,
        AgentKind::KimiWork,
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
            AgentKind::GeminiCli => "gemini-cli",
            AgentKind::QwenCode => "qwen-code",
            AgentKind::Droid => "droid",
            AgentKind::Grok => "grok",
            AgentKind::Cursor => "cursor",
            AgentKind::CopilotCli => "copilot-cli",
            AgentKind::Windsurf => "windsurf",
            AgentKind::Antigravity => "antigravity",
            AgentKind::KimiCode => "kimi-code",
            AgentKind::KimiWork => "kimi-work",
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
            AgentKind::GeminiCli => "Gemini CLI",
            AgentKind::QwenCode => "Qwen Code",
            AgentKind::Droid => "Factory Droid",
            AgentKind::Grok => "Grok CLI",
            AgentKind::Cursor => "Cursor",
            AgentKind::CopilotCli => "Copilot CLI",
            AgentKind::Windsurf => "Windsurf",
            AgentKind::Antigravity => "Antigravity",
            AgentKind::KimiCode => "Kimi Code",
            AgentKind::KimiWork => "Kimi Work",
        }
    }

    pub fn mode(&self) -> AdapterMode {
        match self {
            AgentKind::TraeWork | AgentKind::WorkBuddy | AgentKind::KimiWork => AdapterMode::Watch,
            // DSH：生成原生 Cordis 插件并挂载（见 bark-adapters/src/dsh.rs）
            _ => AdapterMode::Hook,
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.id() == s)
    }
}
