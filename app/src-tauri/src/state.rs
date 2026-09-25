//! 应用共享状态与事件管道：
//! EventServer → mpsc → 规则引擎（子代理过滤 / 免打扰 / 聚合）→ 通知分发
//!   → 前端事件流 + 历史

use bark_adapters::{RegisterCtx, VerifyReport};
use bark_channels::{BarkChannel, Channel, Notification, WebhookChannel};
use bark_core::{AdapterMode, AgentKind, BarkConfig, EventKind, NormalizedEvent};
use chrono::Timelike;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

use crate::glow::GlowState;

pub const HISTORY_CAP: usize = 500;

/// 会话实时状态（由 Activity 心跳与打断/终止事件推导，前端经 bark://sessions 订阅）
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    /// 模型推理中：回合已开始、当前无工具在跑（UserPromptSubmit 之后，
    /// 或每次工具收尾 PostToolUse 之后——工具之间的间隙也是思考期）
    Thinking,
    /// 工具执行中：最近一次心跳带 tool_name（PreToolUse）
    ToolRunning,
    /// 打断：等待用户授权/确认权限
    WaitingPermission,
    /// 打断：等待用户输入
    WaitingInput,
}

/// 一条 agent 会话的实时运行状态
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub agent: String,
    pub session_id: String,
    pub project: Option<String>,
    pub cwd: String,
    pub phase: SessionPhase,
    /// 当前/最近回合的 prompt（截断展示用）
    pub prompt: Option<String>,
    /// 最近一次执行的工具名
    pub last_tool: Option<String>,
    /// 心跳统计：完成的回合数 / 工具调用次数
    pub turn_count: u32,
    pub tool_calls: u32,
    /// 当前回合开始时刻（unix ms）
    pub started_at: i64,
    /// 最近一次心跳（unix ms）
    pub last_activity: i64,
}

/// 心跳静止超过该时长的会话视为僵死（agent 被杀时不会有 Stop 事件），
/// 读取/处理新事件时淘汰，防止状态永远卡在「运行中」
pub const STALE_AFTER_MS: i64 = 10 * 60 * 1000;
/// 覆盖判死时长的环境变量名（见 `stale_after_ms`）
pub const STALE_ENV: &str = "BARK_STALE_AFTER_MS";
/// 覆盖值的下限：低于这个值误杀风险太大（正常工具调用也会有几十秒静默）
const STALE_MIN_MS: i64 = 5_000;

/// 运行时的判死时长：默认 [`STALE_AFTER_MS`]，可用 `BARK_STALE_AFTER_MS`（毫秒）覆盖。
///
/// 存在的理由：有的 agent 被用户中断时**一个事件都不发**（实测 ZCode 7 个事件全埋探针、
/// 手动终止后零事件），我们只能靠「心跳静止」推断它已经停了——10 分钟是防「长工具调用
/// 被误判」的保守值，但对着屏幕等 10 分钟才变红也很难受。需要更快反馈就把这个值调小
/// （代价是长时间跑单个工具时可能被误判成「意外终止」）。
///
/// 只在进程内解析一次：prune 在事件路径上是热路径，不该每次都读环境。
pub fn stale_after_ms() -> i64 {
    static CACHED: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var(STALE_ENV)
            .ok()
            .and_then(|v| parse_stale_override(&v))
            .unwrap_or(STALE_AFTER_MS)
    })
}

/// 解析覆盖值（纯函数，便于测试）：必须是 ≥5s 的整数毫秒，否则忽略
fn parse_stale_override(raw: &str) -> Option<i64> {
    raw.trim()
        .parse::<i64>()
        .ok()
        .filter(|v| *v >= STALE_MIN_MS)
}
/// 状态表上限：超出时淘汰最久未活动的（防泄漏，正常同时活跃会话远达不到）
const ACTIVE_SESSIONS_CAP: usize = 64;
/// 回合刚结束后的「空闲回声」宽限窗：TraeCode 实测每回合 Stop 后都会立即跟一条
/// Notification（空闲类），若按打断处理，流光会在完成色后误转警告色。宽限窗内的
/// 打断事件（仅限无活跃条目时）视为回声：不建档、不通知，完成色照常停留淡出。
pub const IDLE_ECHO_GRACE_MS: i64 = 10_000;

/// 监控线程句柄：停止标志 + 线程 join 句柄
pub type WatcherHandle = (Arc<std::sync::atomic::AtomicBool>, std::thread::JoinHandle<()>);

pub struct AppState {
    pub config: RwLock<BarkConfig>,
    pub config_path: std::path::PathBuf,
    /// 配置写入的**单写者**语义（§2.1）：所有「读-改-写盘-换内存」的配置保存路径
    /// （save_config 的整份覆盖、save_widget_position / set_widget_pinned /
    /// close_widget 的单字段写）全程持它，保证两个并发保存不会交错成
    /// 「旧快照后落盘」的丢更新。锁序恒为 config_write → config(RwLock)，
    /// 且**在 sync_watchers / glow::sync / widget::sync 等副作用之前释放**——
    /// 那些动作会再读配置，不许把它们拖进写锁里。
    pub config_write: Mutex<()>,
    /// 配置文件损坏时的错误，UI 直接展示（不要静默用默认值顶替）
    pub config_error: Mutex<Option<String>>,
    /// 启动自检产生的问题（接入自愈失败 / 因配置损坏被置为未启用），UI 以横幅展示。
    /// 带 agent id：用户修好并重新开启该 agent 后可以精确清掉（见 clear_startup_warnings），
    /// 否则横幅会一直挂到下次重启，看起来像「修复没生效」。
    pub startup_warnings: Mutex<Vec<StartupWarning>>,
    /// 事件服务器启动失败（端口被占等）的错误，UI 经 diagnostics 展示
    pub server_error: Mutex<Option<String>>,
    /// 启动时实际监听的端口 / token 副本：事件服务器与 hook 命令都烧入了这份值，
    /// 运行中改配置不会让它换端口换 token（save_config 据此强制保留，见 B3 注释）
    pub server_port: u16,
    pub server_token: String,
    pub history: Mutex<VecDeque<NormalizedEvent>>,
    /// 会话实时状态表（key = agent|session_id，由心跳事件推导）
    pub active_sessions: Mutex<HashMap<String, SessionStatus>>,
    /// key → 会话最近一次因终态事件被移除的时刻。
    /// 仅当移除时条目真实存在才记录（= 该 agent 会发心跳）——据此把「回合刚结束
    /// 的空闲回声」与「无心跳 agent 的真等待」区分开（见 apply_session_event）
    pub recent_finished: Mutex<HashMap<String, i64>>,
    /// 监控型 adapter 的运行线程（kind_id → 停止标志 + 线程句柄）
    pub watchers: Mutex<HashMap<String, WatcherHandle>>,
    /// 全局静音（托盘开关，仅暂停推送，事件仍入历史；重启后恢复）
    pub muted: std::sync::atomic::AtomicBool,
    /// 屏幕边缘流光的当前状态（glow 模块读写，UI 只经 command 改它做预览）
    pub glow: Mutex<GlowState>,
    /// 正在进行的预览：`Some((序号, 预览前要恢复到的状态))`。
    ///
    /// 预览是**临时接管**，真实事件、托盘重置、保存配置、点「熄灭」都会把它作废；
    /// 挂着的恢复定时器靠序号对不上而自动失效。
    /// 少了这个守卫就会出现「点两次预览之后灯再也灭不掉」：第二次预览会把第一次的
    /// 预览态当成真实状态记下来，恢复时又把它点亮。
    pub glow_preview: Mutex<Option<(u64, GlowState)>>,
    /// 聚合窗口内的待发通知（key = agent|kind|session）。
    /// 每条带派发世代号 `seq`（见 [`PendingNotify`]）：聚合定时器只派发自己那一代的条目
    pending: tokio::sync::Mutex<HashMap<String, PendingNotify>>,
    /// 聚合派发的世代号发放器（§2.3：旧定时器不得提前派发新一代条目）
    notify_seq: std::sync::atomic::AtomicU64,
    /// 已处理事件 id 的环形窗口：实时投递与启动补投可能送来同一条事件
    /// （hook POST 超时后落盘、daemon 稍后又收下同一 id），去重避免重复通知。
    /// **持久化**（§2.10）：新见到的事件 id 落盘到 seen-ids.jsonl，跨 daemon 重启
    /// 仍然判重——「POST 超时但 daemon 实际已收下 → hook 落盘 → 下次启动补投」
    /// 的重复通知正是靠它拦住的（至少一次投递 + 本窗口去重）
    recent_ids: Mutex<RecentIds>,
    /// 最近一次「同 agent 同类同会话」通知的落点时刻。同一状态跃迁被两条
    /// 独立通道各报一次时（如 WorkBuddy 轮询的撕裂快照把同一终态报两遍，
    /// 两次事件 id 不同、id 去重拦不住），在短窗口内压制第二条。
    /// 用 `Instant`（单调钟）度量窗口流逝（§1.7）：墙钟被 NTP 前跳/回拨会让
    /// 6s 窗变成长期压制的通知黑洞。**残余口径**：会话判死（prune_stale 的
    /// ms 时间戳）保持墙钟——跨进程事件时间只能用墙钟（服务端已把客户端时间戳
    /// 夹取到 [now-1h, now]），本窗口是纯进程内流逝时间，才用得起单调钟。
    recent_notified: Mutex<HashMap<String, Instant>>,
    /// 会话快照最近一次推送时刻（§4.4 限频）：纯 last_activity/计数变化的推送
    /// 节流到 ≥500ms 一次；相位跃迁/建档/摘档立即推（见 should_push_snapshot）
    snapshot_pushed: Mutex<Option<Instant>>,
}

/// 固定容量的 id 去重窗口（FIFO 淘汰），带**持久化**（§2.10）。
///
/// 为什么必须落盘：POST 超时但 daemon 实际已收下 → hook 把同一事件落盘 →
/// 下次 daemon 启动补投——内存窗口一重启就清空，同一事件会通知两次。
/// 新见到的事件 id 追加到 `seen-ids.jsonl`（单次 write_all 一行），重启加载后
/// 旧 id 照常判重。文件自身有界：>256KB 就裁到 CAP 行、启动时同样裁到 CAP 行。
#[derive(Default)]
pub struct RecentIds {
    set: std::collections::HashSet<String>,
    order: VecDeque<String>,
    /// 持久化路径；None = 纯内存（测试 / 配置目录不可用）
    path: Option<std::path::PathBuf>,
}

impl RecentIds {
    const CAP: usize = 2048;
    /// 持久化文件的大小上限：超过就裁到 CAP 行（约 2048 × 40B ≈ 80KB，256KB 留足余量）
    const LOG_MAX_BYTES: u64 = 256 * 1024;

    /// 从 `seen-ids.jsonl` 加载（文件不存在 = 空窗口）。启动时裁到 CAP 行。
    pub fn load(path: std::path::PathBuf) -> Self {
        let mut ids = Self { path: Some(path.clone()), ..Default::default() };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return ids;
        };
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        // 启动即裁到 CAP 行（内存窗口只有 CAP，留着更长的历史只是白占磁盘）
        let start = lines.len().saturating_sub(Self::CAP);
        for l in &lines[start..] {
            ids.insert(l.to_string());
        }
        if start > 0 {
            ids.rewrite_log(&lines[start..]);
        }
        ids
    }

    fn insert(&mut self, id: String) {
        if self.set.contains(&id) {
            return;
        }
        self.set.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
    }

    /// 首次见到返回 false；重复事件返回 true。
    /// `persist`：本类事件值不值得跨重启判重（心跳/工具收尾时效为零，不落盘）。
    fn seen(&mut self, id: &str, persist: bool) -> bool {
        if self.set.contains(id) {
            return true;
        }
        self.insert(id.to_string());
        if persist {
            self.append_log(id);
        }
        false
    }

    /// 新 id 追加一行：**单次 write_all**（append 模式原子追加，行不会被并发写交错拼坏）。
    /// 文件明显超量时裁到 CAP 行（读-改-写的窗口极小：只在超量后触发）。
    fn append_log(&mut self, id: &str) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        {
            use std::io::Write;
            let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
                return;
            };
            let _ = f.write_all(format!("{id}\n").as_bytes());
        }
        if std::fs::metadata(path).map(|m| m.len() > Self::LOG_MAX_BYTES).unwrap_or(false) {
            if let Ok(text) = std::fs::read_to_string(path) {
                let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                let start = lines.len().saturating_sub(Self::CAP);
                self.rewrite_log(&lines[start..]);
            }
        }
    }

    /// 把日志文件整体重写成给定行（tmp+rename，读者永远看不到半写文件）。
    /// 失败只吞掉：持久化是去重的增强，不该反过来打断事件管道。
    fn rewrite_log(&self, lines: &[&str]) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("jsonl.trim-tmp");
        let body = lines.join("\n") + "\n";
        if std::fs::write(&tmp, body).is_ok() {
            if std::fs::rename(&tmp, path).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// 启动自检的一条告警：`agent` 是 kind_id（用于用户处理后精确清除），
/// `message` 是给用户看的整句文案（UI 直接展示）。
#[derive(Debug, Clone)]
pub struct StartupWarning {
    pub agent: String,
    pub message: String,
}

/// 聚合窗内的一条待发通知。
///
/// `seq` 是派发世代号（§2.3）：条目由哪一代定时器负责派发，就带那一代的编号。
/// 旧定时器 sleep 到期与 `pending.lock()` 之间若被插入了新一代条目，旧任务
/// remove 后校验 seq 不符就把条目放回——否则新条目会被提前派发（聚合窗被截断），
/// 新定时器 remove 到 None 空转。
struct PendingNotify {
    notification: Notification,
    merged: u32,
    seq: u64,
}

impl AppState {
    pub fn new(config: BarkConfig, config_path: std::path::PathBuf) -> Arc<Self> {
        let server_port = config.server.port;
        let server_token = config.server.token.clone();
        // id 去重窗口从 seen-ids.jsonl 恢复（§2.10）：跨 daemon 重启的重复通知靠它拦。
        // 加载失败/目录不可用只降级为纯内存窗口，不挡启动。
        let recent_ids = bark_core::BarkConfig::dir()
            .map(|d| RecentIds::load(d.join("seen-ids.jsonl")))
            .unwrap_or_default();
        Arc::new(Self {
            config: RwLock::new(config),
            config_path,
            config_write: Mutex::new(()),
            config_error: Mutex::new(None),
            startup_warnings: Mutex::new(Vec::new()),
            server_error: Mutex::new(None),
            server_port,
            server_token,
            history: Mutex::new(VecDeque::with_capacity(HISTORY_CAP)),
            active_sessions: Mutex::new(HashMap::new()),
            recent_finished: Mutex::new(HashMap::new()),
            watchers: Mutex::new(HashMap::new()),
            muted: std::sync::atomic::AtomicBool::new(false),
            glow: Mutex::new(GlowState::Idle),
            glow_preview: Mutex::new(None),
            pending: tokio::sync::Mutex::new(HashMap::new()),
            notify_seq: std::sync::atomic::AtomicU64::new(0),
            recent_ids: Mutex::new(recent_ids),
            recent_notified: Mutex::new(HashMap::new()),
            snapshot_pushed: Mutex::new(None),
        })
    }

    /// 事件 id 去重：返回 true 表示这条事件此前已处理过（应跳过）。
    /// 新见到的**非心跳类**事件 id 持久化（心跳/工具收尾时效为零，不值跨重启判重）。
    pub fn mark_seen(&self, event: &NormalizedEvent) -> bool {
        lock(&self.recent_ids).seen(&event.id, persist_seen(event.kind))
    }

    /// 下一个聚合派发世代号（§2.3）
    fn next_notify_seq(&self) -> u64 {
        self.notify_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// 运行中的事件服务器是否还在用启动时的 port/token（用户改了配置但未重启）
    pub fn restart_required(&self) -> bool {
        let cfg = read(&self.config);
        cfg.server.port != self.server_port || cfg.server.token != self.server_token
    }

    /// 切换静音，返回切换后的状态
    pub fn toggle_mute(&self) -> bool {
        use std::sync::atomic::Ordering;
        // fetch_not 原子取反并返回旧值；取反即新状态。
        // （不能用 swap(true)：那会让开关永远停在「静音」，再也无法取消）
        let prev = self.muted.fetch_not(Ordering::Relaxed);
        !prev
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_config_error(&self, err: Option<String>) {
        *lock(&self.config_error) = err;
    }

    pub fn config_error(&self) -> Option<String> {
        lock(&self.config_error).clone()
    }

    pub fn set_server_error(&self, err: Option<String>) {
        *lock(&self.server_error) = err;
    }

    pub fn server_error(&self) -> Option<String> {
        lock(&self.server_error).clone()
    }

    /// 记录启动自检的告警（空列表 = 一切正常）
    pub fn set_startup_warnings(&self, warnings: Vec<StartupWarning>) {
        *lock(&self.startup_warnings) = warnings;
    }

    /// 给 UI 的告警文案（前端只需要字符串列表）
    pub fn startup_warnings(&self) -> Vec<String> {
        lock(&self.startup_warnings)
            .iter()
            .map(|w| w.message.clone())
            .collect()
    }

    /// 清掉某个 agent 的启动告警：用户按横幅提示处理过之后（重新开启接入 / 关掉接入），
    /// 那条告警就已经过时了，留着会让人以为修复没生效（要等下次重启才消失）。
    pub fn clear_startup_warnings(&self, agent: &str) {
        lock(&self.startup_warnings).retain(|w| w.agent != agent);
    }

    /// 清空事件历史（UI 的「清空显示」）：`None` 清空全部，`Some(kind)` 只清该类型
    /// （对应「清空当前筛选」，不误删其他类型）。返回**实际被删掉的事件 id**。
    ///
    /// 为什么必须清后端这份：`list_events` 读的就是它，而事件流页每次挂载都会拉一遍
    /// 历史补齐，只清前端内存里的副本，切走菜单再切回来被清掉的事件会整批复活（旧 bug）。
    /// 历史是只给界面看的环形缓冲（通知、状态机、去重窗口都不读它），删除无副作用。
    ///
    /// 返回 id 而不是条数：前端要按「后端确实删了哪些」来删本地副本。按「点击瞬间的
    /// 本地快照」删会出现两种反向不一致——清空请求在途时新到的事件后端没删、本地却删了
    /// （重挂载又冒出来），或是后端删了、本地还留着（切页面前一直显示幽灵事件）。
    pub fn clear_history(&self, kind: Option<EventKind>) -> Vec<String> {
        let mut history = lock(&self.history);
        let mut cleared = Vec::new();
        match kind {
            Some(k) => {
                let mut kept = VecDeque::with_capacity(history.len());
                for e in history.drain(..) {
                    if e.kind == k {
                        cleared.push(e.id);
                    } else {
                        kept.push_back(e);
                    }
                }
                *history = kept;
            }
            None => cleared.extend(history.drain(..).map(|e| e.id)),
        }
        cleared
    }
}

// ---------------------------------------------------------------------------
// std 锁统一中毒恢复（B11）：通知/配置路径上一处 panic 不应让全局状态从此不可用，
// 保持原有行为、吞掉中毒标记即可。
// ---------------------------------------------------------------------------
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|p| p.into_inner())
}

/// 事件管道主循环
pub async fn run_pipeline(mut rx: mpsc::Receiver<NormalizedEvent>, app: AppHandle, state: Arc<AppState>) {
    while let Some(event) = rx.recv().await {
        // 0. 事件去重：同一条事件可能既被实时投递又被启动补投（hook 超时后落盘）。
        //    重复处理会让历史和通知都出现两条。
        if state.mark_seen(&event) {
            tracing::debug!(id = %event.id, "duplicate event, skip");
            continue;
        }

        // 0.25 已关闭接入的 agent：磁盘上的 hook / 插件登记已被回收，但宿主进程里
        //      已加载的那份可能还在跑（插件型尤其明显——DSH 重启前插件仍在内存里 POST）。
        //      这类事件仍进历史与前端事件流（看得见残留，便于排查），但不进状态机、
        //      不亮流光、不通知：开关关掉之后就不该再收到这个 agent 的通知。
        //      判定用 agent_disabled（**显式**关闭）而不是 !agent_enabled：后者对
        //      从未写进配置的 agent 也返回 false，会把本该正常上报的事件误杀。
        if read(&state.config).agent_disabled(&event.agent) {
            tracing::debug!(agent = %event.agent, "agent integration disabled, skip state/notify");
            // 顺手摘掉该 agent 残留的「运行中」条目（含流光善后）：它的终态事件从此
            // 被丢弃，否则面板会一直挂着一条永远不会结束的会话。
            // 善后口径与「关闭开关时的即时清理」共用，见 drop_agent_sessions_and_sync。
            drop_agent_sessions_and_sync(&app, &state, &event.agent);
            publish_event(&state, &app, &event);
            continue;
        }

        // 0.3 空 session_id 的事件不进状态机/glow/音效/通知聚合，只入历史与事件流：
        //     stdin 超时的空 payload 照样能 normalize 出事件（§2.11），session_key 会
        //     建出 `"agent|"` 幻影会话——心跳判死时还会凭空亮终止色。幻影会话没有任何
        //     展示价值，但事件本身要留痕（排查「hook 超时」要用）。
        if skips_state_machine(&event) {
            tracing::debug!(agent = %event.agent, "empty session_id, publish only");
            publish_event(&state, &app, &event);
            continue;
        }

        // 0.5 会话实时状态：所有事件（含心跳）都参与推导，变化即推送前端快照。
        //     陈旧补投（时间戳老过判死阈值）不进状态表——它声称的任何状态都已过时。
        let stale = is_stale_replay(&event);
        let (running_lost, suppressed_echo, phase_changed) = {
            let mut sessions = lock(&state.active_sessions);
            let mut finished = lock(&state.recent_finished);
            let delta = apply_session_event_inner(&mut sessions, &mut finished, &event, stale);
            if delta.changed {
                let snapshot = sessions_snapshot(&sessions, event.timestamp);
                drop(sessions);
                // 快照限频（§4.4）：一回合上百条心跳都只刷 last_activity/计数，
                // 全量快照每条一推纯属浪费 IPC；相位跃迁/建档/摘档（urgent）立即推
                if should_push_snapshot(&state.snapshot_pushed, delta.urgent) {
                    let _ = app.emit("bark://sessions", &snapshot);
                }
            }
            (delta.running_lost, delta.suppressed_echo, delta.phase_changed)
        };
        // 0.6 屏幕边缘流光：会话表已更新完，据此推导颜色。
        //     放在锁外——内部可能要建窗口（会派发到主线程）。
        //     终态回声（如上文 Stop 之后紧跟的 SessionEnd）不改状态表，也不该改流光，
        //     否则正常回合的完成色会被压成终止色；但心跳判死仍需照常显终止色。
        //     陈旧补投（stale）不驱动 glow/音效（§1.6）：它声称的状态早已过时，
        //     照常驱动会「开局红光 + 终止音」；历史与通知保留（离线兜底的本意）。
        if should_drive_glow(stale, suppressed_echo, running_lost) {
            crate::glow::update_from_event(&app, &state, &event, running_lost);
            // 状态音效与流光同一套四状态语义（思考/警告/完成/终止），默认全部未选
            // = 静音。子代理事件不响（与通知的子代理过滤同口径，否则一个任务拆出
            // 十几个子代理会连响十几声）。
            if !event.is_subagent {
                if let Some(key) = sound_key_for(&event, running_lost) {
                    if should_play_state_sound(key, phase_changed) {
                        play_state_sound(&state, key);
                    }
                }
            }
        }

        // 心跳与工具收尾不入历史不入事件流：一个回合可达多次，会把 500 条容量的
        // 历史全部冲掉（它们的价值只在实时状态，上面已经消费完了）
        if matches!(event.kind, EventKind::Activity | EventKind::ToolFinished) {
            continue;
        }

        // 1. 历史与前端流（不受任何规则影响）
        publish_event(&state, &app, &event);

        // 2. 回合刚结束的空闲回声不通知：「任务完成」的通知已经覆盖了它，
        //    再来一条「等待输入」纯属噪音（事件本身仍入历史与前端事件流）
        if suppressed_echo {
            continue;
        }

        // 3. SessionStart 不通知
        if !event.kind.should_notify() {
            continue;
        }

        // 4. 子代理事件一律不通知：一个任务被拆成十几个子代理会刷屏
        //    （原为 rules.notify_subagents 开关，已下线——没人需要给子代理开通知）
        if event.is_subagent {
            tracing::debug!(agent = %event.agent, "subagent event, skip notify");
            continue;
        }

        // 5. 免打扰
        let (quiet, window_ms) = {
            let cfg = read(&state.config);
            (in_quiet_hours(&cfg.rules.quiet_hours), cfg.rules.aggregate_window_ms)
        };
        if quiet {
            tracing::info!(agent = %event.agent, "quiet hours, skip notify");
            continue;
        }

        // 6. 聚合窗口 + 6.5 短窗口去重（§1.4 统一语义，不再互相架空）：
        //    - 同 agent + 同事件类型 + **同会话**才有共同的聚合键（不带 session 维度会把
        //      「两个会话都完成了」压成一条，丢掉一个结果）；
        //    - 聚合窗内已有同 key 的 pending 条目 → **合并计数**（`合并了 N 条同类事件`），
        //      不丢事件——旧实现在 6s 窗内直接 `continue`，把真实新事件整条吞掉，
        //      且 merged 计数在默认配置下永远走不到；
        //    - 仅**监控型 agent**（轮询撕裂快照会把同一状态跃迁报两遍，两次 id 不同、
        //      id 去重拦不住）在「无 pending 条目 + 6s 内同 key 已通知」时整条压制；
        //    - hook 型 agent 不受 6s 窗压制：id 去重 + 聚合窗已覆盖它们，快速追问两回合
        //      各自完成的两条真实终态必须都通知到（第二条进新聚合窗）。
        let key = format!("{}|{}|{}", event.agent, kind_name(&event.kind), event.session_id);
        let is_watch = is_watch_agent(&event.agent);
        let recently_notified = {
            let mut recent = lock(&state.recent_notified);
            recent_notified(&mut recent, &key)
        };
        let notification = Notification {
            title: event.notify_title(),
            body: event.notify_body(),
            event: event.kind,
            agent: event.agent.clone(),
            project: event.project.clone(),
        };
        let seq = state.next_notify_seq();
        {
            let mut pending = state.pending.lock().await;
            let has_pending = pending.contains_key(&key);
            match notify_action(is_watch, has_pending, recently_notified) {
                NotifyAction::Suppress => {
                    tracing::debug!(key = %key, "watch 型撕裂快照重复，压制");
                    continue;
                }
                NotifyAction::Merge => {
                    // 记录保持：每次落点都刷新（压制只对 watch 型生效，见 notify_action）
                    lock(&state.recent_notified).insert(key.clone(), Instant::now());
                    if let Some(p) = pending.get_mut(&key) {
                        p.merged += 1;
                    }
                    continue;
                }
                NotifyAction::Enqueue => {
                    lock(&state.recent_notified).insert(key.clone(), Instant::now());
                    pending.insert(key.clone(), PendingNotify { notification, merged: 0, seq });
                }
            }
        }
        let state2 = state.clone();
        tokio::spawn(async move {
            // 窗口夹取：load() 已夹过一次，这里再兜一道（防手改配置绕过）
            let window_ms = window_ms.clamp(1, bark_core::MAX_AGGREGATE_WINDOW_MS);
            tokio::time::sleep(std::time::Duration::from_millis(window_ms)).await;
            // B10：先在小作用域取出并释放锁，再 await dispatch——
            // 持 tokio Mutex guard 跨 await 会把整个聚合表锁死在 dispatch 期间。
            let p = {
                let mut pending = state2.pending.lock().await;
                claim_pending(&mut pending, &key, seq)
            };
            if let Some(mut p) = p {
                if p.merged > 0 {
                    p.notification.body = append_merged_note(p.notification.body, p.merged);
                }
                dispatch(&state2, &p.notification).await;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 通知聚合/去重的纯决策（§1.4、§2.3）：抽出来直接钉死语义，管道只做搬运
// ---------------------------------------------------------------------------

/// 聚合窗内同 key 事件的处置
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotifyAction {
    /// 合并进已有 pending 条目（`merged += 1`，不丢事件）
    Merge,
    /// 整条压制（仅限监控型 agent 的撕裂快照重复，见 [`notify_action`]）
    Suppress,
    /// 进新聚合窗
    Enqueue,
}

/// 聚合/去重的统一决策（纯函数）。
///
/// - 已有 pending 条目 → `Merge`：**命中聚合窗就合并计数**，绝不整条丢弃——
///   旧实现把 6s 压制窗判在聚合窗之前且判中即丢，快速追问两回合的真实终态
///   （T0+2s / T0+4s）会被整条吞掉，且「合并了 N 条」文案在默认配置下永不出现；
/// - 仅**监控型 agent**（`AdapterMode::Watch`，轮询撕裂快照会把同一状态跃迁报两遍、
///   两次 id 不同）在无 pending 且短窗内已通知过时才 `Suppress`；
/// - hook 型 agent 不被 6s 窗压制：id 去重 + 聚合窗已覆盖它们，
///   窗外 2s 的第二条真实事件必须进新聚合窗照常通知。
fn notify_action(is_watch: bool, has_pending: bool, recently_notified: bool) -> NotifyAction {
    if has_pending {
        NotifyAction::Merge
    } else if is_watch && recently_notified {
        NotifyAction::Suppress
    } else {
        NotifyAction::Enqueue
    }
}

/// 该 agent 是不是监控型（轮询/推断式 adapter）。
/// 未知 agent（配置里残留的旧 id）按 hook 型对待：宁可多通知一条，不做撕裂快照压制。
fn is_watch_agent(agent: &str) -> bool {
    AgentKind::from_id(agent).is_some_and(|k| k.mode() == AdapterMode::Watch)
}

/// 撕裂快照去重的短窗口（略大于 watch 轮询间隔，见 [`recent_notified`]）
const NOTIFY_DUP_SUPPRESS: std::time::Duration = std::time::Duration::from_millis(6000);

/// 「同 key 在短窗口内已通知过」判定 + 顺手清过期条目（§1.4）。
///
/// 窗口用 `Instant` 单调钟（§1.7）：墙钟被 NTP 前跳/回拨时，`i64` 差值口径会把
/// 压制窗拉成永久通知黑洞（回拨后 `now - t` 恒小于窗、条目永不过期）。
/// 判定不刷新条目时间戳：窗口从上一次**落点**起算。
fn recent_notified(recent: &mut HashMap<String, Instant>, key: &str) -> bool {
    recent.retain(|_, t| t.elapsed() < NOTIFY_DUP_SUPPRESS);
    recent.contains_key(key)
}

/// 「合并了 N 条同类事件」的附注（纯函数，便于测试钉死文案）
fn append_merged_note(body: String, merged: u32) -> String {
    format!("{body}\n（合并了 {merged} 条同类事件）")
}

/// 认领聚合条目：**seq 对得上**（还是自己派发的那一代）才移除返回；否则把条目
/// 原样放回并返回 None——新代条目属于更晚的事件，旧定时器无权提前派发它
/// （那会把新事件的聚合窗截断，新定时器又 remove 到 None 空转）。
fn claim_pending(
    pending: &mut HashMap<String, PendingNotify>,
    key: &str,
    seq: u64,
) -> Option<PendingNotify> {
    let p = pending.remove(key)?;
    if p.seq == seq {
        Some(p)
    } else {
        pending.insert(key.to_string(), p);
        None
    }
}

/// 本次事件是否驱动 glow/音效（纯函数，§1.6）。
///
/// 陈旧补投（stale）**不驱动**：它声称的状态早已过时，照常驱动会「开局红光 + 终止音」
/// （启动补投一条 40 分钟前的 RunFailed 即复现）；历史与通知保留（离线兜底的本意）。
/// 终态回声不驱动（完成色不能被压成终止色），但回声捎带的心跳判死（running_lost）要驱动。
fn should_drive_glow(stale: bool, suppressed_echo: bool, running_lost: bool) -> bool {
    !stale && (!suppressed_echo || running_lost)
}

/// 空 session_id 的事件只入历史与事件流（§2.11）：stdin 超时的空 payload 照样
/// normalize 出事件，进状态机会建出 `"agent|"` 幻影会话（判死时凭空亮终止色）。
fn skips_state_machine(ev: &NormalizedEvent) -> bool {
    ev.session_id.trim().is_empty()
}

/// 该事件 id 是否值得跨重启判重（§2.10）。心跳/工具收尾一轮可达多次、时效为零，
/// 落盘只会把 seen-ids 冲爆——它们的重复由内存窗口就够（实时路径唯一来源）。
fn persist_seen(kind: EventKind) -> bool {
    !matches!(kind, EventKind::Activity | EventKind::ToolFinished)
}

/// 快照推送限频（§4.4，纯逻辑）：纯 last_activity/计数变化的推送至少间隔 500ms；
/// 相位跃迁/建档/摘档（`urgent`）立即推。窗口用 `Instant`（单调钟），
/// 墙钟跳变不会把限频窗拉长成「永不推送」。
fn should_push_snapshot(last: &Mutex<Option<Instant>>, urgent: bool) -> bool {
    const SNAPSHOT_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut last = lock(last);
    if !urgent && last.is_some_and(|t| t.elapsed() < SNAPSHOT_MIN_INTERVAL) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// 推送闸门（纯函数，§2.5）：静音与**派发时刻**的免打扰都拦下推送。
/// 免打扰必须在 dispatch 时复查——入聚合窗时还没到免打扰、到点已经免打扰的
/// 通知不该照常推出去（音效路径一直有这道复查，推送渠道补齐同口径）。
fn dispatch_gate(muted: bool, quiet_now: bool) -> bool {
    !muted && !quiet_now
}

/// 事件 → 状态音效 key（与流光四色状态同语义）。`None` = 该事件不对应任何状态音。
///
/// - 用户主动中止（run_aborted）不响：与流光同口径同优先级——derive 里
///   RunAborted 分支也在判死之前（「那是你自己按的停止」，即便同一瞬间
///   恰好有别的会话被心跳判死，也不该播「意外终止」吓人）；
/// - 心跳判死（agent 进程没了）算「意外终止」，优先于事件本身；
/// - 完成 / 失败 / 等待（确认与输入）直接按事件角色；
/// - 思考音挂在「不带工具名的心跳」上——那是一次新回合的开始
///   （UserPromptSubmit），同回合后续的工具心跳不重复响；监控型 adapter 的
///   保活心跳是同一种事件载体，靠 [`should_play_state_sound`] 的跃迁门挡住重播；
/// - 工具收尾（ToolFinished）不响：它每次工具结束都来一条，
///   会把「新回合开始」的语义淹没（思考音只标回合起点）。
fn sound_key_for(event: &NormalizedEvent, running_lost: bool) -> Option<&'static str> {
    if event.kind == EventKind::RunAborted {
        return None;
    }
    if running_lost {
        return Some("terminated");
    }
    match event.kind {
        EventKind::RunCompleted => Some("completed"),
        EventKind::RunFailed => Some("terminated"),
        EventKind::PermissionRequired | EventKind::InputRequired => Some("warning"),
        EventKind::Activity if event.tool_name.is_none() => Some("thinking"),
        _ => None,
    }
}

/// 这条状态音此刻是否该响（纯函数，便于测试）。
///
/// 只有思考音需要跃迁门：它与保活心跳共用同一种事件载体（无工具名的
/// Activity）——监控型 adapter（WorkBuddy）运行期间每 60s 重发一条保活
/// （watch.rs 的 HEARTBEAT_EVERY_ROUNDS），不设门的话思考音每隔一个保活
/// 周期就凭空响一声（用户实测），而它与新回合在事件层面无法区分，只能靠
/// 状态表「相位没变」识别。其余状态音的事件（终态 / 打断）天然一次性，
/// 不该被这道门拦——终态会把条目摘掉，相位无从比对，门只会误杀
/// 「daemon 晚启动、终态是唯一信号」的会话。
fn should_play_state_sound(key: &str, phase_changed: bool) -> bool {
    key != "thinking" || phase_changed
}

/// 播放某个状态配置的音效。总开关关闭、未配置（effect 为空）、全局静音、免打扰时段都静默。
///
/// 真正的播放（Windows 上要起 PowerShell 进程）走 spawn_blocking，
/// 不阻塞事件管道；播放失败只记日志。
fn play_state_sound(state: &Arc<AppState>, key: &str) {
    if state.is_muted() {
        return;
    }
    let cfg = read(&state.config);
    if !cfg.notify.enabled {
        return;
    }
    if in_quiet_hours(&cfg.rules.quiet_hours) {
        return;
    }
    let Some(s) = cfg.notify.sound(key) else { return };
    if s.effect.trim().is_empty() {
        return;
    }
    let (effect, plays) = (s.effect.clone(), s.plays);
    drop(cfg);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = bark_channels::play_sound(&effect, plays) {
            tracing::warn!("状态音效播放失败: {e:#}");
        }
    });
}

fn kind_name(kind: &EventKind) -> String {
    serde_json::to_string(kind).unwrap_or_default().trim_matches('"').to_string()
}

/// 摘掉某个 agent 在「运行中会话」表里的全部条目（**纯表操作**，不做任何善后）。
///
/// 返回 `(表是否有变化, 表是否已空)`。**故意不对外可见**：调用方一律走
/// [`drop_agent_sessions_and_sync`]（它补上「推前端快照 + 流光善后」）——
/// 只摘表不碰流光正是「关闭接入后灯常亮」那类 bug 的入口，不给第二次机会。
/// 拆成两步是为了让「表是否被改」这一步能单独测（见 `drop_agent_sessions_matches_whole_agent_id`）。
fn drop_agent_sessions(state: &AppState, agent: &str) -> (bool, bool) {
    let mut sessions = lock(&state.active_sessions);
    let before = sessions.len();
    // key 形如 "agent|session_id"：带上分隔符前缀匹配，避免 "ds" 误伤 "dsh"
    let prefix = format!("{agent}|");
    sessions.retain(|key, _| !key.starts_with(&prefix));
    (sessions.len() != before, sessions.is_empty())
}

/// 摘掉某个 agent 的会话并完成善后：推前端快照 + 把流光重新对齐到剩余会话。
///
/// **两处调用点必须走这里，口径必须一致**：管道里「已关闭接入」的门，以及关闭开关时的
/// 即时清理。历史上后者只手写了 `drop_agent_sessions` 而漏掉流光那一步，后果是一盏
/// 熄不掉的灯：关掉最后一个在跑的 agent 接入后，它的事件从此被门丢弃（`changed` 永远
/// 为 false，再也走不到善后），判死巡检也因表已空而无变化、在 `sweep_stale_sessions`
/// 的 `!changed` 分支早退——思考色 / 警告色会一直挂在屏幕上，只有托盘「重置流光」能解。
///
/// 流光不区分 agent，所以摘完要按**剩余**会话重新推导，而不是只在一处分叉上处理：
/// - 表已空：当前不是终态指示（会自己过期的绿/红不受影响）时收回，免得留下僵尸色；
/// - 表非空：走 `refresh_from_sessions` 按剩余会话聚合。只做「表空才收回」是不够的——
///   被摘掉的恰好是那个等待确认的会话时，橙色得降回其余会话的思考色，
///   否则会一直挂着一条已经不存在的等待指示（下一次事件才自愈）。
pub fn drop_agent_sessions_and_sync(app: &AppHandle, state: &Arc<AppState>, agent: &str) {
    let (changed, empty) = drop_agent_sessions(state, agent);
    if !changed {
        return;
    }
    // 快照按 UI 口径取（`sessions_for_ui` 会剔除已关闭接入的 agent）：
    // 关闭开关这条路里配置已经落盘为 enabled=false，被关掉那个 agent 的残留条目
    // 不该再出现在面板上，免得「关掉了还挂在运行中」。
    let snapshot = sessions_for_ui(state);
    let _ = app.emit("bark://sessions", &snapshot);
    match glow_after_drop(empty, crate::glow::current(state).is_terminal()) {
        GlowAfterDrop::Reset => crate::glow::reset(app, state),
        GlowAfterDrop::Refresh => crate::glow::refresh_from_sessions(app, state, false),
        GlowAfterDrop::Keep => {}
    }
}

/// 摘完某个 agent 的会话后，流光该怎么处理（抽成纯函数便于直接测这条分叉）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlowAfterDrop {
    /// 表已空、当前不是终态：收回光效
    Reset,
    /// 表已空、当前是终态（绿/红）：保持不动，让它自己淡出
    Keep,
    /// 还有别的活跃会话：按剩余会话重新推导
    Refresh,
}

fn glow_after_drop(empty: bool, current_terminal: bool) -> GlowAfterDrop {
    if !empty {
        GlowAfterDrop::Refresh
    } else if current_terminal {
        GlowAfterDrop::Keep
    } else {
        GlowAfterDrop::Reset
    }
}

/// 事件入历史 + 推前端事件流（不受通知规则影响）。
///
/// 心跳与工具收尾不入历史也不进事件流：一个回合可达多次，会把 500 条容量的历史
/// 全部冲掉（它们的价值只在实时状态，调用方在此之前已消费完）。
fn publish_event(state: &AppState, app: &AppHandle, event: &NormalizedEvent) {
    if matches!(event.kind, EventKind::Activity | EventKind::ToolFinished) {
        return;
    }
    {
        let mut history = lock(&state.history);
        history.push_front(event.clone());
        while history.len() > HISTORY_CAP {
            history.pop_back();
        }
    }
    let _ = app.emit("bark://event", event);
}

/// 启动时对齐所有「已开启」的接入：**刷新 + 自愈**。
///
/// 为什么每次启动都重写一遍，而不是只在坏掉时修：
/// - 目标应用会整份重写自己的配置、把我们的 hook 冲掉（实测 Qoder CN 桌面应用启动时
///   连 CN IDE 自己的插件键一起冲），程序被移动/重装会让 hook 指向旧路径，patch 条目
///   或插件文件也可能被清理；只修坏掉的，会让接入在用户不再进设置页时静默死掉；
/// - 我们**生成的产物本身会随应用版本变化**（例如 DSH 插件的事件面）：只在坏掉时才写，
///   就等于「升级了 agent-bark，机器上仍是旧版插件」，用户只能手动关一次再开一次——
///   实测踩过：应用重编重启后 `plugin.js` 仍是旧模板，通知内容与事件面都没更新。
///
/// 幂等性由各 adapter 自己保证：配置文件只有内容变化才写（claude_style / opencode），
/// 生成的插件文件也是内容一致就不写（dsh / opencode），所以「什么都没变」时这里
/// 不产生任何写盘，也不换 mtime。
///
/// `ConfigUnreadable` 例外：**不能写**（写下去会把用户整份配置替换成只剩我们的条目）。
/// 曾经会把该 agent 落成未启用——实测有反例：opencode.json 是 JSONC（带注释）时
/// 我们读不了，但既有 hook 仍然有效，自动关闭等于把一条活着的接入掐死，且用户
/// 每次修好文件重启前都会被反复关闭。改为**保留开关、只告警**：UI 的「配置损坏」
/// 徽标（verify 独立上报）照常显示，用户修好文件后点一下开关即重写。
///
/// 返回需要展示给用户的告警（空 = 一切正常）。每条都带 agent id，
/// 用户处理后（重新开启/关闭该 agent）由 `clear_startup_warnings` 精确清掉。
pub fn sync_hook_integrations(state: &Arc<AppState>) -> Vec<StartupWarning> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (port, token) = {
        let cfg = read(&state.config);
        (cfg.server.port, cfg.server.token.clone())
    };
    let register_ctx = RegisterCtx { exe_path: exe, port, token };
    let mut warnings = Vec::new();

    for adapter in bark_adapters::registry::hook_adapters() {
        let id = adapter.kind().id().to_string();
        let want = {
            let cfg = read(&state.config);
            cfg.agent_enabled(&id)
        };
        if !want || !adapter.is_installed() {
            continue;
        }
        match adapter.verify(&register_ctx) {
            VerifyReport::ConfigUnreadable { path, reason } => {
                // 不能写（会覆盖用户配置），但也**不自动关闭**：配置我们读不了
                // 不代表既有 hook 已失效（JSONC 是典型），掐死活接入比留着更糟
                tracing::warn!(agent = %id, path = %path.display(), "配置无法解析，跳过对齐（保留开关）");
                warnings.push(StartupWarning {
                    agent: id.clone(),
                    message: format!(
                        "「{}」的配置无法解析（{}：{}），本次未对齐、接入开关保持原样；修好该文件后在 Agents 页点一下开关即可重写",
                        adapter.display_name(),
                        path.display(),
                        reason
                    ),
                });
            }
            report => match adapter.register(&register_ctx) {
                Ok(()) => {
                    if matches!(&report, VerifyReport::Ok) {
                        tracing::debug!(agent = %id, "启动对齐：接入已刷新（内容无变化）");
                    } else {
                        tracing::info!(agent = %id, report = ?report, "启动对齐：接入已就地修复");
                    }
                }
                Err(e) => {
                    tracing::warn!(agent = %id, "启动对齐：接入重写失败：{e:#}");
                    warnings.push(StartupWarning {
                        agent: id.clone(),
                        message: format!(
                            "「{}」的接入已失效且自动重写失败：{e:#}；可在 Agents 页手动重新开启",
                            adapter.display_name()
                        ),
                    });
                }
            },
        }
    }
    warnings
}

// ---------------------------------------------------------------------------
// 会话实时状态机：Activity 心跳 → Thinking/ToolRunning，打断 → Waiting*，
// 终态 → 移除。纯函数，便于测试。
// ---------------------------------------------------------------------------

pub(crate) fn session_key(agent: &str, session_id: &str) -> String {
    format!("{agent}|{session_id}")
}

/// 惰性淘汰的结果
struct PruneResult {
    /// 状态表是否发生变化（决定要不要推前端快照）
    changed: bool,
    /// 被淘汰的条目里是否有「正在运行中」的会话。
    /// 这类会话是被心跳超时判死的（agent 进程被杀不会有 Stop 事件），
    /// 对屏幕边缘流光来说就是「意外终止」→ 终止色。
    running_lost: bool,
}

/// 淘汰僵死条目（agent 被杀时不会有 Stop，靠心跳静止判定），返回是否有变化
fn prune_stale(sessions: &mut HashMap<String, SessionStatus>, now_ms: i64) -> PruneResult {
    let stale = stale_after_ms();
    let before = sessions.len();
    let running_lost = sessions.iter().any(|(_, s)| {
        now_ms.saturating_sub(s.last_activity) >= stale
            && matches!(s.phase, SessionPhase::Thinking | SessionPhase::ToolRunning)
    });
    sessions.retain(|_, s| now_ms.saturating_sub(s.last_activity) < stale);
    PruneResult { changed: before != sessions.len(), running_lost }
}

/// 一次事件引起的状态表变化
struct SessionDelta {
    /// 状态表内容有变化（含淘汰），应推前端快照
    changed: bool,
    /// 有「运行中」的会话被判死（心跳超时）
    running_lost: bool,
    /// 本事件是回合刚结束的「空闲回声」：状态表不动、不推送快照、不发通知
    /// （完成色照常停留淡出；事件仍入历史与前端事件流）
    suppressed_echo: bool,
    /// 会话相位真的跃迁了（建档、摘档或相位字段变化）。保活心跳只刷
    /// last_activity、相位不动，此值为 false——思考音靠它挡住监控型 adapter
    /// 周期性重发的保活心跳（见 run_pipeline 的音效门与 should_play_state_sound）。
    phase_changed: bool,
    /// 快照必须**立即**推送（相位跃迁 / 建档 / 摘档）：只有纯 last_activity、
    /// 计数刷新才允许被 should_push_snapshot 节流（§4.4）
    urgent: bool,
}

/// 状态表快照（剔除僵死条目，按回合开始时间倒序，展示稳定）
fn sessions_snapshot(sessions: &HashMap<String, SessionStatus>, now_ms: i64) -> Vec<SessionStatus> {
    let mut out: Vec<SessionStatus> = sessions
        .values()
        .filter(|s| now_ms.saturating_sub(s.last_activity) < stale_after_ms())
        .cloned()
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    out
}

/// 「运行中会话」面板的数据源：剔除僵死条目（心跳静止超时）与**已显式关闭接入**的 agent。
///
/// 关闭接入后该 agent 可能再也不会上报（hook 型：hook 已被摘掉），等不到管道门那条事件
/// 来清表，所以这里按开关即时过滤——EventsPage 每 15s 对账一次，最迟一个周期后消失。
///
/// 过滤口径必须与事件管道门同用 `agent_disabled`（首条匹配，§2.6）：旧实现按
/// 「任一条目 !enabled」收集隐藏名单，手改出重复 kind 条目（首 true 次 false）时
/// 事件照进状态表、UI 却把会话藏掉——正是 config.rs 注释防的「自相矛盾」的镜像。
pub fn sessions_for_ui(state: &AppState) -> Vec<SessionStatus> {
    // 持 config 读锁跨过滤即可：本仓库没有「持 active_sessions 锁再取 config」的
    // 反向路径，锁序无环（config → active_sessions）
    let cfg = read(&state.config);
    let sessions = lock(&state.active_sessions);
    let mut out = sessions_snapshot(&sessions, bark_core::now_millis());
    out.retain(|s| !cfg.agent_disabled(&s.agent));
    out
}

/// 巡检的纯逻辑：淘汰僵死条目并给出新快照。
/// 返回 `(是否变化, 是否有运行中的会话被判死, 新快照)`（无变化时快照为空）。
fn prune_and_snapshot(
    sessions: &mut HashMap<String, SessionStatus>,
    now_ms: i64,
) -> (bool, bool, Vec<SessionStatus>) {
    let r = prune_stale(sessions, now_ms);
    let snapshot = if r.changed { sessions_snapshot(sessions, now_ms) } else { Vec::new() };
    (r.changed, r.running_lost, snapshot)
}

/// 心跳判死巡检：由后台定时器周期调用（见 `lib.rs`）。
///
/// **为什么必须由定时器驱动**：`prune_stale` 过去只在「有事件进来」时才跑，而 agent 被杀
/// 或回合被用户中断之后可能**再也不发任何事件**（实测 ZCode 手动终止：既没有 `Stop`、
/// 也没有任何「会话结束」类事件）——于是流光永远停在思考色（用户实测复现），状态表条目也只在
/// 15s 轮询的快照过滤里「看不见」，并没有真正被清掉。这里让判死自己发生：
/// 清表 + 推前端快照 + 该转终止色的转终止色、该收回的收回。
pub fn sweep_stale_sessions(state: &Arc<AppState>, app: &AppHandle) {
    let (changed, running_lost, snapshot) = {
        let mut sessions = lock(&state.active_sessions);
        prune_and_snapshot(&mut sessions, bark_core::now_millis())
    };
    if !changed {
        return;
    }
    let _ = app.emit("bark://sessions", &snapshot);
    if running_lost {
        // 心跳判死 = 意外终止（agent 进程没了 / 回合被用户中断且没有终态事件）：
        // 还有别的活跃会话就按它们显示，一个不剩才转终止色。
        crate::glow::refresh_from_sessions(app, state, true);
        // 意外终止的事件通道（burst_only）不经过事件管道，音效在这里补上
        play_state_sound(state, "terminated");
    } else if snapshot.is_empty() && !crate::glow::current(state).is_terminal() {
        // 判死的是等待中的会话（警告色）：没有活跃会话了就该收回，别把警告色留在屏幕上
        crate::glow::reset(app, state);
    } else {
        crate::glow::refresh_from_sessions(app, state, false);
    }
}

/// 心跳/打断/终止事件 → 状态表跃迁。返回 true 表示状态表有变化（应推送前端）。
///
/// 心跳语义按 tool_name 是否存在区分：
/// - 无 tool_name（UserPromptSubmit）→ 新回合：Thinking，重置 started_at，记 prompt；
/// - 有 tool_name（PreToolUse）→ ToolRunning，累计 tool_calls。
/// 终态（RunCompleted/RunFailed/RunAborted）移除条目；SessionStart 不建条目（会话建立≠在跑）。
/// 工具失败（ToolFailed）当心跳：会话留在表里、相位回落 Thinking（工具级失败，
/// agent 会自行重试——它不是终态）。
/// 工具收尾（ToolFinished）同款当心跳：相位回落 Thinking。它是**等待状态解除的
/// 第一信号**——答完提问 / 批完权限之后到下一个工具开始之间的唯一事件，
/// 没有它「等待输入/等待确认」会一直卡到下一个 PreToolUse（实测 ZCode）。
///
/// 打断（PermissionRequired/InputRequired）分两种情形：
/// - 会话有条目 → 回合进行中被打断，置 Waiting 相位；
/// - 会话无条目 → 可能是 agent 不发心跳（Claude Code 系，打断是唯一信号，必须建档），
///   也可能是「空闲回声」：agent 发心跳（如 TraeCode），Stop 后紧跟一条 Notification。
///   两者用行为特征区分——只有 `finished` 里记录了「该会话刚因终态移除过条目」
///   （= 它会发心跳且刚才真的在跑、现已结束），宽限窗内的打断才判定为回声：
///   不建档、不通知，否则完成色会被误压成警告色。
/// 会话表更新入口（测试用包装：stale 恒为 false；生产管道走 `apply_session_event_inner`）
#[cfg(test)]
fn apply_session_event(
    sessions: &mut HashMap<String, SessionStatus>,
    finished: &mut HashMap<String, i64>,
    ev: &NormalizedEvent,
) -> SessionDelta {
    apply_session_event_inner(sessions, finished, ev, false)
}

/// `stale` = 这是一条陈旧补投（时间戳比现在老过判死阈值，离线兜底路径会送来
/// 最长 1 小时前的事件）。陈旧事件**整体不进状态表**：它声称的「正在跑/等待/
/// 结束」都早已不可信——建条目会被 sweep 判死（开局红光）、迟到的心跳会复活
/// 已终态会话、迟到的终态会把仍活着的会话误清场。事件本身仍入历史与通知
/// （这是离线兜底的本意，见 run_pipeline）。
fn apply_session_event_inner(
    sessions: &mut HashMap<String, SessionStatus>,
    finished: &mut HashMap<String, i64>,
    ev: &NormalizedEvent,
    stale: bool,
) -> SessionDelta {
    let mut delta = SessionDelta {
        changed: false,
        running_lost: false,
        suppressed_echo: false,
        phase_changed: false,
        urgent: false,
    };
    if stale {
        return delta;
    }
    let pruned = prune_stale(sessions, ev.timestamp);
    delta.changed = pruned.changed;
    delta.urgent = pruned.changed; // 判死淘汰 = 摘档，快照立即推
    delta.running_lost = pruned.running_lost;
    let key = session_key(&ev.agent, &ev.session_id);
    // 跃迁判定的基准：本事件处理前的相位（prune 之后、match 之前）。
    // 摘档后取不到 → None，与建档后的 Some(...) 天然不等。
    let prev_phase = sessions.get(&key).map(|s| s.phase.clone());
    match ev.kind {
        EventKind::RunCompleted | EventKind::RunFailed | EventKind::RunAborted => {
            // 回合结束 → 会话回到空闲，不再出现在「运行中」列表。
            // 移除的条目真实存在 = agent 会发心跳：记下终态时刻，供紧随其后的
            // 打断事件判别「空闲回声」。无心跳 agent 的终态本来就建不了档，
            // 不记录——它们之后的第一条打断仍按真等待处理（行为不变）。
            if sessions.remove(&key).is_some() {
                delta.changed = true;
                // 只保留宽限窗内的记录，天然有界（时钟回拨后「未来」的记录一并清掉）
                finished.retain(|_, t| echo_within_grace(*t, ev.timestamp));
                finished.insert(key.clone(), ev.timestamp);
            } else if finished
                .get(&key)
                .is_some_and(|t| echo_within_grace(*t, ev.timestamp))
            {
                // 终态回声：同一次回合结束会被两个事件报告（实测 Qoder 系是
                // Stop + SessionEnd，相隔约 150ms）。第二条不该再通知一次，
                // 也不该把刚亮起的完成色压成终止色——用户主动中断的回合则只有
                // SessionEnd 一条，不在此列，照常按「已中止」处理（收起流光，不亮终止色）。
                tracing::debug!(agent = %ev.agent, "terminal echo after terminal, suppress");
                delta.suppressed_echo = true;
            }
        }
        EventKind::PermissionRequired | EventKind::InputRequired => {
            // 打断：会话仍在，只是卡在等用户；无心跳的 agent（未注册 Activity
            // 事件的）也会经此路径建档，让「等待中」同样可见
            let phase = if ev.kind == EventKind::PermissionRequired {
                SessionPhase::WaitingPermission
            } else {
                SessionPhase::WaitingInput
            };
            match sessions.get_mut(&key) {
                Some(s) => {
                    if s.phase != phase {
                        s.phase = phase;
                        delta.changed = true;
                    }
                    s.last_activity = ev.timestamp;
                }
                None => {
                    // 无条目：先看是不是回合刚结束的空闲回声
                    if finished.get(&key).is_some_and(|t| echo_within_grace(*t, ev.timestamp)) {
                        tracing::debug!(agent = %ev.agent, "idle echo after terminal, skip");
                        delta.suppressed_echo = true;
                        return delta;
                    }
                    sessions.insert(key.clone(), new_session(ev, phase));
                    delta.changed = true;
                }
            }
        }
        EventKind::Activity => match sessions.get_mut(&key) {
            Some(s) => {
                s.last_activity = ev.timestamp;
                // cwd 兜底回填：插件型（OpenCode）的 cwd 靠插件侧按 sessionID 记忆补送，
                // daemon 晚启动 / 复用旧会话时首个心跳可能带空 cwd——之后任何带 cwd 的
                // 事件都回填，避免会话卡片与通知一直缺项目名
                if s.cwd.is_empty() && !ev.cwd.is_empty() {
                    s.cwd = ev.cwd.clone();
                    s.project = ev.project.clone();
                }
                match &ev.tool_name {
                    Some(tool) => {
                        s.phase = SessionPhase::ToolRunning;
                        s.last_tool = Some(tool.clone());
                        s.tool_calls = s.tool_calls.saturating_add(1);
                    }
                    None => {
                        // UserPromptSubmit：新回合开始
                        s.phase = SessionPhase::Thinking;
                        s.turn_count = s.turn_count.saturating_add(1);
                        s.started_at = ev.timestamp;
                        if !ev.message.is_empty() {
                            s.prompt = Some(bark_core::truncate_chars(&ev.message, 120));
                        }
                    }
                }
                delta.changed = true;
            }
            None => {
                // 带工具名的心跳**不可能开启新回合**：会话刚终态（宽限窗内）又来的
                // PreToolUse/PostToolUse 是乱序回声——Stop 与工具 hook 是两个独立进程，
                // 没有全局顺序保证（工具 hook 慢几百毫秒就会「先完成、后心跳」）。
                // 放它建档会把刚收起的完成色复活成思考色，10 分钟后被判死又亮终止色。
                // 不带工具名的心跳（UserPromptSubmit）是真实新回合，照常建档。
                if ev.tool_name.is_some()
                    && finished
                        .get(&key)
                        .is_some_and(|t| echo_within_grace(*t, ev.timestamp))
                {
                    tracing::debug!(agent = %ev.agent, "tool heartbeat echo after terminal, skip");
                    delta.suppressed_echo = true;
                    return delta;
                }
                let phase = if ev.tool_name.is_some() {
                    SessionPhase::ToolRunning
                } else {
                    SessionPhase::Thinking
                };
                sessions.insert(key.clone(), new_session(ev, phase));
                delta.changed = true;
            }
        },
        EventKind::ToolFailed => {
            // 工具级失败 ≠ 回合失败：agent 会自行重试（实测 ZCode 编辑前没先读文件，
            // 失败后立刻重试成功），会话**留在表里**继续表达「在跑」，只把相位回落
            // 「思考中」（失败的工具已不在执行）。不摘表、不通知、不触发终止色；
            // 事件本身仍入历史与事件流（中性色「工具失败」，可排查不吓人）。
            // 无条目时建档：PostToolUseFailure 可能是低频心跳 agent（Claude Code 系
            // 不订阅 PreToolUse）唯一的「正在跑」信号，漏建会让流光在失败瞬间熄掉。
            match sessions.get_mut(&key) {
                Some(s) => {
                    s.phase = SessionPhase::Thinking;
                    s.last_activity = ev.timestamp;
                    delta.changed = true;
                }
                None => {
                    let mut s = new_session(ev, SessionPhase::Thinking);
                    // 错误文本不是任务 prompt，不占 prompt 位（事件流里看得到）
                    s.prompt = None;
                    sessions.insert(key.clone(), s);
                    delta.changed = true;
                }
            }
        }
        EventKind::ToolFinished => {
            // 工具收尾（PostToolUse）当心跳：工具已结束、回合仍在跑，相位回落思考中，
            // 会话留在表里。它是**等待状态解除的第一信号**——答完 AskUserQuestion /
            // 批完权限之后到下一个工具开始之前没有任何 PreToolUse，不收它的话相位
            // 会一直卡在 Waiting*、警告色一直亮到下一个工具开始（实测 ZCode）。
            // 与 ToolFailed 同款：不摘表、不通知、不触发终止色；不入历史（见
            // run_pipeline），音效也不响（sound_key_for 落在通配臂）。
            match sessions.get_mut(&key) {
                Some(s) => {
                    s.phase = SessionPhase::Thinking;
                    s.last_activity = ev.timestamp;
                    delta.changed = true;
                }
                None => {
                    // 无条目 + 宽限窗内有终态记录 = 乱序回声：Stop 与工具 hook 是两个
                    // 独立进程、没有全局顺序保证，PostToolUse 慢几百毫秒就会「先完成、
                    // 后收尾」。放它建档会把刚收起的完成色复活成思考色，10 分钟后被
                    // 判死又亮终止色（同 Activity 分支对带工具名心跳的防回声处理）。
                    if finished
                        .get(&key)
                        .is_some_and(|t| echo_within_grace(*t, ev.timestamp))
                    {
                        tracing::debug!(agent = %ev.agent, "tool-finished echo after terminal, skip");
                        delta.suppressed_echo = true;
                        return delta;
                    }
                    // 真无条目（daemon 晚启动 / 低频信号 agent）：建档占位
                    let mut s = new_session(ev, SessionPhase::Thinking);
                    // 工具结果不是任务 prompt，不占 prompt 位
                    s.prompt = None;
                    sessions.insert(key.clone(), s);
                    delta.changed = true;
                }
            }
        }
        EventKind::SessionStart => {}
    }

    // 相位跃迁 = 建档 / 摘档 / 相位字段变化。同相位重复心跳（监控型 adapter 的
    // 保活）只刷 last_activity，不算跃迁——要在上限清理之前比对，那一步
    // 淘汰的是别的条目，与本事件的跃迁无关。
    delta.phase_changed = sessions.get(&key).map(|s| s.phase.clone()) != prev_phase;
    // 快照「立即推」的口径（§4.4）：本 key 的建档/摘档/相位变化（phase_changed 全覆盖）、
    // 判死淘汰（pruned）与上限淘汰都是结构变化，必须立即可见；只有纯 last_activity /
    // 计数刷新允许被 should_push_snapshot 节流
    delta.urgent = delta.urgent || delta.phase_changed;

    // 上限保护：超出时淘汰最久未活动的（防泄漏）
    if sessions.len() > ACTIVE_SESSIONS_CAP {
        let mut by_activity: Vec<(String, i64)> =
            sessions.iter().map(|(k, s)| (k.clone(), s.last_activity)).collect();
        by_activity.sort_by_key(|(_, at)| *at);
        for (k, _) in by_activity.into_iter().take(sessions.len() - ACTIVE_SESSIONS_CAP) {
            sessions.remove(&k);
        }
        delta.changed = true;
        delta.urgent = true; // 上限淘汰 = 摘档
    }
    delta
}

/// 「事件时间在终态时刻之后的宽限窗内」＝回声。回拨的系统时钟会让
/// `saturating_sub` 饱和为 0、恒小于宽限窗——所以必须同时要求
/// `ev_ts >= finished_t`：时钟回拨后到达的真实打断不得被当回声吞掉。
fn echo_within_grace(finished_t: i64, ev_ts: i64) -> bool {
    ev_ts >= finished_t && ev_ts.saturating_sub(finished_t) < IDLE_ECHO_GRACE_MS
}

fn new_session(ev: &NormalizedEvent, phase: SessionPhase) -> SessionStatus {
    // 新建条目时 tool_name 存在 → 首个心跳就是 PreToolUse：tool_calls 记 1
    SessionStatus {
        agent: ev.agent.clone(),
        session_id: ev.session_id.clone(),
        project: ev.project.clone(),
        cwd: ev.cwd.clone(),
        phase,
        prompt: if ev.message.is_empty() { None } else { Some(bark_core::truncate_chars(&ev.message, 120)) },
        last_tool: ev.tool_name.clone(),
        turn_count: 1,
        tool_calls: if ev.tool_name.is_some() { 1 } else { 0 },
        started_at: ev.timestamp,
        last_activity: ev.timestamp,
    }
}

/// 陈旧补投判定：事件时间戳比现在还老过判死阈值。离线兜底落盘的事件
/// （hook 写 pending.jsonl、daemon 启动补投，上限 1 小时）会带着旧时间戳
/// 重新进管道——它声称的「正在跑 / 等人」状态早已不可信，不应建档
/// （否则启动 30s 内就被 sweep 判死，开局红光 + 终止音）。事件本身仍入
/// 历史与通知。阈值与 sweep 判死同源（stale_after_ms，可用环境变量覆盖）。
fn is_stale_replay(ev: &NormalizedEvent) -> bool {
    bark_core::now_millis().saturating_sub(ev.timestamp) > stale_after_ms()
}

/// 分发：Bark / Webhook（桌面 Toast 与系统提示音已下线，声音走状态音效）
async fn dispatch(state: &Arc<AppState>, n: &Notification) {
    // 静音 + 免打扰都在**派发时**复查（§2.5）：入聚合窗时未免打扰、到点已免打扰的
    // 不再推送（聚合窗最长 10 分钟，跨过免打扰边界很常见）
    let channels = {
        let cfg = read(&state.config);
        let quiet = in_quiet_hours(&cfg.rules.quiet_hours);
        if !dispatch_gate(state.is_muted(), quiet) {
            tracing::info!("muted / quiet hours at dispatch, skip");
            return;
        }
        // 只 clone 渠道段（§4.3）：整份 BarkConfig 克隆在每条通知路径上都是纯浪费
        cfg.channels.clone()
    };

    let bark = channels.bark.filter(|c| c.enabled && !c.url.is_empty());
    let webhook = channels.webhook.filter(|c| c.enabled && !c.url.is_empty());

    if bark.is_some() || webhook.is_some() {
        let n = n.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(b) = bark {
                if let Err(e) = BarkChannel(b).send(&n) {
                    tracing::warn!("bark channel: {e:#}");
                }
            }
            if let Some(w) = webhook {
                if let Err(e) = WebhookChannel(w).send(&n) {
                    tracing::warn!("webhook channel: {e:#}");
                }
            }
        });
    }
}

/// 免打扰区间判断，支持 "23:00-08:00" 跨零点
pub fn in_quiet_hours(ranges: &[String]) -> bool {
    let now = chrono::Local::now();
    in_quiet_hours_at(ranges, now.hour() as i32 * 60 + now.minute() as i32)
}

/// 纯函数版本，便于测试
pub fn in_quiet_hours_at(ranges: &[String], mins: i32) -> bool {
    if ranges.is_empty() {
        return false;
    }
    for r in ranges {
        let Some((start, end)) = parse_hhmm_range(r) else {
            tracing::warn!("无法解析免打扰区间: {r}");
            continue;
        };
        let hit = if start <= end {
            mins >= start && mins < end
        } else {
            mins >= start || mins < end
        };
        if hit {
            return true;
        }
    }
    false
}

fn parse_hhmm_range(s: &str) -> Option<(i32, i32)> {
    let (a, b) = s.split_once('-')?;
    let (start, end) = (parse_hhmm(a)?, parse_hhmm(b)?);
    // start == end 的区间「头含尾不含」恒为空集（"23:00-23:00" 永非免打扰），
    // 按非法配置跳过并留痕——用户多半想写的是跨零点的 "23:00-23:00" 之外的东西，
    // 静默接受会让免打扰「配了但不生效」，是最难排查的形态
    if start == end {
        tracing::warn!("免打扰区间起止相同（{s}），恒为空集，已按非法跳过");
        return None;
    }
    Some((start, end))
}

fn parse_hhmm(s: &str) -> Option<i32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: i32 = h.trim().parse().ok()?;
    let m: i32 = m.trim().parse().ok()?;
    if h == 24 && m == 0 {
        return Some(24 * 60);
    }
    if (0..24).contains(&h) && (0..60).contains(&m) {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// 按当前配置同步监控型 adapter（启动/停止）。
/// 返回运行时状态，供 UI 区分「真的在跑」和「勾了但起不来」。
pub fn sync_watchers(tx: mpsc::Sender<NormalizedEvent>, state: &Arc<AppState>) -> bark_adapters::registry::WatchRuntime {
    use bark_adapters::registry::WatchRuntime;
    use std::sync::atomic::Ordering;

    let mut runtime = WatchRuntime::default();
    let cfg = read(&state.config);
    let mut watchers = lock(&state.watchers);

    for adapter in bark_adapters::registry::watch_adapters() {
        let id = adapter.kind().id().to_string();
        let want = cfg.agent_enabled(&id) && adapter.is_installed() && adapter.is_available();
        let running = watchers.contains_key(&id);

        if !adapter.is_available() {
            if let Some(reason) = adapter.unavailable_reason() {
                runtime.errors.insert(id.clone(), reason);
            }
        }

        match (want, running) {
            (true, false) => {
                let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                match adapter.spawn(stop.clone(), tx.clone()) {
                    Ok(handle) => {
                        watchers.insert(id.clone(), (stop, handle));
                        runtime.running.insert(id.clone(), true);
                        tracing::info!(agent = %id, "watcher started");
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        runtime.errors.insert(id.clone(), msg.clone());
                        tracing::warn!(agent = %id, "watcher failed: {msg}");
                    }
                }
            }
            (false, true) => {
                if let Some((stop, _)) = watchers.remove(&id) {
                    stop.store(true, Ordering::Relaxed);
                    tracing::info!(agent = %id, "watcher stopping");
                }
            }
            (true, true) => {
                runtime.running.insert(id.clone(), true);
            }
            _ => {}
        }
    }
    runtime
}

/// 只读的监控运行时快照（UI 状态展示用，不启停任何东西）
pub fn watch_runtime(state: &Arc<AppState>) -> bark_adapters::registry::WatchRuntime {
    use bark_adapters::registry::{watch_adapters, WatchRuntime};
    let mut runtime = WatchRuntime::default();
    let watchers = lock(&state.watchers);
    for id in watchers.keys() {
        runtime.running.insert(id.clone(), true);
    }
    for adapter in watch_adapters() {
        if !adapter.is_available() {
            if let Some(reason) = adapter.unavailable_reason() {
                runtime.errors.insert(adapter.kind().id().to_string(), reason);
            }
        }
    }
    runtime
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, ts: i64, tool: Option<&str>, msg: &str) -> NormalizedEvent {
        NormalizedEvent {
            id: format!("id-{ts}"),
            agent: "trae-code".into(),
            kind,
            session_id: "s1".into(),
            cwd: "C:/p".into(),
            project: Some("p".into()),
            message: msg.into(),
            timestamp: ts,
            is_subagent: false,
            tool_name: tool.map(str::to_string),
        }
    }

    /// (活跃表, 终态记录表) 组合，测试用
    fn table() -> (HashMap<String, SessionStatus>, HashMap<String, i64>) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn sound_key_follows_event_roles() {
        let mut e = ev(EventKind::RunCompleted, 1000, None, "");
        assert_eq!(sound_key_for(&e, false), Some("completed"));
        e.kind = EventKind::RunFailed;
        assert_eq!(sound_key_for(&e, false), Some("terminated"));
        // 用户主动中止不响（与流光同口径：那是你自己按的停止）
        e.kind = EventKind::RunAborted;
        assert_eq!(sound_key_for(&e, false), None);
        e.kind = EventKind::PermissionRequired;
        assert_eq!(sound_key_for(&e, false), Some("warning"));
        e.kind = EventKind::InputRequired;
        assert_eq!(sound_key_for(&e, false), Some("warning"));
        // 新回合开始（无工具名的心跳）= 思考；同回合的工具心跳不重复响
        e.kind = EventKind::Activity;
        e.tool_name = None;
        assert_eq!(sound_key_for(&e, false), Some("thinking"));
        e.tool_name = Some("Bash".into());
        assert_eq!(sound_key_for(&e, false), None);
        // 工具收尾不响：每次工具结束都来一条，会把「新回合开始」的语义淹没
        e.kind = EventKind::ToolFinished;
        assert_eq!(sound_key_for(&e, false), None);
        // 心跳判死优先于事件本身：agent 进程没了 = 意外终止
        assert_eq!(sound_key_for(&e, true), Some("terminated"));
        // 用户主动中止永远不响，即便同一瞬间恰好有别的会话被判死
        // （音与流光的优先级必须一致：derive 里 RunAborted 也先于判死收光）
        let mut a = ev(EventKind::RunAborted, 1000, None, "");
        assert_eq!(sound_key_for(&a, true), None);
        a.kind = EventKind::RunCompleted;
        assert_eq!(sound_key_for(&a, true), Some("terminated"), "普通终态事件在判死时仍报终止音");
    }

    #[test]
    fn thinking_sound_gated_on_phase_transition() {
        assert!(should_play_state_sound("thinking", true), "建档/真实跃迁照常响");
        assert!(!should_play_state_sound("thinking", false), "保活心跳不重播思考音");
        // 其余状态音不被门拦：终态会把条目摘掉（无从比对相位），打断是一次性事件
        assert!(should_play_state_sound("completed", false));
        assert!(should_play_state_sound("terminated", false));
        assert!(should_play_state_sound("warning", false));
    }

    #[test]
    fn keepalive_heartbeat_is_not_a_phase_transition() {
        // 监控型 adapter（WorkBuddy）的保活心跳与真实新回合是同一种事件
        // （无工具名的 Activity）：已建档且相位不变的心跳不是跃迁，思考音靠
        // 这道区别挡住，否则思考期间每个保活周期（60s）凭空响一声。
        let (mut sessions, mut finished) = table();
        // 首条心跳建档 = 跃迁：思考音该响这一次
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, None, ""));
        assert!(d.changed && d.phase_changed);
        // 60s 后的保活：仍要刷新 last_activity（否则 10 分钟被判死），但不是跃迁
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 61_000, None, ""));
        assert!(d.changed, "保活心跳照常刷新活跃时间");
        assert!(!d.phase_changed, "同相位重复心跳不是跃迁");
        // 真实相位变化仍是跃迁（工具开始 / 收尾 / 打断）
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 62_000, Some("Bash"), ""));
        assert!(d.phase_changed);
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFinished, 63_000, Some("Bash"), ""));
        assert!(d.phase_changed, "工具执行 → 思考是真实回落");
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFinished, 64_000, Some("Bash"), ""));
        assert!(!d.phase_changed, "思考期内的重复收尾心跳不是跃迁");
        // 终态摘档 = 跃迁；其后的终态回声（条目已不在）不是
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 65_000, None, "done"));
        assert!(d.changed && d.phase_changed);
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 66_000, None, "done"));
        assert!(d.suppressed_echo);
        assert!(!d.phase_changed, "终态回声不是跃迁");
    }

    #[test]
    fn late_tool_heartbeat_does_not_revive_finished_session() {
        // Stop 与工具 hook 是独立进程，无顺序保证：RunCompleted 先到、
        // 带工具名的心跳（PreToolUse/PostToolUse）后到 = 乱序回声。
        // 不得复活会话（否则 10 分钟后被判死，亮终止色 + 播终止音）。
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 2000, None, "done")).changed);
        assert!(sessions.is_empty());

        // 宽限窗内的工具心跳：抑制，不建档
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 2500, Some("Edit"), ""));
        assert!(d.suppressed_echo, "宽限窗内的工具心跳必须判为回声");
        assert!(!d.changed);
        assert!(sessions.is_empty(), "会话不得被迟到心跳复活");
        // 宽限窗外的工具心跳：建档（它可能真是新一轮的起点信号，如低频 agent）
        let d = apply_session_event(
            &mut sessions,
            &mut finished,
            &ev(EventKind::Activity, 2000 + IDLE_ECHO_GRACE_MS + 1, Some("Edit"), ""),
        );
        assert!(d.changed && !d.suppressed_echo);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);
    }

    #[test]
    fn late_prompt_heartbeat_still_starts_new_turn() {
        // 不带工具名的心跳（UserPromptSubmit）是真实新回合：即便紧跟着上一回合的
        // 终态到达，也必须建档——否则用户快速追问时新回合前 10s 不可见
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 2000, None, "done"));
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 2200, None, "再问一句"));
        assert!(d.changed && !d.suppressed_echo, "新回合的 UserPromptSubmit 不是回声");
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::Thinking);
    }

    #[test]
    fn stale_replay_does_not_touch_session_table() {
        // 离线兜底补投的事件（时间戳老过判死阈值）：声称的任何状态都不可信，
        // 整体不进状态表；新鲜事件照常
        let (mut sessions, mut finished) = table();
        let old = ev(EventKind::Activity, 1000, Some("Bash"), "");
        let d = apply_session_event_inner(&mut sessions, &mut finished, &old, true);
        assert!(!d.changed && !d.running_lost, "陈旧心跳不得建档");
        assert!(sessions.is_empty());

        let old_wait = ev(EventKind::InputRequired, 1000, None, "");
        let d = apply_session_event_inner(&mut sessions, &mut finished, &old_wait, true);
        assert!(!d.changed, "陈旧等待不得建档");

        // 活着的会话不会被一条陈旧终态清场（那是 40 分钟前旧回合的补投）
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 2000, Some("Bash"), ""));
        let old_done = ev(EventKind::RunCompleted, 1000, None, "");
        let d = apply_session_event_inner(&mut sessions, &mut finished, &old_done, true);
        assert!(!d.changed, "陈旧终态不得清掉活跃会话");
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn session_lifecycle_from_prompt_to_completion() {
        let (mut sessions, mut finished) = table();
        // UserPromptSubmit → Thinking，记 prompt
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, None, "帮我修 bug")).changed);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::Thinking);
        assert_eq!(s.prompt.as_deref(), Some("帮我修 bug"));
        assert_eq!(s.turn_count, 1);
        // PreToolUse → ToolRunning，累计工具次数
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 2000, Some("Bash"), "")).changed);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::ToolRunning);
        assert_eq!(s.last_tool.as_deref(), Some("Bash"));
        assert_eq!(s.tool_calls, 1);
        // 下一个 UserPromptSubmit → 新回合，重置 started_at
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 3000, None, "再写个测试")).changed);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::Thinking);
        assert_eq!(s.turn_count, 2);
        assert_eq!(s.started_at, 3000);
        // 权限打断 → WaitingPermission（工具数不变）
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::PermissionRequired, 3500, None, "")).changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::WaitingPermission);
        // 授权后继续干活 → 回到 ToolRunning
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 4000, Some("Edit"), "")).changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);
        // Stop → 移除
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 5000, None, "done")).changed);
        assert!(sessions.is_empty());
        // 再收到终态：表已空，无变化
        assert!(!apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 6000, None, "")).changed);
    }

    #[test]
    fn activity_backfills_missing_cwd() {
        // 插件型（OpenCode）首个心跳可能带空 cwd（插件侧 cwd 记忆尚未建立），
        // 之后带 cwd 的事件必须回填，会话卡片/通知不能一直缺项目名
        let (mut sessions, mut finished) = table();
        let mut first = ev(EventKind::Activity, 1000, None, "");
        first.cwd = String::new();
        first.project = None;
        apply_session_event(&mut sessions, &mut finished, &first);
        assert_eq!(sessions["trae-code|s1"].cwd, "");

        let mut later = ev(EventKind::Activity, 2000, Some("Bash"), "");
        later.cwd = r"D:\work\demo".into();
        later.project = Some("demo".into());
        apply_session_event(&mut sessions, &mut finished, &later);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.cwd, r"D:\work\demo");
        assert_eq!(s.project.as_deref(), Some("demo"));
        // 已有 cwd 不被后续事件覆盖（同会话 cwd 理论不变，防乱序事件抖动）
        let mut another = ev(EventKind::Activity, 3000, None, "");
        another.cwd = r"D:\elsewhere".into();
        apply_session_event(&mut sessions, &mut finished, &another);
        assert_eq!(sessions["trae-code|s1"].cwd, r"D:\work\demo");
    }

    #[test]
    fn tool_finished_returns_phase_to_thinking() {
        // PostToolUse 是等待状态解除的第一信号：
        // - 工具收尾 → 思考中（工具之间的间隙也是思考期）
        // - 等待输入被回答 → 思考中（实测 ZCode：答完 AskUserQuestion 后警告色
        //   一直亮到下一个工具开始，就是缺这条信号）
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);

        // 工具收尾：相位回落思考中，会话留在表里，工具次数不重复累计
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFinished, 2000, Some("Bash"), ""));
        assert!(d.changed);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::Thinking);
        assert_eq!(s.tool_calls, 1, "收尾不算新的一次工具调用");
        assert_eq!(s.last_activity, 2000, "工具收尾兼作心跳，刷新活跃时间");

        // 等待输入（提问挂起）→ 收尾信号到达（用户已答完）→ 立即回到思考中
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, 3000, None, ""));
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::WaitingInput);
        let d = apply_session_event(
            &mut sessions,
            &mut finished,
            &ev(EventKind::ToolFinished, 4000, Some("AskUserQuestion"), ""),
        );
        assert!(d.changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::Thinking, "答完提问的收尾信号必须解除等待");

        // 下一个工具 → 回到执行中；回合正常收尾不受影响
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 5000, Some("Edit"), ""));
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 6000, None, "done")).changed);
        assert!(sessions.is_empty());
    }

    #[test]
    fn tool_finished_after_terminal_is_echo_within_grace() {
        // Stop 与工具 hook 是两个独立进程，PostToolUse 乱序晚到 = 回声：
        // 宽限窗内不得复活会话（否则思考色挂 10 分钟后被判死又亮终止色）
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 2000, None, "done"));
        assert!(sessions.is_empty());

        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFinished, 2500, Some("Bash"), ""));
        assert!(d.suppressed_echo, "宽限窗内的乱序收尾必须判为回声");
        assert!(!d.changed);
        assert!(sessions.is_empty(), "回声不得把刚结束的会话重新建档");

        // 宽限窗之外：按真实信号建档（daemon 晚启动 / 低频信号 agent 的占位）
        let late = 2000 + IDLE_ECHO_GRACE_MS + 1;
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFinished, late, Some("Bash"), ""));
        assert!(d.changed && !d.suppressed_echo);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::Thinking);
    }

    #[test]
    fn tool_failure_keeps_session_running() {
        // 工具失败 ≠ 回合失败（实测 ZCode 编辑前没先读文件、失败后立刻重试成功）：
        // 会话必须留在运行中表里、相位回落思考中——既不摘表也不判终态
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 2000, Some("Edit"), ""));

        // 工具失败：留在表中，ToolRunning → Thinking
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::ToolFailed, 2500, Some("Edit"), "File has not been read yet"));
        assert!(d.changed);
        assert!(!d.suppressed_echo);
        let s = &sessions["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::Thinking, "失败的工具已不在执行，相位回落思考中");
        assert_eq!(s.last_activity, 2500, "工具失败兼作心跳，刷新活跃时间");

        // 重试成功 → 回到 ToolRunning；回合正常收尾不受这次失败影响
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 3000, Some("Edit"), ""));
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 4000, None, "done")).changed);
        assert!(sessions.is_empty(), "回合结束后照常清场");

        // 无条目时的工具失败要建档：低频心跳 agent（Claude Code 系不订阅 PreToolUse）
        // 唯一的「正在跑」信号就是它，漏建会让流光在失败瞬间熄掉
        let (mut sessions2, mut finished2) = table();
        let d = apply_session_event(&mut sessions2, &mut finished2, &ev(EventKind::ToolFailed, 1000, Some("Edit"), "boom"));
        assert!(d.changed);
        let s = &sessions2["trae-code|s1"];
        assert_eq!(s.phase, SessionPhase::Thinking);
        assert!(s.prompt.is_none(), "错误文本不是任务 prompt，不占 prompt 位");
    }

    #[test]
    fn terminal_echo_after_terminal_is_suppressed_but_interrupt_is_not() {
        // Qoder 系实测序列：正常回合是 Stop(RunCompleted) + SessionEnd(RunAborted) 相隔约 150ms；
        // 用户主动中断的回合只有 SessionEnd。前者必须被当回声抑制，后者要照常清场。
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 2000, None, ""));
        assert!(sessions.is_empty(), "Stop 已把会话移出运行中列表");

        // 紧随其后的 SessionEnd（映射为 RunAborted）= 终态回声
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunAborted, 2150, None, ""));
        assert!(d.suppressed_echo, "正常回合的第二条终态事件必须被抑制（否则会多一条通知、完成色被压成终止色）");
        assert!(!d.changed);
        assert!(sessions.is_empty());

        // 用户中断：只有 SessionEnd，没有 Stop —— 该会话此前在运行中，必须清场
        let (mut sessions2, mut finished2) = table();
        apply_session_event(&mut sessions2, &mut finished2, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        let d = apply_session_event(&mut sessions2, &mut finished2, &ev(EventKind::RunAborted, 3000, None, ""));
        assert!(!d.suppressed_echo, "中断回合的 SessionEnd 不是回声");
        assert!(d.changed);
        assert!(sessions2.is_empty(), "中断后不得继续留在运行中列表");
    }

    #[test]
    fn idle_echo_after_completion_does_not_relight_waiting() {
        // TraeCode 实测序列：回合结束后 Stop 立即跟一条 Notification，
        // 流光应在完成色后停留淡出，而不是被压成警告色
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, None, "问题"));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 2000, None, ""));
        assert!(sessions.is_empty());

        // 紧随其后的打断 = 空闲回声：不建档、标记回声
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, 2500, None, ""));
        assert!(d.suppressed_echo);
        assert!(!d.changed);
        assert!(sessions.is_empty(), "回声不得把刚结束的会话重新建档");
        // 权限请求类同理
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::PermissionRequired, 2600, None, ""));
        assert!(d.suppressed_echo);
        assert!(sessions.is_empty());

        // 宽限窗过后：视为真实等待，正常建档（orange）
        let late = 2000 + IDLE_ECHO_GRACE_MS + 1;
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, late, None, ""));
        assert!(!d.suppressed_echo);
        assert!(d.changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::WaitingInput);

        // 真实等待中又来一轮「新回合 → 完成 → 回声」：回声只压刚结束的那次
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, late + 100, None, "继续"));
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, late + 200, None, ""));
        assert!(sessions.is_empty());
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, late + 250, None, ""));
        assert!(d.suppressed_echo);
        assert!(sessions.is_empty());
    }

    #[test]
    fn no_heartbeat_agent_waiting_after_completion_still_visible() {
        // Claude Code 系（无心跳）：终态事件建不了档 → 不记终态时刻，
        // 其后哪怕紧贴着的打断也按真等待建档（行为与修复前一致）
        let (mut sessions, mut finished) = table();
        assert!(!apply_session_event(&mut sessions, &mut finished, &ev(EventKind::RunCompleted, 1000, None, "")).changed);
        let d = apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, 1100, None, "问题?"));
        assert!(!d.suppressed_echo);
        assert!(d.changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::WaitingInput);
    }

    #[test]
    fn waiting_event_creates_entry_without_heartbeat() {
        // 未注册心跳事件的 agent（如 claude-code 默认配置）：打断事件也能建档，
        // 「等待中」同样可见
        let (mut sessions, mut finished) = table();
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, 1000, None, "问题?")).changed);
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::WaitingInput);
        // 同相位重复打断：无变化
        assert!(!apply_session_event(&mut sessions, &mut finished, &ev(EventKind::InputRequired, 1100, None, "")).changed);
    }

    #[test]
    fn stale_sessions_are_pruned() {
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Read"), ""));
        // 长时间无心跳（agent 被杀）：下一个事件触发惰性淘汰
        let late = 1000 + STALE_AFTER_MS + 1;
        assert!(apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, late, Some("Read"), "")).changed);
        // 新事件自己建了新条目（旧的被淘汰），turn_count 从 1 重新开始
        assert_eq!(sessions["trae-code|s1"].turn_count, 1);
        // 快照也会剔除僵死条目
        let (mut old, mut finished2) = table();
        apply_session_event(&mut old, &mut finished2, &ev(EventKind::Activity, 1000, Some("Read"), ""));
        assert!(sessions_snapshot(&old, 1000 + STALE_AFTER_MS).is_empty());
    }

    #[test]
    fn stale_running_session_is_reported_as_unexpected_stop() {
        // 流光靠 running_lost 显示红色：agent 进程没了但不会有 Stop 事件
        let late = 1000 + STALE_AFTER_MS + 1;
        let (mut s, mut finished) = table();
        apply_session_event(&mut s, &mut finished, &ev(EventKind::Activity, 1000, Some("Read"), ""));
        assert!(apply_session_event(&mut s, &mut finished, &ev(EventKind::Activity, late, Some("Read"), "")).running_lost);

        // 等待中的会话被判死不算「意外终止」——它本来就在等人，不是跑崩了
        let (mut waiting, mut finished2) = table();
        apply_session_event(&mut waiting, &mut finished2, &ev(EventKind::InputRequired, 1000, None, "问题?"));
        let d = apply_session_event(&mut waiting, &mut finished2, &ev(EventKind::Activity, late, Some("Read"), ""));
        assert!(!d.running_lost);
    }

    #[test]
    fn clear_history_by_kind_and_all() {
        let state = AppState::new(BarkConfig::default(), std::path::PathBuf::from("config.json"));
        let ids = {
            let mut h = lock(&state.history);
            h.push_back(ev(EventKind::RunCompleted, 1000, None, ""));
            h.push_back(ev(EventKind::RunFailed, 2000, None, ""));
            h.push_back(ev(EventKind::RunCompleted, 3000, None, ""));
            h.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
        };
        // 「清空当前筛选」：只掉该类型，别的类型必须留下（否则等于误删）；
        // 返回值必须是被删事件的 id（前端按它删本地副本，不能按本地快照删）
        let mut cleared = state.clear_history(Some(EventKind::RunCompleted));
        cleared.sort();
        let mut want = vec![ids[0].clone(), ids[2].clone()];
        want.sort();
        assert_eq!(cleared, want, "只应清掉 RunCompleted 的两条");
        let left: Vec<EventKind> = lock(&state.history).iter().map(|e| e.kind).collect();
        assert_eq!(left, vec![EventKind::RunFailed]);

        // 「清空显示」：全部清空，返回剩下那条的 id
        assert_eq!(state.clear_history(None), vec![ids[1].clone()]);
        assert!(lock(&state.history).is_empty());
        // 空历史重复清空：幂等，返回空列表
        assert!(state.clear_history(None).is_empty());
        assert!(state.clear_history(Some(EventKind::RunFailed)).is_empty());
    }

    #[test]
    fn startup_warnings_are_cleared_per_agent() {
        let state = AppState::new(BarkConfig::default(), std::path::PathBuf::from("config.json"));
        state.set_startup_warnings(vec![
            StartupWarning { agent: "qoder".into(), message: "qoder 告警".into() },
            StartupWarning { agent: "dsh".into(), message: "dsh 告警".into() },
        ]);
        assert_eq!(state.startup_warnings(), vec!["qoder 告警", "dsh 告警"]);

        // 用户处理了 qoder：只清它那条，别的 agent 的告警不该被顺手抹掉
        state.clear_startup_warnings("qoder");
        assert_eq!(state.startup_warnings(), vec!["dsh 告警"]);
        // 重复清理与未知 agent：幂等
        state.clear_startup_warnings("qoder");
        state.clear_startup_warnings("nope");
        assert_eq!(state.startup_warnings(), vec!["dsh 告警"]);
    }

    #[test]
    fn drop_agent_sessions_matches_whole_agent_id() {
        let state = AppState::new(BarkConfig::default(), std::path::PathBuf::from("config.json"));
        {
            let mut s = lock(&state.active_sessions);
            for key in ["ds|s1", "dsh|s1", "dsh|s2"] {
                let mut e = ev(EventKind::Activity, 1000, Some("Read"), "");
                e.agent = key.split('|').next().unwrap().to_string();
                e.session_id = key.split('|').nth(1).unwrap().to_string();
                s.insert(key.to_string(), new_session(&e, SessionPhase::ToolRunning));
            }
        }
        // 前缀必须带分隔符匹配："ds" 不能摘掉 "dsh" 的会话
        let (changed, empty) = drop_agent_sessions(&state, "ds");
        assert!(changed && !empty, "只摘掉 ds 自己那条");
        assert_eq!(lock(&state.active_sessions).len(), 2);
        let (changed, empty) = drop_agent_sessions(&state, "dsh");
        assert!(changed && empty, "dsh 两条都摘掉后表为空");
        assert!(lock(&state.active_sessions).is_empty());
        // 幂等：没有该 agent 的条目时什么都不发生
        assert_eq!(drop_agent_sessions(&state, "dsh"), (false, true));
    }

    /// 摘完会话后的流光善后必须覆盖三种情形：漏掉 Refresh 会把僵尸等待色（橙色）留在
    /// 屏幕上，漏掉 Reset 会留下一盏熄不掉的灯——两者都实测过（后者正是「关闭最后一个
    /// 在跑的 agent 接入后灯一直亮」的根因）。
    #[test]
    fn glow_after_drop_covers_all_three_cases() {
        // 表空了、当前不是终态 → 收回
        assert_eq!(glow_after_drop(true, false), GlowAfterDrop::Reset);
        // 表空了但当前是终态绿/红 → 不打断，让它自己淡出
        assert_eq!(glow_after_drop(true, true), GlowAfterDrop::Keep);
        // 还有别的会话在跑 → 按剩余会话重新推导（哪怕当前亮的是终态）
        assert_eq!(glow_after_drop(false, false), GlowAfterDrop::Refresh);
        assert_eq!(glow_after_drop(false, true), GlowAfterDrop::Refresh);
    }

    #[test]
    fn sessions_for_ui_hides_disabled_agents() {
        let mut cfg = BarkConfig::default();
        cfg.set_agent_enabled("dsh", false); // 显式关闭
        let state = AppState::new(cfg, std::path::PathBuf::from("config.json"));
        let now = bark_core::now_millis();
        {
            let mut s = lock(&state.active_sessions);
            for (agent, sid) in [("dsh", "s1"), ("qoder", "s2")] {
                let mut e = ev(EventKind::Activity, now, Some("Read"), "");
                e.agent = agent.to_string();
                e.session_id = sid.to_string();
                s.insert(format!("{agent}|{sid}"), new_session(&e, SessionPhase::ToolRunning));
            }
        }
        let out = sessions_for_ui(&state);
        assert_eq!(out.len(), 1, "已关闭接入的 agent 不该出现在面板里");
        assert_eq!(out[0].agent, "qoder");
        // 只是不展示、条目本身保留：重新开启后不必等新事件就能再看到它（僵死照旧由 GC 淘汰）
        assert_eq!(lock(&state.active_sessions).len(), 2);
    }

    #[test]
    fn quiet_hours() {
        assert!(in_quiet_hours(&["00:00-24:00".into()]));
        // 跨零点：23:00-08:00 在 02:00 命中、在 12:00 不命中
        let cross = vec!["23:00-08:00".to_string()];
        assert!(in_quiet_hours_at(&cross, 2 * 60));
        assert!(!in_quiet_hours_at(&cross, 12 * 60));
        // 普通区间
        let day = vec!["12:30-13:30".to_string()];
        assert!(in_quiet_hours_at(&day, 13 * 60));
        assert!(!in_quiet_hours_at(&day, 14 * 60));
        // 空配置
        assert!(!in_quiet_hours_at(&[], 0));
    }

    #[test]
    fn parse_range() {
        assert_eq!(parse_hhmm_range("23:00-08:00"), Some((1380, 480)));
        assert_eq!(parse_hhmm_range("bad"), None);
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("25:00"), None);
    }

    #[test]
    fn toggle_mute_flips_back() {
        // B1 回归：连续切换两次必须回到未静音（swap(true) 的旧实现会卡在静音态）
        let state = AppState::new(BarkConfig::default(), std::path::PathBuf::from("config.json"));
        assert!(!state.is_muted());
        assert!(state.toggle_mute());
        assert!(state.is_muted());
        assert!(!state.toggle_mute());
        assert!(!state.is_muted());
        // 再切一轮确认可反复切换
        assert!(state.toggle_mute());
        assert!(state.is_muted());
    }

    #[test]
    fn sweep_prunes_dead_running_session_and_flags_running_lost() {
        // 用户实测场景：ZCode 手动终止后既没有 Stop 也没有任何终态事件，
        // 流光一直停在思考色——必须靠定时巡检把这条会话判死（→ 终止色 + 清表）
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));
        assert_eq!(sessions["trae-code|s1"].phase, SessionPhase::ToolRunning);

        let (changed, running_lost, snapshot) = prune_and_snapshot(&mut sessions, 1000 + STALE_AFTER_MS);
        assert!(changed, "僵死条目必须被淘汰");
        assert!(running_lost, "运行中的会话判死 = 意外终止（流光转终止色）");
        assert!(snapshot.is_empty(), "快照里不该再有僵死条目");
        assert!(sessions.is_empty(), "状态表本身也要清掉，不能只靠快照过滤");
    }

    #[test]
    fn sweep_prunes_dead_waiting_session_without_claiming_running_lost() {
        // 等待中的会话判死不是「意外终止」：不该转终止色，而是收回光标（调用方负责 reset）
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::PermissionRequired, 1000, None, ""));

        let (changed, running_lost, snapshot) = prune_and_snapshot(&mut sessions, 1000 + STALE_AFTER_MS);
        assert!(changed);
        assert!(!running_lost, "等待中的会话超时不是「运行中判死」");
        assert!(snapshot.is_empty());
        assert!(sessions.is_empty());
    }

    #[test]
    fn sweep_keeps_live_sessions_and_pushes_nothing() {
        let (mut sessions, mut finished) = table();
        apply_session_event(&mut sessions, &mut finished, &ev(EventKind::Activity, 1000, Some("Bash"), ""));

        // 差 1ms 未到判死线：不动状态表、不推快照（避免每 30s 一次无意义的 IPC 与重绘）
        let (changed, running_lost, snapshot) = prune_and_snapshot(&mut sessions, 1000 + STALE_AFTER_MS - 1);
        assert!(!changed && !running_lost);
        assert!(snapshot.is_empty());
        assert_eq!(sessions.len(), 1, "还活着的会话不能被误杀");
    }

    #[test]
    fn stale_override_parsing() {
        // 有 agent 被中断时一个事件都不发（实测 ZCode），只能靠心跳静止判死；
        // 这个覆盖值让「等多久才判死」可调（本机验证时也能用它把等待缩短）
        assert_eq!(parse_stale_override("60000"), Some(60_000));
        assert_eq!(parse_stale_override(" 45000 "), Some(45_000));
        assert_eq!(parse_stale_override("4999"), None, "低于 5s 的覆盖值误杀风险太大，忽略");
        assert_eq!(parse_stale_override("abc"), None);
        assert_eq!(parse_stale_override(""), None);
        assert_eq!(parse_stale_override("-1000"), None);
        // 未设置覆盖时用默认值（设置了就以环境为准，这里不强断）
        if std::env::var(STALE_ENV).is_err() {
            assert_eq!(stale_after_ms(), STALE_AFTER_MS);
        }
    }

    #[test]
    fn sweep_keeps_other_live_sessions_when_one_dies() {
        // 一个会话判死、另一个还在跑：只清死的那条（流光的优先级判定在 glow 侧）。
        // 这里直接构造状态表——走 apply_session_event 会在事件时间戳上先自行淘汰一次，
        // 把「死的那条」提前清掉，测不出巡检本身的行为。
        let now = 1_000_000i64;
        let mk = |sid: &str, last: i64| SessionStatus {
            agent: "zcode".into(),
            session_id: sid.into(),
            project: Some("p".into()),
            cwd: "C:/p".into(),
            phase: SessionPhase::ToolRunning,
            prompt: None,
            last_tool: Some("Bash".into()),
            turn_count: 1,
            tool_calls: 1,
            started_at: last,
            last_activity: last,
        };
        let mut sessions = HashMap::new();
        sessions.insert("zcode|dead".into(), mk("dead", now - STALE_AFTER_MS));
        sessions.insert("zcode|live".into(), mk("live", now - 1000));

        let (changed, running_lost, snapshot) = prune_and_snapshot(&mut sessions, now);
        assert!(changed && running_lost, "死的那条算判死");
        assert_eq!(snapshot.len(), 1, "活着的那条仍在快照里");
        assert_eq!(snapshot[0].session_id, "live");
        assert_eq!(sessions.len(), 1);
        assert!(sessions.contains_key("zcode|live"));
    }

    fn notification(body: &str) -> Notification {
        Notification {
            title: "t".into(),
            body: body.into(),
            event: EventKind::RunCompleted,
            agent: "trae-work".into(),
            project: None,
        }
    }

    /// 测试用临时目录（不引 uuid 依赖：时间戳 + 进程内自增计数足够唯一）
    fn temp_dir() -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "agent-bark-test-{}-{}",
            bark_core::now_millis(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// §1.4 回归①：同 key 两事件落聚合窗内 → 一条通知 +「合并了 1 条」。
    /// 旧实现在 6s 窗内把第二条整条丢弃（不通知、不入 pending、不计 merged），
    /// 「合并了 N 条」在默认配置下永远走不到。
    #[test]
    fn same_key_events_in_window_merge_into_one_notification() {
        let mut pending: HashMap<String, PendingNotify> = HashMap::new();
        let key = "trae-work|run_completed|s1".to_string();
        // 第一条：无 pending → 进聚合窗
        assert_eq!(notify_action(false, pending.contains_key(&key), false), NotifyAction::Enqueue);
        pending.insert(key.clone(), PendingNotify { notification: notification("done"), merged: 0, seq: 1 });
        // 第二条（聚合窗内）：命中 pending → 合并计数，绝不丢
        assert_eq!(notify_action(false, pending.contains_key(&key), true), NotifyAction::Merge);
        pending.get_mut(&key).unwrap().merged += 1;
        // 到点仍只派发一条通知，附注合并条数
        let p = claim_pending(&mut pending, &key, 1).expect("本代定时器正常认领");
        assert_eq!(p.merged, 1);
        assert_eq!(
            append_merged_note(p.notification.body.clone(), p.merged),
            "done\n（合并了 1 条同类事件）"
        );
        assert!(pending.is_empty());
    }

    /// §1.4 回归②：hook 型 agent 不被 6s 窗丢弃——窗外 2s 的第二条真实事件
    ///（快速追问两回合各自完成）必须进新聚合窗照常通知；聚合窗内则合并。
    #[test]
    fn hook_agent_real_second_event_is_never_dropped() {
        assert_eq!(notify_action(false, false, true), NotifyAction::Enqueue, "窗外 2s 的第二条不丢");
        assert_eq!(notify_action(false, true, true), NotifyAction::Merge, "窗内走合并而不是丢弃");
        assert_eq!(notify_action(false, false, false), NotifyAction::Enqueue);
    }

    /// §1.4 回归③：仅监控型 agent 的撕裂快照重复（6s 内、无 pending）被整条压制
    #[test]
    fn watch_agent_torn_snapshot_duplicate_is_suppressed() {
        assert_eq!(notify_action(true, false, true), NotifyAction::Suppress);
        assert_eq!(notify_action(true, true, true), NotifyAction::Merge, "有 pending 合并计数，不压制");
        assert_eq!(notify_action(true, false, false), NotifyAction::Enqueue, "窗外照常");
        // 只有监控型受 6s 窗压制
        assert!(is_watch_agent("trae-work") && is_watch_agent("workbuddy"));
        assert!(!is_watch_agent("claude-code") && !is_watch_agent("zcode"));
        assert!(!is_watch_agent("retired-agent"), "未知 id 按 hook 型对待");
    }

    /// §2.3 回归：聚合派发定时器带世代号——旧定时器不得提前派发新一代条目
    #[test]
    fn flush_timer_only_claims_its_own_generation() {
        let mut pending: HashMap<String, PendingNotify> = HashMap::new();
        let key = "k".to_string();
        pending.insert(key.clone(), PendingNotify { notification: notification("old"), merged: 0, seq: 1 });
        // 旧定时器到点前，同 key 的新一代条目已就位
        pending.insert(key.clone(), PendingNotify { notification: notification("new"), merged: 0, seq: 2 });
        // 旧定时器（seq=1）认领失败，条目原样放回（新条目的聚合窗不被截断）
        assert!(claim_pending(&mut pending, &key, 1).is_none());
        assert_eq!(pending[&key].seq, 2, "条目必须原样放回");
        assert_eq!(pending[&key].notification.body, "new");
        // 新定时器正常认领
        let p = claim_pending(&mut pending, &key, 2).expect("本代定时器认领成功");
        assert_eq!(p.seq, 2);
        // 无条目时幂等
        assert!(claim_pending(&mut pending, &key, 2).is_none());
    }

    /// §1.6 回归：stale 补投不得驱动 glow/音效（历史与通知保留）。
    /// 附 §1.7 交接（S1）：被服务端夹到 now-1h 的过旧事件必须判 stale、不建档。
    #[test]
    fn stale_replay_drives_neither_glow_nor_sound() {
        assert!(!should_drive_glow(true, false, false), "陈旧补投不驱动光效/音效");
        assert!(!should_drive_glow(true, false, true));
        assert!(!should_drive_glow(true, true, false));
        // 新鲜事件照常；终态回声不驱动（完成色不被压成终止色），
        // 但回声捎带的心跳判死（running_lost）仍要驱动
        assert!(should_drive_glow(false, false, false));
        assert!(!should_drive_glow(false, true, false));
        assert!(should_drive_glow(false, true, true));

        // S1：now-1h（服务端夹取下限）的事件比判死阈值（默认 10min）更老 → stale
        if std::env::var(STALE_ENV).is_err() {
            let mut old = ev(EventKind::RunFailed, 0, None, "boom");
            old.timestamp = bark_core::now_millis() - 60 * 60 * 1000;
            assert!(is_stale_replay(&old), "被夹到 now-1h 的过旧事件必须判 stale");
            let (mut sessions, mut finished) = table();
            let d = apply_session_event_inner(&mut sessions, &mut finished, &old, is_stale_replay(&old));
            assert!(!d.changed && !d.urgent && sessions.is_empty(), "stale 事件不建档");
        }
    }

    /// §2.10 回归（重启模拟）：新实例从 seen-ids 加载后旧 id 被判重；
    /// 心跳/工具收尾不落盘（不值跨重启判重）
    #[test]
    fn seen_ids_survive_restart() {
        assert!(persist_seen(EventKind::RunCompleted));
        assert!(persist_seen(EventKind::RunFailed));
        assert!(persist_seen(EventKind::InputRequired));
        assert!(!persist_seen(EventKind::Activity), "心跳不落盘");
        assert!(!persist_seen(EventKind::ToolFinished), "工具收尾不落盘");

        let dir = temp_dir();
        let path = dir.join("seen-ids.jsonl");
        {
            let mut first = RecentIds::load(path.clone());
            assert!(!first.seen("evt-1", true));
            assert!(first.seen("evt-1", true), "进程内判重");
            assert!(!first.seen("hb-1", false), "心跳新 id 照常放行");
        }
        // 重启模拟：新实例加载后旧 id 被判重
        let mut second = RecentIds::load(path.clone());
        assert!(second.seen("evt-1", true), "跨 daemon 重启必须仍判重（§2.10）");
        assert!(!second.seen("hb-1", false), "未持久化的心跳不跨重启判重");
        assert!(!second.seen("evt-2", true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §2.10：seen-ids 文件有界——启动时裁到 CAP=2048 行（窗口本身也只有 CAP）
    #[test]
    fn seen_ids_log_trimmed_to_cap_at_startup() {
        let dir = temp_dir();
        let path = dir.join("seen-ids.jsonl");
        let total = RecentIds::CAP + 52;
        let body: String = (0..total).map(|i| format!("id-{i}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let ids = RecentIds::load(path.clone());
        assert!(ids.set.contains("id-2099") || ids.set.contains(&format!("id-{}", total - 1)));
        assert_eq!(ids.order.len(), RecentIds::CAP, "只保留尾部 CAP 条");
        assert!(!ids.set.contains("id-0"), "最旧的被淘汰");
        // 文件本身也裁到 CAP 行
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines, RecentIds::CAP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §4.4 回归：纯 last_activity/计数变化的快照推送节流到 ≥500ms 一次；
    /// 相位跃迁/建档/摘档（urgent）立即推
    #[test]
    fn snapshot_push_is_throttled_but_urgent_pushes_immediately() {
        let last = Mutex::new(None);
        assert!(should_push_snapshot(&last, false), "首次照常推");
        assert!(!should_push_snapshot(&last, false), "500ms 内纯活跃时间刷新被节流");
        assert!(!should_push_snapshot(&last, false));
        assert!(should_push_snapshot(&last, true), "相位跃迁/建档/摘档立即推");
        // 超过窗口后照常（Instant 单调钟，墙钟跳变不影响窗口）
        *lock(&last) = Instant::now().checked_sub(std::time::Duration::from_millis(600));
        assert!(should_push_snapshot(&last, false));
    }

    /// §1.7（客户端半边）：recent_notified 的 6s 窗用 Instant 单调钟，
    /// 窗口从上一次落点起算、到期必须放行（墙钟回拨不会造成通知黑洞）
    #[test]
    fn recent_notified_window_expires_on_monotonic_clock() {
        let mut recent: HashMap<String, Instant> = HashMap::new();
        recent.insert("k".into(), Instant::now());
        assert!(recent_notified(&mut recent, "k"), "6s 窗内判重");
        // 判定不刷新时间戳：窗口从上一次落点起算
        assert!(recent_notified(&mut recent, "k"));
        // 7s 前的落点已过期：放行并清掉条目（无黑洞）
        recent.insert("k".into(), Instant::now().checked_sub(std::time::Duration::from_secs(7)).unwrap());
        assert!(!recent_notified(&mut recent, "k"));
        assert!(recent.is_empty(), "过期条目顺手清理（map 有界）");
    }

    /// §2.5 回归：免打扰在**派发时**复查（入窗时未免打扰、到点已免打扰的不再推送）；
    /// 起止相同的区间恒为空集，按非法跳过（不再「配了但不生效」）
    #[test]
    fn dispatch_rechecks_quiet_and_empty_range_is_rejected() {
        assert!(dispatch_gate(false, false));
        assert!(!dispatch_gate(true, false), "静音不推送");
        assert!(!dispatch_gate(false, true), "派发时已入免打扰 → 不推送");
        // "23:00-23:00"：头含尾不含恒为空集 → 非法跳过
        assert_eq!(parse_hhmm_range("23:00-23:00"), None, "起止相同的区间按非法跳过");
        assert_eq!(parse_hhmm_range("23:00-08:00"), Some((1380, 480)));
        assert!(!in_quiet_hours_at(&["23:00-23:00".to_string()], 23 * 60 + 30));
    }

    /// §2.6 回归：sessions_for_ui 与事件管道门同用 agent_disabled（首条匹配）。
    /// 重复 kind 条目「首 true 次 false」时事件照进状态表，会话必须仍显示
    /// （旧实现按「任一条目 !enabled」隐藏，自相矛盾）。
    #[test]
    fn sessions_for_ui_follows_first_match_disabled_semantics() {
        let mut cfg = BarkConfig::default();
        cfg.set_agent_enabled("dsh", true);
        cfg.agents.push(bark_core::AgentState { kind: "dsh".into(), enabled: false });
        let state = AppState::new(cfg, std::path::PathBuf::from("config.json"));
        let now = bark_core::now_millis();
        {
            let mut s = lock(&state.active_sessions);
            let mut e = ev(EventKind::Activity, now, Some("Read"), "");
            e.agent = "dsh".into();
            e.session_id = "s1".into();
            s.insert("dsh|s1".to_string(), new_session(&e, SessionPhase::ToolRunning));
        }
        let out = sessions_for_ui(&state);
        assert_eq!(out.len(), 1, "重复条目首 true 次 false → 会话仍显示（首条匹配口径）");
        // 反向：首条 false → 隐藏（与 agent_disabled 同口径）
        let mut cfg2 = BarkConfig::default();
        cfg2.set_agent_enabled("dsh", false);
        cfg2.agents.push(bark_core::AgentState { kind: "dsh".into(), enabled: true });
        let state2 = AppState::new(cfg2, std::path::PathBuf::from("config.json"));
        {
            let mut s = lock(&state2.active_sessions);
            let mut e = ev(EventKind::Activity, now, Some("Read"), "");
            e.agent = "dsh".into();
            e.session_id = "s1".into();
            s.insert("dsh|s1".to_string(), new_session(&e, SessionPhase::ToolRunning));
        }
        assert!(sessions_for_ui(&state2).is_empty(), "首条 false → 隐藏（与管道门一致）");
    }

    /// §2.11 回归：空 session_id 事件只入历史与事件流，不进状态机/glow/音效/通知聚合
    ///（否则 `"agent|"` 幻影会话凭空建档、判死时凭空亮终止色）
    #[test]
    fn empty_session_id_events_skip_state_machine() {
        assert_eq!(session_key("claude-code", ""), "claude-code|", "幻影键形态（回归说明）");
        let mut e = ev(EventKind::Activity, 1000, None, "");
        e.session_id = String::new();
        assert!(skips_state_machine(&e));
        e.session_id = "   ".into();
        assert!(skips_state_machine(&e), "纯空白同视为无会话");
        e.session_id = "s1".into();
        assert!(!skips_state_machine(&e), "真实会话照常进状态机");
    }
}
