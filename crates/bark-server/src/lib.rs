//! 本地事件接收服务：仅监听 127.0.0.1，token 鉴权。
//! hook 子命令 POST /event，daemon 侧消费后进入规则引擎与分发。

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use bark_core::{now_millis, project_name_from_cwd, NormalizedEvent};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 请求体上限（1 MiB）：事件载荷是小 JSON（几 KB 封顶），超限必是误配或滥用。
/// 显式钉死在 Router 上（不依赖 axum `Json` extractor 的隐式默认值），
/// 超限请求在进 handler 前就被 axum 以 413 拒绝。
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// 时间戳夹取窗口（毫秒）：客户端时间戳只可信到「一小时以内」，
/// 夹到 `[now - TIMESTAMP_WINDOW_MS, now]`（见 `normalize_timestamp`）。
const TIMESTAMP_WINDOW_MS: i64 = 3_600_000;

/// bind 遇 AddrInUse 时的有限重试：最多 3 次、间隔 300ms。
/// 快速重启时上一个实例残留的连接（Windows TIME_WAIT）可能短暂占住端口，
/// 短暂重试比立刻报错给用户更友好；仍失败照旧返回 Err（B4 的错误上报口径不变）。
const MAX_BIND_RETRIES: u32 = 3;
const BIND_RETRY_DELAY_MS: u64 = 300;

#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    tx: mpsc::Sender<NormalizedEvent>,
}

/// 事件服务主体。必须在 tokio runtime 上下文中运行
/// （Tauri 侧用 `tauri::async_runtime::spawn` 启动）。
///
/// 返回 `Err` 表示 bind 失败或 serve 中途退出（B4：调用方应把错误记入
/// 应用状态并暴露给 UI，而不是只打日志——端口被占时 GUI 必须能感知）。
pub async fn serve(port: u16, token: String, tx: mpsc::Sender<NormalizedEvent>) -> std::io::Result<()> {
    let state = AppState { token: Arc::new(token), tx };
    let app = router(state);

    let listener = match bind_with_retry(port).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bind 127.0.0.1:{port} failed: {e}");
            return Err(e);
        }
    };
    tracing::info!(port, "bark event server listening");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("event server exited: {e}");
        return Err(std::io::Error::other(e));
    }
    Ok(())
}

/// 路由装配（serve 与测试共用，保证测试打的就是线上那张 Router）。
fn router(state: AppState) -> Router {
    Router::new()
        .route("/event", post(handle_event))
        .route("/health", get(|| async { "ok" }))
        // 显式 body 上限（1 MiB）：对全部路由生效，超限在 extractor 缓冲阶段
        // 就以 413 拒绝，不会走到 handler
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// bind 并做有限重试：只对 `AddrInUse` 重试（快速重启的 TIME_WAIT 残留），
/// 其它错误与重试耗尽一律照旧返回 Err。
async fn bind_with_retry(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    let mut retries = 0u32;
    loop {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && retries < MAX_BIND_RETRIES => {
                retries += 1;
                tracing::warn!(
                    "bind 127.0.0.1:{port} 遇 AddrInUse（快速重启的 TIME_WAIT 残留？），\
                     {BIND_RETRY_DELAY_MS}ms 后重试（{retries}/{MAX_BIND_RETRIES}）"
                );
                tokio::time::sleep(std::time::Duration::from_millis(BIND_RETRY_DELAY_MS)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn handle_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(mut event): Json<NormalizedEvent>,
) -> StatusCode {
    let token = headers
        .get("x-bark-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !token_matches(&state.token, token) {
        return StatusCode::UNAUTHORIZED;
    }
    if event.id.is_empty() {
        event.id = uuid::Uuid::new_v4().to_string();
    }
    event.timestamp = normalize_timestamp(event.timestamp, now_millis());
    fill_project(&mut event);
    // 用 try_send 而不是 send().await：hook 侧 POST 超时只有 100ms，
    // 背压时阻塞响应会让 hook 超时→落盘→下次启动补投，同一条事件被处理两次。
    // 满了直接 503，让 hook 走「落盘 + 兜底提醒」这一条确定路径（daemon 侧另有 id 去重）。
    match state.tx.try_send(event) {
        Ok(()) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// 请求 token 校验。configured 为空串时**直接拒绝一切请求**（防御纵深）：
/// 空对空的 `constant_time_eq` 恒为 true，若不拦，未配置/被清空 token 的部署
/// 会把「空头请求」全部放行（见审查报告 §2.18）——正常路径下 config 校验已
/// 挡住空 token，这里防的是内存态 token 被清空的边角路径。
fn token_matches(configured: &str, presented: &str) -> bool {
    !configured.is_empty() && constant_time_eq(presented, configured)
}

/// 客户端时间戳夹取：非 0 的 `ts` 夹到 `[now-1h, now]`，0（未提供）以收包时刻为准。
///
/// 为什么：服务端此前只把 `timestamp == 0` 换成 now、其余信任客户端。伪造/错算的
/// **未来时间戳**（如插件把秒当毫秒 ×10⁶）会让 daemon 把全部活跃会话判死（误亮
/// 终止色），而未来时间戳的会话反而永不判死（`saturating_sub` 恒 0）；过旧时间戳
/// 绕过 stale 重放判定。夹取是服务端半边的防御（客户端半边另修），见报告 §1.7。
fn normalize_timestamp(ts: i64, now: i64) -> i64 {
    if ts == 0 {
        now
    } else {
        ts.clamp(now - TIMESTAMP_WINDOW_MS, now)
    }
}

/// 补齐从 cwd 推导的项目名。
///
/// hook 子命令那条链路走 `NormalizedEvent::from_raw`，本来就带 project；
/// 插件型 adapter（dsh / opencode）是插件自己归一化后直接 POST JSON，只有 cwd，
/// 于是通知正文首行会退化成一整条路径。这里用同一套规则补上，让两条链路的
/// 通知正文（`project` 首行 + `message`）长得一样。
fn fill_project(event: &mut NormalizedEvent) {
    if event.project.is_none() {
        event.project = project_name_from_cwd(&event.cwd);
    }
}

/// 简单常量时间比较，避免本地 token 计时侧信道（防御性设计，成本为零）
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use bark_core::EventKind;

    fn event(cwd: &str, project: Option<&str>) -> NormalizedEvent {
        NormalizedEvent {
            id: "id".into(),
            agent: "dsh".into(),
            kind: EventKind::RunCompleted,
            session_id: "s".into(),
            cwd: cwd.into(),
            project: project.map(str::to_string),
            message: String::new(),
            timestamp: 1,
            is_subagent: false,
            tool_name: None,
        }
    }

    #[test]
    fn token_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn missing_project_is_derived_from_cwd() {
        // 插件型 adapter 只送 cwd：补出项目名，通知首行不再是一整条路径
        let mut ev = event(r"D:\workspace\gitee\agent-bark", None);
        fill_project(&mut ev);
        assert_eq!(ev.project.as_deref(), Some("agent-bark"));
    }

    #[test]
    fn explicit_project_wins_and_empty_cwd_stays_none() {
        // 已带 project 的事件（hook 链路）不被覆盖
        let mut ev = event("/home/me/proj", Some("自定义"));
        fill_project(&mut ev);
        assert_eq!(ev.project.as_deref(), Some("自定义"));
        // cwd 空时推导不出项目名，保持 None（前端/通知各自回退）
        let mut ev = event("", None);
        fill_project(&mut ev);
        assert_eq!(ev.project, None);
    }

    fn test_state(token: &str) -> (AppState, mpsc::Receiver<NormalizedEvent>) {
        let (tx, rx) = mpsc::channel(8);
        (AppState { token: Arc::new(token.to_string()), tx }, rx)
    }

    fn authed_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-bark-token", token.parse().unwrap());
        h
    }

    #[test]
    fn empty_configured_token_rejects_all_requests() {
        // 防御纵深（报告 §2.18）：空 token 时 constant_time_eq("","") 恒真，
        // 空配置必须直接拒绝一切请求，而不是放行全部「空头请求」
        assert!(!token_matches("", ""));
        assert!(!token_matches("", "x"));
        // 正常口径不受影响
        assert!(token_matches("t", "t"));
        assert!(!token_matches("t", ""));
        assert!(!token_matches("t", "tt"));
    }

    #[tokio::test]
    async fn empty_token_state_rejects_at_handler() {
        // 同一防御在 handler 里的行为契约：内存态 token 被清空时事件不得入队
        let (state, mut rx) = test_state("");
        let status = handle_event(State(state), authed_headers(""), Json(event("", None))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(rx.try_recv().is_err(), "401 后事件不得进消费队列");
    }

    #[test]
    fn timestamp_is_clamped_to_one_hour_window() {
        let now = 1_700_000_000_000_i64;
        assert_eq!(normalize_timestamp(0, now), now, "0 = 未提供 → 以收包时刻为准");
        assert_eq!(normalize_timestamp(now - 1, now), now - 1, "一小时内的新鲜时间戳原样保留");
        assert_eq!(normalize_timestamp(now - TIMESTAMP_WINDOW_MS, now), now - TIMESTAMP_WINDOW_MS);
        // 未来时间戳（如插件把秒当毫秒 ×10⁶）夹到 now：防「处决」全部会话 / 会话永不判死
        assert_eq!(normalize_timestamp(now + 60_000, now), now);
        assert_eq!(normalize_timestamp(i64::MAX, now), now);
        // 过旧夹到下限：防绕过 stale 重放判定
        assert_eq!(normalize_timestamp(now - TIMESTAMP_WINDOW_MS - 1, now), now - TIMESTAMP_WINDOW_MS);
        assert_eq!(normalize_timestamp(i64::MIN, now), now - TIMESTAMP_WINDOW_MS);
    }

    #[tokio::test]
    async fn handler_clamps_future_timestamp_before_queueing() {
        // 行为契约：未来时间戳在进消费队列前已被夹取（报告 §1.7 服务端半边）
        let (state, mut rx) = test_state("t");
        let mut ev = event("", None);
        ev.timestamp = now_millis() + 999_999_999;
        let before = now_millis();
        let status = handle_event(State(state), authed_headers("t"), Json(ev)).await;
        assert_eq!(status, StatusCode::OK);
        let got = rx.try_recv().unwrap();
        assert!(
            (before..=now_millis()).contains(&got.timestamp),
            "未来时间戳必须夹到收包时刻附近: {}",
            got.timestamp
        );
    }

    #[tokio::test]
    async fn bind_retries_then_reports_error_when_port_held() {
        // 快速重启（TIME_WAIT 残留）时有限重试：最多 3 次 × 300ms，仍失败照旧返回 Err
        let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        let start = std::time::Instant::now();
        let err = bind_with_retry(port).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "重试耗尽后照旧返回 Err");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(MAX_BIND_RETRIES as u64 * BIND_RETRY_DELAY_MS),
            "重试必须做满 {MAX_BIND_RETRIES} 次 × {BIND_RETRY_DELAY_MS}ms，实际 {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn bind_succeeds_first_try_when_port_free() {
        // 空闲端口：首次即成功，不引入任何重试延迟
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe); // 未 accept 过连接：无 TIME_WAIT，端口立即可复用
        let start = std::time::Instant::now();
        let l = bind_with_retry(port).await.unwrap();
        assert_eq!(l.local_addr().unwrap().port(), port);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(BIND_RETRY_DELAY_MS),
            "首次成功不得付出重试延迟"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn body_over_one_mib_is_rejected_normal_events_pass() {
        let (state, mut rx) = test_state("t");
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });

        // 正常大小的事件：过 limit、进 handler、鉴权后入队（防止上限误伤正常载荷）
        let body = serde_json::to_string(&event("", None)).unwrap();
        let resp = raw_http(
            addr,
            &format!(
                "POST /event HTTP/1.1\r\nHost: 127.0.0.1\r\nx-bark-token: t\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            ),
            body.as_bytes(),
        );
        assert!(resp.contains(" 200"), "正常事件必须放行: resp={resp:?}");
        assert!(rx.try_recv().is_ok(), "事件应进入消费队列");

        // 超过 1 MiB：进 handler 前就被 413 拒绝（显式钉死上限，不靠 axum 隐式默认值）
        let over = MAX_BODY_BYTES + 1024;
        let resp = raw_http(
            addr,
            &format!(
                "POST /event HTTP/1.1\r\nHost: 127.0.0.1\r\nx-bark-token: t\r\n\
                 content-type: application/json\r\ncontent-length: {over}\r\nConnection: close\r\n\r\n"
            ),
            &vec![b'x'; over],
        );
        assert!(resp.contains(" 413"), "超限必须 413: resp={resp:?}");
    }

    /// 极简 HTTP/1.1 客户端（测试不引入新依赖）：写请求、读到响应头结束即返回。
    /// 读写并发：服务端可在读完 body 前就拒绝并关闭连接，「先写后读」会把已到达的
    /// 响应冲掉；写失败（对端提前拒绝）在此场景是预期的，忽略即可。
    /// 只按 content-length 定界、不做 write 侧 shutdown——实测 hyper 收到半关闭会
    /// 直接掐掉响应不回包（测试客户端的坑，不是服务端行为）。
    fn raw_http(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> String {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut req = head.as_bytes().to_vec();
        req.extend_from_slice(body);
        let writer = std::thread::spawn(move || {
            let _ = writer.write_all(&req);
        });

        let mut resp = Vec::new();
        let mut buf = [0u8; 4096];
        while !resp.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => resp.extend_from_slice(&buf[..n]),
                Err(_) => break, // 连接被复位/超时：已读到的部分就是响应
            }
        }
        writer.join().unwrap();
        String::from_utf8_lossy(&resp).into_owned()
    }
}
