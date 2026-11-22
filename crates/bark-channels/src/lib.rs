//! 通知渠道：状态音效、Bark (iOS)、通用 Webhook 模板。
//! 桌面 Toast 通知已下线；hook 子命令离线时只落盘待补投，不再自行提醒。

#[cfg(any(windows, target_os = "macos"))]
mod audio;
/// Linux 等平台的保活：循环播静音文件的长进程。
/// Win/mac 走 [`audio`] 的命令线程，不需要这套全局状态。
#[cfg(not(any(windows, target_os = "macos")))]
pub mod keepalive;
#[cfg(any(windows, target_os = "macos"))]
mod player_rodio;

use bark_core::config::{BarkChannelConfig, WebhookChannelConfig};
use bark_core::EventKind;
use serde::Serialize;

/// 渠道统一的载荷
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    pub event: EventKind,
    pub agent: String,
    pub project: Option<String>,
}

pub trait Channel: Send + Sync {
    fn name(&self) -> &'static str;
    fn send(&self, n: &Notification) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// 状态音效：思考 / 等待 / 完成 / 失败 各配一个提示音（默认全部未选 = 静音）
// ---------------------------------------------------------------------------

/// 「通知」页「声音」下拉的一个选项
pub struct SoundEffect {
    pub id: &'static str,
    pub label: &'static str,
    /// 内嵌的 WAV 数据（44.1kHz 16bit 单声道，已统一响度）
    pub wav: &'static [u8],
}

/// 内置音效目录。全部是 CC0 开源素材（来源与许可见 assets/sounds/LICENSE.md），
/// 随二进制分发，不再依赖系统自带的提示音——Windows 自带的 ding.wav 又短又轻，
/// 实测听不清。id 与文案必须和前端 api.ts 的 SOUND_EFFECTS 保持一致。
pub const SOUND_EFFECTS: &[SoundEffect] = &[
    SoundEffect { id: "confirm", label: "确认", wav: include_bytes!("../assets/sounds/confirm.wav") },
    SoundEffect { id: "success", label: "成功", wav: include_bytes!("../assets/sounds/success.wav") },
    SoundEffect { id: "drop", label: "水滴", wav: include_bytes!("../assets/sounds/drop.wav") },
    SoundEffect { id: "pluck", label: "弹拨", wav: include_bytes!("../assets/sounds/pluck.wav") },
    SoundEffect { id: "glass", label: "玻璃", wav: include_bytes!("../assets/sounds/glass.wav") },
    SoundEffect { id: "error", label: "错误", wav: include_bytes!("../assets/sounds/error.wav") },
    SoundEffect { id: "deep", label: "低叮", wav: include_bytes!("../assets/sounds/deep.wav") },
];

/// 内置音效查找（含已下架 id 的别名）。抽成函数是为了让别名只有一处、
/// 且能单测而不用真的出声。
pub(crate) fn builtin(effect_id: &str) -> Option<&'static SoundEffect> {
    // 老配置兼容：chime 已下线（与 deep 同源的素材），映射到 confirm。
    // 早期版本写进配置的 chime 因此仍然有声，而不是静默 + 每次事件记一条「未知音效」。
    let id = if effect_id == "chime" { "confirm" } else { effect_id };
    SOUND_EFFECTS.iter().find(|e| e.id == id)
}

/// 该 id 是否是已知的内置音效（含历史别名）。
///
/// 供判定层区分「这个状态没配」与「配置里写了个不存在的 id」——后者要在诊断里
/// 明说，而不是走到播放层报一句被日志吞掉的「未知音效」。
pub fn is_known_effect(effect_id: &str) -> bool {
    builtin(effect_id.trim()).is_some()
}

/// 播放目标：内置素材或用户文件
enum Target<'a> {
    Builtin(&'static SoundEffect),
    Custom(&'a std::path::Path),
}

/// 参数校验：空 effect = 不响；custom 必须指定且存在；未知 id 报错。
///
/// 抽出来是因为「播放」和「试听」需要同一套校验，但对错误的处理不同：
/// 前者只记日志，后者要把错误显示在设置页。
fn resolve<'a>(
    effect_id: &str,
    custom_path: Option<&'a std::path::Path>,
) -> anyhow::Result<Option<Target<'a>>> {
    let id = effect_id.trim();
    if id.is_empty() {
        return Ok(None);
    }
    if id == "custom" {
        let p = custom_path
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| anyhow::anyhow!("自定义音效未选择文件"))?;
        if !p.is_file() {
            anyhow::bail!("自定义音效文件不存在: {}", p.display());
        }
        return Ok(Some(Target::Custom(p)));
    }
    let effect = builtin(id).ok_or_else(|| anyhow::anyhow!("未知音效: {effect_id}"))?;
    Ok(Some(Target::Builtin(effect)))
}

fn sanitize_volume(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// 播放一个音效一次。未配置（空 effect）为静默 no-op。
///
/// `volume` = 0.0~1.0，与系统音量相乘（rodio 走 `Source::amplify`，Linux 三家
/// 播放器各带自己的音量参数）。
///
/// **返回 Ok 只代表「已交给播放链路」**：Win/mac 把命令丢给音频线程（真正开流、
/// 垫预缓冲、失败重试都在那条线程里），Linux 起一个播放器进程。两种情况都立即
/// 返回，事件管道不会被音频时长或设备唤醒挂住。参数不合法 / 文件不存在会同步
/// 报错——那是设置页「测试」需要的那类反馈，见 [`preview_sound`]。
pub fn play_sound(
    effect_id: &str,
    custom_path: Option<&std::path::Path>,
    volume: f32,
) -> anyhow::Result<()> {
    let Some(target) = resolve(effect_id, custom_path)? else {
        return Ok(());
    };
    let volume = sanitize_volume(volume);
    #[cfg(any(windows, target_os = "macos"))]
    {
        match target {
            Target::Builtin(e) => audio::host().play(e.id, None, volume),
            Target::Custom(p) => audio::host().play("custom", Some(p), volume),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    play_process(target, !keepalive::is_warm(), volume);
    Ok(())
}

/// 试听一次：与 [`play_sound`] 同一条链路，但等音频线程确认。
///
/// 设置页的「测试」需要知道「到底响没响」（文件坏了、设备开不出来都要显示出来），
/// 而真实事件只记日志就够了。
pub fn preview_sound(
    effect_id: &str,
    custom_path: Option<&std::path::Path>,
    volume: f32,
) -> Result<(), String> {
    let target = resolve(effect_id, custom_path)
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| "该状态未配置音效".to_string())?;
    let volume = sanitize_volume(volume);
    #[cfg(any(windows, target_os = "macos"))]
    {
        match target {
            Target::Builtin(e) => audio::host().preview(e.id, None, volume),
            Target::Custom(p) => audio::host().preview("custom", Some(p), volume),
        }
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        play_process(target, !keepalive::is_warm(), volume);
        Ok(())
    }
}

/// 保活开关（幂等）。Win/mac 交给音频线程，Linux 起/停那条循环播静音的进程。
pub fn keepalive_set(on: bool) {
    #[cfg(any(windows, target_os = "macos"))]
    audio::host().keepalive(on);
    #[cfg(not(any(windows, target_os = "macos")))]
    keepalive::set(on);
}

/// Linux 等平台：解包 / 直接给路径，起一个后台播放器进程
#[cfg(not(any(windows, target_os = "macos")))]
fn play_process(target: Target<'_>, cold: bool, volume: f32) {
    let cmd = match target {
        Target::Builtin(e) => match unpack(e) {
            Ok(p) => sound_command(&p, cold, volume),
            Err(err) => {
                tracing::warn!("解包音效失败: {err:#}");
                return;
            }
        },
        Target::Custom(p) => sound_command(p, cold, volume),
    };
    match cmd {
        Some(mut c) => spawn_detached(&mut c),
        // 通知类声音允许失败（无播放器 / 静音环境），不打断主流程
        None => tracing::warn!("当前平台没有可用的音频播放器"),
    }
}

/// 生成纯零 WAV（与内置同参数：44.1kHz/16bit/mono），供冷启动 pre-roll 与保活循环
pub fn silence_wav_bytes(secs: f32) -> Vec<u8> {
    let rate = 44100u32;
    let bytes = ((rate as f32 * secs) as u32) * 2;
    let mut v = Vec::with_capacity(44 + bytes as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + bytes).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&bytes.to_le_bytes());
    v.resize(44 + bytes as usize, 0);
    v
}

/// 静音文件路径（十分之一秒为单位：5 = 0.5s pre-roll，20 = 2s 保活循环），按需生成进缓存目录
pub fn silence_path(tenths: u8) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("agent-bark-sounds");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("silence-{tenths}.wav"));
    let want = silence_wav_bytes(tenths as f32 / 10.0);
    let cached = std::fs::metadata(&path)
        .map(|m| m.len() as usize == want.len())
        .unwrap_or(false);
    if !cached {
        let _ = std::fs::write(&path, &want);
    }
    path
}

/// 把内嵌的 WAV 解包到缓存目录，返回文件路径。
/// 以「文件存在且长度一致」为复用条件，避免每次播放都写盘。
#[cfg(any(not(any(windows, target_os = "macos")), test))]
fn unpack(effect: &SoundEffect) -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join("agent-bark-sounds");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.wav", effect.id));
    let cached = std::fs::metadata(&path)
        .map(|m| m.len() as usize == effect.wav.len())
        .unwrap_or(false);
    if !cached {
        std::fs::write(&path, effect.wav)?;
    }
    Ok(path)
}

/// 以「完全无窗口」的方式起一个后台播放器进程。
///
/// stdio 全部置空（不让播放器往父进程的控制台/管子里写）；spawn 后必须有人 wait——
/// Unix 上不 wait 的 Child 会变成僵尸进程，长期运行会持续累积，这里挪到后台线程 reap。
/// 只服务 Linux 等 unix：Win/mac 已改为进程内 rodio，不再起外部进程。
#[cfg(not(any(windows, target_os = "macos")))]
fn spawn_detached(cmd: &mut std::process::Command) {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    // 通知类声音允许失败（无播放器/静音环境），不打断主流程
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// 音频文件 → 播放命令（播一次）。WAV 走 paplay（最轻）；自定义的非 WAV 格式
/// 换支持解码的播放器（ffplay / mpv），都没有再回落 paplay（会失败、只记日志）。
///
/// 音量按各家量纲换算：paplay `--volume` 是 0~65536，ffplay/mpv 是 0~100。
/// 冷启动的前置静音不占音量（它本来就是 0）。
#[cfg(all(unix, not(target_os = "macos")))]
fn sound_command(
    path: &std::path::Path,
    cold: bool,
    volume: f32,
) -> Option<std::process::Command> {
    let quoted = sh_quote(&path.display().to_string());
    let pre = if cold {
        format!("paplay {}; ", sh_quote(&silence_path(5).display().to_string()))
    } else {
        String::new()
    };
    let is_wav = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("wav"))
        .unwrap_or(false);
    let vol = volume.clamp(0.0, 1.0);
    let pa_vol = (vol * 65536.0).round() as u32;
    let pct = (vol * 100.0).round() as u32;
    let script = if is_wav {
        format!("{pre}paplay --volume={pa_vol} {quoted}")
    } else {
        format!(
            "{pre}if command -v ffplay >/dev/null 2>&1; then ffplay -nodisp -autoexit -loglevel quiet -volume {pct} {quoted}; elif command -v mpv >/dev/null 2>&1; then mpv --no-video --really-quiet --volume={pct} {quoted}; else paplay --volume={pa_vol} {quoted}; fi"
        )
    };
    let mut c = std::process::Command::new("sh");
    c.args(["-c", &script]);
    Some(c)
}

/// POSIX sh 单引号字面量转义：`'` → `'\''`（闭合、转义引号、再重开）。
/// 调用方都在非 macOS 的 unix 播放路径（paplay）；macOS 上仅测试引用，
/// Windows 测试构建会因 cfg(test) 编入却无引用，故两者都压掉 dead_code。
#[cfg(any(unix, test))]
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Bark (iOS)
// ---------------------------------------------------------------------------

/// 全局共享的阻塞 HTTP 客户端（5s 超时），Bark 与 Webhook 两个渠道共用。
///
/// 为什么共享：`reqwest::blocking::Client` 自带连接池与 TLS 配置，每条通知新建
/// 一个等于每条通知都重新 TCP+TLS 握手（外加一次配置构建）。通知是低频但长期
/// 存活的路径，用 `std::sync::OnceLock`（无新依赖）进程级复用一个实例。
/// 构建失败（TLS 后端初始化异常）向调用方传播，不 panic。
fn http_client() -> anyhow::Result<&'static reqwest::blocking::Client> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    // 并发首发时可能各自 build 一次，get_or_init 收敛到同一个实例，多余的丢弃即可
    Ok(CLIENT.get_or_init(|| built))
}

pub struct BarkChannel(pub BarkChannelConfig);

impl Channel for BarkChannel {
    fn name(&self) -> &'static str {
        "bark"
    }

    fn send(&self, n: &Notification) -> anyhow::Result<()> {
        let url = self.0.url.trim_end_matches('/').to_string();
        let client = http_client()?;
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "title": n.title,
                "body": n.body,
                "group": "agent-bark",
            }))
            .send()?;
        if !resp.status().is_success() {
            anyhow::bail!("Bark 返回 {}", resp.status());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 通用 Webhook 模板：覆盖飞书/企微/钉钉/ntfy/Server酱等
// ---------------------------------------------------------------------------

pub struct WebhookChannel(pub WebhookChannelConfig);

impl WebhookChannel {
    fn render(&self, n: &Notification) -> String {
        render_template(
            &self.0.template,
            &[
                ("{title}", &escape_json(&n.title)),
                ("{body}", &escape_json(&n.body)),
                ("{agent}", &escape_json(&n.agent)),
                // {event} 渲染成**裸** snake_case 枚举名（run_completed）再转义。
                // 旧实现把 serde_json::to_string 的产物（带引号的 "run_completed"）
                // 整体再 escape_json，模板里出现 \"run_completed\"——值里带字面引号
                // 与反斜杠，下游解析出的事件名对不上任何枚举（双重转义缺陷）。
                ("{event}", &escape_json(&bare_event_name(&n.event))),
                ("{project}", &escape_json(&n.project.clone().unwrap_or_default())),
            ],
        )
    }
}

/// `EventKind` 的裸 snake_case 名（如 `run_completed`，不带引号）。
/// serde 是唯一事实来源：`to_string` 得到带引号的 JSON 字符串，用与 `escape_json`
/// 相同的「只剥一层首尾引号」取出裸名（枚举名固定不含引号，但保持同一口径，
/// 不用 `trim_matches`——那会连剥多层）。
fn bare_event_name(kind: &EventKind) -> String {
    let quoted = serde_json::to_string(kind).unwrap_or_default();
    match quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => quoted,
    }
}

/// 渲染后的 body 必须是合法 JSON；不是则**降级**为固定结构的安全 body 再发送。
///
/// 为什么：`escape_json` 只适配「占位符落在 JSON 字符串内」的上下文，用户自写模板
/// 把 `{body}` 放在键位/裸值位（如 `{"k": {body}}`）时渲染结果会破坏 JSON 结构
/// （footgun）。与其把非法 body 发给 webhook 收 4xx/5xx 静默丢通知，不如降级成
/// `{"title","body","agent"}` 三字段的安全 body——通知内容不丢，结构合法。
/// 常见的模板写错是持续性配置问题，warn 只打一次（不刷日志），修好模板前每次
/// 通知都走安全 body。
fn validated_body(rendered: String, n: &Notification) -> String {
    if serde_json::from_str::<serde_json::Value>(&rendered).is_ok() {
        return rendered;
    }
    static WARN_INVALID_TEMPLATE: std::sync::Once = std::sync::Once::new();
    WARN_INVALID_TEMPLATE.call_once(|| {
        tracing::warn!(
            "webhook 模板渲染结果不是合法 JSON（常见于把 {{body}} 等占位符放在键位/裸值位），\
             已降级为固定安全 body 发送；请修正 template"
        );
    });
    serde_json::json!({ "title": &n.title, "body": &n.body, "agent": &n.agent }).to_string()
}

/// 模板渲染：**单遍扫描**替换占位符。
///
/// 不能用链式 `str::replace`：那是对「整条字符串」反复替换，事件标题里若恰好
/// 含 `{body}` 之类字面量，会在后续一轮被再次展开（二阶注入，通知内容被污染）。
/// 这里按字节位置一次扫完，替换进去的值永远不会被重新扫描。
fn render_template(template: &str, slots: &[(&str, &str)]) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    'scan: while i < bytes.len() {
        for (ph, val) in slots {
            if bytes[i..].starts_with(ph.as_bytes()) {
                out.push_str(val);
                i += ph.len();
                continue 'scan;
            }
        }
        // 按 UTF-8 字符边界推进（占位符全是 ASCII，逐字节跳过非匹配区安全）
        let step = if bytes[i] < 0x80 { 1 } else {
            template[i..].chars().next().map_or(1, |c| c.len_utf8())
        };
        out.push_str(&template[i..i + step]);
        i += step;
    }
    out
}

impl Channel for WebhookChannel {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn send(&self, n: &Notification) -> anyhow::Result<()> {
        if self.0.url.is_empty() {
            anyhow::bail!("webhook url 为空");
        }
        let client = http_client()?;
        let method = self.0.method.to_uppercase();
        let req = if method == "GET" {
            // B14：GET 无请求体，body 模板必然被忽略；只有用户真的配了模板才值得提示
            if !self.0.template.trim().is_empty() {
                static WARN_GET_TEMPLATE: std::sync::Once = std::sync::Once::new();
                WARN_GET_TEMPLATE.call_once(|| {
                    tracing::warn!("webhook 使用 GET 方法：body 模板（template）将被忽略，GET 只带 title/body 查询参数");
                });
            }
            let url = format!("{}{}title={}&body={}", self.0.url, sep(&self.0.url), urlencode(&n.title), urlencode(&n.body));
            client.get(url)
        } else {
            // 渲染后先校验 JSON，失败降级安全 body（模板把 {body} 放键位的 footgun）
            let body = validated_body(self.render(n), n);
            client
                .request(reqwest::Method::from_bytes(method.as_bytes())?, &self.0.url)
                .header("content-type", "application/json")
                .body(body)
        };
        let resp = req.send()?;
        if !resp.status().is_success() {
            anyhow::bail!("webhook 返回 {}", resp.status());
        }
        Ok(())
    }
}

fn sep(url: &str) -> &'static str {
    if url.contains('?') {
        "&"
    } else {
        "?"
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 模板占位符替换发生在 JSON 字符串内部，需转义用户内容。
/// 注意：只剥离 serde_json 添加的首尾引号各一个，不能用 trim_matches
/// （否则会把内容尾部 `\"` 转义中的引号也剥掉）。
fn escape_json(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_default();
    match quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => quoted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_catalog_is_complete_and_unpackable() {
        // 目录里的每个音效都必须是合法的内嵌 WAV（RIFF 头）且有 UI 文案，
        // 并能解包出真实文件（sound_command 只拿路径，不再检查 id）
        for e in SOUND_EFFECTS {
            assert!(!e.label.is_empty(), "音效 {} 缺少 UI 文案", e.id);
            assert!(e.wav.len() > 44, "音效 {} 数据过小", e.id);
            assert_eq!(&e.wav[..4], b"RIFF", "音效 {} 不是 WAV", e.id);
            let path = unpack(e).unwrap();
            assert!(path.exists(), "音效 {} 解包失败: {}", e.id, path.display());
            #[cfg(not(any(windows, target_os = "macos")))]
            assert!(sound_command(&path, false, 1.0).is_some(), "音效 {} 没有播放实现", e.id);
            #[cfg(any(windows, target_os = "macos"))]
            assert!(player_rodio::parse_wav(e.wav).is_ok(), "音效 {} WAV 解析失败", e.id);
        }
        // 未配置（空 effect）在 play_sound 层是静默 no-op；未知 id 报错
        assert!(play_sound("", None, 1.0).is_ok());
        assert!(play_sound("nope", None, 1.0).is_err());
        // 自定义：没选文件 / 文件不存在都报错，不静默吞掉（测试按钮要给用户反馈）
        assert!(play_sound("custom", None, 1.0).is_err());
        assert!(play_sound("custom", Some(std::path::Path::new("/nonexistent/x.mp3")), 1.0).is_err());
    }

    #[test]
    fn legacy_chime_maps_to_confirm() {
        // chime 已从目录下线（素材与 deep 同源），老配置里可能还留着：
        // 必须映射到 confirm 而不是报「未知音效」——否则升级后该状态永久静默
        assert_eq!(builtin("chime").map(|e| e.id), Some("confirm"));
        assert_eq!(builtin("confirm").map(|e| e.id), Some("confirm"));
        assert!(builtin("nope").is_none());
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn rodio_play_private_silence() {
        // 静音缓冲不出声；私有流 open→入队→后台等播完 drop 全路径（冷含唤醒）。
        // play_private 现在立即返回，连续多次调用会短时并存多个流，正是要验的寿命组合
        for i in 0..6 {
            player_rodio::play_private(player_rodio::silence_source(0.3), i == 0).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        // 等后台线程把流放干净再退出（否则进程退出时 join 掉大半个波形）
        std::thread::sleep(std::time::Duration::from_millis(800));
        eprintln!("rodio private silence survived");
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    #[ignore]
    fn audio_host_churn_survives() {
        // 保活高频开关：命令线程必须串行消化、不崩、不死锁。
        // 以前这条测的是全局 Mutex 下的 start/stop 竞态；现在设备只属于一条线程，
        // 剩下要验的是「命令风暴里开流/drop 不会把线程搞挂」。
        for i in 0..60 {
            keepalive_set(i % 2 == 0);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        keepalive_set(true);
        std::thread::sleep(std::time::Duration::from_millis(500));
        keepalive_set(false);
        eprintln!("audio host churn survived");
    }

    #[test]
    fn keepalive_commands_are_idempotent() {
        // 连发同一条命令不卡死、不 panic（真正的设备状态在音频线程里，
        // 调用方无状态可断言——这正是这次重构要的效果）
        keepalive_set(true);
        keepalive_set(true);
        keepalive_set(false);
        keepalive_set(false);
    }

    #[test]
    fn silence_wav_is_valid_and_cached() {
        let b = silence_wav_bytes(0.5);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(&b[8..12], b"WAVE");
        // 0.5s * 44100 * 2 bytes
        assert_eq!(b.len(), 44 + 44100);
        let p5 = silence_path(5);
        let p20 = silence_path(20);
        assert!(p5.exists() && p20.exists());
        assert_eq!(std::fs::metadata(&p20).unwrap().len() as usize, 44 + 44100 * 2 * 2);
        #[cfg(any(windows, target_os = "macos"))]
        {
            let (ch, rate, data) = player_rodio::parse_wav(&b).unwrap();
            assert_eq!(ch, 1);
            assert_eq!(rate, 44100);
            assert_eq!(data.len(), 44100);
        }
    }

    #[test]
    fn webhook_render() {
        let cfg = WebhookChannelConfig {
            enabled: true,
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            template: r#"{"text": "{title} - {agent}"}"#.into(),
        };
        let ch = WebhookChannel(cfg);
        let n = Notification {
            title: "完成 \"x\"".into(),
            body: "b".into(),
            event: EventKind::RunCompleted,
            agent: "claude-code".into(),
            project: Some("app".into()),
        };
        assert_eq!(ch.render(&n), r#"{"text": "完成 \"x\" - claude-code"}"#);
    }

    #[test]
    fn event_placeholder_renders_bare_snake_case_name() {
        // {event} 必须渲染成裸 snake_case 名：无反斜杠、无引号。
        // 旧实现双重转义成 \"run_completed\"（值里带字面引号），下游对不上任何枚举
        let cfg = WebhookChannelConfig {
            enabled: true,
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            template: r#"{"event": "{event}"}"#.into(),
        };
        let ch = WebhookChannel(cfg);
        let n = Notification {
            title: "t".into(),
            body: "b".into(),
            event: EventKind::RunCompleted,
            agent: "a".into(),
            project: None,
        };
        assert_eq!(ch.render(&n), r#"{"event": "run_completed"}"#);
        // 渲染结果是合法 JSON 且解出的就是裸枚举名
        let v: serde_json::Value = serde_json::from_str(&ch.render(&n)).unwrap();
        assert_eq!(v["event"], "run_completed");
    }

    #[test]
    fn invalid_json_render_degrades_to_safe_body() {
        // footgun：用户把 {body} 放在键位/裸值位，渲染结果不是合法 JSON →
        // 降级为 {"title","body","agent"} 安全 body，而不是把坏 JSON 发给 webhook
        let cfg = WebhookChannelConfig {
            enabled: true,
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            template: r#"{"k": {body}}"#.into(),
        };
        let ch = WebhookChannel(cfg);
        let n = Notification {
            title: "标题".into(),
            body: "正文".into(),
            event: EventKind::RunCompleted,
            agent: "claude-code".into(),
            project: None,
        };
        let rendered = ch.render(&n);
        assert!(serde_json::from_str::<serde_json::Value>(&rendered).is_err(), "前置：渲染结果确实不是 JSON");
        let body = validated_body(rendered, &n);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["title"], "标题");
        assert_eq!(v["body"], "正文");
        assert_eq!(v["agent"], "claude-code");

        // 合法渲染原样透传，不多做一次序列化（键序/转义不被改动）
        let cfg = WebhookChannelConfig {
            enabled: true,
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            template: r#"{"text": "{body}"}"#.into(),
        };
        let ch = WebhookChannel(cfg);
        let rendered = ch.render(&n);
        assert_eq!(validated_body(rendered.clone(), &n), rendered);
    }

    #[test]
    fn http_client_is_built_once_and_shared() {
        // 每条通知新建 Client 会重建连接池/TLS 配置：Bark/Webhook 必须复用同一个实例
        let a = http_client().unwrap() as *const reqwest::blocking::Client;
        let b = http_client().unwrap() as *const reqwest::blocking::Client;
        assert_eq!(a, b, "http_client 必须进程级复用同一个 Client");
    }

    #[test]
    fn template_render_is_single_pass_no_second_order_substitution() {
        // 事件内容里恰好含占位符字面量：不能在后续轮次被二次展开
        let cfg = WebhookChannelConfig {
            enabled: true,
            url: "https://example.com/hook".into(),
            method: "POST".into(),
            template: r#"{"title": "{title}", "body": "{body}"}"#.into(),
        };
        let ch = WebhookChannel(cfg);
        let n = Notification {
            title: "标题里有 {body} 和 {project}".into(),
            body: "正文".into(),
            event: EventKind::RunCompleted,
            agent: "claude-code".into(),
            project: Some("app".into()),
        };
        let rendered = ch.render(&n);
        // 占位符字面量原样保留、不被二阶展开
        assert_eq!(
            rendered,
            r#"{"title": "标题里有 {body} 和 {project}", "body": "正文"}"#
        );
        // 中文等多字节字符不破坏扫描
        let n2 = Notification {
            title: "完".into(),
            body: "成".into(),
            event: EventKind::RunCompleted,
            agent: "a".into(),
            project: None,
        };
        assert_eq!(ch.render(&n2), r#"{"title": "完", "body": "成"}"#);
    }

    #[test]
    fn shell_quotes_escape_embedded_single_quotes() {
        // %TEMP% 在含单引号的用户名下（如 O'Brien）时命令不能断裂
        #[cfg(unix)]
        assert_eq!(sh_quote("/tmp/O'Brien/x.wav"), "'/tmp/O'\\''Brien/x.wav'");
    }
}
