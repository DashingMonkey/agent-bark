//! 状态音效的判定：把「这次该不该响、响哪个、多大声」收成一个纯函数。
//!
//! 之前这些条件散在三处——事件管道的两道门、`play_state_sound` 的四道门、
//! 保活编排的四道门——已经出现过「静音了保活还在占设备」的漏同步。现在
//! 播放与保活都从这里取答案，而且每个「不响」都带可展示的原因
//! （[`SkipReason`] 直接进诊断页，不用再翻三个文件猜）。
//!
//! 本模块只做判定，不碰 IO：不开流、不读文件、不播声音。

use std::time::{Duration, Instant};

use bark_core::config::{NotifyConfig, StateSound};
use bark_core::{EventKind, NormalizedEvent};

/// 四个状态键，与「通知」页「声音」的四行一一对应
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundKey {
    Thinking,
    Waiting,
    Completed,
    Failed,
}

impl SoundKey {
    /// 稳定标识：日志、诊断、前端 key 都用它（与配置字段同名）
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    /// 该状态的音效配置
    pub fn config(self, cfg: &NotifyConfig) -> &StateSound {
        match self {
            Self::Thinking => &cfg.thinking,
            Self::Waiting => &cfg.waiting,
            Self::Completed => &cfg.completed,
            Self::Failed => &cfg.failed,
        }
    }
}

/// 播放源：内置素材（随二进制分发）或用户自选文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSource {
    /// 内置音效 id（见 `bark_channels::SOUND_EFFECTS`，含历史别名）
    Builtin(String),
    /// 用户自选音频文件的绝对路径
    Custom(std::path::PathBuf),
}

/// 判定结果
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Play {
        key: SoundKey,
        source: SoundSource,
        /// 0.0~1.0，来自 `notify.volume`
        volume: f32,
    },
    Skip(SkipReason),
}

/// 不响的原因。声明顺序即判定顺序，诊断页按这个措辞解释「上一声为什么没响」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 子代理事件：一个任务拆出十几个子代理会连响十几声
    Subagent,
    /// 用户主动中止（自己按的停止）
    Aborted,
    /// 事件类型不映射到任何状态音（工具调用、会话结束等）
    Unmapped,
    /// 思考音：相位没变（监控型 agent 的保活心跳会被误当成新回合）
    PhaseUnchanged,
    /// 托盘静音
    Muted,
    /// 声音总开关关闭
    Disabled,
    /// 免打扰时段
    QuietHours,
    /// 该状态未配置音效
    NoEffect,
    /// 内置音效 id 不在目录里（配置残留 / 手改写错）
    UnknownEffect,
    /// 选了自定义但没指定文件，或文件已被移走
    NoSoundFile,
    /// 节流窗口内（同状态刚响过）
    Throttled,
}

impl SkipReason {
    /// 是否值得写进诊断（「上一次为什么没响」）。
    ///
    /// 只保留「被开关挡住」和「配置有问题」两类：子代理、相位未变、未映射、已合并
    /// 都是设计内行为，记下来只会让那行诊断长期停在一条无意义的记录上——
    /// 而用户真正会问的是「我明明配了音效，怎么不响」。
    pub fn is_notable(self) -> bool {
        matches!(
            self,
            Self::Muted | Self::Disabled | Self::QuietHours | Self::NoEffect
                | Self::UnknownEffect | Self::NoSoundFile
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
            Self::Aborted => "aborted",
            Self::Unmapped => "unmapped",
            Self::PhaseUnchanged => "phase_unchanged",
            Self::Muted => "muted",
            Self::Disabled => "disabled",
            Self::QuietHours => "quiet_hours",
            Self::NoEffect => "no_effect",
            Self::UnknownEffect => "unknown_effect",
            Self::NoSoundFile => "no_sound_file",
            Self::Throttled => "throttled",
        }
    }
}

/// 判定输入。所有外部状态由调用方取好传进来，[`decide`] 因此可纯测。
pub struct SoundInput<'a> {
    pub event: &'a NormalizedEvent,
    /// 该会话相位是否真的跃迁了（保活心跳不算）
    pub phase_changed: bool,
    pub cfg: &'a NotifyConfig,
    pub muted: bool,
    pub quiet_now: bool,
    /// 同一状态上一次真正响的时刻；`None` = 这段时间内没响过
    pub last_played: Option<Instant>,
    pub now: Instant,
}

/// 事件 → 状态键。不映射的事件（工具调用、会话结束、打断）没有状态音。
pub fn key_for(event: &NormalizedEvent) -> Option<SoundKey> {
    match event.kind {
        EventKind::RunCompleted => Some(SoundKey::Completed),
        EventKind::RunFailed => Some(SoundKey::Failed),
        EventKind::PermissionRequired | EventKind::InputRequired => Some(SoundKey::Waiting),
        // 不带工具名的 Activity = 新回合开始 / 心跳；靠相位门区分
        EventKind::Activity if event.tool_name.is_none() => Some(SoundKey::Thinking),
        _ => None,
    }
}

/// 保活门的输入：什么时候该让音频设备醒着
pub struct KeepAliveInput<'a> {
    pub cfg: &'a NotifyConfig,
    pub muted: bool,
    pub quiet_now: bool,
    /// 有活跃会话
    pub has_sessions: bool,
    /// 距上次见到会话仍在宽限期内（覆盖回合结束的终态音）
    pub in_grace: bool,
    /// 刚点过试听 / 刚播过声音的保持窗
    pub hold: bool,
}

/// 保活门。
///
/// 与 [`decide`] 共用同一份配置和同样的三关（总开关 / 静音 / 免打扰）：
/// 这两组条件天生同源，分开维护过一次，结果就是「静音了还在占着音频设备」。
/// 保持窗（试听 / 刚播过）优先于其它条件——那时即使静音、即使没有会话也要热着。
pub fn wants_keepalive(input: KeepAliveInput<'_>) -> bool {
    let KeepAliveInput { cfg, muted, quiet_now, has_sessions, in_grace, hold } = input;
    if hold {
        return true;
    }
    cfg.enabled && !muted && !quiet_now && (has_sessions || in_grace)
}

/// 该状态此刻该不该响。判定顺序与 [`SkipReason`] 的声明顺序一致。
pub fn decide(input: SoundInput<'_>) -> Verdict {
    let SoundInput { event, phase_changed, cfg, muted, quiet_now, last_played, now } = input;

    // ---- 事件本身能不能出状态音 ----
    if event.is_subagent {
        return Verdict::Skip(SkipReason::Subagent);
    }
    if event.kind == EventKind::RunAborted {
        return Verdict::Skip(SkipReason::Aborted);
    }
    let Some(key) = key_for(event) else {
        return Verdict::Skip(SkipReason::Unmapped);
    };
    // 只有思考音需要相位门：它与保活心跳共用「无工具名的 Activity」这一载体，
    // 不设门的话每个心跳周期都会凭空响一声（实测）。
    if key == SoundKey::Thinking && !phase_changed {
        return Verdict::Skip(SkipReason::PhaseUnchanged);
    }

    // ---- 全局开关 ----
    if muted {
        return Verdict::Skip(SkipReason::Muted);
    }
    if !cfg.enabled {
        return Verdict::Skip(SkipReason::Disabled);
    }
    if quiet_now {
        return Verdict::Skip(SkipReason::QuietHours);
    }

    // ---- 该状态的配置 ----
    let sound = key.config(cfg);
    let effect = sound.effect.trim();
    if effect.is_empty() {
        return Verdict::Skip(SkipReason::NoEffect);
    }
    let source = if effect == "custom" {
        let path = sound.custom_path.trim();
        if path.is_empty() || !std::path::Path::new(path).is_file() {
            return Verdict::Skip(SkipReason::NoSoundFile);
        }
        SoundSource::Custom(std::path::PathBuf::from(path))
    } else {
        if !bark_channels::is_known_effect(effect) {
            return Verdict::Skip(SkipReason::UnknownEffect);
        }
        SoundSource::Builtin(effect.to_string())
    };

    // ---- 节流 ----
    let throttle = Duration::from_millis(cfg.throttle_ms as u64);
    if !throttle.is_zero() {
        if let Some(last) = last_played {
            if now.saturating_duration_since(last) < throttle {
                return Verdict::Skip(SkipReason::Throttled);
            }
        }
    }

    Verdict::Play { key, source, volume: f32::from(cfg.volume.min(100)) / 100.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, tool: Option<&str>) -> NormalizedEvent {
        NormalizedEvent {
            run: None,
            completion_deferred: false,
            id: "id-1".into(),
            agent: "claude-code".into(),
            kind,
            session_id: "s1".into(),
            transcript_path: None,
            cwd: "C:/p".into(),
            project: None,
            message: String::new(),
            timestamp: 0,
            is_subagent: false,
            tool_name: tool.map(str::to_string),
            parent_session_id: None,
            keepalive: false,
            codex: None,
        }
    }

    /// 全部通过时的输入；测试按需改单个字段
    fn inp<'a>(event: &'a NormalizedEvent, cfg: &'a NotifyConfig) -> SoundInput<'a> {
        SoundInput {
            event,
            phase_changed: true,
            cfg,
            muted: false,
            quiet_now: false,
            last_played: None,
            now: Instant::now(),
        }
    }

    fn cfg_with(effect: &str) -> NotifyConfig {
        NotifyConfig {
            throttle_ms: 0,
            completed: StateSound { effect: effect.into(), custom_path: String::new() },
            ..NotifyConfig::default()
        }
    }

    #[test]
    fn completed_with_effect_plays() {
        let cfg = cfg_with("confirm");
        let e = ev(EventKind::RunCompleted, None);
        match decide(inp(&e, &cfg)) {
            Verdict::Play { key, source, volume } => {
                assert_eq!(key, SoundKey::Completed);
                assert_eq!(source, SoundSource::Builtin("confirm".into()));
                assert_eq!(volume, 1.0);
            }
            other => panic!("应播放，实际 {other:?}"),
        }
    }

    #[test]
    fn volume_follows_config_and_is_capped() {
        let e = ev(EventKind::RunCompleted, None);
        let mut cfg = cfg_with("confirm");
        cfg.volume = 40;
        match decide(inp(&e, &cfg)) {
            Verdict::Play { volume, .. } => assert!((volume - 0.4).abs() < 1e-6),
            other => panic!("应播放，实际 {other:?}"),
        }
        cfg.volume = 250; // 手改越界：夹到 100
        match decide(inp(&e, &cfg)) {
            Verdict::Play { volume, .. } => assert_eq!(volume, 1.0),
            other => panic!("应播放，实际 {other:?}"),
        }
    }

    #[test]
    fn each_gate_reports_its_own_reason() {
        let cfg = cfg_with("confirm");
        let done = ev(EventKind::RunCompleted, None);

        let mut sub_event = ev(EventKind::RunCompleted, None);
        sub_event.is_subagent = true;
        assert_eq!(
            decide(inp(&sub_event, &cfg)),
            Verdict::Skip(SkipReason::Subagent)
        );

        let aborted = ev(EventKind::RunAborted, None);
        assert_eq!(decide(inp(&aborted, &cfg)), Verdict::Skip(SkipReason::Aborted));

        let tool_activity = ev(EventKind::Activity, Some("Read"));
        assert_eq!(
            decide(inp(&tool_activity, &cfg)),
            Verdict::Skip(SkipReason::Unmapped)
        );

        let mut muted = inp(&done, &cfg);
        muted.muted = true;
        assert_eq!(decide(muted), Verdict::Skip(SkipReason::Muted));

        let mut quiet = inp(&done, &cfg);
        quiet.quiet_now = true;
        assert_eq!(decide(quiet), Verdict::Skip(SkipReason::QuietHours));

        let mut off = cfg_with("confirm");
        off.enabled = false;
        assert_eq!(decide(inp(&done, &off)), Verdict::Skip(SkipReason::Disabled));

        let empty = cfg_with("");
        assert_eq!(decide(inp(&done, &empty)), Verdict::Skip(SkipReason::NoEffect));

        let unknown = cfg_with("nope");
        assert_eq!(
            decide(inp(&done, &unknown)),
            Verdict::Skip(SkipReason::UnknownEffect)
        );
    }

    #[test]
    fn thinking_needs_phase_change_only() {
        let mut cfg = cfg_with("confirm");
        cfg.thinking.effect = "confirm".into();
        let thinking = ev(EventKind::Activity, None);

        let mut heartbeat = inp(&thinking, &cfg);
        heartbeat.phase_changed = false;
        assert_eq!(decide(heartbeat), Verdict::Skip(SkipReason::PhaseUnchanged));

        // 其余状态不受相位门影响：终态本来就没有「相位」可比
        let done = ev(EventKind::RunCompleted, None);
        let mut terminal = inp(&done, &cfg);
        terminal.phase_changed = false;
        assert!(matches!(decide(terminal), Verdict::Play { .. }));
    }

    #[test]
    fn legacy_chime_alias_is_a_known_effect() {
        // 判定层与播放层的别名口径必须一致，否则 UI 说能响、这里却 Skip
        let cfg = cfg_with("chime");
        let e = ev(EventKind::RunCompleted, None);
        assert!(matches!(decide(inp(&e, &cfg)), Verdict::Play { .. }));
    }

    #[test]
    fn custom_without_file_is_skipped() {
        let mut cfg = NotifyConfig {
            throttle_ms: 0,
            completed: StateSound { effect: "custom".into(), custom_path: String::new() },
            ..NotifyConfig::default()
        };
        let e = ev(EventKind::RunCompleted, None);
        assert_eq!(decide(inp(&e, &cfg)), Verdict::Skip(SkipReason::NoSoundFile));

        // 文件补上就该播
        let dir = std::env::temp_dir().join("agent-bark-policy-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tone.mp3");
        std::fs::write(&p, b"x").unwrap();
        cfg.completed.custom_path = p.to_string_lossy().into_owned();
        match decide(inp(&e, &cfg)) {
            Verdict::Play { source, .. } => {
                assert_eq!(source, SoundSource::Custom(p.clone()));
            }
            other => panic!("有文件应播放，实际 {other:?}"),
        }
    }

    #[test]
    fn throttle_window_blocks_then_expires() {
        let mut cfg = cfg_with("confirm");
        cfg.throttle_ms = 300;
        let e = ev(EventKind::RunCompleted, None);
        let now = Instant::now();

        let mut recent = inp(&e, &cfg);
        recent.now = now;
        recent.last_played = Some(now - Duration::from_millis(100));
        assert_eq!(decide(recent), Verdict::Skip(SkipReason::Throttled));

        let mut stale = inp(&e, &cfg);
        stale.now = now;
        stale.last_played = Some(now - Duration::from_millis(400));
        assert!(matches!(decide(stale), Verdict::Play { .. }));

        // 关掉节流：同一时刻也照响
        let mut off = cfg.clone();
        off.throttle_ms = 0;
        let mut no_throttle = inp(&e, &off);
        no_throttle.now = now;
        no_throttle.last_played = Some(now - Duration::from_millis(10));
        assert!(matches!(decide(no_throttle), Verdict::Play { .. }));
    }

    fn ka<'a>(
        cfg: &'a NotifyConfig,
        muted: bool,
        quiet: bool,
        has: bool,
        grace: bool,
        hold: bool,
    ) -> KeepAliveInput<'a> {
        KeepAliveInput {
            cfg,
            muted,
            quiet_now: quiet,
            has_sessions: has,
            in_grace: grace,
            hold,
        }
    }

    #[test]
    fn keepalive_gate_shares_the_play_gates() {
        let cfg = NotifyConfig::default();
        assert!(wants_keepalive(ka(&cfg, false, false, true, false, false)), "会话中要保活");
        // 三关与播放判定同源：任何一个关掉都不该继续占设备
        assert!(!wants_keepalive(ka(&cfg, true, false, true, false, false)), "静音不保活");
        assert!(!wants_keepalive(ka(&cfg, false, true, true, false, false)), "免打扰不保活");
        let off = NotifyConfig { enabled: false, ..NotifyConfig::default() };
        assert!(!wants_keepalive(ka(&off, false, false, true, false, false)), "总开关关不保活");
        // 没有会话时靠宽限覆盖终态音
        assert!(!wants_keepalive(ka(&cfg, false, false, false, false, false)));
        assert!(wants_keepalive(ka(&cfg, false, false, false, true, false)), "宽限内要保持");
        // 试听/刚播过的保持窗优先：静音 + 无会话也要热着
        assert!(wants_keepalive(ka(&cfg, true, true, false, false, true)), "保持窗优先");
    }

    #[test]
    fn key_mapping_covers_four_states_only() {
        assert_eq!(key_for(&ev(EventKind::RunCompleted, None)), Some(SoundKey::Completed));
        assert_eq!(key_for(&ev(EventKind::RunFailed, None)), Some(SoundKey::Failed));
        assert_eq!(
            key_for(&ev(EventKind::PermissionRequired, None)),
            Some(SoundKey::Waiting)
        );
        assert_eq!(key_for(&ev(EventKind::InputRequired, None)), Some(SoundKey::Waiting));
        assert_eq!(key_for(&ev(EventKind::Activity, None)), Some(SoundKey::Thinking));
        assert_eq!(key_for(&ev(EventKind::Activity, Some("Read"))), None);
        assert_eq!(key_for(&ev(EventKind::RunAborted, None)), None);
    }
}
