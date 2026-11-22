# 状态音效架构设计

> 范围：事件 → 判定 → 播放 → 设备生命周期这条链路。
> 目标：说清楚现在哪里是对的、哪里是债、要不要重构、按什么顺序动。

## 0. 实施状态（2026-11-21，四阶段已落地）

| 阶段 | 落点 | 状态 |
|---|---|---|
| 1 判定收敛 | `app/src-tauri/src/sound_policy.rs`：`decide()` + `SkipReason` | ✅ 完成，含 8 个单测 |
| 2 节流 | `NotifyConfig.throttle_ms`（默认 250），在 `decide` 内按状态键判窗 | ✅ 完成 |
| 3 音量 | `NotifyConfig.volume`（0~100），rodio `amplify` / Linux 三家播放器参数 | ✅ 完成 |
| 4 命令线程 | `crates/bark-channels/src/audio.rs`：`Host::play/preview/keepalive` | ✅ Win/mac；Linux 保留外部播放器进程 |

与设计稿的两处偏差：

- **AudioHost 只覆盖 Win/mac**。Linux 继续用 `keepalive` + `paplay/ffplay/mpv`：
  把 Linux 也搬进 rodio 要引 ALSA/PipeWire 开发库，打包成本大于收益。
- **5s 轮询没有完全去掉，改成「事件驱动 + 500ms 兜底」**。事件决定要响时先
  `keepalive_set(true)`（不再等轮询），编排循环只负责「什么时候关」；音频线程
  保留 500ms 空闲 tick，用于发现被系统抢占后的设备重建。

调用链现在是：

```
事件 → sound_policy::decide() ──Play──► bark_channels::play_sound()
                    │                        └─ audio::Host::play()  (mpsc)
                    └─Skip(reason)──► last_sound_skip（诊断页）
                                              │
                                    音频线程：设备/冷热/预缓冲/音量/重建
```

## 1. 现状（重构前，保留作背景）

### 1.1 调用链

```
事件管道 (state.rs:602)
  ├─ 门1  !deferred && should_drive_glow(stale, suppressed_echo)
  ├─ 门2  !event.is_subagent
  ├─ 映射 sound_key_for(event) → thinking / waiting / completed / failed
  ├─ 门3  should_play_state_sound(key, phase_changed)   ← 只挡 thinking 心跳
  └─ play_state_sound(state, key)                        (state.rs:889)
        ├─ 门4 muted
        ├─ 门5 notify.enabled
        ├─ 门6 quiet_hours
        ├─ 门7 effect 为空
        └─ spawn_blocking → bark_channels::play_sound(effect, custom_path)
              ├─ cold = !keepalive::is_warm()
              ├─ builtin(effect) / custom(path)
              └─ play_source(source, cold)               (lib.rs:141)
                    ├─ 热：live_queue().append(source)
                    └─ 冷：play_private → open() + 队列前垫 PRE_ROLL(1s)

保活编排 (state.rs:923，5s 轮询)
  ├─ normal = enabled && !quiet && !muted && (has_sessions || grace_30s)
  └─ wanted = normal || sound_preview_active()   ← 测试按钮的 30s 窗
        └─ keepalive::set(wanted) → 开/关 rodio 输出流
```

三层各自持有一部分"要不要出声 / 要不要占设备"的知识。

### 1.2 已经做对、不要推翻的

| 决策 | 依据 |
|---|---|
| Win/mac 进程内 rodio，不起外部进程 | 无子进程、无黑框、句柄寿命归 cpal |
| Linux 用 paplay/ffplay 外部进程 | 避免 ALSA/PipeWire 开发库依赖，打包简单 |
| 保活用「长开输出流 + 空槽播静音」 | rodio 文档：drop `OutputStream` 即停止播放；`queue(true)` 空槽保活 |
| 冷启动垫预缓冲 | PulseAudio `pa_buffer_attr.prebuf` 同语义（缓冲够才起播） |
| 播放不阻塞调用方 | sink 移交后台线程等播完；事件管道不被音频时长挂住 |
| 保活失败退避 + 锁中毒兜底 | 无声卡环境不刷日志、不永久失效 |

### 1.3 问题清单

按"是否已经被用户感知到"排序。

**P0 判定分散，已经漏过一次**
`wanted`（保活）与 `play_state_sound`（播放）各自维护四道门。之前 `muted`/`quiet` 只加在播放侧，保活在静音时仍常开设备——本轮才补齐。这两组条件天生应该同源。

**P0 没有节流/合并，多会话并发会叠音**
`recent_notified`（state.rs:773）只挡通知推送的 6s 撕裂快照，声音没有任何对应机制。三个 agent 同时完成 → 三声叠在一起，听感是一声"糊"，还丢失了"完成了几个"的信息。

**P1 没有音量**
只能调系统音量。用户想让提示"轻一点"时，只能整体压系统音量，连音乐一起压。
libcanberra 为此提供 `CA_PROP_CANBERRA_VOLUME`；rodio 侧对应 `Source::amplify()`。

**P1 判定不可解释**
"为什么这次没响"要人肉串 7 道门 + 3 个文件。诊断页（`diagnostics`）拿不到任何声音侧的信息。

**P2 保活判定有 ≤5s 滞后**
`spawn_keepalive_watch` 是纯轮询。会话建立后最多 5s 才开流，这期间的声音全走冷路径（垫 1s）。短回合的 thinking + completed 两声都踩中。

**P2 魔数散落**
`5s`(轮询) / `30s`(宽限) / `1s`(is_warm) / `1s`(PRE_ROLL) 分别在 state.rs 与 bark-channels 两处，无统一出处。

**P3 测试路径与真实路径语义不同**
`test_sound` 绕过静音/免打扰门（刻意），并靠 30s 预览窗拿热路径。这是合理的 hack，但它说明"预览"应该是播放层的一等概念，而不是靠外部窗口模拟。

**P3 平台分支三套实现**
`play_builtin`/`play_custom`/`play_source` 里 win/mac 与 unix 各一份，pre-roll 在 Linux 上是"先播静音文件再播声音"两段式，与队列排队语义不同。行为可接受，但改动要同时改两处。

## 2. 结论：要不要重构

| 层 | 结论 | 理由 |
|---|---|---|
| 播放底座（rodio / pre-roll / keepalive） | **不重写** | 刚稳定、有测试覆盖、机制有官方背书 |
| 策略层（谁该响、响哪个、多响、多大声） | **要收敛** | P0/P1 都是这一层，且已经漏过一次 |
| 平台层（Linux 外部进程） | **不动** | 避免引入 ALSA 依赖，收益不抵成本 |
| 进程模型（全局 Mutex 单例） | **可选升级** | 见阶段 4，收益是消除轮询滞后；风险要单独评估 |

一句话：**不是"重构声音"，而是"把散在三处的判定收成一个模块，并补上节流和音量"。**

## 3. 目标架构

```
                    ┌──────────────────────────────┐
事件 ──► SoundPolicy│ 纯函数：门 + 映射 + 节流 + 音量 │──► SoundVerdict
                    └──────────────────────────────┘
                                  │ Play{source, volume}
                                  ▼
                          AudioHandle::play()      （阶段 4 才有）
                                  │ mpsc
                                  ▼
                    ┌──────────────────────────────┐
                    │ 音频线程：设备/保活/冷热/prebuf │
                    └──────────────────────────────┘
```

### 3.1 SoundPolicy（阶段 1）

放在 `app/src-tauri/src/sound_policy.rs`（依赖 config 与 event，不依赖 Tauri）。

```rust
pub struct SoundInput<'a> {
    pub event: &'a NormalizedEvent,
    pub phase_changed: bool,
    pub cfg: &'a NotifyConfig,
    pub muted: bool,
    pub quiet_now: bool,
    pub now: Instant,
    pub last_same_key: Option<Instant>,   // 节流用（跨事件保留）
}

pub enum Verdict {
    Play { key: SoundKey, source: SoundSource, volume: f32 },
    Skip(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Subagent, Stale, Aborted, Muted, Disabled, QuietHours,
    NoEffect, PhaseUnchanged, Throttled,
}

pub fn decide(input: SoundInput<'_>) -> Verdict;
```

收益：
- 7 道门收成 1 个函数，**一屏看完**；
- `SkipReason` 直接进 `diagnostics`，用户/排障时不用猜；
- 单测覆盖每种 reason，不需要真的出声；
- 保活的 `wanted` 也由同一份 cfg + muted + quiet 推导，不会再漂。

### 3.2 节流（阶段 2）

在 `decide` 内做，纯逻辑：

| 场景 | 规则 | 默认 |
|---|---|---|
| 同 key 同会话，短时间重复 | 只响第一声 | 250ms |
| 同 key 跨会话（3 个 agent 同时完成） | 窗口内合并为一声 | 400ms |
| 不同 key（completed + failed） | 互不压制 | — |

参数进配置 `notify.throttle_ms`（0 = 关闭）。默认值取保守值：只压"同一瞬间的重复"，不压"连续几次"。用户如果想知道"完成了几个"，可以关掉。

### 3.3 音量（阶段 3）

- 配置：`notify.volume: f32`（0.0~1.0，默认 1.0）
- rodio：`source.amplify(volume)`（内置 WAV 是 f32 `SamplesBuffer`，直接乘）
- Linux：`paplay --volume=<0..65536>`、`ffplay -volume <0..100>`、`mpv --volume=<0..100>`，三条分支都要带上
- 参考：libcanberra 的 `CA_PROP_CANBERRA_VOLUME`

### 3.4 AudioHost（阶段 4，可选）

把 `keepalive` 从"只管理设备"升级成"命令消费者"：

```rust
enum AudioCmd {
    Play { source: SoundSource, volume: f32 },
    Preview { source: SoundSource, done: oneshot::Sender<Result<(), String>> },
    KeepAlive(bool),
    Shutdown,
}
```

线程内串行处理：确保流 → 判冷热 → 垫 pre-roll → append。调用方只 send。

收益：
- 消除 5s 轮询滞后（命令即唤醒）；
- 冷热/音量/节流状态集中在有状态的一处；
- 调用方不再 `spawn_blocking`，测试按钮可以用 oneshot 拿到真实结果。

**不解决**"第一声冷"——设备物理唤醒时间，只有常开保活能消。

参考 kira 的 `AudioManager` + 命令队列，但**不照搬** ringbuf/无锁：提示音每秒最多几次，`mpsc` + 互斥完全够，引入无锁反而增加复杂度。

## 4. 迁移路径

| 阶段 | 改动 | 收益 | 风险 | 验收 |
|---|---|---|---|---|
| 1 | 抽 `SoundPolicy`，行为不变 | 判定单点、可解释、可单测 | 极低（纯提取） | 现有 173 个测试全绿 + 新增 reason 覆盖 |
| 2 | 节流 | 不叠音 | 低（默认值保守，可关） | 三会话并发只响一声；关掉后恢复 |
| 3 | 音量 | 独立于系统音量 | 中（Linux 三条播放器分支都要改） | 0/0.5/1 三档听感线性 |
| 4 | AudioHost | 消轮询滞后、调用方零阻塞 | 中高（动刚稳定的播放层） | 会话建立后第一声不再等 5s |

建议：**先做 1+2**（收益直接、风险低），3 看是否有人要，4 单独评估。
每阶段独立可合入、可回滚，不要求一次做完。

## 5. 开源参考

| 项目 | 可借鉴的点 |
|---|---|
| **libcanberra**（GNOME/KDE 事件音标准实现） | `CA_PROP_EVENT_ID` 事件抽象、`ca_context_cache()` 预加载降延迟、`CA_PROP_CANBERRA_VOLUME` 每声音量、`play/cancel/playing` 用 id 管理正在播的声音 |
| **XDG Sound Naming / Theming Spec** | 事件名（`message-new-instant` 等）与主题目录分离，系统可替换整套音效 |
| **PulseAudio `pa_buffer_attr.prebuf`** | "缓冲够才起播"，是冷启动垫静音的官方依据 |
| **kira** | 单音频线程 + 命令通道 + 句柄模型；音频线程不分配、不阻塞 |
| **cpal WASAPI `DefaultDeviceMonitor`** | 默认设备变化靠 `IMMNotificationClient` 上报，不必自己轮询（我们 0.18 已具备） |
| **rodio `OutputStream` 文档** | drop 即停播，所以保活必须持有 sink |

## 6. 明确不做

- **不换 libcanberra**：Linux-only，且要 C 依赖；我们三平台统一走现有实现更简单。
- **不做事件 id + 主题体系**：用户现在是直接选素材（7 个内置 + 自定义文件），直观且够用。真要做主题，再改 `StateSound.effect` 的语义。
- **不把 Linux 切到 rodio**：会引入 ALSA/PipeWire 开发库依赖，打包复杂度上升。
- **不追求"第一声零延迟"**：物理限制，只有常开保活能消；那是产品取舍（设备不睡 vs 第一声快），不是架构问题。

## 7. 验收清单

- [ ] `diagnostics` 能回答"上一声为什么没响"（带 `SkipReason`）
- [ ] 三个会话同时完成只响一声（且可关）
- [ ] 音量 0.5 时听感约为主观一半，不需要动系统音量
- [ ] 静音/免打扰时，保活与播放两侧判定一致（同一函数输出）
- [ ] 现有 173 个 Rust 测试 + 51 个前端测试保持全绿
