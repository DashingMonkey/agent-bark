//! 屏幕光效（glow）——agent 运行状态的「余光可见」指示灯。
//!
//! ## 做什么
//! 在屏幕最外圈铺一层**透明、点击穿透、始终置顶**的覆盖窗口，用颜色表达当前状态：
//!
//! | 状态 | 角色 | 含义 | 触发 |
//! | --- | --- | --- | --- |
//! | `Running` | **思考颜色** | 思考中 / 执行工具 | 心跳 `Thinking` / `ToolRunning` |
//! | `Waiting` | **等待颜色** | 等你确认或输入 | `WaitingPermission` / `WaitingInput` |
//! | `Completed` | **完成颜色** · 停留后淡出 | 任务完成 | `RunCompleted` |
//! | `Failed` | **失败颜色** · 停留后淡出 | 意外终止（失败 / 判死） | `RunFailed` / 心跳超时判死 |
//!
//! 全仓库只认这四个**角色名**，不写色系名与 hex（见 [`GlowState::color`]）；
//! **用户主动中止**（`RunAborted`）是另一回事，它不亮灯，叫「中止」不叫「失败」。
//!
//! 边缘表现按**状态**各选各的（`glow.edge_effects`：无 / 呼吸 / 流光），`glow.edge`
//! 是边缘灯带总开关；打开 `glow.fullscreen` 后，**每次颜色亮起**还会按触发角色的
//! `glow.burst_effects`（无 / 雾散 / 扫描）在整屏补一次全屏特效（见 [`GlowPayload::burst`]）。
//!
//! ## 双通道：边缘状态 × 全屏事件
//! 多会话并行时这两层各司其职、互不覆盖：
//! - **边缘（常驻）**按会话表聚合推导（等待 > 思考 > 空表才看终态）——
//!   只要还有会话在跑/在等，边缘就持续表达它们的聚合状态；
//! - **全屏（一次性）**是独立的**事件通道**：一个会话完成/失败时，哪怕其余
//!   会话还在跑，也以事件角色色（完成 / 失败）补放一次雾散（见 [`burst_only`]）——
//!   「有一个完成了」是值得立刻知道的事件，「还有的在跑」是持续状态，两者不互斥。
//!
//! ## 关键取舍
//! - **懒创建**：第一次要亮的时候才建窗口。没接任何 agent 的用户不会白吃一个 WebView 的内存。
//! - **建好后不再销毁**：显示/隐藏全屏窗口会有明显闪烁，空闲态由前端把 opacity 归零；
//!   窗口是点击穿透 + 不可聚焦的，留着也不挡操作、不抢焦点。
//! - **重建不重放**：显示器数量变化时 `align` 会全量重建窗口，注入的初始态只是一份
//!   「现在长什么样」的快照——不许借它重放全屏雾散（否则热插拔屏幕 / 改「生效显示器」
//!   白闪一次整屏），见 [`create_all`] 的 `replay_burst`。
//! - **窗口操作必须主线程**：事件管道跑在 tokio 线程上，`WebviewWindowBuilder` 不是
//!   线程安全的，所有建窗/销毁/移动都经 `run_on_main_thread` 派发。
//! - **每显示器一个窗口**：全屏覆盖窗无法跨屏，多屏场景只能一屏一个，全部收同一份状态。

use bark_core::{EventKind, GlowConfig, GlowEffect, MonitorTarget, NormalizedEvent};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, EventTarget, Manager, Monitor, WebviewUrl, WebviewWindowBuilder};

use crate::state::{lock, read, AppState, SessionPhase, SessionStatus};

/// glow 窗口 label 前缀（实际 label 形如 `glow-0`）
pub const GLOW_PREFIX: &str = "glow-";
/// 全屏特效窗口 label 前缀（实际 label 形如 `glow-burst-0`）。
///
/// 「位置=顶部」时灯带与特效分窗：边缘窗口缩成顶部条带（见 [`rect_of`]），
/// 特效要有自己的整屏画布，就放在这套独立窗口里。仍以 `glow-` 开头，
/// capabilities 里 `glow-*` 的通配自动覆盖，无需另配。
const BURST_PREFIX: &str = "glow-burst-";
/// 状态推送事件名（前端 src/glow.ts 监听）
pub const GLOW_EVENT: &str = "glow://state";

// ---------------------------------------------------------------------------
// 观感参数（写死）
// ---------------------------------------------------------------------------

// 「外观与节奏」曾开放在设置页，现已固定为下面的常量（即原 UI 默认值），
// 随 payload 下发给覆盖层。速度 6 = 参考稿的原始节奏 = 倍率 1.0（呼吸 2.4s / 流光 2.8s）；
// 全屏时长 = 原公式 `3_000 / 1.0` 的结果。
const GLOW_SPEED_CSS: &str = "1.00";
const GLOW_INTENSITY_CSS: &str = "1.00";
/// 流光细线宽度（逻辑 px，**按 1080p 屏高校准**）：各 glow 窗口在前端按自身
/// 高度等比缩放（glow.ts 的 `applyWidth`），高分屏上线粗与屏高保持同比例；
/// DPI 缩放无需另算——逻辑像素本来就会随缩放变大。呼吸坡是纯渐变，不使用线宽
///
/// 初版 4px 用户实测太细：看不清，且渐变尾迹的量化色带在细线上最扎眼。
/// 按比例放大一档到 6px（×1.5，连带 glow.html 的 `--w` 兜底值同步）；
/// 要再调粗细只改这一个数。
const GLOW_WIDTH: u32 = 6;
const GLOW_RADIUS: u32 = 0;
const GLOW_OPACITY: f32 = 0.9;
const GLOW_BURST_MS: u32 = 3_000;
/// 「完成」绿色停留时长（ms），到点自动淡出
const GLOW_COMPLETED_HOLD_MS: u64 = 6_000;
/// 「失败」（意外终止）色停留时长（ms）
const GLOW_FAILED_HOLD_MS: u64 = 12_000;
/// 「位置=顶部」时覆盖窗的条带高度（逻辑 px）。
///
/// 只亮顶部时窗口不再覆盖整块显示器（见 [`rect_of`]）：前端把灯效**重绘**成
/// 贴顶边的一条横条（glow.html `data-sides="top"` 的独立渲染路径，不是四周
/// 渲染的裁剪），窗口只需装下顶边辉光坡的可见深度（呼吸坡 100px 模糊 +
/// 32px 扩散 = 132px，坡深 × `--glow`）加余量。150 = 132 + 18 余量。
/// 旧值 280 是裁剪版的账（140px 裁剪带 + 132px 被裁剪线藏住的**底边**辉光
/// + 8 余量）：重绘后底边 / 左右边根本不画，不再需要预留。
/// **与 glow.html 顶部横条的坡深是一对耦合数值，改任何一个必须同步另一个**
/// （同 `rect_of` ↔ `#glow` inset 的惯例）。`--glow` 目前写死 1.00
/// （[`GLOW_INTENSITY_CSS`]），强度若开放成可调，本值要跟着坡深缩放。
const GLOW_TOP_STRIP_H: f64 = 150.0;

/// 颜色特效的触发序号：每亮一次 +1。
///
/// 覆盖层是常驻的（建好不销毁），它无法从「状态又变成思考色了」判断出这是新一次触发还是
/// 同一状态的重复推送，因此由 Rust 侧给每次触发编号——序号一变就补放一次全屏特效。
/// 用独立的原子量而不是塞进锁里：它只需要单调递增，不必和任何状态同步。
static BURST_SEQ: AtomicU64 = AtomicU64::new(0);

/// 预览会话序号：每点一次预览 +1，恢复定时器靠它认领自己那一轮。
static PREVIEW_SEQ: AtomicU64 = AtomicU64::new(0);

/// 现存 glow 窗口是按哪种「位置」形态建的（true = 顶部条带）。
///
/// `align` 靠它识别**模式切换**：数量没变但形态变了时不能就地缩放——旧页面的
/// 渲染状态（比如还没拿到带 `data-sides` 的 payload）配上新窗口尺寸，会在屏幕
/// 中间闪出一条光带；必须整组重建，让新页面带着正确的初始态一帧成型。
/// 只在主线程读写（align / create_all 都在主线程），原子量只为免锁。
static BUILT_TOP_ONLY: AtomicBool = AtomicBool::new(false);

/// 最近一次下发给窗口的特效序号。
///
/// `emit` 是所有 payload 的必经漏斗，靠它识别「这次推送是不是新的一次颜色特效」
/// （序号前进 = 是）：只有新触发才为顶部模式按需建特效窗口，同步 / 巡检 / 预览
/// 恢复这类序号不变的重推只补发、不建窗、不重放。仅主线程读写（emit 全在主线程）。
static LAST_EMITTED_BURST: AtomicU64 = AtomicU64::new(0);

/// 最近一次下发的 payload 原文（页面就绪补发用，见 [`resend_latest`]）。
///
/// 新建窗口在页面加载完之前收不到任何 emit——前端 `listen` 还没注册，事件直接丢。
/// 建窗时的 initialization_script 只能带上**建窗那一刻**的状态，之后到页面就绪
/// 之间到达的推送（连点第二下预览就是典型）全部丢失：表现是「第二下没有任何反应、
/// 灯按第一下的旧定时器熄灭」。补发要发**补发时刻**的最新值而不是建窗时的旧值，
/// 否则会把过期状态盖回去。主线程读写（emit / 补发都在主线程），Mutex 只为免 unsafe。
static LAST_PAYLOAD: Mutex<Option<GlowPayload>> = Mutex::new(None);

/// 特效窗口播完后的回收缓冲（ms）：动画时长之外再留一段页面加载 / 渐隐尾巴，
/// 防止窗口在动画真正结束前被销毁（新建窗口的页面加载会推迟动画起点）。
const BURST_DESTROY_BUFFER_MS: u64 = 800;

/// 建窗后的「页面就绪补发」延时（ms，自 PageLoadEvent::Finished 起算）。
///
/// Finished 时模块脚本一般已跑完，但 `listen` 的注册是一次 IPC 往返、可能还差
/// 几毫秒——两级补发兜住这个尾巴。多发无害：`render` 幂等，全屏特效按 burst
/// 序号判重不会重放；少发就是「连点第二下没反应」那类残缺。
const RESEND_DELAYS_MS: [u64; 2] = [250, 1_200];

/// 记一次「颜色特效被触发」，返回新的序号
fn bump_burst() -> u64 {
    BURST_SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

fn current_burst() -> u64 {
    BURST_SEQ.load(Ordering::Relaxed)
}


/// 流光状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GlowState {
    /// 空闲：不显示
    #[default]
    Idle,
    /// 运行中（思考 / 执行工具）：思考色呼吸
    Running,
    /// 等待用户确认或输入：等待色呼吸
    Waiting,
    /// 任务完成：完成色常亮后淡出
    Completed,
    /// 意外终止（失败 / 心跳超时）：失败色脉冲后淡出
    Failed,
}

impl GlowState {
    /// 状态对应的主色。
    ///
    /// **这里是全仓库唯一的配色来源**：改色只改这个函数 + `palette_is_locked` 那个
    /// 锁配色的测试。其它地方一律用角色名（思考 / 等待 / 完成 / 失败），不写色系名、
    /// 更不写 hex——`.md` 文档与注释里出现 hex 就是漏改的前兆。
    ///
    /// 色值取 GitHub Primer **深色主题**的语义色（初版用浅色主题的暗色变体，用户实测
    /// 深色桌面上不明显，整体调亮一档）。深浅底分工：浅底由 glow.html 颜色线下垫的
    /// 暗晕撑对比，深底直接靠颜色亮度——所以亮色变体两边都成立；再低的可见度由
    /// 全屏特效兜底。
    pub fn color(self) -> &'static str {
        match self {
            // Idle 不显示，不会有人看到这个色；与 Running 同色只是为了少一个分支
            GlowState::Idle => "#4493f8",
            GlowState::Running => "#4493f8",   // 思考颜色 · 蓝
            GlowState::Waiting => "#f0883e",   // 等待颜色 · 橙
            GlowState::Completed => "#3fb950", // 完成颜色 · 绿
            GlowState::Failed => "#f85149",    // 失败颜色 · 红
        }
    }

    /// 该状态的停留时长（ms）。0 = 常驻，直到下一个状态覆盖它。
    ///
    /// 只有终态（完成 / 失败）会自己淡出：思考中 / 等待中必须由后续事件驱散，
    /// 否则「还在跑但灯灭了」比不亮更糟。
    pub fn hold_ms(self) -> u64 {
        match self {
            GlowState::Completed => GLOW_COMPLETED_HOLD_MS,
            GlowState::Failed => GLOW_FAILED_HOLD_MS,
            _ => 0,
        }
    }

    /// 终态（会自己淡出的状态）
    pub fn is_terminal(self) -> bool {
        matches!(self, GlowState::Completed | GlowState::Failed)
    }

    /// 配置里每状态效果的键名（`glow.edge_effects` / `burst_effects` 的字段）。
    /// Idle 不点亮任何效果，复用 thinking 的设置（与 `color()` 同样的少分支处理）
    pub fn config_key(self) -> &'static str {
        match self {
            GlowState::Idle | GlowState::Running => "thinking",
            GlowState::Waiting => "waiting",
            GlowState::Completed => "completed",
            GlowState::Failed => "failed",
        }
    }
}

/// 推给 glow 窗口的完整渲染指令
#[derive(Debug, Clone, Serialize)]
pub struct GlowPayload {
    pub state: GlowState,
    pub color: String,
    /// 边缘光效：是否显示边缘灯带。与全屏特效各自独立——关掉边缘后
    /// 全屏特效照常（前端按 data-edge 隐藏灯带层，不动 #burst）。
    /// 总开关 `edge` × **当前边缘状态那一行**没选「无」
    pub edge: bool,
    /// 边缘光效类型："breathing" | "comet"（前端 data-effect），
    /// 取**当前边缘状态那一行**（`edge_effects`）的设置
    pub effect: String,
    /// 边缘光效位置："top"=只画顶部（前端重绘成顶边一条横条，窗口本身
    /// 也按 [`GLOW_TOP_STRIP_H`] 缩成条带）/ "all"=四周（旧行为）。
    /// 取配置 `edge_position` 经 `edge_sides()` 归一后的值。
    /// 全屏特效窗口恒收 "all"（见 [`burst_view`]：特效永远整屏）
    pub sides: String,
    /// 本次下发要不要让边缘灯带「熄灭重放」（前端掐掉 opacity 过渡、立即熄一帧
    /// 再重新淡入——不能靠反转进行中的过渡，见 glow.ts render 的实现注释）。
    ///
    /// 只有**设置页预览点按**为 true：用户连点预览时，第二下要看得见是新的一次
    /// 播放（熄灭 → 重新点亮 + 重置停留计时），而不是同色重触发毫无反应、灯按
    /// 第一下的旧定时器熄灭。真实事件 / 预览恢复 / 页面就绪补发一律 false——
    /// 真实事件要平滑换色（`burst_only` 更是「边缘不动」），补发是幂等重推。
    pub restart_edge: bool,
    /// 灯效速度倍率（已格式化的 CSS 值，如 "1.00"）：
    /// CSS 里所有时长都写成 `calc(参考时长 / var(--speed))`
    pub speed: String,
    /// 辉光强度倍率（已格式化的 CSS 值，如 "1.00"）：
    /// CSS 里内辉光的扩散半径写成 `calc(参考半径 * var(--glow))`
    pub intensity: String,
    /// 停留时长（ms），0 表示常驻
    pub hold_ms: u64,
    pub width: u32,
    pub radius: u32,
    pub opacity: f32,
    /// 是否启用全屏特效：总开关 `fullscreen` × **触发角色那一行**没选「无」
    pub fullscreen: bool,
    /// 全屏特效类型："fog"（雾散）| "scan"（HUD 扫描）（前端 #burst 的 data-effect），
    /// 取**触发角色那一行**（`burst_effects`）的设置
    pub fullscreen_effect: String,
    /// 本次颜色特效的触发序号（0 = 尚无触发）。
    /// 前端记住上一次的值，序号变了才补放一次全屏特效。
    pub burst: u64,
    /// 一次全屏特效的时长（ms）
    pub burst_ms: u32,
    /// 全屏特效的颜色，与边缘色**解耦**：多会话并行时一个会话完成/失败，
    /// 边缘保持思考色持续呼吸，全屏按事件角色（完成 / 失败）补放一次。
    pub burst_color: String,
}

fn payload(cfg: &GlowConfig, state: GlowState) -> GlowPayload {
    payload_with_burst(cfg, state, state)
}

/// 带独立全屏特效色的 payload 构造（双通道：边缘色 ≠ 特效色）。
/// `state` 是边缘表达的状态（决定边缘色与**它那一行**的边缘类型）；
/// `burst` 是全屏特效的触发角色（决定特效色与**它那一行**的全屏类型）。
fn payload_with_burst(cfg: &GlowConfig, state: GlowState, burst: GlowState) -> GlowPayload {
    // 每状态类型：那一行选「无」时对应通道整个不亮（edge/fullscreen 为 false）
    let edge_effect = cfg.edge_effect_for(state.config_key());
    let burst_effect = cfg.burst_effect_for(burst.config_key());
    GlowPayload {
        state,
        color: state.color().to_string(),
        burst_color: burst.color().to_string(),
        edge: cfg.edge && edge_effect.is_some(),
        effect: edge_effect.unwrap_or(GlowEffect::Breathing).id().to_string(),
        sides: cfg.edge_sides().to_string(),
        // 默认不重放灯带；只有预览点按会改写成 true（见 GlowPayload::restart_edge）
        restart_edge: false,
        speed: GLOW_SPEED_CSS.to_string(),
        intensity: GLOW_INTENSITY_CSS.to_string(),
        hold_ms: state.hold_ms(),
        width: GLOW_WIDTH,
        radius: GLOW_RADIUS,
        opacity: GLOW_OPACITY,
        fullscreen: cfg.fullscreen && burst_effect.is_some(),
        fullscreen_effect: burst_effect.unwrap_or("fog").to_string(),
        burst: current_burst(),
        burst_ms: GLOW_BURST_MS,
    }
}

// ---------------------------------------------------------------------------
// 状态推导：会话表 + 事件 → 一个颜色
// ---------------------------------------------------------------------------

/// 由「会话状态表 + 刚到的事件」推导流光状态并应用。
///
/// 边缘优先级：**等待 > 思考 > 终态**。
/// - 等待（等待确认 / 等待输入）的会话需要人去点一下，比「还在跑」更值得打断你；
/// - 终态只在**没有任何活跃会话**时才接管边缘——A 完成了但 B 还在跑，
///   边缘持续表达 B 的思考状态。
///
/// 但**全屏特效是独立的事件通道**（见 [`burst_only`]）：A 完成时哪怕 B 还在跑，
/// 也以完成色补放一次雾散——「有一个完成了」是值得立刻知道的事件，
/// 不该因为边缘还在表达 B 的状态就被静默吞掉。
pub fn update_from_event(app: &AppHandle, state: &Arc<AppState>, ev: &NormalizedEvent, running_lost: bool) {
    let desired = desired_state(state);
    // 事件通道：还有别的活跃会话（边缘不会切终态）时，完成/失败/判死仍补放全屏。
    // 表空的情形由下面的正常推导接管——边缘切完成/失败色时 apply 本身就会触发
    // 同色全屏特效，这里再补就是双闪。
    if desired.is_some() {
        if let Some(burst) = burst_for_event(ev.kind, running_lost) {
            burst_only(app, state, burst);
        }
    }
    let Some(next) = derive(desired, ev.kind, running_lost) else {
        return;
    };
    apply(app, state, next);
}

/// 事件通道补放什么全屏特效（纯函数，§1.3）：`None` = 不补放。
///
/// `RunAborted` **一律不 burst**：手动中止不亮失败色全屏特效——即便同一瞬间恰好有
/// 别的会话被心跳判死（`running_lost`）、判死者是另一会话，RunAborted 也不是判死
/// 来源。保持简单、与另外两处口径对齐（`derive` 里 RunAborted 先于判死收光、
/// `sound_key_for` 里手动中止永远不响）：「手动中止不提醒」，音/光优先级必须一致。
fn burst_for_event(kind: EventKind, running_lost: bool) -> Option<GlowState> {
    match kind {
        EventKind::RunCompleted => Some(GlowState::Completed),
        EventKind::RunFailed => Some(GlowState::Failed),
        EventKind::RunAborted => None,
        // 任意事件捎带的心跳判死 = 意外终止（agent 进程没了）→ 失败色补放
        _ if running_lost => Some(GlowState::Failed),
        _ => None,
    }
}

/// 「只补放一次全屏特效」：边缘状态不动，以 `burst` 的角色色推一次 payload。
///
/// 这是与边缘状态解耦的**事件通道**：多会话并行时一个会话完成/失败，
/// 边缘继续表达其余会话的聚合状态，全屏则按事件角色闪一下。
///
/// 取舍：
/// - **不动 `glow` / `glow_preview`**：不改任何常驻状态，也不是「预览接管」；
///   payload 里的 state/color/hold_ms 原样带回当前边缘态，前端只多看到
///   burst 序号变了 + 特效色是事件角色色；
/// - **边缘窗口不存在时不建窗**：懒创建原则——不为一次 3 秒的特效点亮整个
///   覆盖层。已知代价：托盘「重置流光」或关开关收掉窗口后，若仍有会话在跑且
///   紧接着来了终态事件，这一次雾散会被丢掉（`apply` 因边缘状态没变而早退，
///   窗口也不会重建）。取舍见上：不值得为一次特效重建常驻覆盖层；
///   顶部模式下的**特效窗口**例外——它本就是按需创建、播完即毁的（见
///   [`emit`]），为这一次特效建它正是它的本职；
/// - `fullscreen` 关闭或总开关关闭时整条路径 no-op（闭包**执行时**复查开关，
///   与 [`apply_inner`] 同样的竞态防御）；
/// - **比较/早退/编号/构造 payload/emit 整体在同一个主线程闭包里执行**（§2.2）：
///   编号（`bump_burst`）与 payload 构造若留在调用线程，两个触发线程交错时会
///   「A 线程 bump=5 构造 payload → B 线程 bump=6 构造并 emit → A 线程 emit」，
///   前端按 burst 序号前进方向收特效，乱序的两帧会把状态卡在过期颜色上。
fn burst_only(app: &AppHandle, state: &Arc<AppState>, burst: GlowState) {
    let app = app.clone();
    let state = state.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let cfg = read(&state.config).glow.sanitize();
        if !cfg.enabled || !cfg.fullscreen {
            return;
        }
        if edge_labels(&app).is_empty() {
            return;
        }
        let edge = *lock(&state.glow);
        // 编号必须在构造 payload 之前 +1（payload 在本闭包内同步构造）
        bump_burst();
        let p = payload_with_burst(&cfg, edge, burst);
        emit(&app, &cfg, &p);
    }) {
        tracing::warn!("流光全屏特效推送失败（主线程不可用？）: {e}");
    }
}

/// 会话表当前想要的状态：等待 > 运行中 > None（没有任何活跃会话）
fn desired_state(state: &Arc<AppState>) -> Option<GlowState> {
    let sessions = lock(&state.active_sessions);
    if sessions.values().any(is_waiting) {
        Some(GlowState::Waiting)
    } else if !sessions.is_empty() {
        Some(GlowState::Running)
    } else {
        None
    }
}

/// **无事件版本**：心跳判死巡检用（见 `state::sweep_stale_sessions`）。
///
/// agent 被杀或回合被中断之后可能**再也不发任何事件**，只靠事件路径改流光就会一直停在
/// 思考色。这里借「中性事件」`Activity` 走同一个 `derive`——它只对 `RunCompleted` /
/// `RunFailed` / `running_lost` 特殊处理，正好表达「只看会话表 + 是否判死」：
/// 还有活跃会话就按它们显示（等待/思考优先），一个不剩且判死了思考中的会话才转失败色，
/// 否则保持不动（判死一个等待中的会话不该把终态色掐掉）。
pub fn refresh_from_sessions(app: &AppHandle, state: &Arc<AppState>, running_lost: bool) {
    let desired = desired_state(state);
    // 判死发生在还有别的活跃会话时：边缘不动（继续表达它们），但「意外终止」
    // 是事件，失败色全屏照放（对称于显式的 RunFailed，见 burst_only 的双通道说明）。
    if running_lost && desired.is_some() {
        burst_only(app, state, GlowState::Failed);
    }
    let Some(next) = derive(desired, EventKind::Activity, running_lost) else {
        return;
    };
    apply(app, state, next);
}

/// 纯函数版推导（便于测试）。`desired` 为 None 表示此刻没有任何活跃会话。
/// 返回 None = 保持当前状态不变——`session_start` 这类中间事件不该把刚亮起的
/// 终态色掐掉（会话开始后紧跟的心跳会立刻切到思考色）。
fn derive(desired: Option<GlowState>, kind: EventKind, running_lost: bool) -> Option<GlowState> {
    match desired {
        Some(s) => Some(s),
        None => match kind {
            EventKind::RunCompleted => Some(GlowState::Completed),
            EventKind::RunFailed => Some(GlowState::Failed),
            // 用户主动中止：没有别的会话在跑就**直接收起流光**——不亮失败色（不是失败）
            // 也不亮完成色（没跑完）。还有别的会话在跑时走上面那个分支，照常显示它们的状态。
            EventKind::RunAborted => Some(GlowState::Idle),
            // 心跳超时被判死 = agent 进程没了（被杀时不会有 Stop 事件）→ 意外终止
            _ if running_lost => Some(GlowState::Failed),
            _ => None,
        },
    }
}

fn is_waiting(s: &SessionStatus) -> bool {
    matches!(s.phase, SessionPhase::WaitingPermission | SessionPhase::WaitingInput)
}

// ---------------------------------------------------------------------------
// 状态更新入口
// ---------------------------------------------------------------------------

/// 切换到新状态并推送给所有 glow 窗口（必要时先建窗口）。
///
/// 状态没变时**只在终态下重发**：同一分钟里两个会话接连完成时，前端的淡出计时器
/// 需要重新计时，否则第二条会直接看不见；而心跳事件一个回合能来上百次，
/// 全部重发纯属浪费。
pub fn apply(app: &AppHandle, state: &Arc<AppState>, next: GlowState) {
    apply_inner(app, state, next, true);
}

/// 预览结束后的恢复：把显示退回预览前的状态，但**不补放全屏特效**。
///
/// 恢复不是一次新的颜色触发：「回到刚才那个状态」再闪一遍全屏是纯噪音——走完整
/// [`apply`] 的话，预览播放完总会「补」一个恢复色的全屏动效、边缘也当新点亮重新
/// 走一遍停留时长，这正是「预览播完又补一个绿色完成动效」的来源（恢复目标最常见
/// 的残留就是上次任务的完成绿，见 [`restore_target`]）。序号不 bump：payload 里的
/// `burst` 没变，前端只换色、不 `playBurst`。
fn restore(app: &AppHandle, state: &Arc<AppState>, prev: GlowState) {
    apply_inner(app, state, prev, false);
}

/// [`apply`] / [`restore`] 的共同路径。
///
/// `new_trigger`：这次算不算一次「新的颜色特效」（要发新序号、允许补放全屏）。
/// 真实事件 / 终态重发 = true；预览恢复 = false（见 [`restore`]）。
///
/// **比较 / 早退 / 置 cur / bump_burst / 构造 payload / emit 整体在同一个
/// `run_on_main_thread` 闭包里执行**（§2.2）：拆开的话，事件线程置 `cur=A` 后、
/// 闭包排队前被 IPC 线程插队置 `cur=B`，主线程会发 B 再发 A——前端停在 A、
/// 后端 `cur=B`，后续同态事件全部命中早退不自愈，光效整场卡色。
fn apply_inner(app: &AppHandle, state: &Arc<AppState>, next: GlowState, new_trigger: bool) {
    let app = app.clone();
    let state = state.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        // 真实事件 / 托盘重置 / 保存配置都会走到这里：预览到此结束。
        // 清掉会话之后，挂着的预览恢复定时器会因为序号对不上而自动作废。
        *lock(&state.glow_preview) = None;
        let cfg = read(&state.config).glow.sanitize();
        // **先取值再 match**，不要写成 `match apply_action(*lock(&state.glow), …)`：
        // match 语句的临时锁守卫会一直活到 match 结束（临时作用域规则），分支里的
        // `*lock(&state.glow) = …` 就成了同线程重入——std::sync::Mutex 不可重入，
        // 主线程当场自锁死，UI 永久卡死（2026-09-24 实测踩坑：状态一变就触发，
        // 同态早退的 Skip 分支不重入所以没事，这也是「经常卡死」而非「必死」的原因）。
        let action = apply_action(*lock(&state.glow), next, cfg.enabled);
        match action {
            // 开关已关：复位状态机并回收窗口，然后收工。
            // 复位为 Idle 而不是留在 next：否则重新开启时 sync 会把旧状态（如橙色等待）
            // 原样复活，哪怕那个会话早已不存在。
            // **禁用复位必须判在同态早退之前**（apply_action 的顺序即语义）：
            // glow 关闭时点预览 + 真实心跳推导出同态，早退会跳过这一步与销毁——
            // 覆盖窗残留在屏幕上永远没人回收（§2.2 的第二个症状）。
            ApplyAction::ResetDisabled => {
                *lock(&state.glow) = GlowState::Idle;
                destroy_all(&app);
            }
            ApplyAction::Skip => {}
            ApplyAction::Push => {
                *lock(&state.glow) = next;
                // 走到这里 = 新的一次颜色特效（状态真的变了，或终态重发）。
                // 空闲不编号——「收起光效」不是一次颜色特效，不该闪一下全屏。
                if new_trigger && next != GlowState::Idle {
                    bump_burst();
                }
                let p = payload(&cfg, next);
                push_or_create(&app, &cfg, &p);
            }
        }
    }) {
        tracing::warn!("流光状态更新失败（主线程不可用？）: {e}");
    }
}

/// apply_inner 的处置决策（纯函数，§2.2）：**禁用复位 > 同态早退 > 推送**。
///
/// 顺序即语义、不可换：glow 关闭 + 同态（关着灯点预览，随后真实心跳推导出同一
/// 状态）时，同态早退若判在前面会跳过禁用复位与覆盖窗销毁，残窗永远没人回收。
/// 同态早退只对**常驻态**：终态（绿/红）重发要重新计停留时长——同一分钟里两个
/// 会话接连完成时，前端的淡出计时器需要重新计时，否则第二条会直接看不见。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyAction {
    ResetDisabled,
    Skip,
    Push,
}

fn apply_action(cur: GlowState, next: GlowState, enabled: bool) -> ApplyAction {
    if !enabled {
        ApplyAction::ResetDisabled
    } else if cur == next && !next.is_terminal() {
        ApplyAction::Skip
    } else {
        ApplyAction::Push
    }
}

/// 托盘「重置流光」：无条件销毁覆盖窗并复位状态机。
/// 用于驱散卡住的颜色（等待中的会话没有心跳，agent 崩溃后既无终态事件、
/// 会话表条目也要等 10 分钟僵死判定才消失），或任何残留的覆盖层。
/// 下一次事件到来时按正常逻辑重新点亮。
pub fn reset(app: &AppHandle, state: &Arc<AppState>) {
    *lock(&state.glow_preview) = None;
    *lock(&state.glow) = GlowState::Idle;
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || destroy_all(&app));
}

/// 设置页预览：临时点亮一次（用户想先看看效果，不该被开关挡住）。
///
/// 返回本轮预览的**序号**，调用方的恢复定时器要拿它回调 [`end_preview`]。
pub fn preview(app: &AppHandle, state: &Arc<AppState>, next: GlowState) -> u64 {
    dispatch_preview(app, state, next, None)
}

/// 组合预览：边缘亮 `edge` 状态、全屏特效以 `burst` 的颜色补放一次——
/// 用于预览双通道并存的效果（如「一个完成、其余还在跑」= 思考色呼吸 + 完成色雾散）。
/// 预览的临时接管/恢复语义与 [`preview`] 完全一致。
pub fn preview_burst(app: &AppHandle, state: &Arc<AppState>, edge: GlowState, burst: GlowState) -> u64 {
    dispatch_preview(app, state, edge, Some(burst))
}

/// 预览的整段接管在**同一个主线程闭包**里执行，返回本轮序号（经 channel 交还）。
///
/// 与 [`apply_inner`] 同一纪律（§2.2）：登记会话 / 写状态 / 特效编号 / 构造
/// payload / 建窗下发必须不可分割。拆到调用线程做的话，连点两下预览会交错出
/// 两种怪象（2026-09-26 用户实测「连续点击第二次毫无反应」）：
/// - 两次触发**共用一个序号**（后点的 bump 插进了先点的「bump → 构造 payload」
///   缝里）：前端按序号判重，第二下被吞掉——特效不重放；
/// - **后点的反而先登记**恢复槽：先点那轮的恢复定时器认领成功，在第一轮的 6s
///   处把灯掐灭，之后再没有任何一轮把它点亮。
///
/// 序号在闭包里分配（与登记同步），调用方的恢复定时器拿它认领自己那一轮。
/// `burst`：组合预览的特效角色（`None` = 边缘与特效同角色）。
/// 预览无视总开关（用户点预览就是想先看效果，不该被开关挡住），故不查 `enabled`。
fn dispatch_preview(
    app: &AppHandle,
    state: &Arc<AppState>,
    edge: GlowState,
    burst: Option<GlowState>,
) -> u64 {
    let (tx, rx) = std::sync::mpsc::channel();
    let app2 = app.clone();
    let state2 = state.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let cfg = read(&state2.config).glow.sanitize();
        let seq = begin_preview(&state2, edge);
        let mut p = match burst {
            Some(b) => payload_with_burst(&cfg, edge, b),
            None => payload(&cfg, edge),
        };
        // 预览是用户主动点按：灯带要看得见地「熄灭重放」（见 restart_edge）
        p.restart_edge = true;
        push_or_create(&app2, &cfg, &p);
        let _ = tx.send(seq);
    }) {
        tracing::warn!("流光预览派发失败（主线程不可用？）: {e}");
        return 0;
    }
    // 闭包正常执行后 send 必然成功；失败（闭包 panic）返回 0——
    // 恢复定时器拿 0 认领不到任何一轮，宁可不恢复也不误恢复
    rx.recv().unwrap_or(0)
}

/// 开一轮预览会话：登记恢复槽、点亮边缘态、（非空闲时）给特效编号。
///
/// 仅由 [`dispatch_preview`] 在主线程闭包内调用——登记 / 写状态 / 编号与随后的
/// payload 构造必须是一个不可分割的序列（§2.2，见 dispatch_preview 的文档）。
fn begin_preview(state: &Arc<AppState>, edge: GlowState) -> u64 {
    let seq = PREVIEW_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    {
        let mut slot = lock(&state.glow_preview);
        *slot = Some(open_session(*slot, *lock(&state.glow), seq));
    }
    *lock(&state.glow) = edge;
    // 预览也要编号：用户点「预览」就是想看全屏特效长什么样
    if edge != GlowState::Idle {
        bump_burst();
    }
    seq
}

/// 结束第 `seq` 轮预览，恢复到预览前的状态（经 [`restore`]：不补放全屏特效，
/// 且终态不复活，见 [`restore_target`]）。
///
/// 序号对不上就直接返回：说明这一轮已经被真实事件、保存配置、托盘重置或更新的
/// 一次预览顶掉了，旧定时器无权再动流光。
pub fn end_preview(app: &AppHandle, state: &Arc<AppState>, seq: u64) {
    let prev = {
        let mut slot = lock(&state.glow_preview);
        match claim_session(*slot, seq) {
            Some(prev) => {
                *slot = None;
                prev
            }
            None => return,
        }
    };
    restore(app, state, prev);
}

/// 「熄灭」：作废进行中的预览并立刻收起光效（下一次真实事件会重新点亮）。
///
/// 只推一个 Idle 而不销毁窗口——建窗/销窗会闪，而且用户点「熄灭」多半只是
/// 想让它现在别亮着，不是要关掉整个功能（那是总开关的事）。
pub fn off(app: &AppHandle, state: &Arc<AppState>) {
    *lock(&state.glow_preview) = None;
    apply(app, state, GlowState::Idle);
}

/// 开一轮预览会话：返回该记进槽里的 `(序号, 预览前要恢复的状态)`。
///
/// **只有第一轮预览才去读当前状态**。每次都重读的话，连点两下预览会把第一轮的
/// 预览态当成「真实状态」记下来，第一轮的定时器到点恢复时又把灯点亮——表现就是
/// 「预览完就再也停不下来」。抽成纯函数是为了能直接测这条路径。
///
/// 记进去的值经 [`restore_target`] 映射：**终态不复活**（存的映射值，之后续多少
/// 轮预览都不会把终态捡回来）。
fn open_session(
    current: Option<(u64, GlowState)>,
    live: GlowState,
    seq: u64,
) -> (u64, GlowState) {
    (seq, current.map(|(_, prev)| prev).unwrap_or_else(|| restore_target(live)))
}

/// 预览要恢复到的状态：**终态不复活**，一律映射成空闲。
///
/// 完成 / 失败是「亮几秒就自己淡出」的短命状态（前端按 `hold_ms` 淡出，6s / 12s），
/// 而一次预览至少占 3s、默认 6s——它的停留时长基本被预览吃完了。恢复时还把终态
/// 推回去，前端会当成新点亮重新计一遍停留时长，表现就是「预览播放完，又补了一个
/// 绿色的完成动效」。绿色最常见只是统计巧合：上次任务残留的完成绿是恢复槽里
/// 最常见的值（状态机里终态不过期，见 [`GlowState::hold_ms`]）；预览「完成」
/// 本身也会在结束时再补一次绿，同一条根因。
///
/// 运行中 / 等待是常驻显示（`hold_ms` 为 0、背后多半真有会话在跑 / 在等），
/// 照常恢复。
fn restore_target(live: GlowState) -> GlowState {
    if live.is_terminal() {
        GlowState::Idle
    } else {
        live
    }
}

/// 认领一轮预览会话：序号对得上才返回要恢复的状态，否则 `None`（旧定时器作废）。
fn claim_session(current: Option<(u64, GlowState)>, seq: u64) -> Option<GlowState> {
    match current {
        Some((s, prev)) if s == seq => Some(prev),
        _ => None,
    }
}

/// 当前状态（预览结束后据此恢复）
pub fn current(state: &Arc<AppState>) -> GlowState {
    *lock(&state.glow)
}

/// 重新对齐窗口集合与配置（保存设置后调用）：
/// 关闭开关 → 销毁；开启 → 按当前显示器布局重建/校正位置尺寸。
pub fn sync(app: &AppHandle, state: &Arc<AppState>) {
    // 保存配置 = 用户在界面上按了「保存」，进行中的预览到此结束（关了开关就更该结束）
    *lock(&state.glow_preview) = None;
    let cfg = read(&state.config).glow.sanitize();
    if !cfg.enabled {
        let app = app.clone();
        let _ = app.clone().run_on_main_thread(move || destroy_all(&app));
        *lock(&state.glow) = GlowState::Idle;
        return;
    }
    let app = app.clone();
    let state = state.clone();
    // 这一路会重新枚举显示器并移动窗口，只在配置变更时走，不在事件热路径上。
    // 执行时复查开关：与 push 同样的竞态——入队时开着、执行时可能已被关掉
    // （本闭包与禁用路径的销毁闭包同队列，销毁在前时这里不得把状态再发回去）。
    // payload 也在**闭包内**取当前状态构造：先在调用线程取会留下「读状态→派发」
    // 窗口，与事件管道的 apply 交错时会把过期的 Idle 后发出去——前端熄灭而后端
    // 仍是 Running，之后的心跳全部命中 apply 的同态早退，光效整场无法自愈。
    if let Err(e) = app.clone().run_on_main_thread(move || {
        if !read(&state.config).glow.enabled {
            return;
        }
        let p = payload(&cfg, current(&state));
        align(&app, &cfg, &p);
        emit(&app, &cfg, &p);
    }) {
        tracing::warn!("流光窗口同步失败: {e}");
    }
}

/// 30s 巡检对齐：显示器热插拔 / 分辨率变化后把窗口集合与布局对齐回来。
/// 无变化时只是枚举一遍显示器，开销可忽略；不 emit（状态没变，避免重放全屏特效）。
pub fn maintain(app: &AppHandle, state: &Arc<AppState>) {
    let cfg = read(&state.config).glow.sanitize();
    if !cfg.enabled {
        return;
    }
    let app = app.clone();
    let state = state.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        if !read(&state.config).glow.enabled {
            return;
        }
        let p = payload(&cfg, current(&state));
        align(&app, &cfg, &p);
    }) {
        tracing::warn!("流光巡检对齐失败: {e}");
    }
}

/// 建窗口（如果需要）+ 推事件。**仅主线程调用**（payload 已在主线程构造）。
fn push_or_create(app: &AppHandle, cfg: &GlowConfig, p: &GlowPayload) {
    if edge_labels(app).is_empty() {
        // 懒创建：这一次建窗**是**某次颜色触发的产物，注入的初始态允许放雾散。
        // 顶部模式注入 edge_view：条带窗口不背特效通道——raw p 的 fullscreen
        // 会让 #burst 在条带里播一次，emit 随后摘掉也来不及（动画已经启动）
        if p.sides == "top" {
            let init = edge_view(p);
            create_all(app, cfg, &init, true);
        } else {
            create_all(app, cfg, p, true);
        }
    } else if p.state != GlowState::Idle {
        // 窗口已存在且这次不是「收起光效」：此刻任务栏可能已经重新升到我们上面
        // （屏幕底部那条会被它整条盖住），趁这次点亮压回去。新窗口不必——刚建的
        // 置顶窗本来就在最上层；Idle 也不必——整层透明，抢层级没有收益
        // （与 [`keep_topmost`] 同一口径）。
        raise(app);
    }
    emit(app, cfg, p);
}

/// 把一份 payload 按窗口角色拆开下发。
///
/// 灯带与特效的 payload 本就是两个独立通道（`edge` / `fullscreen`），分窗只是把
/// 两个通道分别发给各自的画布：
/// - **四周模式**：边缘窗口就是特效的画布，`p` 原样下发——与旧版逐字节一致；
/// - **顶部模式**：边缘窗口（条带）拿 [`edge_view`]（摘掉 fullscreen 通道，特效
///   不在条带里放），特效由专属的整屏窗口（[`burst_view`]，摘掉灯带通道）播放。
///
/// 特效窗口**按需创建、播完即毁**（见 [`ensure_burst_windows`]）：全屏矩形只在
/// 特效的几秒里存在，平时屏幕上只有顶部条带——「防全屏误判」的收益不受影响。
fn emit(app: &AppHandle, cfg: &GlowConfig, p: &GlowPayload) {
    remember_payload(p);
    let is_new_trigger = p.burst != LAST_EMITTED_BURST.swap(p.burst, Ordering::Relaxed);
    let top_mode = p.sides == "top";
    let edge_payload;
    let edge_ref = if top_mode {
        edge_payload = edge_view(p);
        &edge_payload
    } else {
        p
    };
    for label in edge_labels(app) {
        if let Err(e) = app.emit_to(EventTarget::webview_window(label), GLOW_EVENT, edge_ref) {
            tracing::warn!("流光事件推送失败: {e}");
        }
    }
    if !top_mode {
        return;
    }
    // 顶部模式的特效窗口：只有**新的一次触发**才按需建窗（同步 / 巡检 / 预览
    // 恢复这类序号不变的重推只补发不建窗——前端按序号判重，本来也不会重放）。
    // 刚建好的窗口页面还没加载完，此刻 emit 的事件会丢：payload 已由
    // initialization_script 注入且注入即本次触发，新窗口不再补发
    // （与 [`push_or_create`] 懒创建边缘窗口的口径一致）。
    // 但**建窗后到页面就绪之间**到达的后续推送（连点预览第二下就落在这段
    // 真空期）不能丢——由 [`create`] 的页面就绪补发捡回来（见 [`resend_latest`]）。
    let is_trigger = is_new_trigger && p.fullscreen && p.state != GlowState::Idle;
    let fresh = if is_trigger {
        ensure_burst_windows(app, cfg, p)
    } else {
        Vec::new()
    };
    let burst = burst_labels(app);
    if burst.is_empty() {
        return;
    }
    if is_trigger {
        schedule_burst_destroy(app, p);
    }
    let burst_payload = burst_view(p);
    for label in burst {
        if fresh.iter().any(|f| f == &label) {
            continue;
        }
        if let Err(e) = app.emit_to(EventTarget::webview_window(label), GLOW_EVENT, &burst_payload) {
            tracing::warn!("流光事件推送失败: {e}");
        }
    }
}

/// 确保全屏特效窗口存在（顶部模式专用，仅主线程）：每块目标显示器一个窗口，
/// 矩形按**这次特效的类型**定形（见 [`burst_rect_of`]），注入的初始态就是这次
/// 的 payload（建窗即本次触发，允许直接播放）。
///
/// 与 `align` 不同，这里不做数量校正：特效窗口生命周期只有几秒，期间显示器
/// 变化由下一次触发自然重建。返回**本次新建**的窗口 label（调用方跳过对它们的
/// 补发）；已有窗口直接复用（连发两次特效 = 复用窗口，按新序号重放动画），
/// 复用时按当前类型的区域就地校正位置尺寸——两次特效类型不同、区域不同也能
/// 无缝接管（等值校正无视觉变化，与 `align` 的差量对齐同一套写法）。
fn ensure_burst_windows(app: &AppHandle, cfg: &GlowConfig, p: &GlowPayload) -> Vec<String> {
    let rects = monitor_rects_with(app, cfg, |m| burst_rect_of(m, &p.fullscreen_effect));
    if rects.is_empty() {
        return Vec::new();
    }
    let init = burst_view(p);
    let mut fresh = Vec::new();
    for (i, r) in rects.iter().enumerate() {
        let label = format!("{BURST_PREFIX}{i}");
        match app.get_webview_window(&label) {
            Some(w) => {
                let (x, y, w_, h_) = *r;
                let _ = w.set_position(tauri::LogicalPosition::new(x, y));
                let _ = w.set_size(tauri::LogicalSize::new(w_, h_));
            }
            None => {
                if let Err(e) = create(app, &label, *r, &init, true) {
                    tracing::warn!(label = %label, "全屏特效窗口创建失败: {e}");
                    continue;
                }
                fresh.push(label);
            }
        }
    }
    fresh
}

/// 特效窗口「播完即毁」的定时器：`burst_ms` + 渐隐缓冲后，若期间没有更新的
/// 一次特效（序号对不上即作废——与预览恢复定时器的认领模式同一套路），整组销毁。
/// 连发特效时窗口被复用，只有最后一次触发的定时器真正动手。
fn schedule_burst_destroy(app: &AppHandle, p: &GlowPayload) {
    let seq = p.burst;
    let ms = u64::from(p.burst_ms) + BURST_DESTROY_BUFFER_MS;
    let app = app.clone();
    // 必须用 tauri::async_runtime::spawn 而不是 tokio::spawn：本函数在 emit 的
    // 主线程调用链上，主线程不在 tokio 运行时上下文里，tokio::spawn 会当场
    // panic（"must be called from the context of a Tokio 1.x runtime"）并穿透
    // 事件循环把整个进程带崩（2026-09-25 实测：点预览即闪退）。
    // async_runtime 持有全局运行时句柄，任意线程都能安全派发。
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        let _ = app.clone().run_on_main_thread(move || {
            if current_burst() == seq {
                destroy_burst_windows(&app);
            }
        });
    });
}

/// 边缘（灯带）窗口 label：`glow-{数字}`。特效窗口（`glow-burst-*`）虽以
/// `glow-` 开头，但后缀不是纯数字，不会混进来。
fn edge_labels(app: &AppHandle) -> Vec<String> {
    app.webview_windows()
        .keys()
        .filter(|l| edge_index(l).is_some())
        .cloned()
        .collect()
}

/// 全屏特效窗口 label：`glow-burst-{数字}`
fn burst_labels(app: &AppHandle) -> Vec<String> {
    app.webview_windows()
        .keys()
        .filter(|l| l.starts_with(BURST_PREFIX))
        .cloned()
        .collect()
}

/// `glow-0` → `Some(0)`；特效窗口 / 其它带 `glow-` 前缀的 label → `None`
fn edge_index(label: &str) -> Option<usize> {
    label.strip_prefix(GLOW_PREFIX)?.parse().ok()
}

/// 边缘窗口视角的 payload：摘掉 fullscreen 通道。顶部模式的条带窗口不负责
/// 全屏特效（#burst 永远不会 `.on`），特效由独立的整屏窗口播放。
fn edge_view(p: &GlowPayload) -> GlowPayload {
    let mut v = p.clone();
    v.fullscreen = false;
    v
}

/// 全屏特效窗口视角的 payload：摘掉灯带通道（`edge=false` → 前端按
/// `data-edge="off"` 隐藏灯带层），`sides` 恒为 "all"——特效窗口不吃「顶部」
/// 的横条形态与线宽换算口径，窗口区域按特效类型定形（见 [`burst_rect_of`]），
/// 雾散/扫描保持完整表现。
fn burst_view(p: &GlowPayload) -> GlowPayload {
    let mut v = p.clone();
    v.edge = false;
    v.sides = "all".to_string();
    v
}

/// 按窗口角色挑一份 payload 的下发视角（补发用；emit 的拆分口径同款）：
/// 边缘窗口拿 [`edge_view`]（顶部模式）或原样（四周），特效窗口拿 [`burst_view`]。
fn view_for(label: &str, p: &GlowPayload) -> GlowPayload {
    if edge_index(label).is_some() {
        if p.sides == "top" {
            edge_view(p)
        } else {
            p.clone()
        }
    } else {
        burst_view(p)
    }
}

fn remember_payload(p: &GlowPayload) {
    *lock(&LAST_PAYLOAD) = Some(p.clone());
}

fn last_payload() -> Option<GlowPayload> {
    lock(&LAST_PAYLOAD).clone()
}

/// 页面就绪补发（主线程）：把**当前最新**的 payload 按窗口角色重发给一个窗口。
///
/// 窗口刚建好时页面还在加载，此前的 emit 会丢（前端 `listen` 未注册）——
/// 补发就是把这些推送捡回来。发的是补发时刻的最新值，绝不会拿建窗时的旧值
/// 覆盖更新的状态；窗口已销毁则静默跳过（特效窗口「播完即毁」是常态）。
fn resend_latest(app: &AppHandle, label: &str) {
    let Some(p) = last_payload() else { return };
    if app.get_webview_window(label).is_none() {
        return;
    }
    let view = view_for(label, &p);
    if let Err(e) = app.emit_to(EventTarget::webview_window(label.to_string()), GLOW_EVENT, &view) {
        tracing::warn!(label = %label, "流光事件补发失败: {e}");
    }
}

/// 把覆盖窗重新按到最上层——压住任务栏，让**屏幕底部**那条光带可见。
///
/// ## 为什么需要它
/// 本层是全屏覆盖窗，而**只有窗口矩形与显示器矩形完全重合**时，系统才会按
/// 「无边框全屏应用」对待它、让任务栏退位——那个身份正是我们主动放弃的
/// （内缩 1px 的误判修复，见 [`rect_of`]）。身份一没，任务栏就回到自己的最上层，
/// 把屏幕底部那条光带整个盖住：底部 5px 边条 + 向内约 60px 的辉光都落在
/// Win11 任务栏那 ~48px 的条带里，肉眼就是「下方没有光效了」。上/左/右三边
/// 没有被盖的东西，所以症状只在下方——这正是任务栏的指纹。
///
/// ## 为什么不能只设一次
/// 任务栏会在自己刷新时（被点击、通知闪烁、托盘变化）重新升到最上层；而 tao 的
/// `set_always_on_top` 只在标志位**变化**时才调 `SetWindowPos`
/// （`WindowState::apply_diff` 在 diff 为空时直接早退），所以重复设置 `true`
/// 是 no-op——必须 `false → true` 切一次才真的重新置顶。
///
/// 这一对调用是纯 z 序操作（`SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE`）：
/// 不动位置尺寸、不抢焦点；中间那一瞬的「非置顶」不可见（窗口内容一帧都没变）。
/// 期间任务栏、开始菜单、通知中心**照常可用**——本层点击穿透且不可聚焦。
fn raise(app: &AppHandle) {
    for label in all_glow_labels(app) {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.set_always_on_top(false);
            let _ = w.set_always_on_top(true);
        }
    }
}

/// 周期性把覆盖窗保持在任务栏之上（内部派发到主线程）。
///
/// 只在**灯常驻亮着**（思考 / 等待）时才动手：
/// - 空闲：整层全透明，抢层级没有任何视觉收益，只会无谓地压在开始菜单、
///   通知中心这类系统面板之上；
/// - 终态（绿/红）：几秒后由前端按 `hold_ms` 淡出，窗层随之看不见；它刚亮起的
///   那几秒由 [`push_or_create`] 里那一次置顶覆盖，不需要按周期续保。
///
/// 调用方是 `lib.rs` 的 5s 定时器（任务栏随时可能重新升上来，周期要够短才不被察觉）。
pub fn keep_topmost(app: &AppHandle, state: &Arc<AppState>) {
    let cur = *lock(&state.glow);
    if cur == GlowState::Idle || cur.is_terminal() {
        return;
    }
    let app = app.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || raise(&app)) {
        tracing::warn!("流光置顶失败（主线程不可用？）: {e}");
    }
}

/// 销毁所有 glow 窗口（关闭开关 / 退出 / 模式切换重建前调用，主线程）。
/// 灯带窗口与全屏特效窗口一起收——特效窗口本就短命，提前收掉也无妨，
/// 挂着的回收定时器随后会因找不到窗口而空转一次。
pub fn destroy_all(app: &AppHandle) {
    destroy_edge_windows(app);
    destroy_burst_windows(app);
}

/// 只销毁灯带窗口
fn destroy_edge_windows(app: &AppHandle) {
    for label in edge_labels(app) {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.destroy();
        }
    }
}

/// 只销毁全屏特效窗口（「播完即毁」的回收路径用；灯带窗口不受影响）
fn destroy_burst_windows(app: &AppHandle) {
    for label in burst_labels(app) {
        if let Some(w) = app.get_webview_window(&label) {
            let _ = w.destroy();
        }
    }
}

/// 灯带窗口 + 全屏特效窗口的全部 label
fn all_glow_labels(app: &AppHandle) -> Vec<String> {
    app.webview_windows()
        .keys()
        .filter(|l| l.starts_with(GLOW_PREFIX))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// 窗口创建（主线程）
// ---------------------------------------------------------------------------

/// 按当前显示器布局创建全套窗口。
///
/// `replay_burst`：本次建窗是不是**一次颜色触发的产物**。true（走 [`dispatch_preview`] /
/// [`apply_inner`]）时注入的初始态就是刚点亮的那次特效，允许前端放一次全屏雾散；false（走 [`align`]
/// 的重建）时注入的只是「现在长什么样」的快照——payload 里的 `burst` 是上一次触发的
/// 旧序号，而新页面的 `lastBurst` 初值是 0，不设防的话热插拔显示器 / 改「生效显示器」
/// 都会白闪一次整屏（注释里说的「不 emit 以免重放全屏特效」只挡住了 emit，
/// 挡不住 initialization_script 这条注入路径）。见 `glow.ts` 的 `__GLOW_INIT_SNAPSHOT__`。
fn create_all(app: &AppHandle, cfg: &GlowConfig, init: &GlowPayload, replay_burst: bool) {
    let rects = monitor_rects(app, cfg);
    if rects.is_empty() {
        tracing::warn!("未检测到显示器，跳过流光窗口创建");
        return;
    }
    for (i, r) in rects.iter().enumerate() {
        let label = format!("{GLOW_PREFIX}{i}");
        if let Err(e) = create(app, &label, *r, init, replay_burst) {
            tracing::warn!(label = %label, "流光窗口创建失败: {e}");
        }
    }
    // 记下这批窗口的「位置」形态，align 靠它识别模式切换（见 BUILT_TOP_ONLY）
    BUILT_TOP_ONLY.store(cfg.edge_sides() == "top", Ordering::Relaxed);
}

/// 校正窗口数量与位置（显示器插拔、主屏↔全部切换、位置模式切换后调用）。
/// 数量不符，或「位置=顶部/四周」形态与现存窗口不符 → 全量重建
/// （模式切换不能就地缩放，见 [`BUILT_TOP_ONLY`]）；数量相符 → 只校正位置尺寸
/// （分辨率/DPI 可能变了）。
fn align(app: &AppHandle, cfg: &GlowConfig, init: &GlowPayload) {
    // init 就是「当前长什么样」的最新快照，先记下来：巡检（maintain）走这条路
    // 只重建不 emit，不记住的话页面就绪补发会拿更旧的 payload 把新窗口刚显示的
    // 状态盖回去（比如一份旧的 Idle 把重建出来的灯条掐灭）。
    remember_payload(init);
    let rects = monitor_rects(app, cfg);
    if rects.is_empty() {
        return;
    }
    let mut existing = edge_labels(app);
    // 按数字后缀排序：字典序会让 glow-10 排在 glow-2 前，>9 块屏时
    // 窗口与显示器按位 zip 会错位映射
    existing.sort_by_key(|l| {
        l.trim_start_matches(GLOW_PREFIX).parse::<usize>().unwrap_or(usize::MAX)
    });
    let top_only = cfg.edge_sides() == "top";
    if existing.len() != rects.len() || BUILT_TOP_ONLY.load(Ordering::Relaxed) != top_only {
        destroy_all(app);
        // 重建不是一次颜色触发：快照照常显示（还在跑的会话必须看得见），
        // 但不许把上次的全屏雾散重放一遍。
        create_all(app, cfg, init, false);
        return;
    }
    for (label, r) in existing.iter().zip(rects.iter()) {
        let Some(w) = app.get_webview_window(label) else { continue };
        let (x, y, w_, h_) = *r;
        let _ = w.set_position(tauri::LogicalPosition::new(x, y));
        let _ = w.set_size(tauri::LogicalSize::new(w_, h_));
    }
}

// `dwmapi!DwmSetWindowAttribute` —— 只用到这一个函数，故手写 extern 声明而不为它引整套
// windows crate。签名与 dwmapi 导出一致；属性值按 DWORD 传入。
// 只在 Win11 起效，更早的系统调用会失败——预期内，忽略即可。
#[cfg(windows)]
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut core::ffi::c_void,
        attribute: u32,
        value: *const core::ffi::c_void,
        size: u32,
    ) -> i32;
}

/// 明确告诉 DWM：**不要**给这个窗口加圆角。
///
/// DWM 会给顶层窗口加圆角（`DWMWA_WINDOW_CORNER_PREFERENCE`），被裁掉的是**窗口矩形**的
/// 角。那块像素不在窗口被绘制的那部分里——前端画什么都盖不住，露出来的是壁纸：浅色壁纸下
/// 就是屏幕角上的白色小尖角（实测形态：半径约 2 CSS px 的四分之一圆，尖点在屏幕角上）。
///
/// 系统本来不会给「最大化 / 贴满屏」的窗口加圆角，但 [`rect_of`] 为了让窗口矩形不等于显示器
/// 矩形，刻意在底边多留了 1px——这层"多出来的部分"正是最容易让系统改判、把圆角加回来的地方
/// （同样的问题在四边内缩那版上实测出现过）。与其去猜系统的脾气，不如直接声明一次：这不是
/// 美化，是"别裁我的角"。
///
/// `DWMWCP_DONOTROUND = 1`（0 = 交给系统，1 = 绝不圆角，2 = 圆角，3 = 小圆角）。失败不影响
/// 光效，不向上报错。
#[cfg(windows)]
fn no_round_corners(win: &tauri::WebviewWindow) {
    // DWMWINDOWATTRIBUTE::DWMWA_WINDOW_CORNER_PREFERENCE
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    // DWM_WINDOW_CORNER_PREFERENCE::DWMWCP_DONOTROUND
    const DWMWCP_DONOTROUND: u32 = 1;

    let Ok(hwnd) = win.hwnd() else {
        tracing::warn!("取窗口句柄失败，跳过 DWM 圆角声明");
        return;
    };
    let value = DWMWCP_DONOTROUND;
    unsafe {
        DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    }
}

fn create(
    app: &AppHandle,
    label: &str,
    rect: (f64, f64, f64, f64),
    init: &GlowPayload,
    replay_burst: bool,
) -> tauri::Result<()> {
    let (x, y, w, h) = rect;
    // 初始态经 initialization_script 注入：窗口建好时页面还没加载完，此刻 emit
    // 的事件会丢，而 WebView 一起来又必须有颜色，否则会闪一下兜底色。
    let init_json = serde_json::to_string(init).unwrap_or_else(|_| "null".to_string());
    // 重建（align）注入的是快照：标记一下，让前端别把这次注入当成新触发（见 glow.ts）
    let init_flag = if replay_burst { "" } else { "window.__GLOW_INIT_SNAPSHOT__=true;" };
    // 页面就绪补发（见 RESEND_DELAYS_MS / resend_latest）：窗口建好到前端
    // `listen` 注册完之前，emit 的事件会丢。页面加载完就把最新 payload 补发
    // 过去——连点预览第二下「毫无反应」的根因正是这段真空期。
    let load_app = app.clone();
    let load_label = label.to_string();
    let win = WebviewWindowBuilder::new(app, label, WebviewUrl::App("glow.html".into()))
        .title("agent-bark 边缘流光")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .closable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        // 切虚拟桌面 / 全屏应用时仍然可见（macOS 起效，Windows 无害）
        .visible_on_all_workspaces(true)
        // 不可聚焦：绝不能把焦点从用户的编辑器里抢走
        .focusable(false)
        .disable_drag_drop_handler()
        .position(x, y)
        .inner_size(w, h)
        .initialization_script(format!("window.__GLOW_INIT__={init_json};{init_flag}"))
        .on_page_load(move |_, event| {
            if !matches!(event.event(), tauri::webview::PageLoadEvent::Finished) {
                return;
            }
            let app = load_app.clone();
            let label = load_label.clone();
            tauri::async_runtime::spawn(async move {
                for delay in RESEND_DELAYS_MS {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    let app2 = app.clone();
                    let label2 = label.clone();
                    let _ = app.clone().run_on_main_thread(move || resend_latest(&app2, &label2));
                }
            });
        })
        .build()?;
    // 点击穿透：所有鼠标事件落到下层窗口（WS_EX_TRANSPARENT / macOS ignoresMouseEvents）。
    // 失败必须销毁窗口：留下的将是一层可聚焦、可拦截点击的全屏置顶透明窗，
    // 比没有光效糟得多。
    if let Err(e) = win.set_ignore_cursor_events(true) {
        let _ = win.destroy();
        return Err(e);
    }
    // 声明「别裁我的角」（Windows 专有；其它平台没有 DWM 这套，什么都不做）
    #[cfg(windows)]
    no_round_corners(&win);
    tracing::info!(label = %label, "流光窗口已创建");
    Ok(())
}

// ---------------------------------------------------------------------------
// 显示器枚举（「屏幕光效」页的「生效显示器」下拉）
// ---------------------------------------------------------------------------

/// 一个显示器在 UI 里需要的信息。
///
/// `index` 就是配置 `glow.monitors` 里存的值：`available_monitors()` 的下标。
/// 不用设备名当标识——Windows 的设备名（`\\.\DISPLAY1`）在换接口/换显卡后会变，
/// 而用户在下拉里选的就是「第几块屏」。
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    /// 友好名，如「显示器1」
    pub name: String,
    /// 物理分辨率（宽）
    pub width: u32,
    /// 物理分辨率（高）
    pub height: u32,
    /// 缩放比例；非 1.0 时 UI 会补一句「@150%」，避免用户把 4K@150% 的
    /// 逻辑分辨率当成另一块屏
    pub scale_factor: f64,
    pub is_primary: bool,
    /// 虚拟桌面里的左上角坐标（仅展示，不参与计算）
    pub x: i32,
    pub y: i32,
}

/// 枚举所有显示器（UI 用）。
///
/// 会阻塞在事件循环上（tauri 的 `available_monitors` 内部要把调用派发到主线程），
/// 因此调用方必须放在 `spawn_blocking` 里。
pub fn list_monitors(app: &AppHandle) -> Vec<MonitorInfo> {
    // 主显示器只认位置：Monitor 没有暴露可比较的句柄，而两块屏不可能同坐标
    let primary_pos = app.primary_monitor().ok().flatten().map(|m| *m.position());
    let monitors = match app.available_monitors() {
        Ok(ms) => ms,
        Err(e) => {
            tracing::warn!("枚举显示器失败: {e}");
            return Vec::new();
        }
    };
    monitors
        .iter()
        .enumerate()
        .map(|(index, m)| {
            let size = m.size();
            let pos = m.position();
            MonitorInfo {
                index,
                name: friendly_name(m.name(), index),
                width: size.width,
                height: size.height,
                scale_factor: m.scale_factor(),
                is_primary: primary_pos == Some(*pos),
                x: pos.x,
                y: pos.y,
            }
        })
        .collect()
}

/// 设备名 → 人话：Windows 的 `\\.\DISPLAY3` 取尾部数字变成「显示器3」，
/// 其它平台（macOS 的 "Built-in Retina Display"、Linux 的 "eDP-1"）能读就读原名。
/// 完全没有名字时用下标兜底，保证下拉里不出现空白项。
fn friendly_name(raw: Option<&String>, index: usize) -> String {
    let Some(raw) = raw.map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return format!("显示器{}", index + 1);
    };
    let tail: String = raw.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    if tail.is_empty() {
        raw.to_string()
    } else {
        format!("显示器{}", tail.chars().rev().collect::<String>())
    }
}

/// 目标显示器的逻辑坐标矩形 (x, y, w, h)——按配置的「位置」决定形态
/// （顶部 = 条带，见 [`rect_of`]；四周 = 整屏）
fn monitor_rects(app: &AppHandle, cfg: &GlowConfig) -> Vec<(f64, f64, f64, f64)> {
    monitor_rects_with(app, cfg, |m| rect_of(m, cfg.edge_sides() == "top"))
}

/// 同上，但每个显示器的矩形由 `rect_of_fn` 决定：全屏特效窗口按**特效类型**
/// 传入自己的区域形态（见 [`burst_rect_of`]）。
fn monitor_rects_with(
    app: &AppHandle,
    cfg: &GlowConfig,
    rect_of_fn: impl Fn(&Monitor) -> (f64, f64, f64, f64),
) -> Vec<(f64, f64, f64, f64)> {
    match cfg.monitor_target() {
        MonitorTarget::All => match app.available_monitors() {
            Ok(ms) if !ms.is_empty() => return ms.iter().map(|m| rect_of_fn(m)).collect(),
            Ok(_) => tracing::warn!("未枚举到任何显示器，回退主显示器"),
            Err(e) => tracing::warn!("枚举显示器失败，回退主显示器: {e}"),
        },
        MonitorTarget::Index(i) => match app.available_monitors() {
            Ok(ms) => match ms.get(i) {
                Some(m) => return vec![rect_of_fn(m)],
                // 下标越界（拔掉了那块屏，或配置是从别的机器拷来的）：
                // 回退主显示器，而不是一个窗口都不建——后者在用户眼里就是「流光坏了」
                None => tracing::warn!(index = i, count = ms.len(), "生效显示器下标越界，回退主显示器"),
            },
            Err(e) => tracing::warn!("枚举显示器失败，回退主显示器: {e}"),
        },
        MonitorTarget::Primary => {}
    }
    match app.primary_monitor() {
        Ok(Some(m)) => vec![rect_of_fn(&m)],
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!("获取主显示器失败: {e}");
            Vec::new()
        }
    }
}

/// 目标显示器的逻辑坐标矩形 (x, y, w, h)：四边与显示器对齐，**只有底边向外多 1 物理像素**。
///
/// 物理像素 → 逻辑像素（Tauri 的 position / inner_size 用逻辑像素，
/// 而 Monitor 报告的是物理像素；4K 屏上不换算会只有左上角 1/4 有光带）
///
/// 已知局限（刻意不改，需多屏不同缩放的真机验证后再动）：这里把每块屏的
/// 物理坐标除以**它自己的** scale_factor；Tauri/Windows 的逻辑坐标系并非按
/// 各屏自身缩放定义（混合 DPI 下统一到主屏口径），主屏 150% + 副屏 100% 时
/// 副屏窗口可能错位。单屏 / 同缩放场景恒正确。
///
/// ## 底边为什么多 1px
/// 窗口矩形与显示器矩形**完全重合**时，游戏检测（GeForce Experience、Windows 游戏模式、
/// 壁纸引擎的 Smart Pause 等）会按「无边框全屏应用」矩形匹配把它认成游戏 / 全屏程序，
/// 触发游戏覆盖层提示、自动暂停等连锁 bug。矩形只要不完全重合，这类匹配就落空。
///
/// 关键是**往外扩**、而且**只动底边**：
/// - 往外（而不是像早先那版四边各内缩 1px）：窗口把屏幕完整包住，屏沿不会留下一条没被
///   覆盖的 1px。内缩那版在浅色壁纸上会露出一条亮线——那圈像素不在窗口里，前端画什么都
///   盖不住；往外扩时多出来的部分落在屏幕外，本来就不会显示。
/// - 仍然**覆盖**整块显示器（不是比屏幕小）：Shell 判「无边框全屏应用」看的是「窗口是否
///   遮挡整个桌面」，覆盖住才能让任务栏自然退位（见 [`raise`]）。内缩会把这条身份也一起
///   丢掉，正是那版换来「任务栏盖住底部光带」的原因。
/// - 只动底边：代价最小。四边都扩会和相邻显示器各重叠 1px；只有底边扩时最多碰到下方那块
///   屏，而配合 `glow.html` 里 `#glow` 的 `inset: 0 0 1px 0`（绘制层同侧内缩 1px），屏外
///   那 1px 里没有任何像素，下屏完全无感。
///
/// 偏移量按**物理**像素算（`1.0 / s`）：1 逻辑像素在 150% 屏上是 1.5 物理像素，取整后会在
/// 1~2px 之间抖，而这里要的正是"恰好 1 个物理像素"这个最小值。
///
/// ## `top_only`：位置=顶部 的条带形态
/// 「位置=顶部」时窗口只保留屏幕顶部一条 [`GLOW_TOP_STRIP_H`] 高的带子（顶左与
/// 显示器顶左对齐、宽度整屏，底边不再外扩——矩形本来就远不等于显示器矩形，
/// 那个 1px 把戏只服务于「覆盖整屏但不重合」这组约束）。窗口矩形只盖住
/// 屏顶一小条（150px ≈ 1080p 屏高的一成多），「无边框全屏应用」的一切矩形/覆盖率匹配（Windows 请勿打扰的
/// 全屏判定、GeForce 游戏检测等）都落空——这正是该模式的动机。绘制侧是
/// 独立的顶部横条渲染（glow.html 按 `data-sides="top"` 重绘成贴顶边的一条
/// 线，不是四周渲染的裁剪），条带高度口径见 [`GLOW_TOP_STRIP_H`]。
fn rect_of(m: &Monitor, top_only: bool) -> (f64, f64, f64, f64) {
    let s = scale_or_one(m.scale_factor());
    let p = m.position();
    let sz = m.size();
    if top_only {
        return (
            p.x as f64 / s,
            p.y as f64 / s,
            sz.width as f64 / s,
            GLOW_TOP_STRIP_H,
        );
    }
    (
        p.x as f64 / s,
        p.y as f64 / s,
        sz.width as f64 / s,
        sz.height as f64 / s + 1.0 / s,
    )
}

/// 全屏特效各类型的画布形态：**每种特效类型一个自己的窗口区域**。
///
/// 新增特效类型的完整扩展点（四处，缺一不可）：
/// 1. `config.rs` 的 `canon_burst_effect` / `fullscreen_effect_kind`（合法 id）；
/// 2. `app/src/api.ts` 的 `BURST_EFFECTS`（设置页下拉）；
/// 3. `app/glow.html` 的 `#burst[data-effect=…]`（动画表现）；
/// 4. 这里（窗口区域）。
///
/// 当前全部类型（雾散 / 扫描）＝ 整屏、**顶部内缩 1 物理像素**：窗口矩形与
/// 显示器矩形不再重合，与边缘窗口「底边外扩 1px」同防「矩形完全重合」式的
/// 全屏误判（对按覆盖率判定的检测器无效，见 [`rect_of`] 的说明）；窗口完全
/// 落在显示器内，与相邻显示器零重叠。顶部空出的 1px 由常驻的边缘窗口盖着
/// （两种位置模式都从屏幕顶边起算），不会露壁纸亮线。底边与屏底对齐——
/// `#glow` 的绘制内缩会少画屏底 1 像素，渐变尾部的 1px 差异不可感知，
/// 不为它让窗口越过显示器底边。
fn burst_rect_of(m: &Monitor, effect: &str) -> (f64, f64, f64, f64) {
    match effect {
        // 雾散 / 扫描：当前同一种形态。某类型要专属区域时单独成臂
        "fog" | "scan" => rect_top_inset(m),
        // 未识别 / 未来新增的类型：先按当前标准形态兜底
        _ => rect_top_inset(m),
    }
}

/// 整屏、顶部内缩 1 物理像素（`1.0 / s`，与 [`rect_of`] 的底边外扩同一换算口径：
/// 1 逻辑像素在 150% 屏上是 1.5 物理像素，按物理像素算才不抖）
fn rect_top_inset(m: &Monitor) -> (f64, f64, f64, f64) {
    let s = scale_or_one(m.scale_factor());
    let p = m.position();
    let sz = m.size();
    (
        p.x as f64 / s,
        p.y as f64 / s + 1.0 / s,
        sz.width as f64 / s,
        sz.height as f64 / s - 1.0 / s,
    )
}

/// scale_factor 防御（§2.18）：物理 → 逻辑换算的统一除数归一。
/// 0 / 负数 / 非有限值会让换算产出 NaN/Inf 坐标（窗口落进打不开的位置、
/// 夹取运算产出 NaN），一律当 1.0 处理。widget.rs / tray_menu.rs 的换算同口径。
pub(crate) fn scale_or_one(s: f64) -> f64 {
    if s.is_finite() && s > 0.0 {
        s
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁配色：四个角色 ↔ 四个色值。
    ///
    /// 本测试是**唯一**允许写死 hex 的地方（连同 `GlowState::color`）。改配色时
    /// 只改这两处，其余全仓库用角色名（思考 / 等待 / 完成 / 失败）。故意让改色
    /// 必须动测试：色值悄悄漂移过一版（黄 → 蓝），当时就是靠人眼发现漏改的。
    #[test]
    fn palette_is_locked() {
        let palette = [
            (GlowState::Running, "思考", "#4493f8"),
            (GlowState::Waiting, "等待", "#f0883e"),
            (GlowState::Completed, "完成", "#3fb950"),
            (GlowState::Failed, "失败", "#f85149"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (state, role, hex) in palette {
            assert_eq!(state.color(), hex, "{role}颜色变了：同步 color() 的注释与本测试");
            assert!(seen.insert(hex), "四个角色的色值不能重复");
            assert!(state.color().starts_with('#'));
        }
        // Idle 不显示，允许复用思考色（少一个分支）
        assert_eq!(GlowState::Idle.color(), GlowState::Running.color());
    }

    #[test]
    fn only_terminal_states_hold() {
        // 思考中 / 等待中必须常驻：靠后续事件驱散，不能自己熄灭
        assert_eq!(GlowState::Running.hold_ms(), 0);
        assert_eq!(GlowState::Waiting.hold_ms(), 0);
        assert_eq!(GlowState::Completed.hold_ms(), GLOW_COMPLETED_HOLD_MS);
        assert_eq!(GlowState::Failed.hold_ms(), GLOW_FAILED_HOLD_MS);
        assert!(GlowState::Completed.is_terminal());
        assert!(!GlowState::Running.is_terminal());
    }

    #[test]
    fn payload_carries_sanitized_config() {
        let cfg =
            GlowConfig { effect: "  ".into(), fullscreen_effect: "rain".into(), edge: false, ..Default::default() }
                .sanitize();
        let p = payload(&cfg, GlowState::Running);
        // 类型归一：未知值回默认，覆盖层永远拿得到认识的 id
        assert_eq!(p.effect, "breathing");
        assert_eq!(p.fullscreen_effect, "fog");
        // 边缘开关独立进 payload；全屏默认开
        assert!(!p.edge);
        assert!(p.fullscreen);
        assert_eq!(p.color, GlowState::Running.color());
        // 观感参数写死下发（"1.00" 为两位小数格式，与旧 speed_css 口径一致）
        assert_eq!(p.speed, GLOW_SPEED_CSS);
        assert_eq!(p.intensity, GLOW_INTENSITY_CSS);
        // 位置：默认配置下发「顶部」
        assert_eq!(p.sides, "top");
        assert_eq!(p.width, GLOW_WIDTH);
        assert_eq!(p.radius, GLOW_RADIUS);
        assert_eq!(p.opacity, GLOW_OPACITY);
        assert_eq!(p.burst_ms, GLOW_BURST_MS);
        // sanitize 不得改动显式给出的开关
        let cfg = GlowConfig { edge: true, fullscreen: false, ..Default::default() }.sanitize();
        let p = payload(&cfg, GlowState::Running);
        assert!(p.edge);
        assert!(!p.fullscreen);
        // 位置="all"（四周）原样下发；乱值经 edge_sides 归一成顶部
        let cfg = GlowConfig { edge_position: "all".into(), ..Default::default() };
        assert_eq!(payload(&cfg, GlowState::Running).sides, "all");
        let cfg = GlowConfig { edge_position: "bottom".into(), ..Default::default() };
        assert_eq!(payload(&cfg, GlowState::Running).sides, "top");
    }

    #[test]
    fn payload_views_split_edge_and_burst_channels() {
        // 顶部模式分窗下发：边缘窗口摘 fullscreen（特效不进条带），
        // 特效窗口摘 edge（不画灯带）且恒整屏（不吃顶部横条形态）
        let cfg = GlowConfig { edge_position: "top".into(), ..Default::default() }.sanitize();
        let p = payload(&cfg, GlowState::Completed);
        assert!(p.edge);
        assert!(p.fullscreen);
        let e = edge_view(&p);
        assert!(!e.fullscreen, "条带窗口不背全屏特效");
        assert!(e.edge);
        assert_eq!(e.sides, "top");
        let b = burst_view(&p);
        assert!(!b.edge, "特效窗口不画灯带");
        assert_eq!(b.sides, "all", "特效窗口永远整屏");
        assert!(b.fullscreen, "特效通道原样保留");
        // 两个视图互不越界：状态、颜色、序号原样带回
        assert_eq!(b.state, p.state);
        assert_eq!(b.burst, p.burst);
        assert_eq!(b.burst_color, p.burst_color);
    }

    #[test]
    fn edge_index_distinguishes_edge_and_burst_labels() {
        assert_eq!(edge_index("glow-0"), Some(0));
        assert_eq!(edge_index("glow-10"), Some(10));
        // 特效窗口虽以 glow- 开头，但不是灯带窗口——emit / 对齐 / 回收都要分开
        assert_eq!(edge_index("glow-burst-0"), None);
        assert_eq!(edge_index("glow-x"), None);
        assert_eq!(edge_index("widget"), None);
    }

    #[test]
    fn restart_edge_defaults_off_and_is_preview_only() {
        // 灯带「熄灭重放」只属于设置页预览点按（dispatch_preview 手动置 true）：
        // 真实事件要平滑换色、burst_only 要「边缘不动」、补发是幂等重推
        let cfg = GlowConfig::default();
        assert!(!payload(&cfg, GlowState::Completed).restart_edge);
        assert!(!payload_with_burst(&cfg, GlowState::Running, GlowState::Completed).restart_edge);
    }

    #[test]
    fn view_for_splits_by_window_role() {
        // 页面就绪补发按窗口角色拆视角，与 emit 的口径必须一致
        let cfg = GlowConfig { edge_position: "top".into(), ..Default::default() }.sanitize();
        let mut p = payload(&cfg, GlowState::Completed);
        p.restart_edge = true; // 模拟预览点按
        let e = view_for("glow-0", &p);
        assert!(!e.fullscreen, "条带窗口不背全屏特效");
        assert!(e.restart_edge, "熄灭重放标记要带给条带窗口");
        let b = view_for("glow-burst-0", &p);
        assert!(!b.edge, "特效窗口不画灯带");
        assert_eq!(b.sides, "all");
        // 四周模式：边缘窗口就是特效画布，原样下发
        let cfg = GlowConfig { edge_position: "all".into(), ..Default::default() };
        let p = payload(&cfg, GlowState::Completed);
        let e = view_for("glow-0", &p);
        assert!(e.fullscreen, "四周模式特效就在边缘窗口里放");
        assert_eq!(e.sides, "all");
    }

    #[test]
    fn burst_seq_is_monotonic_and_first_trigger_is_one() {
        // 每次触发必须拿到**不同**的序号，否则前端会把它当成同一状态的重发而不补放特效。
        // 序号从 1 起（0 保留给「尚无触发」的初始 payload，见 GlowPayload::burst）
        let a = bump_burst();
        assert_eq!(a, 1, "首次触发序号是 1（0 = 尚无触发）");
        let b = bump_burst();
        assert!(b > a);
        assert_eq!(current_burst(), b);
    }

    #[test]
    fn repeated_previews_restore_the_real_state_not_a_previous_preview() {
        // 第一轮预览：真实状态是 Idle，记下的 prev 必须是 Idle
        let live = GlowState::Idle;
        let first = open_session(None, live, 1);
        assert_eq!(first, (1, GlowState::Idle));
        // 第二轮预览（此时流光正被第一轮预览点亮成 Running）：prev 仍然是 Idle，
        // 不能被新的 live（Running）覆盖——覆盖了就会出现「预览完灯再也灭不掉」
        let second = open_session(Some(first), GlowState::Running, 2);
        assert_eq!(second, (2, GlowState::Idle));
        // 第三轮同理
        assert_eq!(open_session(Some(second), GlowState::Waiting, 3), (3, GlowState::Idle));
    }

    #[test]
    fn stale_preview_timer_cannot_touch_the_glow() {
        let slot = Some((2u64, GlowState::Idle));
        // 当前是第 2 轮：第 1 轮的旧定时器认领失败
        assert_eq!(claim_session(slot, 1), None);
        // 真实事件/熄灭已经把会话清空：任何定时器都认领失败
        assert_eq!(claim_session(None, 2), None);
        // 轮到自己才允许恢复
        assert_eq!(claim_session(slot, 2), Some(GlowState::Idle));
    }

    #[test]
    fn preview_never_restores_a_terminal_state() {
        // 终态亮几秒就淡出，一次预览又至少占 3s：恢复目标一律空闲——否则预览播完
        // 会「补」一个绿色完成动效（用户实测报告的怪现象，见 restore_target）
        assert_eq!(restore_target(GlowState::Completed), GlowState::Idle);
        assert_eq!(restore_target(GlowState::Failed), GlowState::Idle);
        // 常驻状态照常恢复：背后多半真有会话在跑 / 在等
        assert_eq!(restore_target(GlowState::Running), GlowState::Running);
        assert_eq!(restore_target(GlowState::Waiting), GlowState::Waiting);
        assert_eq!(restore_target(GlowState::Idle), GlowState::Idle);
        // 记进槽里的就是映射后的值：终态残留不会被任何一轮预览捡回来
        let first = open_session(None, GlowState::Completed, 1);
        assert_eq!(first, (1, GlowState::Idle));
        assert_eq!(open_session(Some(first), GlowState::Waiting, 2), (2, GlowState::Idle));
    }

    #[test]
    fn friendly_name_handles_device_paths_and_unnamed() {
        let n = |s: &str, i: usize| friendly_name(Some(&s.to_string()), i);
        assert_eq!(n(r"\\.\DISPLAY1", 0), "显示器1");
        assert_eq!(n(r"\\.\DISPLAY3", 2), "显示器3");
        // 非 Windows 的可读名原样保留
        assert_eq!(n("Built-in Retina Display", 0), "Built-in Retina Display");
        // 没有名字/名字是空白：用下标兜底，下拉里不能出现空项
        assert_eq!(friendly_name(None, 1), "显示器2");
        assert_eq!(n("   ", 0), "显示器1");
    }

    #[test]
    fn payload_uses_per_state_effects_and_none_gates() {
        use bark_core::StateEffects;
        let cfg = GlowConfig {
            edge_effects: StateEffects { thinking: "none".into(), completed: "comet".into(), ..Default::default() },
            burst_effects: StateEffects { completed: "scan".into(), failed: "none".into(), ..Default::default() },
            ..Default::default()
        }
        .sanitize();
        // 思考行边缘选「无」→ 灯带不亮；全屏按思考行走默认雾散
        let p = payload(&cfg, GlowState::Running);
        assert!(!p.edge);
        assert!(p.fullscreen);
        assert_eq!(p.fullscreen_effect, "fog");
        // 完成：边缘流光 + 全屏扫描（触发角色 = 完成）
        let p = payload(&cfg, GlowState::Completed);
        assert!(p.edge);
        assert_eq!(p.effect, "comet");
        assert_eq!(p.fullscreen_effect, "scan");
        // 双通道：边缘是思考行（无），全屏触发角色是失败行（无 → 不放）
        let p = payload_with_burst(&cfg, GlowState::Running, GlowState::Failed);
        assert!(!p.edge);
        assert!(!p.fullscreen, "失败行选「无」时全屏也不放");
        assert_eq!(p.burst_color, GlowState::Failed.color());
    }

    #[test]
    fn burst_color_is_independent_of_edge_state() {
        // 双通道：多会话并行时一个完成——边缘保持思考色，全屏按事件角色（完成色）补放。
        // burst_only 走的正是这个构造：edge 状态原样带回，只有特效色是终态的。
        let cfg = GlowConfig::default();
        let p = payload_with_burst(&cfg, GlowState::Running, GlowState::Completed);
        assert_eq!(p.color, GlowState::Running.color(), "边缘色保持思考色");
        assert_eq!(p.burst_color, GlowState::Completed.color(), "全屏特效色是完成色");
        // 状态切换（表空、边缘转终态）触发的全屏仍与边缘同色
        let p = payload(&cfg, GlowState::Completed);
        assert_eq!(p.burst_color, p.color);
        // 失败通道对称：失败色全屏
        let p = payload_with_burst(&cfg, GlowState::Running, GlowState::Failed);
        assert_eq!(p.burst_color, GlowState::Failed.color());
    }

    #[test]
    fn derive_prefers_active_sessions_over_terminal_events() {
        // A 完成但 B 还在跑 → 边缘保持思考色（全屏另有完成色事件通道，见 burst_only）
        assert_eq!(derive(Some(GlowState::Running), EventKind::RunCompleted, false), Some(GlowState::Running));
        // 等待确认优先于思考：需要人去点一下
        assert_eq!(derive(Some(GlowState::Waiting), EventKind::RunFailed, false), Some(GlowState::Waiting));
        // 无活跃会话 + 终态事件 → 完成 / 失败
        assert_eq!(derive(None, EventKind::RunCompleted, false), Some(GlowState::Completed));
        assert_eq!(derive(None, EventKind::RunFailed, false), Some(GlowState::Failed));
        // 心跳超时判死 → 意外终止
        assert_eq!(derive(None, EventKind::Activity, true), Some(GlowState::Failed));
        // 会话开始这类中间事件不许掐掉刚亮起的终态
        assert_eq!(derive(None, EventKind::SessionStart, false), None);
    }

    #[test]
    fn user_abort_hides_glow_unless_other_sessions_run() {
        // 用户自己按的停止：没有别的会话在跑 → **收起流光**（不亮失败色、不亮完成色）
        assert_eq!(derive(None, EventKind::RunAborted, false), Some(GlowState::Idle));
        // 还有别的会话在跑 → 照常显示它们的状态（思考 / 等待优先）
        assert_eq!(derive(Some(GlowState::Running), EventKind::RunAborted, false), Some(GlowState::Running));
        assert_eq!(derive(Some(GlowState::Waiting), EventKind::RunAborted, false), Some(GlowState::Waiting));
        // 中止不是心跳判死，不许被 running_lost 覆盖成失败色
        assert_eq!(derive(None, EventKind::RunAborted, true), Some(GlowState::Idle));
    }

    /// §1.3 回归 + 音/光对称（对齐 state.rs 的 `sound_key_follows_event_roles`）：
    /// 用户主动中止**永远**不放失败色全屏特效，即便同一瞬间恰好有别的会话被判死
    /// （running_lost=true、判死者是另一会话）——手动中止不提醒，音/光优先级必须一致
    #[test]
    fn abort_never_bursts_even_with_running_lost() {
        assert_eq!(burst_for_event(EventKind::RunAborted, false), None);
        assert_eq!(burst_for_event(EventKind::RunAborted, true), None, "判死者是另一会话也不 burst");
        // 普通终态按事件角色补放
        assert_eq!(burst_for_event(EventKind::RunCompleted, false), Some(GlowState::Completed));
        assert_eq!(burst_for_event(EventKind::RunFailed, false), Some(GlowState::Failed));
        // 任意事件捎带的心跳判死 = 意外终止 → 失败色（与音效 "failed" 同口径）
        assert_eq!(burst_for_event(EventKind::Activity, true), Some(GlowState::Failed));
        assert_eq!(burst_for_event(EventKind::SessionStart, true), Some(GlowState::Failed));
        assert_eq!(burst_for_event(EventKind::ToolFinished, true), Some(GlowState::Failed));
        // 中性事件、不带判死：不补放
        assert_eq!(burst_for_event(EventKind::Activity, false), None);
        assert_eq!(burst_for_event(EventKind::SessionStart, false), None);
    }

    /// §2.2 回归：同态早退不得绕过禁用复位（glow 关闭时点预览 + 真实心跳同态 →
    /// 覆盖窗残留）；禁用复位判在同态早退之前；同态早退只对常驻态（终态重发要重新计时）
    #[test]
    fn disabled_reset_precedes_same_state_skip() {
        // 关闭 + 同态（旧实现这里早退 → 跳过复位与销毁，残窗没人回收）
        assert_eq!(apply_action(GlowState::Running, GlowState::Running, false), ApplyAction::ResetDisabled);
        assert_eq!(apply_action(GlowState::Idle, GlowState::Idle, false), ApplyAction::ResetDisabled);
        // 开启 + 同态常驻：早退（心跳一个回合上百条，不必条条重发）
        assert_eq!(apply_action(GlowState::Running, GlowState::Running, true), ApplyAction::Skip);
        assert_eq!(apply_action(GlowState::Waiting, GlowState::Waiting, true), ApplyAction::Skip);
        // 开启 + 同态终态：照推（前端淡出计时器要重新计时，否则第二条看不见）
        assert_eq!(apply_action(GlowState::Completed, GlowState::Completed, true), ApplyAction::Push);
        assert_eq!(apply_action(GlowState::Failed, GlowState::Failed, true), ApplyAction::Push);
        // 状态变化：照推
        assert_eq!(apply_action(GlowState::Running, GlowState::Waiting, true), ApplyAction::Push);
        assert_eq!(apply_action(GlowState::Running, GlowState::Idle, true), ApplyAction::Push);
    }

    /// §2.18 回归：scale_factor 的 0 / 负数 / 非有限值一律当 1.0（物理→逻辑换算的除数）
    #[test]
    fn scale_factor_defense_falls_back_to_one() {
        assert_eq!(scale_or_one(1.5), 1.5);
        assert_eq!(scale_or_one(0.0), 1.0, "0 不能当除数");
        assert_eq!(scale_or_one(-2.0), 1.0, "负缩放按异常处理");
        assert_eq!(scale_or_one(f64::NAN), 1.0);
        assert_eq!(scale_or_one(f64::INFINITY), 1.0);
    }
}
