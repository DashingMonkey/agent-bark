use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// agent-bark 全局配置。
/// 实际路径见 [`BarkConfig::path`]：平台配置目录下的 `agent-bark/config.json`
/// （Windows: `%APPDATA%\agent-bark\config.json`）。
/// daemon 与 hook 子命令共用（子命令需要读取端口与 token）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BarkConfig {
    pub server: ServerConfig,
    pub notify: NotifyConfig,
    pub rules: RulesConfig,
    pub channels: ChannelsConfig,
    /// 各 agent 的接入开关（是否注册 hook / 启动监控）
    pub agents: Vec<AgentState>,
    /// 屏幕光效（边缘光效 + 全屏特效）设置
    pub glow: GlowConfig,
    /// 桌面悬浮窗（当前活动会话的小卡片）
    pub widget: WidgetConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// 本地事件服务端口（127.0.0.1）
    pub port: u16,
    /// 随机 token，首次启动生成，写入 hook 命令
    pub token: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 36573,
            token: uuid::Uuid::new_v4().simple().to_string(),
        }
    }
}

/// 单个状态的音效设置（「通知」页「声音」区的一行）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateSound {
    /// 音效 id（见 bark-channels 的 `SOUND_EFFECTS`）；空串 = 该状态不响
    pub effect: String,
    /// 播放次数（播放时夹到 1~10）
    pub plays: u32,
}

impl Default for StateSound {
    fn default() -> Self {
        Self { effect: String::new(), plays: 1 }
    }
}

/// 通知设置。
///
/// 桌面 Toast 与系统提示音已下线，只剩「状态音效」：思考 / 等待 / 完成 / 失败
/// 四个状态各自可选一个常见音效，默认全部未选（= 完全静音）。
/// `enabled` 是「声音」区标题右侧的总开关，关闭后所有状态都不响，
/// 各状态自己的音效设置保留（重新打开即恢复）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// 总开关。默认开：音效本来就是逐状态自愿配置的，再默认关等于双重关闭
    pub enabled: bool,
    /// 思考（新回合开始 / agent 干活中）
    pub thinking: StateSound,
    /// 等待（等待确认 / 等待输入）
    pub waiting: StateSound,
    /// 完成
    pub completed: StateSound,
    /// 失败（任务失败 / 心跳判死）
    pub failed: StateSound,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thinking: StateSound::default(),
            waiting: StateSound::default(),
            completed: StateSound::default(),
            failed: StateSound::default(),
        }
    }
}

impl NotifyConfig {
    /// 取某个状态（"thinking" / "waiting" / "completed" / "failed"）的音效设置
    pub fn sound(&self, state: &str) -> Option<&StateSound> {
        match state {
            "thinking" => Some(&self.thinking),
            "waiting" => Some(&self.waiting),
            "completed" => Some(&self.completed),
            "failed" => Some(&self.failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RulesConfig {
    /// 聚合窗口（毫秒）：同一聚合键的事件在该窗口内合并为一条通知。
    /// 聚合键含会话维度——`agent|kind|session` 三段（构造处见 app/src-tauri/src/state.rs
    /// 的聚合键构造），不是「同 agent 同类事件」：同一 agent 同类事件若分属不同会话，
    /// 互不合并。
    pub aggregate_window_ms: u64,
    /// 免打扰时段，格式 "23:00-08:00"，跨零点
    pub quiet_hours: Vec<String>,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            aggregate_window_ms: 1500,
            quiet_hours: Vec::new(),
        }
    }
}

/// 聚合窗口上限（10 分钟）：防手改 config.json 填出天文数字后
/// 通知被「无限聚合」。load() 与使用方（state.rs）都按此夹取。
pub const MAX_AGGREGATE_WINDOW_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Bark (iOS) 推送
    pub bark: Option<BarkChannelConfig>,
    /// 通用 Webhook 模板（飞书/企微/钉钉/ntfy/Server酱等）
    pub webhook: Option<WebhookChannelConfig>,
}

// 渠道配置带 struct 级 serde default：用户手改 config.json 漏掉 url/method
// 等键时按默认值补齐，而不是让整份配置解析失败。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BarkChannelConfig {
    pub enabled: bool,
    /// 例：https://api.day.app/YourKey 或自建 Bark 服务地址
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookChannelConfig {
    pub enabled: bool,
    pub url: String,
    /// 请求方法，默认 POST
    pub method: String,
    /// JSON body 模板，占位符：{title} {body} {agent} {event} {project}
    pub template: String,
}

impl Default for WebhookChannelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            method: "POST".to_string(),
            template: default_webhook_template(),
        }
    }
}

/// 通用 webhook 默认 body 模板（飞书/企微/钉钉/ntfy 等可直接用）。
/// 与前端 ChannelsPage 的默认模板保持一致：漏配 template 时发空 body 会被服务端 4xx。
fn default_webhook_template() -> String {
    r#"{"title": "{title}", "body": "{body}", "agent": "{agent}"}"#.to_string()
}

/// 桌面悬浮窗配置（当前活动会话的小卡片：左图标右状态，可拖动、可固定位置）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    /// 总开关。关闭后不创建窗口（默认开启；悬浮窗右键「关闭悬浮窗」会落盘关闭）
    pub enabled: bool,
    /// 窗口位置（逻辑像素）。-1 = 尚未放置过，按默认位置（主屏右上角）打开
    pub x: f64,
    pub y: f64,
    /// 固定位置：true 时卡片不响应拖动（悬浮窗右键菜单里切换）
    pub pinned: bool,
    /// 自动隐藏：安静时（无行 / 全部行属思考类）2 秒后隐藏卡片，
    /// 出现需要关注的行（等待确认 / 等待输入 / 任务完成 / 任务失败）立即弹出；
    /// 手动中止（run_aborted）不弹出也不阻止隐藏（与光效「手动中止不提醒」同口径）。
    /// 默认开：「安静即隐、有事即现」就是悬浮窗的默认体验；隐藏只是展示态、
    /// 不影响 `enabled`，不想要到「悬浮窗」页关掉即可。
    pub auto_hide: bool,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self { enabled: true, x: -1.0, y: -1.0, pinned: false, auto_hide: true }
    }
}

// ---------------------------------------------------------------------------
// 屏幕光效（边缘光效 + 全屏特效）
// ---------------------------------------------------------------------------

/// 边缘光效种类（设置页「边缘光效 → 类型」下拉的两项）。
///
/// `Comet`（流光）是上一版的默认表现，这次随「类型」下拉放出；
/// 下拉的显示名（呼吸 / 流光）只写在 api.ts 的 `EDGE_EFFECTS` 里，与枚举名无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlowEffect {
    /// 呼吸：边缘一圈辉光坡（外缘最实、向内渐隐）整层明暗呼吸
    Breathing,
    /// 流光：整圈细线常亮打底，一道高光沿线绕圈
    Comet,
}

impl GlowEffect {
    /// 传给覆盖层前端的 id（glow.html 的 `data-effect`）
    pub fn id(self) -> &'static str {
        match self {
            GlowEffect::Breathing => "breathing",
            GlowEffect::Comet => "comet",
        }
    }
}

/// 生效显示器。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorTarget {
    /// 所有显示器
    All,
    /// 仅主显示器
    Primary,
    /// `available_monitors()` 里第 i 个
    Index(usize),
}

/// 四态 × 效果类型（「通知」页每状态一行，与「声音」区同款排法）。
/// 值域：边缘 `"none" | "breathing" | "comet"`，全屏 `"none" | "fog" | "scan"`；
/// `""` = 未设置，只存在于迁移前的旧配置（[`GlowConfig::sanitize`] 按旧全局类型补齐）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StateEffects {
    pub thinking: String,
    pub waiting: String,
    pub completed: String,
    pub failed: String,
}

impl StateEffects {
    /// 按状态语义名取值（"thinking" / "waiting" / "completed" / "failed"，未知按 thinking）
    pub fn get(&self, state: &str) -> &str {
        match state {
            "waiting" => &self.waiting,
            "completed" => &self.completed,
            "failed" => &self.failed,
            _ => &self.thinking,
        }
    }

    pub fn set(&mut self, state: &str, value: String) {
        match state {
            "waiting" => self.waiting = value,
            "completed" => self.completed = value,
            "failed" => self.failed = value,
            _ => self.thinking = value,
        }
    }
}

/// 屏幕光效配置。
///
/// 运行时在屏幕最外圈覆盖一层透明、点击穿透的窗口，用颜色表达 agent 的实时状态
/// （思考色=运行中、等待色=等你确认、完成色=完成、失败色=意外终止）；打开 `fullscreen` 后，
/// 每次颜色亮起还会在整块屏幕上补一次全屏特效（「雾散」/「HUD 扫描」；
/// 全屏色可能与边缘色不同，见 glow.rs）。
///
/// **边缘 / 全屏的类型按状态各选各的**（`edge_effects` / `burst_effects`，
/// 与「声音」区四行同款口径），`"none"` = 该状态不亮边缘 / 不放全屏。
///
/// 观感参数（速度 / 亮度 / 辉光强度 / 光带宽度 / 圆角 / 停留时长）曾经开放在设置页，
/// 现已写死为 `app/src-tauri/src/glow.rs` 顶部的常量，不再进配置；老配置文件里的
/// 这些键会被 serde 静默忽略。
///
/// 角色名与 hex 的对应只写在 `app/src-tauri/src/glow.rs` 的 `GlowState::color` 一处，
/// 这里不重复色值。全部字段带 serde(default)：老配置文件没有 `glow` 段也能正常解析。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlowConfig {
    /// 总开关。关闭后不创建覆盖窗口，零开销
    pub enabled: bool,
    /// 边缘光效开关：关掉后只隐藏边缘灯带（各状态的边缘类型保留），全屏特效照常
    pub edge: bool,
    /// 边缘光效位置："top"=只亮顶部一条（默认）/ "all"=四周（旧行为）。
    ///
    /// 默认顶部的动机：四周光效的覆盖窗与显示器矩形几乎重合（见 glow.rs `rect_of`
    /// 的 1px 外扩），会被 Windows 请勿打扰按「全屏应用」自动静音、被游戏覆盖层类
    /// 软件误判为游戏；顶部模式把窗口缩成顶部条带，矩形不再近似覆盖整屏，从根上
    /// 避开这类判定。运行时经 [`edge_sides`](Self::edge_sides) 归一后下发。
    pub edge_position: String,
    /// 旧版全局边缘类型：只作**迁移源**——`edge_effects` 里为空的状态按它补齐；
    /// 运行时一律读 `edge_effects`，再改这个不再有任何效果
    pub effect: String,
    /// 全屏特效开关：每次颜色亮起时整屏补一次特效
    pub fullscreen: bool,
    /// 旧版全局全屏特效类型：只作迁移源（同 `effect`）
    pub fullscreen_effect: String,
    /// 生效显示器："all"=所有显示器 / 数字=`available_monitors()` 的下标 /
    /// 其它（含旧版的 "primary"）=主显示器。UI 只写 "all" 或下标。
    pub monitors: String,
    /// 各状态的边缘光效（"none"=该状态不亮边缘 / "breathing" / "comet"）
    pub edge_effects: StateEffects,
    /// 各状态的全屏特效（"none"=该状态不放全屏 / "fog" / "scan"）
    pub burst_effects: StateEffects,
}

impl Default for GlowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            edge: true,
            // 顶部条带是默认体验：整屏覆盖窗容易被判成全屏（见 edge_position 的文档）。
            // 老配置没有这个键，serde 补的也是它——升级后默认变顶部，切「四周」回旧行为
            edge_position: "top".to_string(),
            effect: GlowEffect::Breathing.id().to_string(),
            // 全屏特效默认开：它是这一版的主角，用户进「屏幕光效」页第一眼就能看到
            fullscreen: true,
            fullscreen_effect: "fog".to_string(),
            monitors: "primary".to_string(),
            // 每状态段必须留空（= 未迁移）：Default 同时是「配置里缺新键」的填充值，
            // 若这里写死呼吸/雾散，老配置里的 comet/scan 就永远迁移不过来
            edge_effects: StateEffects::default(),
            burst_effects: StateEffects::default(),
        }
    }
}

impl GlowConfig {
    /// 生效显示器（无法识别的值一律按主显示器处理）
    pub fn monitor_target(&self) -> MonitorTarget {
        let v = self.monitors.trim();
        if v.eq_ignore_ascii_case("all") {
            return MonitorTarget::All;
        }
        match v.parse::<usize>() {
            Ok(i) => MonitorTarget::Index(i),
            // "primary"（旧版默认值）以及用户手改出来的乱值
            Err(_) => MonitorTarget::Primary,
        }
    }

    /// 是否覆盖所有显示器
    pub fn all_monitors(&self) -> bool {
        self.monitor_target() == MonitorTarget::All
    }

    /// 边缘光效位置（无法识别的值一律按顶部处理）："top"=只亮顶部 / "all"=四周。
    ///
    /// 与 `monitor_target` 同款「读时归一」口径：手改出来的乱值不出现在运行时，
    /// 不进 `sanitize`（设置页只会写这两个值）。
    pub fn edge_sides(&self) -> &'static str {
        if self.edge_position.trim().eq_ignore_ascii_case("all") {
            "all"
        } else {
            "top"
        }
    }

    /// 边缘光效类型（未知值回落到呼吸）
    pub fn effect_kind(&self) -> GlowEffect {
        match self.effect.trim().to_ascii_lowercase().as_str() {
            "comet" => GlowEffect::Comet,
            _ => GlowEffect::Breathing,
        }
    }

    /// 全屏特效类型 id（未知值回落到雾散）。
    ///
    /// 目前有 "fog"（雾散）/ "scan"（HUD 扫描）两种；设置页的类型下拉、payload
    /// 与覆盖层的 `data-effect`（#burst 元素）都按这个口径走，加类型在这里补分支。
    pub fn fullscreen_effect_kind(&self) -> &'static str {
        match self.fullscreen_effect.trim().to_ascii_lowercase().as_str() {
            "scan" => "scan",
            _ => "fog",
        }
    }

    /// 某状态的边缘光效。None = 「无」（该状态不亮边缘灯带）。
    /// 读 [`sanitize`](Self::sanitize) 后的值；对未归一的空值按呼吸兜底，绝不 panic。
    pub fn edge_effect_for(&self, state: &str) -> Option<GlowEffect> {
        match self.edge_effects.get(state).trim().to_ascii_lowercase().as_str() {
            "none" => None,
            "comet" => Some(GlowEffect::Comet),
            _ => Some(GlowEffect::Breathing),
        }
    }

    /// 某状态的全屏特效 id（"fog" / "scan"，前端 #burst 的 data-effect）。
    /// None = 「无」（该状态不放全屏特效）。
    pub fn burst_effect_for(&self, state: &str) -> Option<&'static str> {
        match self.burst_effects.get(state).trim().to_ascii_lowercase().as_str() {
            "none" => None,
            "scan" => Some("scan"),
            _ => Some("fog"),
        }
    }

    /// 把手改 / 旧版配置归一成运行时认识的形态：
    /// - 旧全局 `effect` / `fullscreen_effect` 先归一（未知值回默认），它同时是**迁移源**；
    /// - 每状态为空（旧配置没有每状态段）→ 按迁移源补齐；未知 id → 回落迁移源；
    ///   `"none"`（无）是合法值，保留。
    ///
    /// load / load_or_init / 旧 toml 迁移路径都调用它：配置文件随下次保存自然
    /// 收敛到每状态形态（与 migrate_agent_ids 同一「内存态迁移」口径）。
    pub fn sanitize(&self) -> Self {
        let mut g = self.clone();
        g.effect = g.effect_kind().id().to_string();
        g.fullscreen_effect = g.fullscreen_effect_kind().to_string();
        g.edge_effects = normalize_state_effects(&g.edge_effects, &g.effect, canon_edge_effect);
        g.burst_effects = normalize_state_effects(&g.burst_effects, &g.fullscreen_effect, canon_burst_effect);
        g
    }
}

/// 边缘光效的合法 id（含 "none"：该状态不亮边缘）
fn canon_edge_effect(id: &str) -> Option<&'static str> {
    match id {
        "none" => Some("none"),
        "breathing" => Some("breathing"),
        "comet" => Some("comet"),
        _ => None,
    }
}

/// 全屏特效的合法 id（含 "none"：该状态不放全屏）
fn canon_burst_effect(id: &str) -> Option<&'static str> {
    match id {
        "none" => Some("none"),
        "fog" => Some("fog"),
        "scan" => Some("scan"),
        _ => None,
    }
}

/// 每状态效果归一：空（未设置）与未知 id 都回落 `fallback`（= 归一后的旧全局类型），
/// 合法 id（含 "none"）原样保留。四态逐个处理，互不影响。
fn normalize_state_effects(
    e: &StateEffects,
    fallback: &str,
    canon: fn(&str) -> Option<&'static str>,
) -> StateEffects {
    let one = |s: &str| -> String {
        let v = s.trim().to_ascii_lowercase();
        if v.is_empty() {
            return fallback.to_string();
        }
        canon(&v).map(str::to_string).unwrap_or_else(|| fallback.to_string())
    };
    StateEffects {
        thinking: one(&e.thinking),
        waiting: one(&e.waiting),
        completed: one(&e.completed),
        failed: one(&e.failed),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// AgentKind::id()
    pub kind: String,
    pub enabled: bool,
}



impl BarkConfig {
    /// 配置目录：遵循平台惯例
    /// Windows: %APPDATA%\agent-bark · macOS: ~/Library/Application Support/agent-bark · Linux: ~/.config/agent-bark
    pub fn dir() -> Result<PathBuf> {
        dirs::config_dir()
            .context("无法获取系统配置目录")
            .map(|d| d.join("agent-bark"))
    }

    /// 配置文件：<config_dir>/agent-bark/config.json
    pub fn path() -> Result<PathBuf> {
        Ok(Self::dir()?.join("config.json"))
    }

    /// 旧版路径（<=0.1 骨架期使用），仅用于一次性迁移
    fn legacy_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".agent-bark").join("config.toml"))
    }

    /// 只读加载：**绝不写盘**。
    ///
    /// hook 子命令必须用这个入口。若这里回退到「写一份默认配置」，会重新生成随机
    /// token，而 daemon 内存里仍是旧 token，之后每个事件都会被判 401 静默丢弃
    /// （用户看到的现象是「装好了但一条通知都没有」）。
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("读取配置失败: {}", path.display()))?;
        // 容忍 UTF-8 BOM：PowerShell 的 Set-Content/Out-File 默认会写入 BOM，
        // 而 serde_json 会把 BOM 当成非法首字符。
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let mut cfg = Self::parse_and_validate(text)
            .with_context(|| format!("解析配置失败: {}", path.display()))?;
        cfg.migrate_agent_ids();
        // 光效按状态归一（顺带把旧全局类型迁到每状态）：文件在下次保存时收敛
        cfg.glow = cfg.glow.sanitize();
        Ok(cfg)
    }

    /// 解析 + 解析后校验（server.token 键存在且非空、聚合窗口夹取）。load() 与测试共用。
    fn parse_and_validate(text: &str) -> Result<Self> {
        // 先按**原始 JSON** 断言 server.token 键真的存在且非空，再走 typed 解析：
        // ServerConfig 带 serde(default)，缺键时 Default 会填一个**新随机** token（非空），
        // 直接绕过下面的空串检查——daemon 与每个 hook/插件进程各自解析出互不相同的
        // token，之后全部事件 401 静默丢弃（用户看到「装好了但一条通知都没有」）。
        // migrate_legacy 对 toml 路径防了同一失效模式，JSON 路径在这里补齐。
        let raw: serde_json::Value = serde_json::from_str(text)?;
        // 根不是对象时交给下面 typed 解析报 invalid type，不在这里误报缺键
        if raw.is_object() {
            match raw.get("server").and_then(|s| s.get("token")) {
                None => anyhow::bail!(
                    "配置缺少 server.token：请编辑配置文件填入 token（或删除该文件后重启应用重新生成）"
                ),
                Some(v) => {
                    // 非字符串 token（数字 / null）留给 typed 解析报 invalid type；
                    // 字符串则必须 trim 后非空
                    if v.as_str().is_some_and(|t| t.trim().is_empty()) {
                        anyhow::bail!(
                            "server.token 为空：请编辑配置文件填入 token（或删除该文件后重启应用重新生成）"
                        );
                    }
                }
            }
        }
        let mut cfg: Self = serde_json::from_str(text)?;
        if cfg.server.token.trim().is_empty() {
            // 与 legacy 迁移同一口径（见 migrate_legacy）：空 token 会让服务端把
            // 「空头请求」全部放行（constant_time_eq("", "") == true），必须显式报错
            // 而不是静默生成新 token（已注册 hook/插件内嵌旧 token，静默换掉会全体 401）。
            anyhow::bail!("server.token 为空：请编辑配置文件填入 token（或删除该文件后重启应用重新生成）");
        }
        // 手改配置可能填出离谱聚合窗口：上限夹到 10 分钟，防止手滑填出
        // `1e15` 后该类通知 effectively 永不分发、聚合条目永不回收
        cfg.rules.aggregate_window_ms = cfg.rules.aggregate_window_ms.min(MAX_AGGREGATE_WINDOW_MS);
        Ok(cfg)
    }

    /// 合并/下线过的 agent id 迁移（内存态；文件在下次保存时自然收敛）。
    ///
    /// `qoder-cn`（Qoder CN 国内版）已并入 `qoder`（国际版 + 国内版一条 adapter）。
    /// 不迁移的话，只装了国内版的用户升级后会看到开关莫名其妙变关——hook 其实还在，
    /// 只是合并后的 id 没被标记为开启。
    fn migrate_agent_ids(&mut self) {
        if let Some(pos) = self.agents.iter().position(|a| a.kind == "qoder-cn") {
            let legacy_enabled = self.agents.remove(pos).enabled;
            match self.agents.iter_mut().find(|a| a.kind == "qoder") {
                Some(existing) => existing.enabled |= legacy_enabled,
                None => self.agents.push(AgentState { kind: "qoder".to_string(), enabled: legacy_enabled }),
            }
        }
    }

    /// daemon 启动时调用：不存在则创建（含旧 toml 一次性迁移）。
    ///
    /// 解析失败时**直接报错**，既不覆盖用户文件也不生成新 token——让调用方把错误
    /// 显式展示给用户，而不是悄悄换掉凭据。
    pub fn load_or_init() -> Result<(Self, PathBuf)> {
        let path = Self::path()?;
        if path.exists() {
            let cfg = Self::load()?;
            return Ok((cfg, path));
        }
        // 一次性迁移：~/.agent-bark/config.toml → config.json
        if let Some(legacy) = Self::legacy_path() {
            if let Some(cfg) = Self::migrate_legacy(&legacy, &path)? {
                return Ok((cfg, path));
            }
        }
        let mut cfg = BarkConfig::default();
        // 归一后再落盘：Default 的每状态段是空串（= 未迁移语义），
        // 全新配置也必须写成真正的每状态形态
        cfg.glow = cfg.glow.sanitize();
        cfg.save(&path)?;
        Ok((cfg, path))
    }

    /// 旧版 toml 一次性迁移。返回 Ok(Some(cfg)) 表示迁移成功；旧文件不存在返回
    /// Ok(None)；旧文件存在但读取/解析失败时**显式报错**——旧凭据已无法恢复，
    /// 绝不能静默落入默认初始化（那会生成新随机 token，而已注册插件内嵌的仍是
    /// 旧 token，之后每个事件都会被判 401 静默丢弃）。
    fn migrate_legacy(legacy: &Path, path: &Path) -> Result<Option<Self>> {
        if !legacy.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(legacy)
            .with_context(|| format!("读取旧版配置失败: {}", legacy.display()))?;
        // ServerConfig 带 serde(default)，缺 server.token 时会被 Default 填成新随机 uuid，
        // 于是「解析成功」但凭据已被悄悄换掉——必须先按 toml::Value 检查原始文件里是否真有 token。
        let raw: toml::Value = toml::from_str(&text).with_context(|| {
            format!(
                "旧版配置 {} 已无法解析，旧凭据不可恢复。请修复或删除该文件后重试\
                 （删除后将按首次启动生成全新 token，已注册插件需重新接入）",
                legacy.display()
            )
        })?;
        let token_ok = raw
            .get("server")
            .and_then(|s| s.get("token"))
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.trim().is_empty());
        if !token_ok {
            anyhow::bail!(
                "旧版配置 {} 缺少 server.token（或为空），旧凭据不可恢复。请修复或删除该文件后重试\
                 （删除后将按首次启动生成全新 token，已注册插件需重新接入）",
                legacy.display()
            );
        }
        let mut cfg: BarkConfig = toml::from_str(&text).with_context(|| {
            format!(
                "旧版配置 {} 已无法解析，旧凭据不可恢复。请修复或删除该文件后重试\
                 （删除后将按首次启动生成全新 token，已注册插件需重新接入）",
                legacy.display()
            )
        })?;
        cfg.glow = cfg.glow.sanitize();
        cfg.save(path)?;
        Ok(Some(cfg))
    }

    /// 原子写入：同目录临时文件 + fsync + rename，避免半写文件被读到。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)? + "\n";
        let tmp = path.with_file_name(format!(
            ".{}.tmp-{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("config.json"),
            uuid::Uuid::new_v4().simple()
        ));
        {
            let mut f = std::fs::File::create(&tmp).with_context(|| format!("创建临时配置失败: {}", tmp.display()))?;
            // 配置含 token：Unix 下新建文件的权限受 umask 影响（umask 022 时世界可读），
            // 显式收紧到 0o600（仅属主可读写）后再 rename，落盘文件永不开权限窗口。
            // Windows 继承 %APPDATA% 的 ACL，无需处理（且没有 mode 语义）。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| format!("收紧配置文件权限失败: {}", tmp.display()))?;
            }
            use std::io::Write;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path).with_context(|| {
            let _ = std::fs::remove_file(&tmp);
            format!("替换配置失败: {}", path.display())
        })?;
        Ok(())
    }

    pub fn agent_enabled(&self, kind_id: &str) -> bool {
        self.agents
            .iter()
            .find(|a| a.kind == kind_id)
            .map(|a| a.enabled)
            .unwrap_or(false)
    }

    /// 该 agent 是否被**显式**关闭（配置里有记录且 `enabled=false`）。
    ///
    /// 与 [`agent_enabled`](Self::agent_enabled) 的区别在「没有记录」这一档：
    /// 前者对未知 id 返回 false，后者返回 false 但含义相反。事件管道必须用这个判定
    /// 「用户关掉了接入」——直接用 `!agent_enabled(id)` 会把从未写进配置的 agent
    /// （旧版本 config.json、手写配置，如只装了 hook 没点过开关的 claude-code）
    /// 的事件全部静默丢掉。
    ///
    /// 取条目的口径与 `agent_enabled` 严格一致（都是**首条**匹配），二者互为补集：
    /// 若手改过的配置里同一 kind 出现重复条目，用 `any` 会得到「开关显示开、事件却被
    /// 管道门丢掉」的自相矛盾状态（两边各看一条），比漏一个事件更难排查。
    pub fn agent_disabled(&self, kind_id: &str) -> bool {
        self.agents
            .iter()
            .find(|a| a.kind == kind_id)
            .is_some_and(|a| !a.enabled)
    }

    pub fn set_agent_enabled(&mut self, kind_id: &str, enabled: bool) {
        match self.agents.iter_mut().find(|a| a.kind == kind_id) {
            Some(state) => state.enabled = enabled,
            None => self.agents.push(AgentState { kind: kind_id.to_string(), enabled }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_json_roundtrip() {
        let mut cfg = BarkConfig::default();
        cfg.set_agent_enabled("claude-code", true);
        cfg.notify.completed = StateSound { effect: "ding".into(), plays: 2 };
        cfg.channels.bark = Some(BarkChannelConfig {
            enabled: true,
            url: "https://api.day.app/key".into(),
        });

        let dir = std::env::temp_dir().join(format!("agent-bark-test-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("config.json");
        cfg.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let loaded: BarkConfig = serde_json::from_str(&text).unwrap();
        assert!(loaded.agent_enabled("claude-code"));
        assert_eq!(loaded.notify.completed.effect, "ding");
        assert_eq!(loaded.notify.completed.plays, 2);
        // 未配置的状态默认不响
        assert_eq!(loaded.notify.thinking, StateSound::default());
        assert!(loaded.notify.thinking.effect.is_empty());
        assert_eq!(loaded.channels.bark.as_ref().unwrap().url, "https://api.day.app/key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_legacy_toml() {
        // 迁移路径中旧 toml 的解析（migrate_legacy 内部使用同一逻辑）
        let text = r#"[server]
port = 36573
token = "abc123"

[notify]
system = false
"#;
        let cfg: BarkConfig = toml::from_str(text).unwrap();
        assert_eq!(cfg.server.port, 36573);
        assert_eq!(cfg.server.token, "abc123");
        // notify 段的旧键（system/sound）已下线：未知键忽略，音效全部默认未选
        assert_eq!(cfg.notify, NotifyConfig::default());
    }

    #[test]
    fn legacy_toml_migrates_when_parseable() {
        let dir = std::env::temp_dir().join(format!("agent-bark-legacy-ok-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("config.toml");
        let path = dir.join("config.json");
        std::fs::write(&legacy, "[server]\ntoken = \"abc123\"\n").unwrap();
        let cfg = BarkConfig::migrate_legacy(&legacy, &path).unwrap().unwrap();
        assert_eq!(cfg.server.token, "abc123");
        // 迁移后已写入新 json
        assert!(path.exists());
        // 旧文件不存在时返回 None（走默认初始化）
        let missing = dir.join("nope.toml");
        assert!(BarkConfig::migrate_legacy(&missing, &path).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_toml_parse_failure_is_an_error() {
        // 旧 toml 存在但解析失败：必须显式报错，不能静默生成新 token
        // （已注册插件内嵌旧 token，换新 token 后事件全部 401）
        let dir = std::env::temp_dir().join(format!("agent-bark-legacy-bad-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("config.toml");
        let path = dir.join("config.json");
        std::fs::write(&legacy, "{ 这不是合法 toml").unwrap();
        let err = BarkConfig::migrate_legacy(&legacy, &path).unwrap_err();
        assert!(format!("{err:#}").contains("旧凭据不可恢复"));
        // 不会生成新配置文件，也不动旧文件
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), "{ 这不是合法 toml");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_config_missing_keys_still_parse() {
        // 手改 config.json 漏了 url/method 等键时按默认值补齐，不应整份解析失败
        let cfg: BarkConfig = serde_json::from_str(
            r#"{"channels":{"bark":{"enabled":true},"webhook":{"enabled":true}}}"#,
        )
        .unwrap();
        let bark = cfg.channels.bark.unwrap();
        assert!(bark.enabled);
        assert_eq!(bark.url, "");
        let webhook = cfg.channels.webhook.unwrap();
        assert!(webhook.enabled);
        assert_eq!(webhook.url, "");
        assert_eq!(webhook.method, "POST");
        // template 必须保持非空默认：空 body 会被 webhook 服务端 4xx，
        // 且前端 ChannelsPage 用的就是这个默认模板
        assert!(webhook.template.contains("{title}"), "默认模板不能为空: {}", webhook.template);
    }

    #[test]
    fn notify_master_switch_defaults_on() {
        // 老配置没有 notify.enabled 键：serde default 必须补成开，
        // 否则升级后声音全哑（各状态的音效设置明明还在）
        let cfg: BarkConfig = serde_json::from_str(
            r#"{"server":{"port":1,"token":"t"},"notify":{"completed":{"effect":"ding","plays":2}}}"#,
        )
        .unwrap();
        assert!(cfg.notify.enabled);
        assert_eq!(cfg.notify.completed, StateSound { effect: "ding".into(), plays: 2 });
        assert!(BarkConfig::default().notify.enabled);
    }

    #[test]
    fn glow_missing_key_falls_back_to_default() {
        // 老配置文件没有 glow 段：必须解析成功并拿到默认值，而不是整份配置失效
        let cfg: BarkConfig = serde_json::from_str(r#"{"agents":[{"kind":"claude-code"}]}"#).unwrap();
        assert!(cfg.glow.enabled);
        assert!(cfg.glow.edge, "边缘光效默认开");
        // 老配置没有 edge_position 键：serde 按默认补「顶部」——升级后默认只亮顶部
        assert_eq!(cfg.glow.edge_position, "top");
        assert_eq!(cfg.glow.edge_sides(), "top");
        assert_eq!(cfg.glow.monitors, "primary");
        assert_eq!(cfg.glow.effect_kind(), GlowEffect::Breathing);
        assert!(cfg.glow.fullscreen, "全屏特效默认开");
        assert_eq!(cfg.glow.fullscreen_effect_kind(), "fog");
        // 部分字段缺失时按默认补齐
        let cfg: BarkConfig = serde_json::from_str(r#"{"glow":{"enabled":false,"edge":false}}"#).unwrap();
        assert!(!cfg.glow.enabled);
        assert!(!cfg.glow.edge);
        assert!(cfg.glow.fullscreen);
    }

    #[test]
    fn widget_missing_auto_hide_defaults_on_and_roundtrips() {
        // 老配置没有 widget 段 / 没有 auto_hide 键：解析成功且默认开——
        // 「安静即隐、有事即现」是悬浮窗的默认体验
        let cfg: BarkConfig = serde_json::from_str(r#"{"agents":[]}"#).unwrap();
        assert!(cfg.widget.enabled);
        assert!(cfg.widget.auto_hide);
        let cfg: BarkConfig =
            serde_json::from_str(r#"{"widget":{"enabled":false,"x":10,"y":20,"pinned":true}}"#).unwrap();
        assert!(!cfg.widget.enabled);
        assert!(cfg.widget.auto_hide);
        assert_eq!((cfg.widget.x, cfg.widget.y, cfg.widget.pinned), (10.0, 20.0, true));
        // 显式 false 能存能读（「悬浮窗」页把「自动隐藏」关掉后的形态）
        let cfg: BarkConfig = serde_json::from_str(r#"{"widget":{"auto_hide":false}}"#).unwrap();
        assert!(!cfg.widget.auto_hide);
    }

    #[test]
    fn glow_monitor_target_parses_all_index_and_legacy() {
        let t = |m: &str| GlowConfig { monitors: m.to_string(), ..Default::default() }.monitor_target();
        assert_eq!(t("all"), MonitorTarget::All);
        assert_eq!(t(" ALL "), MonitorTarget::All);
        assert_eq!(t("0"), MonitorTarget::Index(0));
        assert_eq!(t("2"), MonitorTarget::Index(2));
        // 旧版默认值与手改乱值都回落到主显示器
        assert_eq!(t("primary"), MonitorTarget::Primary);
        assert_eq!(t(""), MonitorTarget::Primary);
        assert_eq!(t("显示器1"), MonitorTarget::Primary);
        assert!(GlowConfig { monitors: "all".into(), ..Default::default() }.all_monitors());
        assert!(!GlowConfig { monitors: "1".into(), ..Default::default() }.all_monitors());
    }

    #[test]
    fn glow_edge_sides_parses_top_and_all() {
        let t = |p: &str| GlowConfig { edge_position: p.to_string(), ..Default::default() }.edge_sides();
        assert_eq!(t("top"), "top");
        assert_eq!(t("all"), "all");
        assert_eq!(t(" ALL "), "all");
        // 手改乱值 / 旧值一律回落顶部（默认体验），不能让覆盖层拿到未知值后不知所措
        assert_eq!(t(""), "top");
        assert_eq!(t("bottom"), "top");
        assert_eq!(t("TOP"), "top");
        // 能存能读（设置页切到「四周」再保存的形态）
        let cfg: BarkConfig = serde_json::from_str(r#"{"glow":{"edge_position":"all"}}"#).unwrap();
        assert_eq!(cfg.glow.edge_sides(), "all");
    }

    #[test]
    fn glow_effect_unknown_falls_back_to_breathing() {
        let e = |s: &str| GlowConfig { effect: s.to_string(), ..Default::default() }.effect_kind();
        assert_eq!(e("breathing"), GlowEffect::Breathing);
        assert_eq!(e("COMET"), GlowEffect::Comet);
        // 手改出没实现的名字：回落呼吸，不能让覆盖层拿到未知 data-effect 后什么都不画
        assert_eq!(e("rainbow"), GlowEffect::Breathing);
        assert_eq!(GlowConfig { effect: "  ".into(), ..Default::default() }.sanitize().effect, "breathing");
        assert_eq!(GlowConfig { effect: "Comet".into(), ..Default::default() }.sanitize().effect, "comet");
    }

    #[test]
    fn glow_fullscreen_effect_unknown_falls_back_to_fog() {
        let f = |s: &str| GlowConfig { fullscreen_effect: s.to_string(), ..Default::default() }.fullscreen_effect_kind();
        assert_eq!(f("fog"), "fog");
        assert_eq!(f(" FOG "), "fog");
        assert_eq!(f("scan"), "scan");
        assert_eq!(f("Scan"), "scan");
        // 手改出没实现的类型：回落雾散，不能让覆盖层拿到未知的 data-effect 后什么都不画
        assert_eq!(f("rain"), "fog");
        assert_eq!(GlowConfig { fullscreen_effect: "  ".into(), ..Default::default() }.sanitize().fullscreen_effect, "fog");
    }

    #[test]
    fn glow_per_state_effects_migrate_from_legacy_global() {
        // 旧配置只有全局类型：四态各按它补齐（迁移源语义）
        let cfg: BarkConfig =
            serde_json::from_str(r#"{"glow":{"effect":"comet","fullscreen_effect":"scan"}}"#).unwrap();
        let g = cfg.glow.sanitize();
        for s in ["thinking", "waiting", "completed", "failed"] {
            assert_eq!(g.edge_effects.get(s), "comet", "{s} 按旧全局迁移");
            assert_eq!(g.burst_effects.get(s), "scan", "{s} 按旧全局迁移");
        }
        // 什么都没写的老配置：按老默认（呼吸 / 雾散）补齐
        let cfg: BarkConfig = serde_json::from_str(r#"{}"#).unwrap();
        let g = cfg.glow.sanitize();
        assert_eq!(g.edge_effects.get("thinking"), "breathing");
        assert_eq!(g.burst_effects.get("completed"), "fog");
    }

    #[test]
    fn glow_per_state_effects_keep_none_and_fix_unknown() {
        let cfg: BarkConfig = serde_json::from_str(
            r#"{"glow":{
                "effect":"comet",
                "edge_effects":{"thinking":"none","waiting":"breathing","completed":"","failed":"rainbow"},
                "fullscreen_effect":"scan",
                "burst_effects":{"thinking":"none","waiting":"","completed":"fog","failed":"rain"}
            }}"#,
        )
        .unwrap();
        let g = cfg.glow.sanitize();
        assert_eq!(g.edge_effects.get("thinking"), "none", "「无」是合法值，必须保留");
        assert_eq!(g.edge_effects.get("waiting"), "breathing");
        assert_eq!(g.edge_effects.get("completed"), "comet", "空 = 未设置 → 跟随旧全局");
        assert_eq!(g.edge_effects.get("failed"), "comet", "未知 id → 跟随旧全局");
        assert_eq!(g.burst_effects.get("thinking"), "none");
        assert_eq!(g.burst_effects.get("waiting"), "scan");
        assert_eq!(g.burst_effects.get("completed"), "fog");
        assert_eq!(g.burst_effects.get("failed"), "scan");
    }

    #[test]
    fn glow_effect_accessors_map_states_and_none() {
        let g = GlowConfig {
            edge_effects: StateEffects { thinking: "none".into(), waiting: "comet".into(), ..Default::default() },
            burst_effects: StateEffects { completed: "none".into(), failed: "scan".into(), ..Default::default() },
            ..Default::default()
        }
        .sanitize();
        assert!(g.edge_effect_for("thinking").is_none(), "「无」= 不亮边缘");
        assert_eq!(g.edge_effect_for("waiting"), Some(GlowEffect::Comet));
        assert_eq!(g.edge_effect_for("completed"), Some(GlowEffect::Breathing));
        assert!(g.burst_effect_for("completed").is_none(), "「无」= 不放全屏");
        assert_eq!(g.burst_effect_for("failed"), Some("scan"));
        assert_eq!(g.burst_effect_for("thinking"), Some("fog"));
    }

    #[test]
    fn agents_missing_enabled_key_still_parse() {
        // agents 是用户最常手改的段落，缺 enabled 键不应让整份配置解析失败
        let cfg: BarkConfig = serde_json::from_str(r#"{"agents":[{"kind":"claude-code"}]}"#).unwrap();
        assert_eq!(cfg.agents.len(), 1);
        assert_eq!(cfg.agents[0].kind, "claude-code");
        assert!(!cfg.agents[0].enabled);
    }

    #[test]
    fn legacy_qoder_cn_flag_migrates_to_merged_qoder() {
        // Qoder CN 国内版已并入 Qoder：老配置里的 qoder-cn 开关要并到 qoder 上，
        // 否则只装国内版的用户升级后会看到开关变关（hook 还在，只是标记丢了）
        let mut cfg: BarkConfig = serde_json::from_str(r#"{"agents":[{"kind":"qoder-cn","enabled":true}]}"#).unwrap();
        cfg.migrate_agent_ids();
        assert_eq!(cfg.agents.len(), 1);
        assert_eq!(cfg.agents[0].kind, "qoder");
        assert!(cfg.agent_enabled("qoder"));

        // 两个都在：任一开着就是开着（国内版的 true 不能被国际版的 false 吃掉）
        let mut cfg: BarkConfig = serde_json::from_str(
            r#"{"agents":[{"kind":"qoder","enabled":false},{"kind":"qoder-cn","enabled":true}]}"#,
        )
        .unwrap();
        cfg.migrate_agent_ids();
        assert_eq!(cfg.agents.len(), 1);
        assert!(cfg.agent_enabled("qoder"));

        // 国内版关着：不改变已有状态
        let mut cfg: BarkConfig =
            serde_json::from_str(r#"{"agents":[{"kind":"qoder","enabled":true},{"kind":"qoder-cn","enabled":false}]}"#)
                .unwrap();
        cfg.migrate_agent_ids();
        assert!(cfg.agent_enabled("qoder"));
        assert_eq!(cfg.agents.len(), 1);
    }

    #[test]
    fn legacy_toml_without_token_is_an_error() {
        // 可解析但缺 server.token：ServerConfig 的 serde default 会填一个**新随机** token，
        // 若不拦下来就等于静默换掉凭据（opencode 插件内嵌旧 token → 全部 401）
        let dir = std::env::temp_dir().join(format!("agent-bark-legacy-notoken-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("config.toml");
        let path = dir.join("config.json");
        for body in ["[server]\nport = 36573\n", "", "[notify]\nsystem = false\n"] {
            std::fs::write(&legacy, body).unwrap();
            let err = BarkConfig::migrate_legacy(&legacy, &path).unwrap_err();
            assert!(format!("{err:#}").contains("缺少 server.token"), "body={body:?} err={err:#}");
            assert!(!path.exists(), "不得生成新配置: body={body:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn agent_disabled_only_for_explicit_off() {
        let mut cfg = BarkConfig::default();
        // 从未写进配置的 agent：既不是 enabled 也不是 disabled
        // （事件管道用 agent_disabled 判定，不能把这类 agent 的事件误杀）
        assert!(!cfg.agent_enabled("claude-code"));
        assert!(!cfg.agent_disabled("claude-code"));

        cfg.set_agent_enabled("claude-code", true);
        assert!(cfg.agent_enabled("claude-code") && !cfg.agent_disabled("claude-code"));

        cfg.set_agent_enabled("claude-code", false);
        assert!(!cfg.agent_enabled("claude-code") && cfg.agent_disabled("claude-code"));
    }

    #[test]
    fn agent_disabled_and_enabled_agree_on_duplicate_entries() {
        // 手改/合并过的配置可能出现同一 kind 的重复条目：两者必须看同一条（首条），
        // 否则会出现「开关显示开、事件却被管道门丢掉」的自相矛盾状态。
        let mut cfg = BarkConfig {
            agents: vec![
                AgentState { kind: "qoder".into(), enabled: true },
                AgentState { kind: "qoder".into(), enabled: false },
            ],
            ..Default::default()
        };
        assert!(cfg.agent_enabled("qoder"));
        assert!(!cfg.agent_disabled("qoder"), "首条为开时不得判为已关闭");

        cfg.agents = vec![
            AgentState { kind: "qoder".into(), enabled: false },
            AgentState { kind: "qoder".into(), enabled: true },
        ];
        assert!(!cfg.agent_enabled("qoder"));
        assert!(cfg.agent_disabled("qoder"), "首条为关时两者必须一致");
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("agent-bark-atomic-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("config.json");
        let cfg = BarkConfig::default();
        cfg.save(&path).unwrap();
        // 目录里只应有目标文件，没有残留 .tmp-*
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
        let loaded: BarkConfig = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.server.token, cfg.server.token);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_owner_only_on_unix() {
        // 配置含 token：Unix 下不得对 group/other 开放读权限（umask 022 时默认是世界可读）
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agent-bark-perm-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("config.json");
        BarkConfig::default().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config.json 含 token，必须 0o600（仅属主可读写）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reports_error_instead_of_regenerating_token() {
        // 坏配置必须报错，不能悄悄换成新 token（否则 daemon 侧鉴权全挂）
        let dir = std::env::temp_dir().join(format!("agent-bark-bad-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = serde_json::from_str::<BarkConfig>(&std::fs::read_to_string(&path).unwrap());
        assert!(err.is_err());
        // 文件内容保持不变
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ this is not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bom_prefixed_config_is_accepted() {
        // PowerShell 的 Set-Content 默认写 UTF-8 BOM；不带 BOM 处理会解析失败，
        // 进而让 hook 走到 fallback（token 不匹配 → 事件全丢）。
        let text = "\u{feff}{\"server\":{\"port\":1234,\"token\":\"t\"}}";
        let stripped = text.strip_prefix('\u{feff}').unwrap_or(text);
        let cfg: BarkConfig = serde_json::from_str(stripped).unwrap();
        assert_eq!(cfg.server.port, 1234);
        assert_eq!(cfg.server.token, "t");
    }

    #[test]
    fn empty_token_is_rejected_by_load_semantics() {
        // 手改配置把 token 清空：空 token 会让服务端放行一切空头请求，
        // load 必须显式报错（而非静默放行或悄悄换 token 导致全体 401）
        let err = BarkConfig::parse_and_validate(r#"{"server":{"port":1234,"token":""}}"#);
        assert!(err.is_err());
        assert!(format!("{:#}", err.unwrap_err()).contains("server.token 为空"));
        // 纯空白同理
        let err = BarkConfig::parse_and_validate(r#"{"server":{"port":1234,"token":"   "}}"#);
        assert!(err.is_err());
        // 正常 token 不受影响
        assert!(BarkConfig::parse_and_validate(r#"{"server":{"port":1234,"token":"t"}}"#).is_ok());
    }

    #[test]
    fn missing_server_token_key_is_an_error_not_a_random_token() {
        // 手改出 "server":{"port":1}（缺 token 键）：ServerConfig 的 serde default 会生成
        // **新随机** token（非空）绕过空串检查 → daemon 与每个 hook 各自解析出不同的
        // token、全部事件 401 静默丢弃。parse_and_validate 必须按原始 JSON 拦下缺键。
        for body in [r#"{"server":{"port":1}}"#, r#"{}"#, r#"{"notify":{}}"#] {
            let err = BarkConfig::parse_and_validate(body).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("缺少 server.token"), "body={body:?} err={msg}");
        }
        // 非字符串 token 同样必须报错（走 typed 解析的 invalid type），绝不落回随机 token
        assert!(BarkConfig::parse_and_validate(r#"{"server":{"port":1,"token":null}}"#).is_err());
        // 键在且非空：正常解析，且保留用户填的 token（绝不重新生成）
        let cfg = BarkConfig::parse_and_validate(r#"{"server":{"port":1,"token":"t"}}"#).unwrap();
        assert_eq!(cfg.server.port, 1);
        assert_eq!(cfg.server.token, "t");
    }

    #[test]
    fn aggregate_window_is_clamped_to_max() {
        // 手改 config.json 填出天文数字：夹到上限，通知不能被「无限聚合」
        let cfg = BarkConfig::parse_and_validate(r#"{"rules":{"aggregate_window_ms":1000000000000},"server":{"token":"t"}}"#).unwrap();
        assert_eq!(cfg.rules.aggregate_window_ms, MAX_AGGREGATE_WINDOW_MS);
        let cfg = BarkConfig::parse_and_validate(r#"{"rules":{"aggregate_window_ms":1500},"server":{"token":"t"}}"#).unwrap();
        assert_eq!(cfg.rules.aggregate_window_ms, 1500);
    }
}
