//! 检查更新（「关于」页）
//!
//! 同时问 Gitee（主源）与 GitHub（备源）的 `releases/latest`：两路并行、各 15s 超时，
//! 谁成功算谁的，两边都成功以 Gitee 为准；单边失败静默（另一边成功时不暴露失败细节），
//! 两边都失败才把主源的错误抛给 UI。主源选 Gitee 的理由与 quota-widget 一致：
//! 国内可达性最好、海外也能访问，两个平台同 tag 发版；Gitee API v5 仿 GitHub API
//! 设计（`tag_name` 等字段一致），解析代码一套通吃。
//!
//! 版本号必须逐段数值比较：字符串序会把 `1.1.10` 判成小于 `1.1.9`。
//! `-beta.1` 这类预发布后缀与 `+build` 元数据在解析时截掉——远端等于本地即视为
//! 「没有新版本」，不把预发布版推给正式版用户。
//!
//! 请求走阻塞 reqwest（与 bark-channels 的 webhook 同一套），整个检查在
//! `spawn_blocking` 里跑，不占 async 运行时的线程；请求放后端而不是前端，
//! 是因为主窗口 CSP 的 `connect-src 'self'` 不允许页面直连外网。

use serde::Serialize;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// 仓库坐标：`owner/name`。Gitee 与 GitHub 的 owner/repo 都不区分大小写，两边共用一个 slug。
/// 主源是 Gitee（国内可达性最好、海外也能访问），GitHub 为备源，两边同 tag 发版。
const REPO: &str = "dashingmonkey/agent-bark";

/// 单源超时。两路并行，所以点一次按钮最坏也就等这么久，不是两倍
const TIMEOUT: Duration = Duration::from_secs(15);

/// 实际使用的仓库坐标：`BARK_UPDATE_REPO=owner/name` 可覆盖默认值。
///
/// 只为联调/自测留的口子（与 `BARK_DEBUG`、`BARK_STALE_AFTER_MS` 同一套做法）：
/// 指到一个**已经发过版本**的仓库，就能验证「发现新版本 → 下载新版本」这条路径，
/// 不必等本仓库先发版。正常用户不会设置它。
fn repo() -> String {
    match std::env::var("BARK_UPDATE_REPO") {
        Ok(v) if is_safe_slug(v.trim()) => v.trim().to_string(),
        // 拼出来的地址最终会交给 `cmd /C start` 打开，`&`、`|` 这类字符会被 cmd
        // 当成命令分隔符——非法取值一律退回默认仓库
        Ok(v) => {
            tracing::warn!("BARK_UPDATE_REPO={v:?} 不是合法的 owner/name，改用默认仓库 {REPO}");
            REPO.to_string()
        }
        Err(_) => REPO.to_string(),
    }
}

/// 仓库 slug 的合法字符集（字母数字与 `.` `_` `-` `/`），保证拼出的 URL 无需转义
fn is_safe_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Gitee 主源地址（接口, 发布页）
fn gitee_urls(slug: &str) -> (String, String) {
    (
        format!("https://gitee.com/api/v5/repos/{slug}/releases/latest"),
        format!("https://gitee.com/{slug}/releases/latest"),
    )
}

/// GitHub 备源地址（接口, 发布页）
fn github_urls(slug: &str) -> (String, String) {
    (
        format!("https://api.github.com/repos/{slug}/releases/latest"),
        format!("https://github.com/{slug}/releases/latest"),
    )
}

/// 检查结果（前端 `UpdateInfo`）
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// 本地版本：取自 Tauri 的 package_info，与安装包（tauri.conf.json）同一个口径
    pub current: String,
    /// 远端最新版本号（已去 v 前缀与预发布后缀）
    pub latest: String,
    /// 远端是否比本地新
    pub newer: bool,
    /// 实际应答的源："gitee" | "github"
    pub source: String,
    /// 应答源的发布页（「下载新版本」按钮打开它）
    pub page_url: String,
}

/// 上次检查实际应答的发布页。
///
/// 存后端而不是让前端传 URL 过来：IPC 面越小越好，也免得把任意 URL 交给 shell 打开
/// （`open_release_page` 只认这里记下的地址）。
fn last_page() -> &'static Mutex<Option<String>> {
    static PAGE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    PAGE.get_or_init(|| Mutex::new(None))
}

/// 锁中毒（持锁线程 panic）也要继续用：这里只是记一个 URL
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// 「关于」页「检查更新」：问两个源要最新版本，返回最终结论。
#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> Result<UpdateInfo, String> {
    // package_info 在主线程/运行时线程都能读，先取出来再进阻塞线程
    let current = app.package_info().version.to_string();
    tokio::task::spawn_blocking(move || check(&current))
        .await
        .map_err(|e| e.to_string())?
}

/// 「关于」页「下载新版本」：用系统默认浏览器打开上次应答源的发布页。
#[tauri::command]
pub fn open_release_page() -> Result<(), String> {
    let url = lock(last_page())
        .clone()
        .ok_or_else(|| "还没有可打开的发布页，请先检查更新".to_string())?;
    open_url(&url).map_err(|e| format!("打开发布页失败: {e}"))
}

/// 双源检查（阻塞，调用方在后台线程执行）。
///
/// 内部再开两个线程并行请求：最坏耗时 = 单源超时而不是两倍；一边卡住不影响另一边。
fn check(current: &str) -> Result<UpdateInfo, String> {
    let slug = repo();
    let (gitee_api, gitee_page) = gitee_urls(&slug);
    let (github_api, github_page) = github_urls(&slug);
    let (gitee, github) = std::thread::scope(|s| {
        let g = s.spawn(|| fetch(&gitee_api, &gitee_page, "gitee", current));
        let h = s.spawn(|| fetch(&github_api, &github_page, "github", current));
        (join(g), join(h))
    });
    let info = match (gitee, github) {
        // 两边都成功：以主源为准（同 tag 发版，正常情况下两边版本号一致）
        (Ok(info), _) => info,
        // 主源失败、备源成功：静默用备源，不打扰用户
        (Err(_), Ok(info)) => info,
        // 两边都失败：报主源的错误（主源是用户网络环境下更可能通的那个）
        (Err(primary), Err(_)) => return Err(primary),
    };
    *lock(last_page()) = Some(info.page_url.clone());
    Ok(info)
}

/// 收线程结果：子线程 panic 也算该源失败，不把异常升级到命令层
fn join(handle: std::thread::ScopedJoinHandle<'_, Result<UpdateInfo, String>>) -> Result<UpdateInfo, String> {
    handle.join().unwrap_or_else(|_| Err("检查线程异常退出".into()))
}

/// 单源检查：请求 `releases/latest` 并解析 `tag_name`。
///
/// 404（仓库没发过版本）/ 网络失败 / 解析失败一律按「该源失败」抛出，由 check 决定静默与否。
fn fetch(api: &str, page: &str, source: &str, current: &str) -> Result<UpdateInfo, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        // GitHub 强制要求 User-Agent（缺了直接 403），Gitee 一并带上便于排查
        .user_agent(format!("agent-bark/{current}"))
        .build()
        .map_err(|e| format!("HTTP 客户端初始化失败: {e}"))?;

    let resp = client
        .get(api)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| format!("网络请求失败: {e}"))?;
    let status = resp.status();
    // 先取正文再判状态：限流的具体原因（rate limit）在 body 里，不在状态码上
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        return Err(http_error(status.as_u16(), &body));
    }

    let root: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "接口响应解析失败".to_string())?;
    let tag = root.get("tag_name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let remote = parse_ver(&tag).ok_or_else(|| format!("远端版本号无法解析：{tag}"))?;
    // 本地版本理论上一定解析得出来；真解析不出来时不敢说「有新版本」，按最新处理
    let newer = parse_ver(current).is_some_and(|local| compare_ver(&remote, &local).is_gt());

    Ok(UpdateInfo {
        current: current.to_string(),
        latest: tag.trim_start_matches(|c| c == 'v' || c == 'V').to_string(),
        newer,
        source: source.to_string(),
        page_url: page.to_string(),
    })
}

/// 更新场景专属的错误翻译。
///
/// 限流（GitHub 403 / Gitee 429，两平台匿名配额均为每小时 60 次）说成「凭证失效」会误导；
/// 404 在这个接口上是「仓库还没发过版本」，而不是地址写错了。
fn http_error(status: u16, body: &str) -> String {
    let lower = body.to_lowercase();
    if lower.contains("rate limit") || lower.contains("请求过于频繁") {
        return "请求过于频繁，请稍后再试".into();
    }
    match status {
        401 | 403 => "接口拒绝访问（可能是请求过于频繁，请稍后再试）".into(),
        404 => "仓库尚未发布任何版本".into(),
        429 => "请求过于频繁，请稍后再试".into(),
        s => format!("接口返回 HTTP {s}"),
    }
}

/// `v1.2.10` → `[1, 2, 10]`。先去掉 v 前缀，再截掉预发布后缀与构建元数据。
/// 任何一段非数字、为空或超过 4 段返回 None（视为解析失败）。
fn parse_ver(s: &str) -> Option<Vec<u64>> {
    let trimmed = s.trim().trim_start_matches(|c| c == 'v' || c == 'V');
    let core = trimmed.split(['-', '+']).next().unwrap_or("");
    if core.is_empty() {
        return None;
    }
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    parts.iter().map(|p| p.parse::<u64>().ok()).collect()
}

/// 逐段数值比较，长度不齐的补 0（`1.2` 与 `1.2.0` 相等）
fn compare_ver(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// 用系统默认浏览器打开 URL。
///
/// Windows 走 `cmd /C start`：必须带 `CREATE_NO_WINDOW`，否则用户会看到黑框闪一下
/// （与 bark-channels 播放音效同一套做法）。浏览器进程不归我们管，spawn 后交给
/// 后台线程 reap（Unix 上不 wait 的 Child 会变僵尸进程）。
fn open_url(url: &str) -> std::io::Result<()> {
    let mut cmd;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd = std::process::Command::new("cmd");
        // start 的第一个参数会被当成窗口标题，故先给一个空的占位参数
        cmd.args(["/C", "start", "", url]);
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        cmd = std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" });
        cmd.arg(url);
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn parse_ver_接受常见写法() {
        assert_eq!(parse_ver("0.1.0"), Some(vec![0, 1, 0]));
        assert_eq!(parse_ver("v1.2.10"), Some(vec![1, 2, 10]));
        assert_eq!(parse_ver(" V2.0 "), Some(vec![2, 0]));
        // 预发布与构建元数据截掉
        assert_eq!(parse_ver("v1.2.0-beta.1"), Some(vec![1, 2, 0]));
        assert_eq!(parse_ver("1.2.0+abc123"), Some(vec![1, 2, 0]));
    }

    #[test]
    fn parse_ver_拒绝非法输入() {
        assert_eq!(parse_ver(""), None);
        assert_eq!(parse_ver("v"), None);
        assert_eq!(parse_ver("1.2.x"), None);
        assert_eq!(parse_ver("1.2.3.4.5"), None);
    }

    #[test]
    fn compare_ver_必须数值比较() {
        // 字符串序会把 1.1.10 判成小于 1.1.9
        let a = parse_ver("1.1.10").unwrap();
        let b = parse_ver("1.1.9").unwrap();
        assert_eq!(compare_ver(&a, &b), Ordering::Greater);
        // 段数不齐按补 0 处理
        assert_eq!(compare_ver(&parse_ver("1.2").unwrap(), &parse_ver("1.2.0").unwrap()), Ordering::Equal);
        assert_eq!(compare_ver(&parse_ver("0.1.0").unwrap(), &parse_ver("0.1.0").unwrap()), Ordering::Equal);
        assert_eq!(compare_ver(&parse_ver("0.1.0").unwrap(), &parse_ver("0.2.0").unwrap()), Ordering::Less);
    }

    #[test]
    fn http_error_翻译限流与未发版() {
        assert_eq!(http_error(404, "{}"), "仓库尚未发布任何版本");
        assert_eq!(http_error(429, ""), "请求过于频繁，请稍后再试");
        assert_eq!(http_error(403, "API rate limit exceeded"), "请求过于频繁，请稍后再试");
        assert!(http_error(500, "").contains("500"));
    }

    #[test]
    fn 两个平台的地址都由同一个_slug_拼出() {
        assert_eq!(gitee_urls("a/b").0, "https://gitee.com/api/v5/repos/a/b/releases/latest");
        assert_eq!(gitee_urls("a/b").1, "https://gitee.com/a/b/releases/latest");
        assert_eq!(github_urls("a/b").0, "https://api.github.com/repos/a/b/releases/latest");
        assert_eq!(github_urls("a/b").1, "https://github.com/a/b/releases/latest");
        // 联调覆盖变量没设/非法时用默认坐标（合法时尊重它，这里不断言）
        if std::env::var("BARK_UPDATE_REPO").map(|v| !is_safe_slug(v.trim())).unwrap_or(true) {
            assert_eq!(repo(), REPO);
        }
        assert!(is_safe_slug("dashingmonkey/agent-bark"));
        assert!(!is_safe_slug("a/b&calc"));
        assert!(!is_safe_slug(""));
    }
}
