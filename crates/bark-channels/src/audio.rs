//! 音频宿主（Win/mac）：一条线程持有输出设备，播放与保活都以命令进入。
//!
//! 之前设备状态是全局 `Mutex<Option<Alive>>`，调用方各自判冷热、各自往队列里塞。
//! 现在只有这条线程碰设备：
//! - 调用方 send 即返回，不再 `spawn_blocking` 等设备；
//! - 冷热、预缓冲、音量、设备重建收在同一处，状态不用共享；
//! - 设备 open/drop（cpal 会 join 渲染线程）不挡事件管道。
//!
//! 队列用 `std::sync::mpsc`：提示音每秒最多几次，不需要无锁环形队列——
//! kira 那套 ringbuf 是给 60fps 游戏音频准备的，在这里是过度设计。
//!
//! 为什么仍保留一个 500ms 的空闲 tick：设备被系统抢占后错误回调只置一个
//! `dead` 标志，没有新命令进来时总得有人发现它。tick 只做「要不要重建」的
//! 检查，开销是一次 `Instant::now()` 比较。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::player_rodio::{self, Sink, PRE_ROLL_SECS};

/// 设备启动后多久算「热」：唤醒期间放声会被吞掉开头，这段时间仍按冷路径垫预缓冲
const WARM_AFTER: Duration = Duration::from_secs(1);
/// 空闲检查间隔（设备死后重建的最坏延迟）
const IDLE_TICK: Duration = Duration::from_millis(500);
/// 开流失败的重试退避上限
const MAX_BACKOFF: Duration = Duration::from_secs(16);

enum Cmd {
    Play {
        effect: String,
        custom: Option<PathBuf>,
        volume: f32,
    },
    /// 与 Play 相同，但把结果回给调用方（设置页「测试」需要同步反馈）
    Preview {
        effect: String,
        custom: Option<PathBuf>,
        volume: f32,
        reply: Sender<Result<(), String>>,
    },
    KeepAlive(bool),
}

/// 调用方持有的句柄。Clone 廉价（Sender 是 Arc 语义）。
#[derive(Clone)]
pub struct Host {
    tx: Sender<Cmd>,
}

static HOST: OnceLock<Host> = OnceLock::new();

pub fn host() -> &'static Host {
    HOST.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        // 起线程失败几乎不可能（OS 拒绝建线程），真发生了就退化成「所有声音静默」，
        // 不值得为此 panic 掉整个托盘应用
        match std::thread::Builder::new()
            .name("agent-bark-audio".into())
            .spawn(move || run(rx))
        {
            Ok(_) => {}
            Err(e) => tracing::error!("音频线程创建失败，状态音效将不可用: {e}"),
        }
        Host { tx }
    })
}

impl Host {
    /// 保活开关（幂等）。false 立刻放设备睡（正在播的声音会被切断，与旧 stop 同语义）。
    pub fn keepalive(&self, on: bool) {
        let _ = self.tx.send(Cmd::KeepAlive(on));
    }

    /// 播一次，立即返回。只代表「已入队」——真正的失败在音频线程里记日志。
    pub fn play(&self, effect: &str, custom: Option<&Path>, volume: f32) {
        let _ = self.tx.send(Cmd::Play {
            effect: effect.to_string(),
            custom: custom.map(Path::to_path_buf),
            volume,
        });
    }

    /// 播一次并等线程确认（文件/解码/开流错误能同步回传）。设置页「测试」用。
    pub fn preview(&self, effect: &str, custom: Option<&Path>, volume: f32) -> Result<(), String> {
        let (reply, got) = mpsc::channel();
        self.tx
            .send(Cmd::Preview {
                effect: effect.to_string(),
                custom: custom.map(Path::to_path_buf),
                volume,
                reply,
            })
            .map_err(|_| "音频线程已退出".to_string())?;
        // 这里等的是「开流 + 解码 + 入队」，不含预缓冲的播放时间。冷设备唤醒可能
        // 几百毫秒，自定义 mp3 解码也要时间，留 3s 余量；超时只代表没等到回执，
        // 声音仍会在队列里照常播。
        got.recv_timeout(Duration::from_secs(3))
            .map_err(|_| "音频线程无响应（声音仍会播放）".to_string())?
    }
}

/// 一条活着的输出流
struct Device {
    sink: Sink,
    started: Instant,
}

impl Device {
    fn alive(&self) -> bool {
        !self.sink.dead.load(Ordering::Relaxed)
    }

    /// 设备醒着且已过唤醒期
    fn warm(&self) -> bool {
        self.alive() && self.started.elapsed() > WARM_AFTER
    }
}

fn run(rx: Receiver<Cmd>) {
    let mut dev: Option<Device> = None;
    let mut keepalive = false;
    let mut fails: u32 = 0;
    let mut retry_at = Instant::now();

    loop {
        let cmd = match rx.recv_timeout(IDLE_TICK) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if let Some(Cmd::KeepAlive(on)) = cmd.as_ref() {
            keepalive = *on;
            if !keepalive {
                // drop 会 join cpal 渲染线程，但在这条线程里，不挡调用方
                dev = None;
            }
        }

        // 要保活就必须有活设备：没开 / 已死（被系统抢占）→ 重建，失败按指数退避重试
        let need_open = keepalive && dev.as_ref().is_none_or(|d| !d.alive());
        if need_open && Instant::now() >= retry_at {
            match player_rodio::open() {
                Ok(sink) => {
                    dev = Some(Device { sink, started: Instant::now() });
                    fails = 0;
                    retry_at = Instant::now();
                    tracing::debug!("音频设备已就绪");
                }
                Err(e) => {
                    fails += 1;
                    let backoff = Duration::from_millis(500) * 2u32.pow(fails.min(5));
                    retry_at = Instant::now() + backoff.min(MAX_BACKOFF);
                    // 1/2/4/8 次打 warn，其余降 debug：无声卡环境不能刷屏
                    if fails.is_power_of_two() {
                        tracing::warn!("音频设备打开失败（第 {fails} 次，{}s 后重试）: {e:#}", backoff.min(MAX_BACKOFF).as_secs());
                    } else {
                        tracing::debug!("音频设备打开失败（第 {fails} 次）: {e:#}");
                    }
                }
            }
        }

        match cmd {
            Some(Cmd::Play { effect, custom, volume }) => {
                if let Err(e) = play(&mut dev, &effect, custom.as_deref(), volume) {
                    tracing::warn!("状态音效播放失败: {e}");
                }
            }
            Some(Cmd::Preview { effect, custom, volume, reply }) => {
                // 测试按钮：结果同步回传（前端要显示「播放失败：…」）
                let _ = reply.send(play(&mut dev, &effect, custom.as_deref(), volume));
            }
            Some(Cmd::KeepAlive(_)) | None => {}
        }
    }
    tracing::debug!("音频线程退出");
}

/// 播一次：热设备直接插队；冷设备垫预缓冲；没有设备就开一条私有流播完即弃。
fn play(
    dev: &mut Option<Device>,
    effect: &str,
    custom: Option<&Path>,
    volume: f32,
) -> Result<(), String> {
    match custom {
        Some(path) => {
            let file = std::fs::File::open(path).map_err(|e| format!("打开自定义音效失败: {e}"))?;
            let decoder = rodio::Decoder::new(std::io::BufReader::new(file))
                .map_err(|e| format!("解码自定义音效失败: {e}"))?;
            emit(dev, decoder, volume)
        }
        None => {
            let effect = crate::builtin(effect).ok_or_else(|| format!("未知音效: {effect}"))?;
            let source = player_rodio::wav_source(effect.wav).map_err(|e| format!("{e:#}"))?;
            emit(dev, source, volume)
        }
    }
}

/// 统一的出声点：音量在这里乘，冷热在这里判，预缓冲在这里垫。
fn emit(
    dev: &mut Option<Device>,
    source: impl rodio::Source<Item = f32> + Send + 'static,
    volume: f32,
) -> Result<(), String> {
    let source = source.amplify(volume.clamp(0.0, 1.0));
    match dev {
        Some(d) => {
            if !d.warm() {
                d.sink.queue.append(player_rodio::silence_source(PRE_ROLL_SECS));
            }
            d.sink.queue.append(source);
            Ok(())
        }
        // 保活没开：开一条私有流，后台线程等播完再 drop（设备能睡）
        None => player_rodio::play_private(source, true).map_err(|e| format!("{e:#}")),
    }
}
