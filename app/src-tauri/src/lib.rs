mod abort_watch;
mod commands;
mod glow;
mod state;
mod tray_menu;
mod update;
mod widget;

use bark_core::NormalizedEvent;
use state::AppState;
use tauri::tray::TrayIconBuilder;
use tauri::Manager;
use tokio::sync::mpsc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            // 二次启动 exe 视为用户显式要打开应用，唤出面板；
            // 例行启动（含开机自启）不弹面板——主窗口 visible:false，走托盘。
            // （唯一例外：一个 agent 都没接上时的启动引导，由前端主动弹出，见 App.vue）
            show_main(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            commands::agent_statuses,
            commands::needs_agent_setup,
            commands::set_agent_enabled,
            commands::get_config,
            commands::save_config,
            commands::list_events,
            commands::clear_events,
            commands::list_active_sessions,
            commands::test_channel,
            commands::test_sound,
            commands::diagnostics,
            commands::glow_preview,
            commands::glow_preview_burst,
            commands::glow_off,
            commands::list_monitors,
            commands::save_widget_position,
            commands::set_widget_pinned,
            commands::close_widget,
            commands::show_main_panel,
            commands::reset_glow,
            commands::toggle_mute,
            commands::quit_app,
            update::check_update,
            update::open_release_page,
        ])
        .on_window_event(|window, event| match event {
            // 关闭窗口 → 隐藏到托盘，退出走托盘菜单。
            // 只拦主窗口：流光覆盖窗没有关闭按钮，但被系统/其它途径关闭时应真的关掉，
            // 否则会留下一层无法回收的透明覆盖层。
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
            // 托盘菜单是自绘小窗：失焦即收（点窗口外任何地方、切到别的应用都会触发），
            // 与原生菜单「点外面就消失」同款行为。Esc / 菜单项的自收在 menu.ts。
            // 刚打开的宽限期内例外：懒建窗首帧 WebView2 会抖出虚假失焦，照收的话
            // 菜单刚弹出就被藏掉——「第一次右键托盘没反应」的元凶。
            tauri::WindowEvent::Focused(false) => {
                if window.label() == tray_menu::LABEL && !tray_menu::opened_recently() {
                    let _ = window.hide();
                }
            }
            _ => {}
        })
        .setup(|app| {
            // 配置：损坏时**不再 panic**，用默认值把界面拉起来并把错误展示给用户，
            // 让用户有机会去修文件（而不是应用直接起不来）。
            let (config, config_path, config_error) = match bark_core::BarkConfig::load_or_init() {
                Ok((c, p)) => (c, p, None),
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::error!("配置加载失败: {msg}");
                    let path = bark_core::BarkConfig::path().unwrap_or_else(|_| std::path::PathBuf::from("config.json"));
                    (bark_core::BarkConfig::default(), path, Some(msg))
                }
            };
            let port = config.server.port;
            let token = config.server.token.clone();

            // 渠道「填了 URL 但没启用」是常见配置误操作：静默不推送很难排查
            if let Some(b) = config.channels.bark.as_ref() {
                if !b.enabled && !b.url.trim().is_empty() {
                    tracing::warn!("Bark 渠道已填 URL 但未启用（enabled=false），不会推送");
                }
            }
            if let Some(w) = config.channels.webhook.as_ref() {
                if !w.enabled && !w.url.trim().is_empty() {
                    tracing::warn!("Webhook 渠道已填 URL 但未启用（enabled=false），不会推送");
                }
            }

            let state = AppState::new(config, config_path);
            state.set_config_error(config_error);

            // 事件管道（server 需在 Tauri 的 async runtime 中启动）
            // B4：serve 返回 Result，bind 失败记入 AppState，diagnostics 命令暴露给 UI
            let (tx, rx) = mpsc::channel::<NormalizedEvent>(256);
            {
                let server_state = state.clone();
                let server_tx = tx.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = bark_server::serve(port, token, server_tx).await {
                        let msg = format!("事件服务器启动失败（127.0.0.1:{port}）: {e}");
                        tracing::error!("{msg}");
                        server_state.set_server_error(Some(msg));
                    }
                });
            }

            app.manage(state.clone());
            app.manage(tx.clone());

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(state::run_pipeline(rx, handle, state.clone()));

            // 补投 daemon 离线期间落盘的事件（hook 兜底路径写入的）
            // B5：通道容量 256 < PENDING_CAP 500，try_send 失败时把剩余事件
            // requeue 回 pending.jsonl，否则事件静默丢失。
            // §1.16：drain 之后**不删** pending.jsonl.draining* 文件——此刻事件只在
            // 内存里，先删后投的崩溃窗口会丢事件。语义是**至少一次投递 + daemon id
            // 去重**（RecentIds 的 seen-ids 持久化，见 state.rs）：投递全部处置完
            // （全部 try_send 成功，或剩余 requeue 写回 pending.jsonl）之后才 ack 删除；
            // 中途崩溃则残留 draining 文件被下次 drain 重复回放，重复事件由 id 去重吞掉。
            let drained = bark_cli::drain_pending();
            let mut overflow_at = None;
            for (i, ev) in drained.iter().enumerate() {
                if tx.try_send(ev.clone()).is_err() {
                    overflow_at = Some(i);
                    break;
                }
            }
            if let Some(i) = overflow_at {
                tracing::warn!("补投通道已满，{} 条事件写回 pending.jsonl 等待下次启动", drained.len() - i);
                bark_cli::requeue_pending(&drained[i..]);
            }
            // 无论全部投递成功还是部分 requeue（requeue 已把剩余事件写回 pending.jsonl），
            // 本轮消费到此全部处置完毕，确认删除 draining 文件
            bark_cli::ack_drained();

            // 启动对齐：已开启的接入一律刷新一遍（并自愈被冲掉/漂移的 hook 与插件），
            // 这样「升级应用 → 重启应用」就能把新版的生成产物（如 DSH 插件）刷到用户机器上；
            // 只有配置本身读不了才置为未启用，原因交给 UI 展示。
            // 必须在 sync_watchers 之前：对齐可能改开关（写 config.json），
            // 让监控型的启停看到的是最终配置。
            let warnings = state::sync_hook_integrations(&state);
            if !warnings.is_empty() {
                for w in &warnings {
                    tracing::warn!("启动对齐[{}]: {}", w.agent, w.message);
                }
            }
            state.set_startup_warnings(warnings);

            // ZCode 回合终态看门狗（非官方）：中断与致命失败都不发 hook，只能读它
            // 自己的日志拿秒级信号（中断合成 run_aborted、回合失败合成 run_failed）。
            // 必须在 sync_watchers 之前——那一步会拿走 tx 的所有权。
            abort_watch::spawn(state.clone(), tx.clone());

            // 监控型 adapter
            state::sync_watchers(tx, &state);

            // 悬浮窗：开关开着就按记住的位置建窗（sync 内部会派发到主线程）
            widget::sync(&app.handle(), &state);

            // 心跳判死巡检：agent 被杀 / 回合被用户中断之后可能**再也不发任何事件**，
            // 只靠「下一条事件」来判死会让流光一直停在思考色（实测 ZCode 手动终止即如此）。
            // 每 30s 清一次僵死会话并据结果改流光——判死阈值本身是 10 分钟，
            // 周期取小值只是让失败色亮得及时，不改变判死口径。
            {
                let handle = app.handle().clone();
                let sweep_state = state.clone();
                tauri::async_runtime::spawn(async move {
                    const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
                    loop {
                        tokio::time::sleep(SWEEP_INTERVAL).await;
                        state::sweep_stale_sessions(&sweep_state, &handle);
                        // 流光窗口随巡检对齐显示器布局：热插拔 / 改分辨率后自愈
                        // （对齐是差量的：布局没变时只是枚举一遍显示器）
                        glow::maintain(&handle, &sweep_state);
                    }
                });
            }

            // 覆盖层保持在任务栏之上：任务栏在它自己刷新时会重新升到最上层，把屏幕
            // **底部**那条光带整条盖住（本层已经没有「无边框全屏应用」身份让任务栏退位
            // 了，见 glow.rs 的 `raise`）。只在灯常驻亮着时动手，5s 一次——周期要短到
            // 用户察觉不到底边光效断过。
            {
                let handle = app.handle().clone();
                let topmost_state = state.clone();
                tauri::async_runtime::spawn(async move {
                    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
                    loop {
                        tokio::time::sleep(INTERVAL).await;
                        glow::keep_topmost(&handle, &topmost_state);
                    }
                });
            }

            // 托盘（B15：默认图标缺失时降级为系统默认，不再 panic）。
            // 右键单击弹出 tray_menu 的自绘菜单——样式与悬浮窗右键菜单同一套；
            // 双击左键直接开面板。主面板只从这里（双击托盘 / 菜单「显示主窗口」）
            // 和悬浮窗双击打开，启动时不再自动显示——唯一例外：本机一个 agent 都没接上时，
            // 前端会主动引导弹出并停在「Agents 接入」页（commands::needs_agent_setup）。
            let mut tray = TrayIconBuilder::with_id("main-tray");
            match app.default_window_icon() {
                Some(icon) => {
                    tray = tray.icon(icon.clone());
                }
                None => tracing::warn!("未找到默认窗口图标，托盘将使用系统默认图标"),
            }
            tray
                .tooltip("AgentBark")
                .on_tray_icon_event(|tray, event| match event {
                    // 双击（左键）：直接开面板。Windows 双击序列是 DOWN/UP/DBLCLK/UP，
                    // 两侧的 Click(Up) 是左键、走不到下面的右键分支，无需额外压制。
                    tauri::tray::TrayIconEvent::DoubleClick {
                        button: tauri::tray::MouseButton::Left,
                        ..
                    } => {
                        show_main(tray.app_handle());
                    }
                    // 右键抬起弹菜单（原生菜单同款时机）；左键单击不做事
                    tauri::tray::TrayIconEvent::Click {
                        position,
                        rect,
                        button: tauri::tray::MouseButton::Right,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } => {
                        tray_menu::open(tray.app_handle(), position, rect.clone());
                    }
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running agent-bark");
}

/// 打开主面板（托盘菜单「显示主窗口」/ 悬浮窗双击 / 二次启动唤起共用）。
/// 隐藏状态打开时先居中（tauri.conf.json 的 center 只管启动首 placement，
/// 托盘唤起走这里）；已可见（含最小化）只聚焦唤醒，不打断用户摆好的位置。
pub(crate) fn show_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if matches!(win.is_visible(), Ok(false)) {
            let _ = win.center();
        }
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}
