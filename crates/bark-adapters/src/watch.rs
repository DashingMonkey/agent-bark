//! 监控型适配器：无官方 hook 的 agent（本文件为 WorkBuddy；Kimi Work / 豆包工作见各自模块）
//! 通过轮询本地 SQLite 会话库的状态跃迁产出统一事件。非官方方案，
//! 软件升级可能导致失效，UI 需显著标注。
//!
//! SQLite 轮询采用「复制后读取」策略：先复制 db 与 -wal 到临时目录再打开，
//! 避免与宿主应用的写锁冲突（WAL 模式库）。注意 db 与 -wal 是两次独立复制，
//! 无法保证同一时点，极端情况下会得到撕裂快照——单轮失败可接受（下轮自愈），
//! 靠连续失败计数告警暴露持续性问题。
//!
//! 与 hook 型 agent 不同，监控型拿不到每回合的 UserPromptSubmit / PreToolUse，
//! 只能靠状态轮询模拟心跳：会话处于运行态（working/planning）时周期性重发
//! Activity 事件：首次信号建档点亮，后续 keepalive 只刷新已有条目的活动时间。
//! 重复 running 状态不代表进程仍在线，静默不用于推断终止。
//!
//! 轮询循环运行在独立 OS 线程（不依赖 tokio runtime），
//! 通过 AtomicBool 停止标志协调退出。

use bark_core::{now_millis, project_name_from_cwd, AgentKind, EventKind, NormalizedEvent};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;

pub trait WatchAdapter: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn is_installed(&self) -> bool;
    /// 启动轮询线程。stop 为停止标志；事件经 tx 送出（通道满时丢弃，绝不阻塞）。
    fn spawn(
        &self,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> anyhow::Result<std::thread::JoinHandle<()>>;
    /// 该监控是否可用（schema 未逆向 / 库加密的 adapter 返回 false，UI 不应显示「监控中」）
    fn is_available(&self) -> bool {
        true
    }
    /// 不可用原因（直接整句展示给用户，按界面语言取词）。
    /// 实现里探测缓存的错误串必须与语言无关：语言相关的分支要在进缓存之前处理。
    fn unavailable_reason(&self, lang: bark_core::Lang) -> Option<String> {
        let _ = lang;
        None
    }
}

pub(crate) fn emit(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    event: EventKind,
    session_id: &str,
    cwd: &str,
    message: &str,
    tool_name: Option<&str>,
) {
    emit_inner(stop, tx, kind, event, session_id, cwd, message, tool_name, false);
}

pub(crate) fn emit_keepalive(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    event: EventKind,
    session_id: &str,
    cwd: &str,
    message: &str,
    tool_name: Option<&str>,
) {
    emit_inner(stop, tx, kind, event, session_id, cwd, message, tool_name, true);
}

fn emit_inner(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    event: EventKind,
    session_id: &str,
    cwd: &str,
    message: &str,
    tool_name: Option<&str>,
    keepalive: bool,
) {
    // 停止标志置位后不再发事件：在飞行中的最后一轮也不能漏出
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let ev = NormalizedEvent {
        run: None,
        completion_deferred: false,
        id: uuid::Uuid::new_v4().to_string(),
        agent: kind.id().to_string(),
        kind: event,
        session_id: session_id.to_string(),
        // 监控型来源读的是数据库/日志，不是 hook 会话日志；没有 transcript_path 可声明
        transcript_path: None,
        cwd: cwd.to_string(),
        project: project_name_from_cwd(cwd),
        message: message.to_string(),
        timestamp: now_millis(),
        is_subagent: false,
        tool_name: tool_name.map(str::to_string),
        parent_session_id: None,
        // 监控型 adapter 的轮询心跳是**真实信号**（它同时也是建档信号，见 is_watch_agent 的
        // 撕裂快照去重）：只有插件型 adapter 明确知道自己「只是在续命」时才置 keepalive
        keepalive,
        codex: None,
    };
    // try_send：通道满时丢弃而不是阻塞轮询线程（否则 stop 标志永远等不到）
    let _ = tx.try_send(ev);
}

/// 定时核验与托盘操作共用；各监控保留自己的游标，不重启、不重放历史。
static RECHECK_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn request_recheck() {
    RECHECK_GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn recheck_generation() -> u64 {
    RECHECK_GENERATION.load(Ordering::Relaxed)
}

/// 可中断睡眠（秒粒度足够）；主动核验会唤醒所有数据库监控。
pub(crate) fn sleep_interruptible(stop: &AtomicBool, secs: u64) -> bool {
    let generation = recheck_generation();
    for _ in 0..secs {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        if generation != recheck_generation() {
            break;
        }
    }
    !stop.load(Ordering::Relaxed)
}

/// 尽力而为的密钥/密码缓冲区覆写清零（§2.7d）。
///
/// 不引入 zeroize 依赖（本 crate 刻意零依赖增长）：这里只需要「用完顺手覆写」的
/// 强度——威胁面是进程内存被 dump / 换页到磁盘后残留的明文凭据与整库明文会话。
/// `write_volatile` 防止覆写被编译器优化成死代码；真正的保证仍是「少持有、快释放」。
pub fn wipe_secret(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: b 是可写单字节引用，写 0 不破坏任何不变量
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

/// 删除一份快照临时目录；失败必须留痕（§2.7c）。
/// 明文/敏感快照残留在 %TEMP% 是隐私暴露面（杀毒、索引器、云备份都会扫描带走），
/// 旧实现 `let _ = remove_dir_all(...)` 会把「删不掉」（AV/索引器瞬时锁很常见）
/// 变成无人知晓的永久残留。
fn remove_snapshot_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        // 目录本就没建起来（建目录之前的步骤就失败了）：不是残留，无需告警
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "删除快照临时目录 {} 失败：{e}（其中的明文会话数据可能残留，请手动清理）",
            dir.display()
        ),
    }
}

/// 「会话行从库里消失」的闭环事件（所有监控型 adapter 统一口径，§1.12a）。
///
/// 归一成 **RunAborted**（中止）而不是 RunFailed（失败）：行消失意味着用户删了
/// 会话，不是任务失败——RunFailed 会弹「任务失败」通知 + 亮失败色 + 失败音，
/// 删除仅证明会话已移除。RunAborted 不通知、不亮失败色。
pub(crate) fn emit_lost_row(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    id: &str,
    source: &str,
) {
    emit(
        stop,
        tx,
        kind,
        EventKind::RunAborted,
        id,
        "",
        &format!("会话记录已从{source}会话库消失（可能被删除）"),
        None,
    );
}

// ---------------------------------------------------------------------------
// WorkBuddy：轮询 ~/.workbuddy/workbuddy.db 的 sessions 表
// ---------------------------------------------------------------------------

/// 状态机实测校准（2026-09，WorkBuddy 2.137.1，app.asar 内官方注释交叉印证）：
///
/// - 真实会话库是 `~/.workbuddy/workbuddy.db`（早期社区逆向的
///   `%APPDATA%\WorkBuddy\codebuddy-sessions.vscdb` 在新版本上不存在，已废弃）
/// - `sessions.status` 值域全小写：`active`（新建/从终态恢复）、`working`（turn 进行中）、
///   `planning`（规划阶段运行中）、`completed`（turn 正常结束）、`terminated`（runtime 退出）、
///   `error`（不可恢复错误）、`pending`（等待授权/提问/暂停）、`archived`（归档）
/// - daemon 冷启动的 `sweepStaleActiveSessions()` 会把残留 `active`/`working` 批量置
///   `terminated`——所以轮询侧的 `working→terminated` 恰好覆盖宿主启动时确认的残留会话结束
/// - 2026-10-14 校准：手动停止也会写 completed；回合 JSONL 的 assistant incomplete
///   + providerData.error.message="Interrupted by user" 才能区别中止，见 workbuddy.rs。
///   等待授权/提问归 pending；runtime 崩溃仍按 terminated/error 处理。

pub struct WorkBuddyWatch;

/// 轮询状态表条目：状态 + 最近出现的轮次 + 最近一次心跳的轮次
struct SeenSession {
    status: String,
    last_seen: u64,
    last_hb: u64,
}

/// 会话行从快照中消失后条目再保留的轮数（约 1 分钟）。
/// 快照每轮返回全表，故「本轮未出现」对任何状态都意味着行已被删除。
const MISSING_KEEP_ROUNDS: u64 = 12;
/// 终态条目的内存上限：超出时淘汰最旧
const TERMINAL_MAX: usize = 512;
/// tombstone 有效期（轮）：过期后同 id 会话再次出现仍可正常通知
/// （会话被删除后重建的场景不能被永久压制）
const TOMBSTONE_TTL_ROUNDS: u64 = 24;
/// 快照连续失败每满该次数打一条 warn（约 1 分钟一次），便于发现持续性失败
const SNAPSHOT_WARN_EVERY: u32 = 12;
/// 残留快照目录的回收门槛：只动修改时间超过 1 小时的（不误删并发 watcher 正在用的）
const STALE_SNAPSHOT_MIN_AGE: Duration = Duration::from_secs(3600);

/// 运行态：应发心跳、出现在「运行中会话」列表、流光蓝色
fn is_running(status: &str) -> bool {
    matches!(status, "working" | "planning")
}

/// 稳定终态：条目可淘汰 / 可进 tombstone（archived 也是稳定终态，只是不发运行事件）
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "terminated" | "error" | "archived")
}

/// 已知状态全集，含本 adapter 不产事件的中性态（active/pending）。
///
/// 集合外的值意味着 WorkBuddy 又改了 `sessions.status` 的语义——2026-10 改过取消
/// 的写法、2026-11（5.12.1）把回合结束从 completed 改成 active。这类变化不会让
/// schema 探测失败，界面照样显示「监控中」，只能靠一条日志留下证据。
fn is_known_status(status: &str) -> bool {
    matches!(
        status,
        "active" | "pending" | "working" | "planning"
            | "completed" | "terminated" | "error" | "archived"
    )
}

/// 状态跃迁 → 事件判定（纯函数，便于测试）。
/// - `first`：首轮只播种，不产终态事件（否则启动时重放全部历史终态会话）；
///   运行中心跳由 needs_heartbeat 单独负责，首轮就会点亮
/// - `suppress`：该会话刚被淘汰且仍在 tombstone 有效期内——仅压住「淘汰后回表」
///   （prev 为空）的重复通知；若它重新进入运行态，新一轮跃迁照常通知
/// - 终态在 prev 为**任何非终态**时都发事件：prev 为空覆盖「会话在两轮轮询之间
///   走完一生」（新建 + 完成发生在同一个 5s 轮询间隔内）；prev 为 active/pending
///   覆盖「还没跑起来就挂了 / 等待中失败」——都不能漏，否则 daemon 侧状态表
///   留着没人闭环
fn run_event(prev: &str, status: &str, suppress: bool, first: bool) -> Option<EventKind> {
    if first || (suppress && prev.is_empty()) {
        return None;
    }
    let was_running = is_running(prev);
    match status {
        // 运行中 → 等待授权/提问/暂停：流光橙色 + 状态表 WaitingInput
        "pending" if was_running => Some(EventKind::InputRequired),
        // 非终态 → 终态：通知（已终态的行再变终态是重复通知，不发）
        "completed" if !is_terminal(prev) => Some(EventKind::RunCompleted),
        // WorkBuddy 5.12.1（ACP）起：回合正常结束不再把会话写成 completed，而是留在
        // `active`，终态只落在回合记录的 acp-terminal-state 行（stopReason=end_turn）。
        // 运行态回到 active 即本回合结束——不认这条，悬浮窗会永远停在「思考中」。
        // 仅从运行态跃迁才认：新建会话本来就是 active，不能误报完成。
        "active" if was_running => Some(EventKind::RunCompleted),
        "terminated" | "error" if !is_terminal(prev) => Some(EventKind::RunFailed),
        // 运行中被归档：turn 实际已结束，闭环掉状态表条目，避免它长期残留在显示中。
        // 归一成 RunAborted 而不是 RunCompleted：归档不是「跑完了」，
        // 亮完成色属于谎报完成（用户实测就见过这个完成色）。
        "archived" if was_running => Some(EventKind::RunAborted),
        _ => None,
    }
}

/// 是否应为本行发一次心跳（纯函数，便于测试）。
/// 进入运行态立即发一次；持续运行时每 `every` 轮重发一次保活
/// （默认 5s×12=60s，只刷新已有条目）。
fn needs_heartbeat(status: &str, prev: Option<&SeenSession>, round: u64, every: u64) -> bool {
    if !is_running(status) {
        return false;
    }
    match prev {
        None => true,
        Some(p) => p.status != status || round.saturating_sub(p.last_hb) >= every,
    }
}

/// 清理轮询状态表，防止 last 无界增长。
/// 返回本轮被遗忘的「运行中」条目 id——运行中会话的行从库里消失意味着
/// 会话被删除（不会有终态跃迁），需由调用方发 RunAborted 闭环 daemon 侧状态表
/// （§1.12a：用户删的会话按「中止」收场，不弹「任务失败」、不亮失败色），
/// 避免已经移除的会话长期残留在显示中。
///
/// 其余规则：
/// 1. 连续 MISSING_KEEP_ROUNDS 轮未再出现的条目一律遗忘；
///    其中终态条目压入 tombstone，避免行重现（查询条件变化/撕裂快照）时重复通知；
/// 2. 条目数超过 TERMINAL_MAX 时淘汰最旧的终态条目，同样压入 tombstone；
/// 3. tombstone 过期后释放，保证「删除后重建的同 id 会话」最终能再次通知。
fn prune_last(
    last: &mut HashMap<String, SeenSession>,
    tombstone: &mut HashMap<String, u64>,
    round: u64,
) -> Vec<String> {
    let mut dropped: Vec<String> = Vec::new();
    let mut lost_running: Vec<String> = Vec::new();
    last.retain(|id, s| {
        let keep = round.saturating_sub(s.last_seen) < MISSING_KEEP_ROUNDS;
        if !keep {
            if is_running(&s.status) {
                lost_running.push(id.clone());
            } else if is_terminal(&s.status) {
                dropped.push(id.clone());
            }
        }
        keep
    });
    for id in dropped {
        tombstone.insert(id, round);
    }

    // 仅当条目总数超限时才统计/淘汰（避免每轮全量遍历）
    if last.len() > TERMINAL_MAX {
        let overflow = last
            .values()
            .filter(|s| is_terminal(&s.status))
            .count()
            .saturating_sub(TERMINAL_MAX);
        if overflow > 0 {
            let mut terminal: Vec<(String, u64)> = last
                .iter()
                .filter(|(_, s)| is_terminal(&s.status))
                .map(|(k, s)| (k.clone(), s.last_seen))
                .collect();
            terminal.sort_by_key(|(_, seen)| *seen);
            for (id, _) in terminal.into_iter().take(overflow) {
                last.remove(&id);
                tombstone.insert(id, round);
            }
        }
    }

    tombstone.retain(|_, at| round.saturating_sub(*at) < TOMBSTONE_TTL_ROUNDS);
    lost_running
}

/// 终态事件的通知正文：优先用会话标题（「任务完成 · WorkBuddy」下面直接看到是哪件事），
/// 无标题时退回状态值。
fn terminal_message(title: &str, status: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        format!("状态: {status}")
    } else {
        title.to_string()
    }
}

impl WorkBuddyWatch {
    /// home 口径与其余 adapter 一致（dirs::home_dir()，Windows 走 known-folder API），
    /// 环境变量只作兜底。曾经直接读 USERPROFILE：注册表 ProfileList 重定向时
    /// 「installed 检测」与「监控对象」会指向不同文件。
    fn db_path() -> Option<PathBuf> {
        let home = dirs::home_dir()
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
        Some(home.join(".workbuddy").join("workbuddy.db"))
    }

    /// 惰性 schema 探测结果缓存：本机 schema 校准后自动恢复监控，
    /// 不必改代码；schema 不符则明确不可用（而不是让用户看到「监控中」却零事件）。
    fn schema_probe() -> &'static OnceLock<Result<(), String>> {
        static PROBE: OnceLock<Result<(), String>> = OnceLock::new();
        &PROBE
    }

    /// 表/列名于 2026-09 在 WorkBuddy 2.137.1 上实测校准（sqlite_master 导出 +
    /// asar 内 `UPDATE sessions SET status=...` 语句印证）。升版后若失效，
    /// 重新核对列：id / title / status / cwd（status/cwd NOT NULL）。
    const QUERY: &'static str =
        "SELECT id, COALESCE(title, ''), status, COALESCE(cwd, ''), updated_at FROM sessions WHERE deleted_at IS NULL";
    const POLL_SECS: u64 = 5;
    /// 运行态心跳重发间隔（轮）：12 × 5s = 60s，只刷新已有条目
    const HEARTBEAT_EVERY_ROUNDS: u64 = 12;
}

impl WatchAdapter for WorkBuddyWatch {
    fn kind(&self) -> AgentKind {
        AgentKind::WorkBuddy
    }

    fn is_installed(&self) -> bool {
        Self::db_path().is_some_and(|p| p.exists())
    }

    /// 可用性由一次真实探测决定（结果缓存）：
    /// schema 能跑通 → 可用；跑不通 → 不可用并把原因展示给用户。
    fn is_available(&self) -> bool {
        let Some(db) = Self::db_path() else { return false };
        if !db.exists() {
            return false;
        }
        Self::schema_probe()
            .get_or_init(|| match snapshot_rows(&db, Self::QUERY) {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("{e:#}")),
            })
            .is_ok()
    }

    fn unavailable_reason(&self, lang: bark_core::Lang) -> Option<String> {
        // 库缺失在进探测缓存**之前**按语言给出原因：缓存串必须与语言无关
        // （OnceLock 缓存的结果会被不同语言的调用方读到）
        let db = match Self::db_path().filter(|p| p.exists()) {
            Some(p) => p,
            None => {
                return Some(match lang {
                    bark_core::Lang::Zh => {
                        "未找到 ~/.workbuddy/workbuddy.db（未安装或从未运行过会话）".into()
                    }
                    bark_core::Lang::En => {
                        "~/.workbuddy/workbuddy.db not found (WorkBuddy is not installed or has never run a session)"
                            .into()
                    }
                });
            }
        };
        let probe = Self::schema_probe().get_or_init(|| match snapshot_rows(&db, Self::QUERY) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{e:#}")),
        });
        match probe {
            Ok(()) => None,
            Err(e) => Some(match lang {
                bark_core::Lang::Zh => format!(
                    "WorkBuddy 会话库探测失败，暂停监控：{e}。\
                     表/列名已按 2.137.1 校准，若刚升级 WorkBuddy，请核对 watch.rs 的 QUERY 常量\
                     （库文件：{}）",
                    db.display()
                ),
                bark_core::Lang::En => format!(
                    "WorkBuddy session database probe failed; monitoring paused: {e}. \
                     Table/column names are calibrated for 2.137.1; if you just upgraded WorkBuddy, \
                     verify the QUERY constant in watch.rs (database: {})",
                    db.display()
                ),
            }),
        }
    }

    fn spawn(
        &self,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> anyhow::Result<std::thread::JoinHandle<()>> {
        let db = Self::db_path().ok_or_else(|| anyhow::anyhow!("无法定位用户主目录"))?;
        if !db.exists() {
            anyhow::bail!("WorkBuddy 会话库不存在: {}", db.display());
        }
        let kind = AgentKind::WorkBuddy;
        cleanup_stale_snapshot_dirs(&std::env::temp_dir(), STALE_SNAPSHOT_MIN_AGE);
        Ok(std::thread::spawn(move || {
            let mut last: HashMap<String, SeenSession> = HashMap::new();
            // 未识别状态每个取值只告警一次，避免每轮刷屏
            let mut unknown_status: HashSet<String> = HashSet::new();
            // 被淘汰的终态会话 → 淘汰时的轮次；有效期内压制「回表」重复通知
            let mut tombstone: HashMap<String, u64> = HashMap::new();
            // 首轮轮询只播种终态、不产事件：否则启动时会把库中所有历史
            // completed/terminated 会话全部重放成通知（启动风暴）。
            // 运行中的会话例外——daemon 启动时它正在跑，心跳应当立即点亮流光。
            let mut first = true;
            let mut round: u64 = 0;
            let mut fail_count: u32 = 0;
            loop {
                match snapshot_rows(&db, Self::QUERY) {
                    Ok(snap) => {
                        fail_count = 0;
                        round += 1;
                        for (session, title, status, cwd, updated_at) in snap {
                            // 语义变更不会让 schema 探测失败，只会让状态走 `_ => None`
                            // 静默吞掉——留一条一次性证据，下次升级不用从头查。
                            if !is_known_status(&status) && unknown_status.insert(status.clone()) {
                                tracing::warn!(
                                    "WorkBuddy 出现未识别的会话状态「{status}」（示例 session={session}）：\
                                     状态判定可能已随版本失效，请核对 watch.rs 的 is_running / is_terminal"
                                );
                            }
                            let prev = last.get(&session).map(|s| s.status.as_str()).unwrap_or("");
                            let suppress = prev.is_empty() && tombstone.contains_key(&session);
                            let hb = needs_heartbeat(&status, last.get(&session), round, Self::HEARTBEAT_EVERY_ROUNDS);
                            if hb {
                                // 心跳（Activity，不通知不入历史）：让流光亮思考色 +
                                // 会话进入「运行中会话」列表；message 用标题作 prompt 展示
                                if prev == status {
                                    emit_keepalive(&stop, &tx, kind, EventKind::Activity, &session, &cwd, &title, None);
                                } else {
                                    emit(&stop, &tx, kind, EventKind::Activity, &session, &cwd, &title, None);
                                }
                            }
                            if let Some(ev) = run_event(prev, &status, suppress, first) {
                                // 会话级 completed 不代表回合成功；先补齐结束原因，再进入统一状态机。
                                let ev = if matches!(ev, EventKind::RunCompleted | EventKind::RunFailed) {
                                    db.parent().and_then(|home| crate::workbuddy::terminal_override(
                                        &home.join("projects"), &session, updated_at,
                                    )).unwrap_or(ev)
                                } else { ev };
                                emit(&stop, &tx, kind, ev, &session, &cwd, &terminal_message(&title, &status), None);
                            }
                            let last_hb = if hb {
                                round
                            } else {
                                last.get(&session).map(|s| s.last_hb).unwrap_or(0)
                            };
                            last.insert(session, SeenSession { status, last_seen: round, last_hb });
                        }
                        for id in prune_last(&mut last, &mut tombstone, round) {
                            // 运行中的行从库里消失 = 会话被删除，没有终态跃迁；
                            // 主动闭环（RunAborted：不通知、不亮失败色，§1.12a），
                            // 避免已删除会话长期残留
                            emit_lost_row(&stop, &tx, kind, &id, "WorkBuddy");
                        }
                        first = false;
                    }
                    Err(e) => {
                        // 快照失败全部吞掉会掩盖持续性故障：连续失败每满 N 次告警一次
                        fail_count += 1;
                        if fail_count.is_multiple_of(SNAPSHOT_WARN_EVERY) {
                            tracing::warn!(
                                "WorkBuddy 会话库快照已连续失败 {fail_count} 次（最近错误：{e:#}）。\
                                 若持续出现，说明 schema 或文件布局已变，监控实际处于失效状态。"
                            );
                        }
                    }
                }
                if !sleep_interruptible(&stop, Self::POLL_SECS) {
                    break;
                }
            }
        }))
    }
}

/// 复制 db（及 -wal，存在才复制）到临时目录后只读查询四列
/// (id, title, status, cwd)。返回 Err 时携带阶段摘要，由调用方做连续失败计数告警。
fn snapshot_rows(db: &Path, query: &str) -> anyhow::Result<Vec<(String, String, String, String, i64)>> {
    let tmp_dir = std::env::temp_dir().join(format!("agent-bark-watch-{}", uuid::Uuid::new_v4().simple()));
    let result = snapshot_rows_in(db, query, &tmp_dir);
    // 用完即删；删失败必须留痕（§2.7c，见 remove_snapshot_dir）
    remove_snapshot_dir(&tmp_dir);
    result
}

/// 快照临时目录的两套前缀（§2.7a）：现行 `agent-bark-watch-`，以及已下线的
/// TraeWork 监控留下的 `agent-bark-traework-`（旧版本把**明文整库快照**写在那里，
/// 升级后仍需回收，否则会永久残留在 %TEMP%）。
fn is_snapshot_dir_name(name: &str) -> bool {
    name.starts_with("agent-bark-watch-") || name.starts_with("agent-bark-traework-")
}

/// 回收进程被强杀时残留的快照临时目录（正常路径在 snapshot / snapshot_rows 里已自清）。
/// 只动修改时间超过 `min_age` 的同名前缀目录：再保守也不至于误删并发 watcher 正在
/// 用的那份（测试传 `Duration::ZERO` 立即回收）。两套前缀都清（§2.7a），
/// 其中 `agent-bark-traework-` 是旧版 TraeWork 监控的遗留目录（该 adapter 已下线）。
pub fn cleanup_stale_snapshot_dirs(tmp: &Path, min_age: Duration) {
    let Ok(rd) = std::fs::read_dir(tmp) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("agent-bark-") {
            continue;
        }
        if !is_snapshot_dir_name(&name.to_string_lossy()) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d >= min_age);
        if stale {
            remove_snapshot_dir(&entry.path());
        }
    }
}

fn snapshot_rows_in(db: &Path, query: &str, tmp_dir: &Path) -> anyhow::Result<Vec<(String, String, String, String, i64)>> {
    std::fs::create_dir_all(tmp_dir).map_err(|e| anyhow::anyhow!("创建临时目录失败: {e}"))?;
    let tmp_db = tmp_dir.join("snapshot.db");
    // 先复制主库、再复制 -wal：两步之间宿主可能继续写库，故无法保证同一时点
    // （撕裂快照的根因，顺序无法根治，只能缩短窗口）。单轮失败下轮自愈。
    // 路径用 OsString push 构造（display() 对非 UTF-8 路径有损），fs::copy 流式复制。
    // 不复制 -shm：只读打开 WAL 库不需要它，复制反而可能因共享冲突让整轮失败。
    std::fs::copy(db, &tmp_db).map_err(|e| anyhow::anyhow!("复制 db 失败: {e}"))?;
    {
        let mut src = db.as_os_str().to_os_string();
        src.push("-wal");
        let mut dst = tmp_db.as_os_str().to_os_string();
        dst.push("-wal");
        match std::fs::copy(PathBuf::from(src), PathBuf::from(dst)) {
            Ok(_) => {}
            // 宿主不在 WAL 模式（或刚 checkpoint 完）时 -wal 不存在，属正常
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::anyhow!("复制 -wal 失败: {e}")),
        }
    }
    let conn = rusqlite::Connection::open_with_flags(&tmp_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("打开快照库失败: {e}"))?;
    let mut stmt = conn
        .prepare(query)
        .map_err(|e| anyhow::anyhow!("SQL prepare 失败（schema 可能已变，需重新校准）: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0).unwrap_or_default(),
                row.get::<_, String>(1).unwrap_or_default(),
                row.get::<_, String>(2).unwrap_or_default(),
                row.get::<_, String>(3).unwrap_or_default(),
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| anyhow::anyhow!("查询失败: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: &str, last_seen: u64) -> SeenSession {
        SeenSession { status: status.to_string(), last_seen, last_hb: 0 }
    }

    #[test]
    fn first_round_never_emits_terminal() {
        // 启动播种：历史终态会话不得重放
        assert_eq!(run_event("", "completed", false, true), None);
        assert_eq!(run_event("working", "completed", false, true), None);
        // 运行中的会话不受此限：首轮由 needs_heartbeat 发心跳点亮流光
    }

    #[test]
    fn transition_only_on_entering_terminal() {
        assert_eq!(run_event("", "completed", false, false), Some(EventKind::RunCompleted));
        assert_eq!(run_event("working", "completed", false, false), Some(EventKind::RunCompleted));
        assert_eq!(run_event("planning", "completed", false, false), Some(EventKind::RunCompleted));
        assert_eq!(run_event("working", "terminated", false, false), Some(EventKind::RunFailed));
        assert_eq!(run_event("working", "error", false, false), Some(EventKind::RunFailed));
        assert_eq!(run_event("active", "error", false, false), Some(EventKind::RunFailed));
        // 等待中失败 / 等待后完成：非终态 → 终态都要通知
        assert_eq!(run_event("pending", "error", false, false), Some(EventKind::RunFailed));
        assert_eq!(run_event("pending", "completed", false, false), Some(EventKind::RunCompleted));
        // 已经是终态：不重复通知
        assert_eq!(run_event("completed", "completed", false, false), None);
        // 非终态：不通知（新建行是 pending/active/working 等，静默播种）
        assert_eq!(run_event("", "working", false, false), None);
        assert_eq!(run_event("", "pending", false, false), None);
        // 运行中 → pending：等待授权/提问，打断类事件
        assert_eq!(run_event("working", "pending", false, false), Some(EventKind::InputRequired));
        // 但首次见到就是 pending（从未运行过）：不打断
        assert_eq!(run_event("", "pending", false, false), None);
        // 运行中被归档：闭环为完成
        assert_eq!(run_event("working", "archived", false, false), Some(EventKind::RunAborted));
        assert_eq!(run_event("archived", "archived", false, false), None);
        // tombstone 有效期内压住「淘汰后回表」
        assert_eq!(run_event("", "completed", true, false), None);
        // 但重新进入运行态后的新一轮跃迁仍通知
        assert_eq!(run_event("working", "completed", true, false), Some(EventKind::RunCompleted));
    }

    /// WorkBuddy 5.12.1（ACP）：回合正常结束把会话写回 `active` 而不是 `completed`。
    /// 只认「运行态 → active」——新建会话、从终态恢复本来就停在 active，不能误报完成。
    #[test]
    fn workbuddy_active_after_running_closes_turn() {
        assert_eq!(run_event("working", "active", false, false), Some(EventKind::RunCompleted));
        assert_eq!(run_event("planning", "active", false, false), Some(EventKind::RunCompleted));
        // 新建会话首轮 / 持续空闲：不通知
        assert_eq!(run_event("", "active", false, false), None);
        assert_eq!(run_event("active", "active", false, false), None);
        // 从终态恢复成 active：不是新的完成
        assert_eq!(run_event("completed", "active", false, false), None);
        assert_eq!(run_event("error", "active", false, false), None);
        // 还没跑起来就回到 active：不发完成
        assert_eq!(run_event("pending", "active", false, false), None);
        // 播种轮依旧不产终态事件
        assert_eq!(run_event("working", "active", false, true), None);
    }

    /// 5.12.1 的 `active` 原本是个未识别值，被 `_ => None` 静默吞掉、没有任何痕迹。
    /// 这条测试锁住「未知值必须能报出来」，下次再改语义至少留得下证据。
    #[test]
    fn unknown_status_is_detected() {
        for s in [
            "active", "pending", "working", "planning",
            "completed", "terminated", "error", "archived",
        ] {
            assert!(is_known_status(s), "{s} 是已校准的状态");
        }
        for s in ["idle", "waiting", "queued", "running", ""] {
            assert!(!is_known_status(s), "{s} 应触发未识别告警");
        }
    }

    #[test]
    fn heartbeat_on_entering_running_and_periodically() {
        let every = 12;
        // 首次见到运行态（含 daemon 启动时已在跑的会话）：立即心跳
        assert!(needs_heartbeat("working", None, 1, every));
        assert!(needs_heartbeat("planning", None, 1, every));
        // 非运行态：从不心跳
        assert!(!needs_heartbeat("completed", None, 1, every));
        assert!(!needs_heartbeat("pending", None, 1, every));
        // 刚心跳过：不重发
        let prev = SeenSession { status: "working".into(), last_seen: 5, last_hb: 5 };
        assert!(!needs_heartbeat("working", Some(&prev), 6, every));
        // 到保活周期（12 轮 = 60s）：重发
        let prev = SeenSession { status: "working".into(), last_seen: 16, last_hb: 5 };
        assert!(needs_heartbeat("working", Some(&prev), 17, every));
        // planning → working 状态变化：立即刷新
        let prev = SeenSession { status: "planning".into(), last_seen: 6, last_hb: 6 };
        assert!(needs_heartbeat("working", Some(&prev), 7, every));
    }

    #[test]
    fn prune_drops_missing_entries_and_tombstones_terminal() {
        let mut last: HashMap<String, SeenSession> = HashMap::new();
        let mut tombstone: HashMap<String, u64> = HashMap::new();
        last.insert("done".into(), seen("completed", 1));
        last.insert("running".into(), seen("working", 1));

        // 还在保留窗口内：不清理
        let lost = prune_last(&mut last, &mut tombstone, 1 + MISSING_KEEP_ROUNDS - 1);
        assert!(lost.is_empty());
        assert_eq!(last.len(), 2);
        assert!(tombstone.is_empty());

        // 超过窗口：两类条目都遗忘；运行中的返回给调用方发 RunAborted 闭环，
        // 终态的进 tombstone
        let lost = prune_last(&mut last, &mut tombstone, 1 + MISSING_KEEP_ROUNDS);
        assert!(last.is_empty(), "消失的行不应继续占用内存");
        assert_eq!(lost, vec!["running".to_string()], "运行中的行消失应上报");
        assert_eq!(tombstone.get("done"), Some(&(1 + MISSING_KEEP_ROUNDS)));
    }

    #[test]
    fn tombstone_expires_so_rebuilt_session_can_notify_again() {
        let mut last: HashMap<String, SeenSession> = HashMap::new();
        let mut tombstone: HashMap<String, u64> = HashMap::new();
        last.insert("s1".into(), seen("completed", 1));
        prune_last(&mut last, &mut tombstone, 1 + MISSING_KEEP_ROUNDS);
        assert!(tombstone.contains_key("s1"));

        let expire_at = 1 + MISSING_KEEP_ROUNDS + TOMBSTONE_TTL_ROUNDS;
        prune_last(&mut last, &mut tombstone, expire_at);
        assert!(tombstone.is_empty(), "tombstone 过期后必须释放，否则重建的同 id 会话永久静音");
        // 过期后同一会话重新出现并能通知
        assert_eq!(run_event("", "completed", false, false), Some(EventKind::RunCompleted));
    }

    #[test]
    fn overflow_evicts_oldest_terminal_only_when_over_cap() {
        let mut last: HashMap<String, SeenSession> = HashMap::new();
        let mut tombstone: HashMap<String, u64> = HashMap::new();
        let round = 1 + MISSING_KEEP_ROUNDS - 1; // 保持在保留窗口内，隔离出「超上限」路径
        for i in 0..(TERMINAL_MAX + 2) {
            last.insert(format!("s{i}"), seen("completed", round));
        }
        let lost = prune_last(&mut last, &mut tombstone, round);
        assert!(lost.is_empty(), "终态条目淘汰不应上报运行中丢失");
        assert_eq!(last.len(), TERMINAL_MAX, "超出上限的终态条目应被淘汰");
        assert_eq!(tombstone.len(), 2, "被淘汰的条目应进 tombstone 防重复通知");
    }

    #[test]
    fn terminal_message_prefers_title() {
        assert_eq!(terminal_message("修一下登录页", "completed"), "修一下登录页");
        assert_eq!(terminal_message("  ", "completed"), "状态: completed");
        assert_eq!(terminal_message("", "terminated"), "状态: terminated");
    }

    // ---- §1.12 / §2.7 回归 --------------------------------------------------

    /// §1.12a：「会话行消失」的闭环必须是 RunAborted（中止）而不是 RunFailed（失败）。
    /// WorkBuddy 曾发 RunFailed——弹「任务失败 · WorkBuddy」+ 亮失败色 + 失败音，
    /// 删除不代表任务失败；所有监控型 adapter 同口径。
    #[test]
    fn lost_row_reports_run_aborted_not_failed() {
        let (tx, mut rx) = mpsc::channel(4);
        let stop = AtomicBool::new(false);
        emit_lost_row(&stop, &tx, AgentKind::WorkBuddy, "sess-lost", "WorkBuddy");
        let ev = rx.try_recv().expect("必须发出闭环事件");
        assert_eq!(ev.kind, EventKind::RunAborted, "行消失按中止收场（不通知不亮失败色）");
        assert!(!ev.kind.should_notify(), "不该弹「任务失败」通知");
        assert_eq!(ev.session_id, "sess-lost");
        assert!(ev.message.contains("WorkBuddy"), "正文要说明来源: {}", ev.message);
        // 停止标志置位后不再发（emit 的既有语义不得回归）
        stop.store(true, Ordering::Relaxed);
        emit_lost_row(&stop, &tx, AgentKind::WorkBuddy, "x", "WorkBuddy");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn stale_cleanup_covers_both_snapshot_prefixes() {
        let tmp = std::env::temp_dir().join(format!("ab-wtest-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        for name in ["agent-bark-watch-1", "agent-bark-traework-1"] {
            std::fs::create_dir_all(tmp.join(name)).unwrap();
        }
        for name in ["agent-bark", "agent-bark-evil", "agent-bark-foo", "agent-bark-traeworkish"] {
            std::fs::create_dir_all(tmp.join(name)).unwrap();
        }
        cleanup_stale_snapshot_dirs(&tmp, Duration::ZERO);
        for name in ["agent-bark-watch-1", "agent-bark-traework-1"] {
            assert!(!tmp.join(name).exists(), "{name} 应被回收");
        }
        for name in ["agent-bark", "agent-bark-evil", "agent-bark-foo", "agent-bark-traeworkish"] {
            assert!(tmp.join(name).exists(), "{name} 不是快照目录，不得误删");
        }
        // 文件形态的同名条目不删（只清目录）
        std::fs::write(tmp.join("agent-bark-watch-9"), b"x").unwrap();
        cleanup_stale_snapshot_dirs(&tmp, Duration::ZERO);
        assert!(tmp.join("agent-bark-watch-9").exists(), "文件不是快照目录");
        // min_age 门槛：刚创建的新目录不删（不误伤并发 watcher 正在用的那份）
        std::fs::create_dir_all(tmp.join("agent-bark-watch-fresh")).unwrap();
        cleanup_stale_snapshot_dirs(&tmp, Duration::from_secs(3600));
        assert!(tmp.join("agent-bark-watch-fresh").exists(), "未到 min_age 不回收");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// §2.7d：wipe_secret 原地覆写清零，不改长度、不挑长度
    #[test]
    fn wipe_secret_erases_in_place() {
        let mut long = b"hunter2-secret-token-32-bytes!!".to_vec();
        let len = long.len();
        wipe_secret(&mut long);
        assert_eq!(long.len(), len, "清零不得改变长度");
        assert!(long.iter().all(|&b| b == 0), "密钥缓冲区必须被覆写: {long:?}");

        let mut short = b"pw".to_vec();
        wipe_secret(&mut short);
        assert_eq!(short, vec![0, 0], "短 secret 也要清零");

        let mut empty: Vec<u8> = Vec::new();
        wipe_secret(&mut empty); // 空缓冲区不 panic
        assert!(empty.is_empty());

        // 幂等：重复清零无副作用
        wipe_secret(&mut long);
        assert!(long.iter().all(|&b| b == 0));
    }

    /// 删除不存在的快照目录不告警不 panic（建目录之前的步骤失败 ≠ 残留）
    #[test]
    fn remove_snapshot_dir_tolerates_missing_dir() {
        let ghost = std::env::temp_dir().join(format!("ab-ghost-{}", uuid::Uuid::new_v4().simple()));
        remove_snapshot_dir(&ghost); // NotFound 静默路径
        // 真实目录删除成功
        let real = std::env::temp_dir().join(format!("ab-real-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&real).unwrap();
        remove_snapshot_dir(&real);
        assert!(!real.exists());
    }
}
