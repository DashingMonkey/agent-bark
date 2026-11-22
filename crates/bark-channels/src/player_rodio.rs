//! rodio 播放底座（Win/mac）：queue(true) 空槽播静音 = 保活；append 零延迟插队
//! （上游 rodio `no_delay_when_added` 测试背书）。缓冲与事件句柄寿命归 rodio/cpal，
//! 不再手写 winmm——header/event 手工寿命那类 UAF 整类移除。
//!
//! 冷启动用「预缓冲」而不是别的：静音先排在真声音前面，设备唤醒/流启动的那段静默
//! 由静音承担，开头几个音符就不会被吞。这是音频栈的通行做法（PulseAudio 的
//! `pa_buffer_attr.prebuf` 就是这个语义），时长见 [`PRE_ROLL_SECS`]。

use std::num::{NonZeroU16, NonZeroU32};
use std::time::{Duration, Instant};

use rodio::queue::SourcesQueueInput;
use rodio::buffer::SamplesBuffer;
use rodio::Source;

/// 冷启动预缓冲时长。
///
/// 这不是「拍一个数字」，而是音频栈的标准行为：PulseAudio 的 `pa_buffer_attr.prebuf`
/// 官方定义就是「缓冲里攒够 prebuf 字节之前不开始播放」，未指定时默认等于 `tlength`，
/// 文档说这个值可以达到 2s 以上（见 freedesktop pulseaudio doxygen `pa_buffer_attr`）。
/// 目的正是本文这一条——不让设备唤醒 / 流启动的静默把开头几个音符吃掉。
///
/// 取 1s：覆盖 USB DAC、蓝牙 A2DP 唤醒的常见量级，又不至于让每一声都明显迟到。
/// 只有「保活没开」的冷路径才付这个代价，会话期内走共享队列是零延迟。
pub const PRE_ROLL_SECS: f32 = 1.0;

/// 一条长开的输出流 + 保活队列。持有 = 设备醒；drop = cpal 自己 join 渲染线程。
pub struct Sink {
    _sink: rodio::MixerDeviceSink,
    pub queue: std::sync::Arc<SourcesQueueInput>,
    pub dead: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub fn open() -> anyhow::Result<Sink> {
    let t0 = Instant::now();
    let dead = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = dead.clone();
    let sink = rodio::DeviceSinkBuilder::from_default_device()
        .map_err(|e| anyhow::anyhow!("无默认输出设备: {e}"))?
        .with_error_callback(move |e| {
            tracing::warn!("音频流错误（设备失效/被抢占）: {e}");
            d.store(true, std::sync::atomic::Ordering::Relaxed);
        })
        .open_stream()
        .map_err(|e| anyhow::anyhow!("打开输出流失败: {e}"))?;
    let (queue, rx) = rodio::queue::queue(true);
    sink.mixer().add(rx);
    // 每次开流记一条（只有冷路径会开流，热路径复用保活流，不会刷屏）：
    // 这是「声音为什么迟到 / 到底有没有走到播放」最直接的证据。
    tracing::info!("音频输出流已打开，耗时 {}ms", t0.elapsed().as_millis());
    Ok(Sink { _sink: sink, queue, dead })
}

/// 解析 RIFF/WAV（只认 PCM），返回 (声道, 采样率, PCM 数据)
pub fn parse_wav(wav: &[u8]) -> anyhow::Result<(u16, u32, &[u8])> {
    if wav.len() < 44 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        anyhow::bail!("不是 RIFF/WAV 数据");
    }
    let mut fmt: Option<(u16, u32)> = None;
    let mut data: Option<&[u8]> = None;
    let mut i = 12usize;
    while i + 8 <= wav.len() {
        let id = &wav[i..i + 4];
        let size = u32::from_le_bytes(wav[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &wav[i + 8..(i + 8 + size).min(wav.len())];
        if id == b"fmt " && body.len() >= 16 {
            let tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            if tag != 1 {
                anyhow::bail!("非 PCM WAV（tag={tag}）");
            }
            fmt = Some((
                u16::from_le_bytes(body[2..4].try_into().unwrap()),
                u32::from_le_bytes(body[4..8].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(body);
        }
        i += 8 + size + (size & 1);
    }
    match (fmt, data) {
        (Some((ch, rate)), Some(d)) => Ok((ch, rate, d)),
        _ => anyhow::bail!("WAV 缺少 fmt/data chunk"),
    }
}

/// 内置 WAV（i16）→ f32 SamplesBuffer；格式转换交给 rodio mixer
pub fn wav_source(wav: &[u8]) -> anyhow::Result<SamplesBuffer> {
    let (ch, rate, data) = parse_wav(wav)?;
    // 末尾若剩单字节（不合规的奇数 data 长度）由 as_chunks 自动丢弃，与旧 chunks_exact 同口径
    let samples: Vec<f32> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
        .collect();
    Ok(SamplesBuffer::new(
        NonZeroU16::new(ch).ok_or_else(|| anyhow::anyhow!("WAV 声道为 0"))?,
        NonZeroU32::new(rate).ok_or_else(|| anyhow::anyhow!("WAV 采样率为 0"))?,
        samples,
    ))
}

/// 纯静音源（冷启动 pre-roll 用）
pub fn silence_source(secs: f32) -> SamplesBuffer {
    let n = (44100.0 * secs) as usize;
    SamplesBuffer::new(
        NonZeroU16::new(1).unwrap(),
        NonZeroU32::new(44100).unwrap(),
        vec![0.0; n],
    )
}

/// 私有流播放：开流 →（冷时垫预缓冲静音）→ 入队，然后把「等播完 + drop」交给后台线程。
///
/// 为什么要移交：早 drop 会把声音截断，所以必须有人持有 sink 直到播完；而调用方
/// （事件管道 / 测试按钮）不该被音频时长挂住——自定义音效可能长达几分钟。
/// 同步返回的错误只剩「开不出输出设备」，参数与文件校验在更上层（play_sound）完成。
/// 用于保活未开时（无会话点测试等）；保活开着时走共享队列插队，不用这条。
pub fn play_private(source: impl Source + Send + 'static, cold: bool) -> anyhow::Result<()> {
    let sink = open()?;
    if cold {
        // 预缓冲：静音排在真声音前面，让设备唤醒/流启动的静默落在静音上（见 PRE_ROLL_SECS）
        sink.queue.append(silence_source(PRE_ROLL_SECS));
    }
    let done = sink.queue.append_with_signal(source);
    std::thread::spawn(move || {
        // 上限 10 分钟：超长自定义文件不无限挂住后台线程（宁可截断也不漏线程）
        let _ = done.recv_timeout(Duration::from_secs(600));
        drop(sink);
    });
    Ok(())
}
