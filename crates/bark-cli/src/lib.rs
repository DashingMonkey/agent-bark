//! hook 子命令入口：agent 触发 hook 时以
//! `agent-bark hook --agent <kind> --event <Event>` 启动本进程，
//! stdin 为该 agent 的 hook JSON。
//!
//! 流程：读 stdin（带超时）→ adapter 归一化 → POST 本地 daemon。
//! daemon 不可达（应用未运行）时兜底：事件落盘到 pending.jsonl（等 daemon 起来补投）。
//!
//! 三条不可协商的约束：
//! 1. **永远退出 0**——除了显式的阻断特性，绝不通过退出码影响 agent；
//! 2. **读 stdin 必须有超时**——Stop / PreToolUse 是阻塞型 hook，宿主不关管道就会卡住 agent；
//! 3. **只读配置，绝不写盘**——写盘会生成新 token，导致 daemon 侧鉴权全部失败。

use bark_adapters::registry::find_hook_adapter;
use bark_core::{BarkConfig, EventKind, NormalizedEvent};
use clap::Parser;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

/// 读 stdin 的最长等待时间
const STDIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 转发给 daemon 的超时。
/// daemon 就在本机 loopback 上，正常情况是微秒级；这个值同时决定了
/// **daemon 未运行时阻塞型 hook 的最坏等待**，所以取小值。
/// 实测：进程冷启动约 2ms，POST 超时是这条路径的主要成本。
const POST_TIMEOUT: Duration = Duration::from_millis(100);
/// 补投队列上限（超过则丢弃最旧的，避免无限增长）
const PENDING_CAP: usize = 500;
/// 补投时只回放最近这么久内的事件，避免重启后弹出陈年旧事
const PENDING_MAX_AGE_MS: i64 = 60 * 60 * 1000;

/// 供宿主二进制（同一 exe）调用的 hook 入口：解析参数并执行。
/// args 为完整进程参数：[exe, "hook", --agent, ...]，需跳过子命令名。
pub fn run_hook_from(args: &[String]) -> i32 {
    let skip = if args.len() >= 2 && args[1] == "hook" { 2 } else { 1 };
    // clap 把 argv[0] 当 bin 名，需补一个占位
    let argv: Vec<&str> = std::iter::once("agent-bark")
        .chain(args.iter().skip(skip).map(String::as_str))
        .collect();
    match HookArgs::try_parse_from(argv) {
        Ok(hook_args) => run(hook_args),
        Err(e) => {
            eprint!("{e}");
            // 参数错误也不能阻断 agent
            0
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "agent-bark hook", about = "接收 agent hook 事件并转发给 agent-bark daemon")]
pub struct HookArgs {
    /// AgentKind::id()
    #[arg(long)]
    pub agent: String,
    /// agent 侧的原始事件名（如 Stop / Notification）
    #[arg(long)]
    pub event: String,
}

pub fn run(args: HookArgs) -> i32 {
    let kind = match bark_core::AgentKind::from_id(&args.agent) {
        Some(k) => k,
        None => {
            eprintln!("agent-bark: unknown agent '{}'", args.agent);
            return 0;
        }
    };

    let raw_text = read_stdin_with_timeout(STDIN_TIMEOUT);
    let raw: serde_json::Value = serde_json::from_str(&raw_text).unwrap_or(serde_json::json!({}));

    // 归一化（未知事件直接成功退出，绝不阻塞 agent）
    let Some(adapter) = find_hook_adapter(kind) else {
        return 0;
    };
    let Some(event) = adapter.normalize(&args.event, &raw) else {
        return 0;
    };

    // 只读加载配置：失败（文件损坏）时绝不写盘
    let config = match BarkConfig::load() {
        Ok(c) => {
            // load() 在文件不存在时返回 Ok(default())，而 default() 的 token 是**新随机值**，
            // 拿去 POST 必然 401（daemon 用的是自己的 token）。这条路径要和「损坏」同等对待。
            let missing = BarkConfig::path().map(|p| !p.exists()).unwrap_or(false);
            if missing {
                static WARNED_MISSING: std::sync::Once = std::sync::Once::new();
                WARNED_MISSING.call_once(|| {
                    eprintln!(
                        "agent-bark: 配置文件不存在，事件已落盘等待补投；\
                         请启动 agent-bark 主程序生成配置（否则 hook 无法上报）"
                    );
                });
                persist_pending(&event);
                return 0;
            }
            c
        }
        Err(e) => {
            // B6：配置损坏时不能退回 BarkConfig::default() 再 POST——
            // default() 会生成全新随机 token，发到 daemon 只会得到 401。
            // 直接落盘等补投，并明确提示用户修配置。
            // 每个 hook 进程只提示一次（一次运行可能连发多个事件）。
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "agent-bark: 配置文件损坏（{e:#}），事件已落盘等待补投；\
                     请修复或删除配置文件后重启 agent-bark"
                );
            });
            persist_pending(&event);
            return 0;
        }
    };

    // 尝试转发给 daemon
    if post_to_daemon(&config, &event) {
        return 0;
    }

    // daemon 离线：落盘等补投（桌面兜底提醒已随系统通知一起下线）
    persist_pending(&event);
    0
}

/// 离线落盘的统一入口。心跳（Activity）与工具收尾（ToolFinished）事件例外：
/// 一轮可达多次、时效为零，daemon 起来后早已过时，落盘只会撑爆补投队列、挤掉真正
/// 该补投的通知——直接丢弃即可（实时状态反正已不可恢复）。
fn persist_pending(event: &NormalizedEvent) {
    if matches!(event.kind, EventKind::Activity | EventKind::ToolFinished) {
        return;
    }
    append_pending(event);
}

/// 带超时地读 stdin。
///
/// `read_to_string` 会一直等到 EOF；阻塞型 hook 的宿主若不关管道，
/// 进程就永远不返回，agent 这一轮直接卡死。这里用工作线程 + 超时兜住。
fn read_stdin_with_timeout(timeout: Duration) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_to_string(&mut s);
        let _ = tx.send(s);
    });
    match rx.recv_timeout(timeout) {
        Ok(s) => s,
        // 超时：按空 payload 继续，agent 不会因为我们而卡住
        Err(_) => {
            eprintln!("agent-bark: 读取 stdin 超时（{}s），按空 payload 继续", timeout.as_secs());
            String::new()
        }
    }
}

fn pending_path() -> Option<PathBuf> {
    BarkConfig::dir().ok().map(|d| d.join("pending.jsonl"))
}

/// 事件落盘，等 daemon 启动后补投
fn append_pending(event: &NormalizedEvent) {
    let Some(path) = pending_path() else { return };
    append_pending_at(&path, event);
}

/// 指定路径版（测试用；生产走 [`append_pending`]）
fn append_pending_at(path: &PathBuf, event: &NormalizedEvent) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(line) = serde_json::to_string(event) else { return };
    append_line(path, &line);
    // 只在明显超量时才裁剪（见 trim_pending 注释：裁剪有并发丢行的窗口，
    // 故不放每次 append 的热路径上）
    if std::fs::metadata(path).map(|m| m.len() > TRIM_TRIGGER_BYTES).unwrap_or(false) {
        trim_pending(path);
    }
}

/// 把一行（不含换行）**单次 write**追加到补投队列。
///
/// 为什么不能 `writeln!`：它经 `write_fmt` 拆成「行体 + \n」两次 write，并发 hook
/// 进程（或 daemon requeue 与 hook 同时写）交错可产生 `lineAlineB\n\n`，两行一起报废
/// （§1.15 实测形态）。整行拼好后一次 `write_all`，配合 append 模式的原子追加语义，
/// 行与行之间不再有交错窗口。
fn append_line(path: &PathBuf, line: &str) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = f.write_all(format!("{line}\n").as_bytes());
}

/// 补投队列的文件大小上限（超过才触发裁剪，约 1 MiB ≈ 数千条事件）
const TRIM_TRIGGER_BYTES: u64 = 1024 * 1024;

/// 超过上限时只保留最近的 PENDING_CAP 条。
///
/// 已知局限（实测确认，不再宣称「并发安全」）：read 与 rename 之间若有其它 hook 进程
/// append，那些行写进的是即将被替换掉的旧文件对象，会随替换丢失。用 tmp+rename 只保证
/// 「读者永远看不到半写文件」，不保证不丢并发写入。因此本函数只在文件明显超量时调用
/// （每进程 append 路径上的概率极低），且丢失窗口仅为超量期间的新事件。
fn trim_pending(path: &PathBuf) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= PENDING_CAP {
        return;
    }
    let kept = lines[lines.len() - PENDING_CAP..].join("\n") + "\n";
    // 临时名带 uuid：pid 在「同进程并发 trim」（多线程 hook 宿主）下不唯一，会互踩临时文件
    let tmp = path.with_extension(format!("jsonl.trim-tmp-{}", uuid::Uuid::new_v4().simple()));
    if std::fs::write(&tmp, kept).is_ok() {
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// 读取补投队列并把它标记为「已消费待确认」；只返回足够新的事件。
///
/// B5：先 rename 成**唯一名**再读——rename 之后 hook 侧 append 会落到新文件，
/// 「读完删文件」之间不再有丢事件的窗口。
/// 上一轮若在投递确认前崩溃，会残留 draining 文件：这里先把它读出来一并回放
/// （崩溃残留的回收，现有逻辑一直支持）。
///
/// **消费后不删文件**（§1.16）：旧实现在「删除后、投递确认前」崩溃即丢事件。
/// 改为投递全部处置完（含 requeue）后由调用方 [`ack_drained`] 统一删除——
/// 语义是**至少一次投递 + daemon 侧 id 去重**：崩溃后残留的 draining 文件下次
/// drain 会再回放一遍，重复的事件由 daemon 的 `seen-ids` 去重窗口吞掉，宁重不丢。
pub fn drain_pending() -> Vec<NormalizedEvent> {
    let Some(path) = pending_path() else { return Vec::new() };
    drain_pending_at(&path)
}

/// 指定路径版（测试用；生产走 [`drain_pending`]）
fn drain_pending_at(path: &PathBuf) -> Vec<NormalizedEvent> {
    let mut out = Vec::new();

    // 1) 消费上次崩溃残留的 draining 文件（唯一名各不相同，全部回收）
    if let Some(dir) = path.parent() {
        let prefix = "pending.jsonl.draining";
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with(prefix) {
                    continue;
                }
                let p = entry.path();
                // 读失败（被占用等）就留着，下次再试；**读成功也不删**——
                // 删除统一由 ack_drained 在投递处置完后做（见函数头注释）
                if let Ok(text) = std::fs::read_to_string(&p) {
                    out.extend(parse_or_warn(&p, &text));
                }
            }
        }
    }

    // 2) rename 主文件到唯一名再读（唯一名用 uuid：同进程并发 drain 互不覆盖）
    let tmp = path.with_extension(format!("jsonl.draining-{}", uuid::Uuid::new_v4().simple()));
    if std::fs::rename(path, &tmp).is_ok() {
        if let Ok(text) = std::fs::read_to_string(&tmp) {
            out.extend(parse_or_warn(&tmp, &text));
        }
    }
    out
}

/// 解析一个补投文件的行；坏行计数并 warn 留痕（不再静默丢弃，见 §1.15）
fn parse_or_warn(path: &PathBuf, text: &str) -> Vec<NormalizedEvent> {
    let (events, bad) = parse_pending_lines(text);
    if bad > 0 {
        tracing::warn!(
            path = %path.display(),
            "补投队列有 {bad} 行损坏（并发写交错/半写），已跳过（留痕排查，不再静默丢弃）"
        );
    }
    events
}

/// 投递全部处置完（成功投出 + requeue 写回）后，删除所有 draining 文件。
///
/// 删除失败只 warn 留痕：宁可下次 drain 重复回放（id 去重兜住），也不能让
/// 「删不掉」变成静默异常。
pub fn ack_drained() {
    let Some(path) = pending_path() else { return };
    ack_drained_at(&path);
}

/// 指定路径版（测试用；生产走 [`ack_drained`]）
fn ack_drained_at(path: &PathBuf) {
    let Some(dir) = path.parent() else { return };
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("pending.jsonl.draining") {
            continue;
        }
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!(path = %entry.path().display(), "补投 draining 文件删除失败（下次 drain 会重复回放，由 id 去重兜住）: {e}");
        }
    }
}

/// 解析补投队列的行：过滤坏行（计数返回）与过旧条目，**强制取尾部 PENDING_CAP 条**。
///
/// 返回 `(有效事件, 坏行数)`。尾部截断是 README「限 500 条」承诺的另一半——
/// `PENDING_CAP` 过去只在文件 >1MiB 的 trim 里生效，<1MiB 时数千条事件全量补投
/// 会把用户淹死在通知风暴里（§1.16）。取尾部（最新）而不是头部：最新的事件才是
/// 还活着的会话状态。
fn parse_pending_lines(text: &str) -> (Vec<NormalizedEvent>, usize) {
    let cutoff = bark_core::now_millis() - PENDING_MAX_AGE_MS;
    let mut bad = 0usize;
    let mut out: Vec<NormalizedEvent> = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<NormalizedEvent>(l) {
            Ok(e) if e.timestamp >= cutoff => out.push(e),
            Ok(_) => {} // 过旧：静默丢（这是设计内过滤，不算坏行）
            Err(_) => bad += 1,
        }
    }
    let start = out.len().saturating_sub(PENDING_CAP);
    (out.split_off(start), bad)
}

/// B5：把 drain 后未能投递的事件写回补投队列（如补投通道已满）。
pub fn requeue_pending(events: &[NormalizedEvent]) {
    if events.is_empty() {
        return;
    }
    let Some(path) = pending_path() else { return };
    requeue_pending_at(&path, events);
}

/// 指定路径版（测试用；生产走 [`requeue_pending`]）
fn requeue_pending_at(path: &PathBuf, events: &[NormalizedEvent]) {
    if events.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    for ev in events {
        if let Ok(line) = serde_json::to_string(ev) {
            // 与 append_pending 同款：整行单次 write（writeln! 会拆两次 write，
            // 并发交错会把 JSON 行拼坏，见 append_line 的注释）
            let _ = f.write_all(format!("{line}\n").as_bytes());
        }
    }
    drop(f);
    if std::fs::metadata(path).map(|m| m.len() > TRIM_TRIGGER_BYTES).unwrap_or(false) {
        trim_pending(path);
    }
}

fn post_to_daemon(config: &BarkConfig, event: &NormalizedEvent) -> bool {
    let url = format!("http://127.0.0.1:{}/event", config.server.port);
    let client = match reqwest::blocking::Client::builder().timeout(POST_TIMEOUT).build() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let Ok(body) = serde_json::to_string(event) else {
        return false;
    };
    let res = client
        .post(&url)
        .header("x-bark-token", &config.server.token)
        .header("content-type", "application/json")
        .body(body)
        .send();
    if std::env::var_os("BARK_DEBUG").is_some() {
        // 注意：不要打印 token 明文
        match &res {
            Ok(r) => eprintln!("agent-bark[debug]: POST {url} -> {}", r.status()),
            Err(e) => eprintln!("agent-bark[debug]: POST {url} -> error {e}"),
        }
    }
    res.map(|r| r.status().is_success()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_core::EventKind;

    fn sample() -> NormalizedEvent {
        NormalizedEvent {
            id: "id".into(),
            agent: "claude-code".into(),
            kind: EventKind::RunCompleted,
            session_id: "s".into(),
            cwd: "C:/p".into(),
            project: Some("p".into()),
            message: "done".into(),
            timestamp: bark_core::now_millis(),
            is_subagent: false,
            tool_name: None,
        }
    }

    #[test]
    fn pending_event_roundtrip() {
        // 落盘格式契约：补投队列的行必须能被反序列化回 NormalizedEvent
        let e = sample();
        let line = serde_json::to_string(&e).unwrap();
        let back: NormalizedEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(back.agent, "claude-code");
        assert_eq!(back.id, e.id);
    }

    #[test]
    fn trim_keeps_only_cap() {
        let dir = std::env::temp_dir().join(format!("agent-bark-pending-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pending.jsonl");
        let line = serde_json::to_string(&sample()).unwrap();
        let body = vec![line.as_str(); PENDING_CAP + 10].join("\n");
        std::fs::write(&p, body + "\n").unwrap();
        trim_pending(&p);
        let n = std::fs::read_to_string(&p).unwrap().lines().count();
        assert_eq!(n, PENDING_CAP);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_queue() -> (std::path::PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agent-bark-pending-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pending.jsonl");
        (dir, p)
    }

    /// §1.15 回归：每条事件必须以**单次 write**写成完整一行——`writeln!` 拆两次 write
    /// 会在并发交错下产出 `lineAlineB\n\n` 这类坏行。这里钉死「落盘后每行都能独立
    /// 反序列化」的契约（交错竞态本身没法在单测里稳定复现，锁住的是产物形态）。
    #[test]
    fn append_writes_whole_parseable_line_per_event() {
        let (dir, p) = temp_queue();
        let mut a = sample();
        a.id = "a".into();
        let mut b = sample();
        b.id = "b".into();
        append_pending_at(&p, &a);
        append_pending_at(&p, &b);
        let text = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "一行一条事件，行尾换行完整");
        for l in &lines {
            serde_json::from_str::<NormalizedEvent>(l).expect("每行必须是完整独立的 JSON 行");
        }
        // requeue 同款契约
        let (dir2, p2) = temp_queue();
        requeue_pending_at(&p2, &[a, b]);
        let text2 = std::fs::read_to_string(&p2).unwrap();
        assert_eq!(text2.lines().count(), 2);
        for l in text2.lines() {
            serde_json::from_str::<NormalizedEvent>(l).expect("requeue 的每行也必须完整");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// §1.15 回归：坏行不再被静默 `filter_map(ok())` 丢掉——必须计数（调用方 warn 留痕）。
    /// §1.16 回归：年龄过滤后强制取尾部 PENDING_CAP 条（README「限 500 条」承诺）。
    #[test]
    fn parse_counts_bad_lines_and_caps_tail() {
        let line = serde_json::to_string(&sample()).unwrap();
        // 坏行 + 两条好行：坏行计数为 1，好行两条都回来
        let text = format!("{line}\n{{ this is not json\n{line}\n");
        let (events, bad) = parse_pending_lines(&text);
        assert_eq!(bad, 1, "坏行必须被计数（warn 留痕），不再静默");
        assert_eq!(events.len(), 2, "好行不受坏行影响");
        // 过旧条目是设计内过滤，不算坏行
        let mut old = sample();
        old.timestamp = bark_core::now_millis() - PENDING_MAX_AGE_MS - 1;
        let text = serde_json::to_string(&old).unwrap();
        let (events, bad) = parse_pending_lines(&text);
        assert!(events.is_empty() && bad == 0, "过旧条目静默丢弃但不计坏行");
        // 尾部截断：<1MiB（trim 不触发）时也只回放最近 PENDING_CAP 条
        let text = vec![line.as_str(); PENDING_CAP + 10].join("\n");
        let (events, bad) = parse_pending_lines(&text);
        assert_eq!(bad, 0);
        assert_eq!(events.len(), PENDING_CAP, "强制取尾部 PENDING_CAP 条");
    }

    /// §1.16 回归（先删后投的崩溃窗口）：
    /// - drain **不删**已消费的 draining 文件（事件此时只在内存里，删了就丢）；
    /// - 崩溃残留的 draining 文件会被下次 drain 回收（至少一次投递 + daemon id 去重）；
    /// - 投递处置完后 ack_drained 统一删除。
    #[test]
    fn drain_keeps_files_until_ack_and_recycles_crash_residue() {
        let (dir, p) = temp_queue();
        let mut a = sample();
        a.id = "live".into();
        append_pending_at(&p, &a);
        // 上一轮 drain 之后、ack 之前崩溃留下的残留（唯一名形如 pending.jsonl.draining-*）
        let mut stale = sample();
        stale.id = "crash-residue".into();
        let residue = dir.join("pending.jsonl.draining-0old0crash0");
        std::fs::write(&residue, serde_json::to_string(&stale).unwrap() + "\n").unwrap();

        let got = drain_pending_at(&p);
        let ids: Vec<&str> = got.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"live") && ids.contains(&"crash-residue"), "崩溃残留必须被回收");
        // drain 后文件仍在：此刻事件只在内存里，删文件 = 删事件（旧实现的崩溃丢事件窗口）
        assert!(residue.exists(), "drain 不得删除 draining 文件（待 ack）");
        let draining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("pending.jsonl.draining"))
            .collect();
        assert_eq!(draining.len(), 2, "残留 + 本次 rename 的两个 draining 文件都还在");

        // 投递处置完（成功投出或 requeue 写回）后统一删除
        ack_drained_at(&p);
        let draining_after: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("pending.jsonl.draining"))
            .collect();
        assert!(draining_after.is_empty(), "ack 后所有 draining 文件必须删除");
        // 幂等：没有可删文件时不炸
        ack_drained_at(&p);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
