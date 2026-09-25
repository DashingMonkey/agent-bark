//! 监控型适配器：无官方 hook 的 agent（WorkBuddy / TraeWork）
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
//! Activity 事件——否则 daemon 侧状态表 10 分钟没有活动就会把会话判死，
//! 流光永远亮不了思考色（这就是「接入但流光不亮」的根因）。
//!
//! 轮询循环运行在独立 OS 线程（不依赖 tokio runtime），
//! 通过 AtomicBool 停止标志协调退出。

use bark_core::{now_millis, project_name_from_cwd, AgentKind, EventKind, NormalizedEvent};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// 不可用原因（展示给用户）
    fn unavailable_reason(&self) -> Option<String> {
        None
    }
}

fn emit(
    stop: &AtomicBool,
    tx: &mpsc::Sender<NormalizedEvent>,
    kind: AgentKind,
    event: EventKind,
    session_id: &str,
    cwd: &str,
    message: &str,
) {
    // 停止标志置位后不再发事件：在飞行中的最后一轮也不能漏出
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let ev = NormalizedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent: kind.id().to_string(),
        kind: event,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        project: project_name_from_cwd(cwd),
        message: message.to_string(),
        timestamp: now_millis(),
        is_subagent: false,
        tool_name: None,
    };
    // try_send：通道满时丢弃而不是阻塞轮询线程（否则 stop 标志永远等不到）
    let _ = tx.try_send(ev);
}

/// 可中断睡眠（秒粒度足够）
fn sleep_interruptible(stop: &AtomicBool, secs: u64) -> bool {
    for _ in 0..secs {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
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

/// 「会话行从库里消失」的闭环事件（WorkBuddy / TraeWork 统一口径，§1.12a）。
///
/// 归一成 **RunAborted**（中止）而不是 RunFailed（失败）：行消失意味着用户删了
/// 会话，不是任务失败——RunFailed 会弹「任务失败」通知 + 亮失败色 + 失败音，
/// 而这条闭环的目标恰是「别等僵死判死在随机时刻亮失败色」，自己先亮失败色等于
/// 换个时机复现同一问题。RunAborted 不通知、不亮失败色（TraeWork 一直如此）。
fn emit_lost_row(
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
///   `terminated`——所以轮询侧的 `working→terminated` 恰好覆盖「WorkBuddy 进程被杀」判死
/// - 打断（用户停止 / 等待授权 / 提问）在运行时是子状态（interrupted / waiting_permission /
///   waiting_question），DB 侧归入 `error` / `pending` 组：
///   `working→pending` 视为等待输入，`working→error` 视为失败

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
        "terminated" | "error" if !is_terminal(prev) => Some(EventKind::RunFailed),
        // 运行中被归档：turn 实际已结束，闭环掉状态表条目，避免它 10 分钟后
        // 被当僵死判死误亮失败色。
        // 归一成 RunAborted 而不是 RunCompleted：归档不是「跑完了」，
        // 亮完成色属于谎报完成（用户实测就见过这个完成色）。
        "archived" if was_running => Some(EventKind::RunAborted),
        _ => None,
    }
}

/// 是否应为本行发一次心跳（纯函数，便于测试）。
/// 进入运行态立即发一次；持续运行时每 `every` 轮重发一次保活
/// （daemon 状态表 STALE_AFTER_MS=10min 判死线，5s×12=60s 足够安全）。
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
/// 否则它会在 10 分钟后被当僵死判死、在不相干的事件里误亮失败色。
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
        "SELECT id, COALESCE(title, ''), status, COALESCE(cwd, '') FROM sessions WHERE deleted_at IS NULL";
    const POLL_SECS: u64 = 5;
    /// 运行态心跳重发间隔（轮）：12 × 5s = 60s，远小于 daemon 侧 10 分钟判死线
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

    fn unavailable_reason(&self) -> Option<String> {
        let db = Self::db_path();
        let probe = Self::schema_probe().get_or_init(|| match &db {
            Some(p) if p.exists() => match snapshot_rows(p, Self::QUERY) {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("{e:#}")),
            },
            _ => Err("未找到 ~/.workbuddy/workbuddy.db（未安装或从未运行过会话）".into()),
        });
        match probe {
            Ok(()) => None,
            Err(e) => Some(format!(
                "WorkBuddy 会话库探测失败，暂停监控：{e}。\
                 表/列名已按 2.137.1 校准，若刚升级 WorkBuddy，请核对 watch.rs 的 QUERY 常量\
                 （库文件：{}）",
                db.map(|p| p.display().to_string()).unwrap_or_else(|| "未找到".into())
            )),
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
                        for (session, title, status, cwd) in snap {
                            let prev = last.get(&session).map(|s| s.status.as_str()).unwrap_or("");
                            let suppress = prev.is_empty() && tombstone.contains_key(&session);
                            let hb = needs_heartbeat(&status, last.get(&session), round, Self::HEARTBEAT_EVERY_ROUNDS);
                            if hb {
                                // 心跳（Activity，不通知不入历史）：让流光亮思考色 +
                                // 会话进入「运行中会话」列表；message 用标题作 prompt 展示
                                emit(&stop, &tx, kind, EventKind::Activity, &session, &cwd, &title);
                            }
                            if let Some(ev) = run_event(prev, &status, suppress, first) {
                                emit(&stop, &tx, kind, ev, &session, &cwd, &terminal_message(&title, &status));
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
                            // 别等 daemon 侧 10 分钟僵死判死在随机时刻亮失败色
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
fn snapshot_rows(db: &Path, query: &str) -> anyhow::Result<Vec<(String, String, String, String)>> {
    let tmp_dir = std::env::temp_dir().join(format!("agent-bark-watch-{}", uuid::Uuid::new_v4().simple()));
    let result = snapshot_rows_in(db, query, &tmp_dir);
    // 用完即删；删失败必须留痕（§2.7c，见 remove_snapshot_dir）
    remove_snapshot_dir(&tmp_dir);
    result
}

/// 快照临时目录的两套前缀（§2.7a）：WorkBuddy 的 `agent-bark-watch-` 与 TraeWork 的
/// `agent-bark-traework-`。只匹配前者会让 TraeWork 的**明文整库快照**永久残留在
/// %TEMP%（此前就是如此）。
fn is_snapshot_dir_name(name: &str) -> bool {
    name.starts_with("agent-bark-watch-") || name.starts_with("agent-bark-traework-")
}

/// 回收进程被强杀时残留的快照临时目录（正常路径在 snapshot / snapshot_rows 里已自清）。
/// 只动修改时间超过 `min_age` 的同名前缀目录：再保守也不至于误删并发 watcher 正在
/// 用的那份（测试传 `Duration::ZERO` 立即回收）。两套前缀都清（§2.7a），
/// 且 TraeWork 的 spawn 与 WorkBuddy 的 spawn 都调用（§2.7b——此前只有后者调）。
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

fn snapshot_rows_in(db: &Path, query: &str, tmp_dir: &Path) -> anyhow::Result<Vec<(String, String, String, String)>> {
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
            ))
        })
        .map_err(|e| anyhow::anyhow!("查询失败: {e}"))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// TraeWork（TRAE SOLO CN）：轮询加密会话库（SQLCipher 4 页面级解密）
// ---------------------------------------------------------------------------

/// 状态机实测校准（2026-09，TraeWork CN 0.1.69，75MB / 19265 页库解密实测）：
///
/// - 库 `%APPDATA%\TRAE SOLO CN\ModularData\ai-agent\database.db` 是 SQLCipher 4
///   加密；**密钥固定可派生**（与 Trae CN 同源常量链，见 traework_db.rs——
///   社区旧结论「SOLO CN 密钥随机」已过时，本机 HMAC 实测通过）
/// - 回合生命周期在 `chat_turn` 表（每回合一行）：`turn_status` 终态值域
///   `completed` / `failed` / `canceled`；**`canceled` = 用户主动停止 → RunAborted**
///   （hook 型方案都拿不到这么干净的中止信号）
/// - 非终态 turn_status 一律视为运行中（未来值域扩展也兼容）
/// - 回合进行中的活动信号：`chat_message` / `history_v2` / `chat_turn` 的
///   max(id) 增长（实测 history_v2 回合内持续追加，约 13 行/回合），
///   用它发心跳点亮流光——监控型拿不到 UserPromptSubmit/PreToolUse，只能这样模拟
/// - 宿主是 SQLite WAL 模式：新提交先落 `database.db-wal`（实测常驻 4-5MB），
///   快照必须叠加 WAL 帧（`traework_db::decrypt_db`）、轮询指纹必须盯 `-wal`
///   （`db_fingerprint`）——否则 checkpoint 之前的新回合全程不可见
/// - ⚠️ 非官方机制：TraeWork 升级可能改 schema / 密钥常量（探针工具见
///   `tools/traework-probe.mjs`），失效时 is_available 会给出具体原因

struct SeenTurn {
    status: String,
    last_seen: u64,
}

struct SeenSess {
    churn: i64,
    last_seen: u64,
    last_hb: u64,
}

/// TraeWork 会话级心跳判定（纯函数，便于测试）。
///
/// - **首轮**对 `open` 集内（有非终态回合、正在跑）的会话直接心跳：daemon 启动时
///   正在跑的会话要立刻点亮流光——旧实现是 `hb = !first && (...)`，首轮整体压掉，
///   daemon 启动时正在跑的会话最长 60s 不亮流光（§1.12b）；
/// - 首轮对非 open 会话仍不心跳，终态更不重放（`turn_event` 的 `first` 语义不变，
///   `first_round_never_emits_terminal` 继续成立）；
/// - 非首轮：活动计数器变化（回合内行持续追加）或运行中会话到保活周期才发。
fn sess_needs_heartbeat(
    prev: Option<&SeenSess>,
    churn: i64,
    round: u64,
    open: bool,
    first: bool,
    every: u64,
) -> bool {
    let changed = match prev {
        None => !first,
        Some(p) => p.churn != churn,
    };
    let keepalive = open
        && prev
            .map(|p| round.saturating_sub(p.last_hb) >= every)
            .unwrap_or(false);
    (prev.is_none() && open) || (!first && (changed || keepalive))
}

/// TraeWork 回合终态集合（binary 状态：其余一切值都算运行中）
fn is_turn_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "canceled")
}

/// 回合状态跃迁 → 事件判定（纯函数，便于测试）。
/// - `first`：首轮只播种，不重放历史终态（否则启动风暴）
/// - `suppress`：撕裂快照/查询条件波动导致行短暂消失又重现（prev 为空）时
///   压住重复的终态通知（tombstone 有效期内）
/// - 终态只在 prev 为非终态时发：`completed`→完成、`failed`→失败、
///   `canceled`→RunAborted（用户按的停止：不通知、无别的会话时收起光效）
fn turn_event(prev: &str, status: &str, suppress: bool, first: bool) -> Option<EventKind> {
    if first || (suppress && prev.is_empty()) || is_turn_terminal(prev) {
        return None;
    }
    match status {
        "completed" => Some(EventKind::RunCompleted),
        "failed" => Some(EventKind::RunFailed),
        "canceled" => Some(EventKind::RunAborted),
        _ => None,
    }
}

/// 清理回合状态表：行从快照中持续消失（约 1 分钟）才遗忘——
/// 单轮撕裂快照丢行不会误判。运行中回合的行消失（会话被删除、没有终态跃迁）
/// 返回给调用方主动闭环；终态条目压入 tombstone 防重现重复通知；超限淘汰最旧终态。
fn prune_turns(
    last: &mut HashMap<String, SeenTurn>,
    tombstone: &mut HashMap<String, u64>,
    round: u64,
) -> Vec<String> {
    let mut lost_running: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    last.retain(|id, t| {
        let keep = round.saturating_sub(t.last_seen) < MISSING_KEEP_ROUNDS;
        if !keep {
            if is_turn_terminal(&t.status) {
                dropped.push(id.clone());
            } else {
                lost_running.push(id.clone());
            }
        }
        keep
    });
    for id in dropped {
        tombstone.insert(id, round);
    }
    if last.len() > TERMINAL_MAX {
        let overflow = last
            .values()
            .filter(|t| is_turn_terminal(&t.status))
            .count()
            .saturating_sub(TERMINAL_MAX);
        if overflow > 0 {
            let mut terminal: Vec<(String, u64)> = last
                .iter()
                .filter(|(_, t)| is_turn_terminal(&t.status))
                .map(|(k, t)| (k.clone(), t.last_seen))
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

/// 一轮快照：回合行 + 会话元信息（标题/工作目录）+ 活动计数器
#[derive(Clone, Default)]
struct TwSnapshot {
    /// (turn_id, session_id, turn_status)
    turns: Vec<(String, String, String)>,
    /// session_id → (session_title, cwd)
    meta: HashMap<String, (String, String)>,
    /// session_id → 活动计数器（各表 max(id) 之和，单调不减）
    churn: HashMap<String, i64>,
}

pub struct TraeWorkWatch;

impl TraeWorkWatch {
    fn db_path() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| {
            PathBuf::from(d)
                .join("TRAE SOLO CN")
                .join("ModularData")
                .join("ai-agent")
                .join("database.db")
        })
    }

    /// 惰性可用性探测（结果缓存）：密钥 HMAC 校验 + 解密 + schema 查询一并跑通
    /// 才算可用；失败原因（密钥换了 / schema 变了 / 库损坏）展示给用户。
    /// 重试 3 次再定论：解密读的是整库文件，宿主恰好在写入时可能读到撕裂页。
    fn probe() -> &'static std::sync::OnceLock<Result<(), String>> {
        static PROBE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
        &PROBE
    }

    fn probe_with_retry(db: &Path) -> Result<(), String> {
        let mut last_err = String::new();
        for _ in 0..3 {
            match snapshot(db) {
                Ok(_) => return Ok(()),
                Err(e) => last_err = format!("{e:#}"),
            }
        }
        Err(last_err)
    }

    /// 表/列名于 2026-09 在 TraeWork CN 0.1.69 解密库上实测校准。
    /// 升版后失效时重新核对：chat_turn / chat_session / session_project /
    /// project / chat_message / history_v2（探针：tools/traework-probe.mjs）。
    const Q_TURNS: &'static str =
        "SELECT turn_id, session_id, COALESCE(turn_status, '') FROM chat_turn WHERE deleted_at = 0";
    const Q_META: &'static str = "SELECT s.session_id, COALESCE(s.session_title, ''), \
         COALESCE(p.absolute_path, '') \
         FROM chat_session s \
         LEFT JOIN session_project sp ON sp.session_id = s.session_id \
         LEFT JOIN project p ON p.project_id = sp.project_id \
         WHERE s.deleted_at = 0";
    /// 活动计数器：三张随回合增长的表各自 max(id) 求和（单表 max 单调、和也单调）
    const Q_CHURN: &'static str = "SELECT session_id, SUM(mx) FROM ( \
         SELECT session_id, MAX(id) AS mx FROM chat_message WHERE deleted_at = 0 GROUP BY session_id \
         UNION ALL SELECT session_id, MAX(id) FROM history_v2 WHERE deleted_at = 0 GROUP BY session_id \
         UNION ALL SELECT session_id, MAX(id) FROM chat_turn WHERE deleted_at = 0 GROUP BY session_id \
         ) GROUP BY session_id";

    const POLL_SECS: u64 = 5;
    /// 运行中回合的保活心跳间隔（轮）：12 × 5s = 60s，远小于 daemon 侧 10 分钟判死线
    const HEARTBEAT_EVERY_ROUNDS: u64 = 12;
}

impl WatchAdapter for TraeWorkWatch {
    fn kind(&self) -> AgentKind {
        AgentKind::TraeWork
    }

    fn is_installed(&self) -> bool {
        Self::db_path().is_some_and(|p| p.exists())
    }

    fn is_available(&self) -> bool {
        let Some(db) = Self::db_path() else { return false };
        if !db.exists() {
            return false;
        }
        Self::probe().get_or_init(|| Self::probe_with_retry(&db)).is_ok()
    }

    fn unavailable_reason(&self) -> Option<String> {
        let db = Self::db_path();
        let probe = Self::probe().get_or_init(|| match &db {
            Some(p) if p.exists() => Self::probe_with_retry(p),
            _ => Err("未找到 TraeWork 会话库（未安装或从未使用过 SOLO 模式）".into()),
        });
        match probe {
            Ok(()) => None,
            Err(e) => Some(format!(
                "TraeWork 会话库暂不可用，暂停监控：{e}。\
                 可用 tools/traework-probe.mjs 核对密钥常量与 schema（库文件：{}）",
                db.map(|p| p.display().to_string()).unwrap_or_else(|| "未找到".into())
            )),
        }
    }

    fn spawn(
        &self,
        stop: Arc<AtomicBool>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> anyhow::Result<std::thread::JoinHandle<()>> {
        let db = Self::db_path().ok_or_else(|| anyhow::anyhow!("APPDATA 未设置，无法定位会话库"))?;
        if !db.exists() {
            anyhow::bail!("TraeWork 会话库不存在: {}", db.display());
        }
        let kind = AgentKind::TraeWork;
        // TraeWork 的快照目录同样要回收（§2.7b：此前只在 WorkBuddy 的 spawn 调用，
        // 且旧清理只认 `agent-bark-watch-` 前缀 → TraeWork 的明文快照永久残留）
        cleanup_stale_snapshot_dirs(&std::env::temp_dir(), STALE_SNAPSHOT_MIN_AGE);
        Ok(std::thread::spawn(move || {
            let mut turns: HashMap<String, SeenTurn> = HashMap::new();
            let mut tombstone: HashMap<String, u64> = HashMap::new();
            let mut sess: HashMap<String, SeenSess> = HashMap::new();
            let mut first = true;
            let mut round: u64 = 0;
            let mut fail_count: u32 = 0;
            // 解密结果复用：主库 + -wal 的指纹（大小+修改时间）没变就跳过重新解密，
            // 空闲时零开销；活跃时每轮约 75MB AES（有 AES-NI，几十毫秒级）
            let mut last_fp: Option<DbFingerprint> = None;
            let mut last_snap: Option<TwSnapshot> = None;
            loop {
                let fp = db_fingerprint(&db);
                let snap = match (&last_snap, fp) {
                    (Some(prev), Some(f)) if last_fp == Some(f) => Ok(prev.clone()),
                    _ => snapshot(&db),
                };
                match snap {
                    Ok(snap) => {
                        fail_count = 0;
                        round += 1;
                        last_fp = fp;
                        last_snap = Some(snap.clone());

                        // 本轮处于运行中（有非终态回合）的会话
                        let open: std::collections::HashSet<&str> = snap
                            .turns
                            .iter()
                            .filter(|(_, _, st)| !is_turn_terminal(st))
                            .map(|(_, sid, _)| sid.as_str())
                            .collect();

                        // 1) 会话级心跳：活动计数器变化（回合内行持续追加）就发，
                        //    运行中会话另加周期保活——两者都只点亮状态机，不通知
                        for (sid, churn) in &snap.churn {
                            let (title, cwd) = snap
                                .meta
                                .get(sid)
                                .map(|(t, c)| (t.as_str(), c.as_str()))
                                .unwrap_or(("", ""));
                            let prev = sess.get(sid);
                            let hb = sess_needs_heartbeat(
                                prev,
                                *churn,
                                round,
                                open.contains(sid.as_str()),
                                first,
                                Self::HEARTBEAT_EVERY_ROUNDS,
                            );
                            if hb {
                                emit(&stop, &tx, kind, EventKind::Activity, sid, cwd, title);
                            }
                            sess.insert(
                                sid.clone(),
                                SeenSess {
                                    churn: *churn,
                                    last_seen: round,
                                    last_hb: if hb { round } else { prev.map(|p| p.last_hb).unwrap_or(0) },
                                },
                            );
                        }

                        // 2) 回合级终态跃迁
                        for (turn_id, sid, status) in &snap.turns {
                            let prev = turns.get(turn_id).map(|t| t.status.as_str()).unwrap_or("");
                            let suppress = prev.is_empty() && tombstone.contains_key(turn_id);
                            if let Some(ev) = turn_event(prev, status, suppress, first) {
                                let (title, cwd) = snap
                                    .meta
                                    .get(sid)
                                    .map(|(t, c)| (t.as_str(), c.as_str()))
                                    .unwrap_or(("", ""));
                                emit(&stop, &tx, kind, ev, sid, cwd, &terminal_message(title, status));
                            }
                            turns.insert(turn_id.clone(), SeenTurn { status: status.clone(), last_seen: round });
                        }

                        // 3) 回合行持续消失（会话被删）时主动闭环运行中条目：
                        //    RunAborted（用户删的：不通知不亮色，也别等 10 分钟僵死判死）
                        for id in prune_turns(&mut turns, &mut tombstone, round) {
                            emit_lost_row(&stop, &tx, kind, &id, "TraeWork");
                        }
                        sess.retain(|_, s| round.saturating_sub(s.last_seen) < MISSING_KEEP_ROUNDS);
                        first = false;
                    }
                    Err(e) => {
                        fail_count += 1;
                        if fail_count.is_multiple_of(SNAPSHOT_WARN_EVERY) {
                            tracing::warn!(
                                "TraeWork 会话库快照已连续失败 {fail_count} 次（最近错误：{e:#}）。\
                                 若持续出现，说明密钥或 schema 已随 TraeWork 升级变化，监控实际处于失效状态。"
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

/// 轮询指纹：主库与 `-wal` 各自的（大小，修改时间）。
///
/// **必须包含 -wal**：宿主是 SQLite WAL 模式，新提交先落 `database.db-wal`、
/// checkpoint 后才进主库——2026-09-24 实测 -wal 常驻 4-5MB（上千帧）是常态，
/// 期间主库指纹纹丝不动。只看主库会让解密缓存永不重读，新回合全程不可见
/// （当天两次提问均无光效的直接原因）；主库 + -wal 一起盯，WAL 每次追加都能触发重读。
type DbFingerprint = ((u64, Option<std::time::SystemTime>), (u64, Option<std::time::SystemTime>));

fn db_fingerprint(db: &Path) -> Option<DbFingerprint> {
    let stat = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok()));
    let main = stat(db)?;
    Some((main, stat(&crate::traework_db::wal_path(db)).unwrap_or((0, None))))
}

/// 一轮快照：整库读入内存 → SQLCipher 页面级解密（**含 `-wal` 帧叠加**，见
/// `traework_db::decrypt_db` 的模块头注释）→ 明文库写临时文件 → 只读查询。
/// 单轮撕裂/查询失败由调用方计数告警，下一轮自愈。
fn snapshot(db: &Path) -> anyhow::Result<TwSnapshot> {
    let tmp_dir = std::env::temp_dir().join(format!("agent-bark-traework-{}", uuid::Uuid::new_v4().simple()));
    let result = snapshot_in(db, &tmp_dir);
    // 明文整库用完即删；删失败必须留痕（§2.7c，见 remove_snapshot_dir）
    remove_snapshot_dir(&tmp_dir);
    result
}

fn snapshot_in(db: &Path, tmp_dir: &Path) -> anyhow::Result<TwSnapshot> {
    std::fs::create_dir_all(tmp_dir).map_err(|e| anyhow::anyhow!("创建临时目录失败: {e}"))?;
    let mut plain = crate::traework_db::decrypt_db(db)?;
    let tmp_db = tmp_dir.join("plain.db");
    std::fs::write(&tmp_db, &plain).map_err(|e| anyhow::anyhow!("写明文快照失败: {e}"))?;
    // 整库明文会话（AI 对话内容）用完即覆写清零（§2.7d，尽力而为）
    wipe_secret(&mut plain);
    drop(plain);
    let conn = rusqlite::Connection::open_with_flags(&tmp_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| anyhow::anyhow!("打开明文快照失败: {e}"))?;

    let mut snap = TwSnapshot::default();
    {
        let mut stmt = conn
            .prepare(TraeWorkWatch::Q_TURNS)
            .map_err(|e| anyhow::anyhow!("SQL prepare 失败（schema 可能已变，需重新校准）: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            })
            .map_err(|e| anyhow::anyhow!("查询失败: {e}"))?;
        for r in rows {
            snap.turns.push(r.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?);
        }
    }
    {
        let mut stmt = conn.prepare(TraeWorkWatch::Q_META).map_err(|e| anyhow::anyhow!("SQL prepare 失败: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0).unwrap_or_default(),
                    row.get::<_, String>(1).unwrap_or_default(),
                    row.get::<_, String>(2).unwrap_or_default(),
                ))
            })
            .map_err(|e| anyhow::anyhow!("查询失败: {e}"))?;
        for r in rows {
            let (sid, title, cwd) = r.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?;
            snap.meta.insert(sid, (title, cwd));
        }
    }
    {
        let mut stmt = conn.prepare(TraeWorkWatch::Q_CHURN).map_err(|e| anyhow::anyhow!("SQL prepare 失败: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0).unwrap_or_default(), row.get::<_, i64>(1).unwrap_or(0)))
            })
            .map_err(|e| anyhow::anyhow!("查询失败: {e}"))?;
        for r in rows {
            let (sid, churn) = r.map_err(|e| anyhow::anyhow!("读取行失败: {e}"))?;
            snap.churn.insert(sid, churn);
        }
    }
    Ok(snap)
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

    // ---- TraeWork（TRAE SOLO CN）回合状态机 ----

    #[test]
    fn trae_work_turn_events_map_terminal_statuses() {
        // 首轮只播种：不重放历史终态
        assert_eq!(turn_event("", "completed", false, true), None);
        assert_eq!(turn_event("", "failed", false, true), None);
        // 非终态 → 终态（含「两轮之间跑完一生」：prev 为空也发）
        assert_eq!(turn_event("running", "completed", false, false), Some(EventKind::RunCompleted));
        assert_eq!(turn_event("", "completed", false, false), Some(EventKind::RunCompleted));
        assert_eq!(turn_event("running", "failed", false, false), Some(EventKind::RunFailed));
        // canceled = 用户主动停止 → RunAborted（不通知、无别的会话时收起光效）
        assert_eq!(turn_event("running", "canceled", false, false), Some(EventKind::RunAborted));
        // 终态不重复；未知非终态值只当运行中，不产事件；tombstone 压住重现
        assert_eq!(turn_event("completed", "completed", false, false), None);
        assert_eq!(turn_event("running", "thinking", false, false), None);
        assert_eq!(turn_event("", "canceled", true, false), None);
    }

    #[test]
    fn trae_work_prune_reports_lost_open_turns_and_tombstones_terminal() {
        let mut last: HashMap<String, SeenTurn> = HashMap::new();
        let mut tombstone: HashMap<String, u64> = HashMap::new();
        last.insert("done".into(), SeenTurn { status: "completed".into(), last_seen: 1 });
        last.insert("open".into(), SeenTurn { status: "running".into(), last_seen: 1 });

        assert!(prune_turns(&mut last, &mut tombstone, 1 + MISSING_KEEP_ROUNDS - 1).is_empty());
        let lost = prune_turns(&mut last, &mut tombstone, 1 + MISSING_KEEP_ROUNDS);
        assert!(last.is_empty(), "消失的行不应继续占用内存");
        assert_eq!(lost, vec!["open".to_string()], "运行中回合的行消失（会话被删）应上报闭环");
        assert!(tombstone.contains_key("done"), "终态条目进 tombstone 防撕裂快照重现时重复通知");
    }

    // ---- §1.12 / §2.7 回归 --------------------------------------------------

    /// §1.12a：「会话行消失」的闭环必须是 RunAborted（中止）而不是 RunFailed（失败）。
    /// WorkBuddy 曾发 RunFailed——弹「任务失败 · WorkBuddy」+ 亮失败色 + 失败音，
    /// 而这条闭环的目标恰是「别等僵死判死在随机时刻亮失败色」；与 TraeWork 口径统一。
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

        emit_lost_row(&stop, &tx, AgentKind::TraeWork, "turn-lost", "TraeWork");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.kind, EventKind::RunAborted, "两个监控型 adapter 同口径");
        assert!(ev.message.contains("TraeWork"), "正文要带来源标签: {}", ev.message);
        // 停止标志置位后不再发（emit 的既有语义不得回归）
        stop.store(true, Ordering::Relaxed);
        emit_lost_row(&stop, &tx, AgentKind::TraeWork, "x", "TraeWork");
        assert!(rx.try_recv().is_err());
    }

    /// §1.12b：TraeWork 首轮对 open 集内（正在跑）的会话直接心跳——
    /// 旧实现 `hb = !first && (...)` 让 daemon 启动时正在跑的会话最长 60s 不亮流光。
    #[test]
    fn trae_work_first_round_heartbeats_open_sessions() {
        let every = 12;
        assert!(
            sess_needs_heartbeat(None, 5, 1, true, true, every),
            "首轮 open 会话必须立即心跳（点亮流光）"
        );
        // 首轮非 open（空闲/只有历史回合）：不心跳——心跳只给运行中会话
        assert!(!sess_needs_heartbeat(None, 5, 1, false, true, every));
        // 首轮已有 prev（理论上不存在）也不该重复心跳
        let prev = SeenSess { churn: 5, last_seen: 1, last_hb: 0 };
        assert!(!sess_needs_heartbeat(Some(&prev), 5, 1, true, true, every));
    }

    /// §1.12b 的对偶：首轮不重放终态语义不变（心跳判定不影响 turn_event 的 first 门）
    #[test]
    fn trae_work_first_round_still_never_emits_terminal() {
        // 与 first_round_never_emits_terminal 同款断言，钉死「补首轮心跳」没有
        // 顺手放开「首轮重放历史终态」——启动风暴不能回来
        assert_eq!(turn_event("", "completed", false, true), None);
        assert_eq!(turn_event("", "failed", false, true), None);
        assert_eq!(turn_event("running", "canceled", false, true), None);
    }

    /// 非首轮的心跳节奏：活动计数器变化触发
    #[test]
    fn sess_heartbeat_fires_on_churn_change() {
        let every = 12;
        let same = SeenSess { churn: 5, last_seen: 20, last_hb: 10 };
        // churn 变化（回合内行持续追加）→ 心跳（无论是否 open）
        assert!(sess_needs_heartbeat(Some(&same), 6, 21, true, false, every));
        assert!(sess_needs_heartbeat(Some(&same), 6, 21, false, false, every));
        // 无变化、刚心跳过 → 不发
        let fresh = SeenSess { churn: 5, last_seen: 20, last_hb: 20 };
        assert!(!sess_needs_heartbeat(Some(&fresh), 5, 21, true, false, every));
        // 非首轮的新会话（prev 为空）：changed = !first = true → 心跳
        assert!(sess_needs_heartbeat(None, 0, 30, false, false, every));
    }

    /// 保活重发只给 open（运行中）会话：空闲会话不该周期性点亮
    #[test]
    fn sess_heartbeat_keepalive_only_for_open_sessions() {
        let every = 12;
        let aged = SeenSess { churn: 5, last_seen: 20, last_hb: 5 };
        assert!(sess_needs_heartbeat(Some(&aged), 5, 17, true, false, every), "open + 到保活周期 → 重发");
        assert!(!sess_needs_heartbeat(Some(&aged), 5, 17, false, false, every), "非 open 无保活");
        // 未到保活周期不发
        let fresh = SeenSess { churn: 5, last_seen: 20, last_hb: 16 };
        assert!(!sess_needs_heartbeat(Some(&fresh), 5, 17, true, false, every));
    }

    /// §2.7a：残留快照目录清理必须同时覆盖 `agent-bark-watch-` 与
    /// `agent-bark-traework-` 两套前缀，且绝不误删同前缀的无关目录
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

    /// WAL 回归（2026-09-24「两次提问无光效」事故）：指纹必须盯住 `-wal`——
    /// WAL 增长而主库不动时也要触发重读，否则缓存快照永不更新
    #[test]
    fn db_fingerprint_tracks_wal_changes() {
        let dir = std::env::temp_dir().join(format!("ab-fp-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("database.db");
        std::fs::write(&db, b"main").unwrap();
        let fp0 = db_fingerprint(&db).expect("主库存在就该有指纹");
        assert_eq!(fp0.1, (0, None), "无 -wal 时指纹的 wal 侧为空");
        let wal = crate::traework_db::wal_path(&db);
        std::fs::write(&wal, b"wal-data").unwrap();
        let fp1 = db_fingerprint(&db).unwrap();
        assert_ne!(fp0, fp1, "-wal 新增必须改变指纹");
        std::fs::write(&wal, b"wal-data-longer").unwrap();
        let fp2 = db_fingerprint(&db).unwrap();
        assert_ne!(fp1, fp2, "-wal 增长必须改变指纹");
        assert_eq!(fp2.0, fp0.0, "主库没变时指纹的主库侧不动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端冒烟：解密**真实** TraeWork 库并查询（验证 rusqlite 能打开
    /// reserve=80 的明文快照 + 本机密钥常量仍有效）。默认跳过，
    /// 本机装了 TraeWork 时手动跑：`cargo test -p bark-adapters --lib -- --ignored`
    #[test]
    #[ignore]
    fn trae_work_live_snapshot_smoke() {
        let db = TraeWorkWatch::db_path().expect("APPDATA 未设置");
        if !db.exists() {
            eprintln!("跳过：本机没有 TraeWork 会话库（{}）", db.display());
            return;
        }
        let snap = snapshot(&db).expect("解密 + 查询应成功");
        eprintln!(
            "turns={} sessions={} churn={}",
            snap.turns.len(),
            snap.meta.len(),
            snap.churn.len()
        );
        assert!(!snap.turns.is_empty(), "应能读到 chat_turn 行");
    }
}
