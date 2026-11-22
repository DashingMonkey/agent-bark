//! 会话期保活：有活跃会话（或刚结束的宽限期内）且声音总开关开着时，
//! 保持音频设备唤醒 + 引擎热，让状态音与光效同档出声、不被空闲门控吞掉。
//! Win/mac = 长开 rodio 输出流（queue(true) 空槽播静音）；linux = 循环播静音文件的长进程。
//! 托盘空闲期不保活，设备照睡（省电语义保留）。
//!
//! 并发约定：`set()` 只由 `spawn_keepalive_watch` 的单条轮询任务串行调用，
//! 其余入口（`is_warm` / `live_queue` / `mark_dead`）会被播放线程并发调用。
//! 因此设备的 open / drop（可能阻塞几十到上百毫秒）一律放在锁外，锁内只做
//! 「看状态 + 换指针」——播放线程不能因为设备 IO 被挡住而延迟出声。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(any(windows, target_os = "macos"))]
use rodio::queue::SourcesQueueInput;

struct Alive {
    started: Instant,
    dead: Arc<AtomicBool>,
    #[cfg(any(windows, target_os = "macos"))]
    sink: Option<crate::player_rodio::Sink>,
    #[cfg(not(any(windows, target_os = "macos")))]
    child: Option<std::process::Child>,
}

static STATE: Mutex<Option<Alive>> = Mutex::new(None);
/// 连续打开失败次数：设备缺失时每 5s 重试一次，日志按 2 的幂退避，不能线性涨
static OPEN_FAILS: AtomicU32 = AtomicU32::new(0);

/// 锁中毒也照常取用：音频热路径上的一次 panic 不该让保活永久失效
fn state() -> MutexGuard<'static, Option<Alive>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 编排层入口：幂等。desired=true 且当前不活/已死 → 重建；false → 停。
pub fn set(active: bool) {
    if active {
        start();
    } else {
        stop();
    }
}

/// 保活是否已热：活着且启动超过 1s（启动瞬间设备还在唤醒，那一声仍按冷处理）
pub fn is_warm() -> bool {
    let g = state();
    match g.as_ref() {
        Some(a) => !a.dead.load(Ordering::Relaxed) && a.started.elapsed() > Duration::from_secs(1),
        None => false,
    }
}

/// 活着的保活队列：播放往这里 append = 零延迟插队（空槽静音垫着）
#[cfg(any(windows, target_os = "macos"))]
pub fn live_queue() -> Option<Arc<SourcesQueueInput>> {
    let g = state();
    match g.as_ref() {
        Some(a) if !a.dead.load(Ordering::Relaxed) => a.sink.as_ref().map(|s| s.queue.clone()),
        _ => None,
    }
}

/// 播放失败（设备失效/被抢占）时标记死亡：下一轮编排重建保活流
pub fn mark_dead() {
    if let Some(a) = state().as_ref() {
        a.dead.store(true, Ordering::Relaxed);
    }
}

fn start() {
    // 先把旧载体摘下来（锁内只 take），设备 drop / 打开都放到锁外：
    // reap 里 drop rodio sink 会 join cpal 渲染线程，持锁做会把播放线程堵住
    let stale = {
        let mut g = state();
        match g.as_ref() {
            Some(a) if !a.dead.load(Ordering::Relaxed) => return,
            _ => g.take(),
        }
    };
    if let Some(mut a) = stale {
        reap(&mut a);
    }
    if let Some(alive) = open_alive() {
        *state() = Some(alive);
        tracing::debug!("音频保活已启动");
    }
    // 失败时 STATE 保持空：is_warm / live_queue 自然回落冷路径，下一轮再重试
}

fn stop() {
    let stale = state().take();
    if let Some(mut a) = stale {
        reap(&mut a);
        tracing::debug!("音频保活已停止");
    }
}

/// 起一个活体。失败返回 None 并记日志（带退避），由下一轮编排重试。
#[cfg(any(windows, target_os = "macos"))]
fn open_alive() -> Option<Alive> {
    match crate::player_rodio::open() {
        Ok(sink) => {
            OPEN_FAILS.store(0, Ordering::Relaxed);
            let dead = sink.dead.clone();
            Some(Alive { started: Instant::now(), dead, sink: Some(sink) })
        }
        Err(e) => {
            note_open_failure(&e);
            None
        }
    }
}

/// linux 等：循环播静音文件的长进程（没有 sh / 播放器时算失败）
#[cfg(not(any(windows, target_os = "macos")))]
fn open_alive() -> Option<Alive> {
    let path = crate::silence_path(20);
    let script = format!("while :; do paplay {}; done", crate::sh_quote(&path.display().to_string()));
    let spawned = std::process::Command::new("sh")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(child) => {
            OPEN_FAILS.store(0, Ordering::Relaxed);
            Some(Alive {
                started: Instant::now(),
                dead: Arc::new(AtomicBool::new(false)),
                child: Some(child),
            })
        }
        Err(e) => {
            note_open_failure(&anyhow::anyhow!("保活进程启动失败（无 sh / 播放器？）: {e}"));
            None
        }
    }
}

/// 连续失败按 1/2/4/8… 次打 warn，其余降为 debug：无声卡环境（RDP、纯远程）
/// 每 5s 重试一次，不能让日志以小时为单位涨到几百行。
fn note_open_failure(e: &anyhow::Error) {
    let n = OPEN_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::warn!("保活输出流打开失败（连续 {n} 次，其余失败降为 debug）: {e:#}");
    } else {
        tracing::debug!("保活输出流打开失败（连续 {n} 次）: {e:#}");
    }
}

/// 等旧保活载体死透：rodio drop sink 时 cpal 自己 join 渲染线程；
/// unix 杀循环进程并 wait（残留 paplay 子进程 <=2s 自然结束）。
/// 调用方保证此处不持锁。
fn reap(a: &mut Alive) {
    #[cfg(any(windows, target_os = "macos"))]
    {
        a.sink.take();
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    if let Some(c) = a.child.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
}
