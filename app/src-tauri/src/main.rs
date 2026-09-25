// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // hook 子命令：无头模式，不启动 GUI
    let is_hook = args.get(1).map(String::as_str) == Some("hook");
    // hook 子进程写独立日志文件（§2.18）：hook 是每次事件拉起的短命进程、并发量大，
    // 与 GUI 共用一个 append 文件会互相穿插，GUI 启动时的轮转截断还会把 hook 刚写下的
    // 排障记录抹掉。分开后各看各的、各轮转各的（同目录、同轮转策略）。
    init_logging(is_hook);
    if is_hook {
        std::process::exit(bark_cli::run_hook_from(&args));
    }
    agent_bark_app::run();
}

/// 安装 tracing 订阅者。
///
/// 在此之前全项目没有订阅者，所有 tracing::warn!/error! 都是空操作——用户看不到
/// 「配置被忽略」「快照持续失败」「备份失败」这类告警。GUI 进程在 Windows 上没有
/// 控制台、hook 子进程的 stderr 也没人看，故日志统一落到配置文件同目录：
/// GUI 写 `agent-bark.log`，hook 子进程写 `agent-bark-hook.log`（超过 1 MiB 时
/// 各自在启动时轮转一次，避免无界增长）。
fn init_logging(hook: bool) {
    use std::io::Write;
    let Ok(dir) = bark_core::BarkConfig::dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(if hook { "agent-bark-hook.log" } else { "agent-bark.log" });
    // 简单轮转：启动时若超过 1 MiB，截断重建（日志仅用于排障）
    if std::fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::File::create(&path);
    }
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let writer = LogMakeWriter(std::sync::Arc::new(std::sync::Mutex::new(file)));
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();
    let _ = writeln!(
        std::io::stderr(),
        "agent-bark 日志: {}",
        path.display()
    );
}

/// fmt 需要 MakeWriter：这里把 Arc<Mutex<File>> 包装成可克隆的 writer 工厂
#[derive(Clone)]
struct LogMakeWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogMakeWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

struct LogWriter(std::sync::Arc<std::sync::Mutex<std::fs::File>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut f) => f.write(buf),
            // 锁中毒（写入线程 panic）也要继续写，日志本身不该因此丢失
            Err(p) => p.into_inner().write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self.0.lock() {
            Ok(mut f) => f.flush(),
            Err(p) => p.into_inner().flush(),
        }
    }
}
