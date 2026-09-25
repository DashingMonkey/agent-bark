//! Tauri IPC 命令（前端调用）

use crate::glow;
use crate::state::{AppState, SessionStatus, HISTORY_CAP};
use bark_adapters::registry;
use bark_adapters::{AgentStatus, InstallCtx, RegisterCtx};
use bark_channels::{BarkChannel, Channel, Notification, WebhookChannel};
use bark_core::config::BarkChannelConfig;
use bark_core::{BarkConfig, ChannelsConfig, EventKind, NormalizedEvent, WebhookChannelConfig};
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State, Webview};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// 自定义命令的调用方窗口白名单（§2.18 ACL 纵深防御）
// ---------------------------------------------------------------------------
//
// 自定义命令不需要 capabilities 里的权限项（ACL 只管 core:*/plugin:* 命令），
// 任何 webview 发起 IPC 就能调用——CSP 挡住了直接利用链，但「托盘小窗能 save_config、
// 悬浮窗能 quit_app」这类面该收小。写命令的签名带 `webview: tauri::Webview` 首参，
// 按调用方窗口 label 白名单校验，违反返回 Err。
//
// 白名单与各窗口入口的**实际 invoke 调用面**一一对应（前端 grep 核对）：
// - "main"（index.html → main.ts → App.vue + pages）：save_config / set_agent_enabled /
//   test_channel / test_sound / glow_preview / glow_preview_burst / glow_off；
// - "main" + "tray-menu"（menu.html → menu.ts 直接 invoke）：reset_glow / toggle_mute / quit_app；
// - "widget"（widget.html → widget.ts）：save_widget_position / set_widget_pinned / close_widget；
// - 任意窗口：show_main_panel（App.vue / widget.ts 都调）与只读命令
//   （agent_statuses / needs_agent_setup / get_config / diagnostics / list_events /
//   clear_events / list_active_sessions / list_monitors / check_update / open_release_page）；
// - "glow-*"（glow.html → glow.ts）：只 listen 事件，零命令。
const MAIN: &[&str] = &["main"];
/// 托盘菜单窗口里直接调用的动作（menu.ts）：主窗口与托盘菜单共用
const MAIN_OR_TRAY: &[&str] = &["main", crate::tray_menu::LABEL];
/// 悬浮窗右键菜单的动作（widget.ts）：只许悬浮窗自己调
const WIDGET_ONLY: &[&str] = &[crate::widget::WIDGET_LABEL];

/// 校验命令调用方窗口是否在白名单内（判定逻辑见纯函数 [`label_allowed`]）
fn require_window(webview: &Webview, allowed: &[&str]) -> Result<(), String> {
    let window = webview.window();
    let label = window.label();
    if label_allowed(label, allowed) {
        Ok(())
    } else {
        Err(format!("命令拒绝来自窗口「{label}」的调用"))
    }
}

/// 窗口 label 白名单判定（纯函数，便于测试）
fn label_allowed(label: &str, allowed: &[&str]) -> bool {
    allowed.contains(&label)
}

/// 诊断信息（配置损坏、事件服务器启动失败、hook 漂移等需要用户处理的问题）
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostics {
    pub config_path: String,
    pub config_error: Option<String>,
    /// 事件服务器 bind/serve 失败（B4）：端口被占时 GUI 必须能看到
    pub server_error: Option<String>,
    /// 配置里的 port/token 与运行中的事件服务器不一致 → 需重启才生效
    pub restart_required: bool,
    /// 启动自检产生的问题（接入自愈失败 / 因配置损坏被置为未启用）
    pub startup_warnings: Vec<String>,
}

/// 所有 agent 的检测状态
///
/// 注意：带 State 引用入参的 async 命令必须返回 Result（tauri 宏约束）；
/// 入体即把 State 转成 Arc，future 不再持有引用（'static 要求）。
/// Ok 路径对前端 invoke 透明，无需前端改动。
#[tauri::command]
pub async fn agent_statuses(state: State<'_, Arc<AppState>>) -> Result<Vec<AgentStatus>, String> {
    let state = state.inner().clone();
    let statuses = tokio::task::spawn_blocking(move || scan_statuses(&state))
        .await
        .map_err(|e| e.to_string())?;
    Ok(statuses)
}

/// 扫描所有 agent 的接入状态（**阻塞**，含 FS 探测；调用方负责 spawn_blocking）。
///
/// B12：先 clone 配置缩小持锁范围，FS 探测整体在锁外进行。
fn scan_statuses(state: &Arc<AppState>) -> Vec<AgentStatus> {
    let cfg = crate::state::read(&state.config).clone();
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let runtime = crate::state::watch_runtime(state);
    registry::statuses(&cfg, &exe, &runtime)
}

/// 「这个 agent 接上了吗」：与 Agents 页开关同口径（AgentsPage 的 isOn）。
///
/// 只认「配置里开着 **且** 真的写进去了」这一种状态，两个方向都不能放宽：
/// - 不看 `registered` 会把「开关开着但 hook 被目标应用重写掉」当成已接入——用户其实
///   一个通知都收不到，启动时却看不到引导；
/// - 不看 `enabled` 会把「用户刚关掉接入、hook 已回收」当成还接着。
fn agent_integrated(s: &AgentStatus) -> bool {
    s.enabled && s.registered
}

/// 启动引导判定：本机是否**一个 agent 都没接上**（前端据此弹面板并停在 Agents 接入页）。
///
/// 为什么需要它：主窗口默认隐藏、例行启动只走托盘（见 lib.rs 的托盘注释），
/// 新用户装完不知道要先去开接入——这一页可能永远不会被打开。
///
/// 这是**当前状态**判定，不是「是否点过开关」：一个都没接上（含全被关掉的情况）时每次
/// 启动都会引导，接上任意一个即不再打扰。口径与 Agents 页的开关一致（见 `agent_integrated`）。
#[tauri::command]
pub async fn needs_agent_setup(state: State<'_, Arc<AppState>>) -> Result<bool, String> {
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || !scan_statuses(&state).iter().any(agent_integrated))
        .await
        .map_err(|e| e.to_string())
}

/// 诊断信息
#[tauri::command]
pub async fn diagnostics(state: State<'_, Arc<AppState>>) -> Result<Diagnostics, String> {
    Ok(Diagnostics {
        config_path: state.config_path.display().to_string(),
        config_error: state.config_error(),
        server_error: state.server_error(),
        restart_required: state.restart_required(),
        startup_warnings: state.startup_warnings(),
    })
}

/// 统一开关：勾选 = 接入（hook 型写入配置 / 监控型启动轮询），取消 = 回收/停止。
/// 返回需要展示的提示（如 CodeBuddy 的 /hooks 信任引导）。
///
/// 失败时**不留半成品**：先写 agent 配置，再落配置开关；若开关保存失败，
/// 立刻把刚写入的 hook 回滚掉，避免出现「配置里有 hook 但 UI 显示未接入」。
#[tauri::command]
pub async fn set_agent_enabled(
    webview: Webview,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    kind: String,
    enabled: bool,
    tx: State<'_, mpsc::Sender<NormalizedEvent>>,
) -> Result<Option<String>, String> {
    require_window(&webview, MAIN)?;
    // B12：含 agent 配置文件写入与 fsync，整体挪进 spawn_blocking。
    // Arc<AppState> 与 mpsc::Sender 都是 'static，可直接 move。
    let state = state.inner().clone();
    let tx = tx.inner().clone();
    tokio::task::spawn_blocking(move || set_agent_enabled_blocking(app, state, kind, enabled, tx))
        .await
        .map_err(|e| e.to_string())?
}

/// 配置损坏时的写盘短路（B12 补丁）：此刻内存里的配置是 `BarkConfig::default()`
/// （含**新随机 token**、渠道全空），任何落盘都会把用户真正的 config.json 覆盖掉。
/// save_config 原本就有这道守卫，但开关 / 悬浮窗位置等其余写盘路径没有——
/// 配置损坏后用户拖一下悬浮窗就会触发覆盖。所有写盘命令统一走这里。
fn ensure_config_writable(state: &Arc<AppState>) -> Result<(), String> {
    match state.config_error() {
        Some(e) => Err(format!(
            "配置文件损坏，已阻止写入以免覆盖你的配置：{e}\n\
             请修复或删除配置文件后重启应用。"
        )),
        None => Ok(()),
    }
}

/// set_agent_enabled 的阻塞实现（见上）
fn set_agent_enabled_blocking(
    app: AppHandle,
    state: Arc<AppState>,
    kind: String,
    enabled: bool,
    tx: mpsc::Sender<NormalizedEvent>,
) -> Result<Option<String>, String> {
    let agent = bark_core::AgentKind::from_id(&kind).ok_or_else(|| format!("未知 agent: {kind}"))?;
    // 配置损坏 → 整个开关操作短路（见 ensure_config_writable：此刻落任何盘都会覆盖用户配置）
    ensure_config_writable(&state)?;

    let mut hint: Option<String> = None;
    let mut wrote_hook = false;

    if let Some(adapter) = registry::find_hook_adapter(agent) {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let (port, token) = {
                let cfg = crate::state::read(&state.config);
                (cfg.server.port, cfg.server.token.clone())
            };
            let ctx = RegisterCtx { exe_path: exe.to_string_lossy().into_owned(), port, token };
            adapter.register(&ctx).map_err(|e| format!("{e:#}"))?;
            wrote_hook = true;
            hint = adapter.trust_hint().map(String::from);
        } else {
            let mut ctx = InstallCtx {
                exe_path: std::env::current_exe()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                dry_run: false,
                backup: true,
            };
            adapter.unregister(&mut ctx).map_err(|e| format!("{e:#}"))?;
            // 插件型（DSH / OpenCode）：删除的只是磁盘上的插件与登记，宿主进程里
            // 已加载的那份仍在内存中继续上报，必须提示用户重启宿主才彻底停止
            // （在此之前 daemon 会按开关忽略该 agent 的事件）。
            hint = adapter.unregister_hint().map(String::from);
        }
    } else if agent.mode() == bark_core::AdapterMode::Watch {
        // 监控型（WorkBuddy / TraeWork）：没有 hook 配置文件可写，开关落盘后
        // 由 sync_watchers 启停。启用前先做可用性预检，把失败挡在开关翻转之前——
        // 否则开关写成功了 watcher 却起不来，UI 只会显示「已开启未生效」，
        // 用户不知道为什么（真实原因：库不存在 / schema 失效 / 已加密）。
        if enabled {
            match registry::find_watch_adapter(agent) {
                Some(a) if !a.is_installed() => {
                    return Err(format!("{} 未检测到安装，无法开启监控", agent.display_name()));
                }
                Some(a) if !a.is_available() => {
                    let reason = a.unavailable_reason().unwrap_or_else(|| "监控暂不可用".into());
                    return Err(reason);
                }
                _ => {}
            }
        }
    } else {
        // 无 hook 适配器且非监控型：枚举未注册或接入方式尚未支持
        return Err(format!("{} 暂不支持接入（无适配器）", agent.display_name()));
    }

    // 监控型：只写开关，sync_watchers 负责启停。
    // 整个「读-改-写盘」持 config_write（§2.1 单写者语义）：与 save_config 的整份
    // 覆盖、悬浮窗的单字段写串行，防并发保存交错丢更新（同类写路径一并纳入）。
    // 锁在 sync_watchers 等副作用之前释放（见 AppState::config_write 的注释）。
    {
        let _write_guard = crate::state::lock(&state.config_write);
        let mut cfg = crate::state::write(&state.config);
        // B2：保存旧开关值，save 失败时连同内存态一起回滚，
        // 避免「内存已开 / 磁盘未写 / hook 已回滚」三方不一致。
        let old_enabled = cfg.agent_enabled(&kind);
        cfg.set_agent_enabled(&kind, enabled);
        if let Err(e) = cfg.save(&state.config_path) {
            cfg.set_agent_enabled(&kind, old_enabled);
            drop(cfg);
            // 回滚刚写入的 hook，避免状态不一致。
            // exe_path 必须传真实路径：adapter 的 verify/unregister 用它定位我们的条目，
            // 传空串会定位不到，回滚就成了空操作。
            if wrote_hook {
                if let Some(adapter) = registry::find_hook_adapter(agent) {
                    let mut ctx = InstallCtx {
                        exe_path: std::env::current_exe()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        dry_run: false,
                        backup: false,
                    };
                    let _ = adapter.unregister(&mut ctx);
                }
            }
            return Err(format!("配置保存失败，已回滚接入改动: {e:#}"));
        }
    }
    // 开关已按用户意图落定：启动自检关于这个 agent 的告警（如「配置无法解析，已置为未启用」）
    // 到这就过时了——留着会一直挂到下次重启，看起来像「修复没生效」。
    state.clear_startup_warnings(&kind);
    // 关闭接入：立刻摘掉它在「运行中会话」面板里的残留条目，并做好流光善后。
    // 管道里那道门要等该 agent 的下一条事件才触发，而 hook 型 agent 关掉后可能再也不
    // 上报（最长挂到 10 分钟僵死判定）；善后口径必须与那道门一致——
    // 少做流光那一步就会留下一盏熄不掉的灯（历史上正是这里漏了，见
    // `state::drop_agent_sessions_and_sync` 的注释）。
    if !enabled {
        crate::state::drop_agent_sessions_and_sync(&app, &state, &kind);
    }
    let _ = crate::state::sync_watchers(tx, &state);
    Ok(hint)
}

/// 读取配置
#[tauri::command]
pub async fn get_config(state: State<'_, Arc<AppState>>) -> Result<BarkConfig, String> {
    Ok(crate::state::read(&state.config).clone())
}

/// 保存配置（前端提交完整配置）
#[tauri::command]
pub async fn save_config(
    webview: Webview,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    config: BarkConfig,
    tx: State<'_, mpsc::Sender<NormalizedEvent>>,
) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    // B12：fsync 落盘挪进 spawn_blocking
    let state = state.inner().clone();
    let tx = tx.inner().clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        // 配置损坏时内存里是 BarkConfig::default()（含**新随机 token**），
        // 落盘会覆盖用户真正的 token 与全部设置。这里直接拒绝写入，
        // 让用户先修复文件（AgentsPage 已提示「修复文件后重启应用」）。
        ensure_config_writable(&state)?;

        let mut incoming = config;
        // 端口/token 由用户手改时不再静默回退成启动值（那会让 UI 保存看起来「没生效」）：
        // 保存用户提交的值，运行中的服务器继续用启动值，UI 通过 diagnostics.restart_required
        // 提示「需重启生效」。空值视为未填写，保留运行值以免把 token 清掉。
        if incoming.server.token.trim().is_empty() {
            incoming.server.token = state.server_token.clone();
        }
        if incoming.server.port == 0 {
            incoming.server.port = state.server_port;
        }
        if incoming.server.port != state.server_port || incoming.server.token != state.server_token {
            tracing::warn!(
                "server.port/token 已保存，但运行中的事件服务器仍使用启动值（127.0.0.1:{}），重启后生效",
                state.server_port
            );
        }

        // 「读-改-写盘-换内存」全程持 config_write（§2.1 单写者语义）：
        // 与 save_widget_position / set_widget_pinned / close_widget 的单字段写盘
        // 串行——否则刚拖的悬浮窗位置会被这份旧快照整份覆盖回去（丢更新）。
        // 悬浮窗的几何记忆（x/y/pinned）保留逻辑紧贴落盘、同在锁内：
        // 前端提交的是打开页面时的快照，以**窗口侧实时上报**的那份为准；
        // 开关（enabled）仍以页面为准。锁在副作用（sync_watchers 等）之前释放。
        {
            let _write_guard = crate::state::lock(&state.config_write);
            preserve_widget_geometry(&mut incoming, &crate::state::read(&state.config));
            // 先落盘（不持配置读写锁本体），再原子替换内存值：
            // save() 内部有 fsync，持 RwLock 写锁会阻塞 run_pipeline 的读锁
            incoming.save(&state.config_path).map_err(|e| format!("配置保存失败: {e:#}"))?;
            *crate::state::write(&state.config) = incoming;
        }

        let _ = crate::state::sync_watchers(tx, &state);
        // 流光开关 / 显示器范围 / 宽度等可能变了：重建或回收覆盖窗口
        glow::sync(&app, &state);
        // 悬浮窗开关可能变了：创建 / 销毁窗口
        crate::widget::sync(&app, &state);
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 整份保存时保留悬浮窗几何记忆（x/y/pinned）：以窗口侧实时上报的那份为准。
/// 抽成纯函数钉死「整份覆盖不得回退刚拖到的位置」（§2.1 回归）。
fn preserve_widget_geometry(incoming: &mut BarkConfig, current: &BarkConfig) {
    incoming.widget.x = current.widget.x;
    incoming.widget.y = current.widget.y;
    incoming.widget.pinned = current.widget.pinned;
}

/// 悬浮窗位置记忆（拖动松手后上报，低频）。
///
/// 只写 widget 段、不走整份 save_config：位置上报与用户在主窗口的编辑并发，
/// 整份落盘会互相覆盖。口径见 widget.rs 模块注释（x/y = 卡片左上角，逻辑像素）。
#[tauri::command]
pub async fn save_widget_position(webview: Webview, state: State<'_, Arc<AppState>>, x: f64, y: f64) -> Result<(), String> {
    require_window(&webview, WIDGET_ONLY)?;
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        ensure_config_writable(&state)?;
        // 单写者（§2.1）：与 save_config 的整份覆盖串行，「读-改-写盘」全程持锁
        let _write_guard = crate::state::lock(&state.config_write);
        let mut cfg = crate::state::write(&state.config);
        cfg.widget.x = x;
        cfg.widget.y = y;
        cfg.save(&state.config_path)
            .map_err(|e| format!("保存悬浮窗位置失败: {e:#}"))?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 悬浮窗右键菜单「固定位置」：只写 widget.pinned（并发考量同 save_widget_position）
#[tauri::command]
pub async fn set_widget_pinned(webview: Webview, state: State<'_, Arc<AppState>>, pinned: bool) -> Result<(), String> {
    require_window(&webview, WIDGET_ONLY)?;
    let state = state.inner().clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        ensure_config_writable(&state)?;
        // 单写者（§2.1）：与 save_config 的整份覆盖串行，「读-改-写盘」全程持锁
        let _write_guard = crate::state::lock(&state.config_write);
        let mut cfg = crate::state::write(&state.config);
        cfg.widget.pinned = pinned;
        cfg.save(&state.config_path)
            .map_err(|e| format!("保存固定状态失败: {e:#}"))?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 悬浮窗右键菜单「关闭悬浮窗」：等价于把开关关掉（落盘 + 销毁窗口）。
/// 广播 bark://widget-closed 让主窗口的悬浮窗设置页同步勾选态。
#[tauri::command]
pub async fn close_widget(webview: Webview, app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    require_window(&webview, WIDGET_ONLY)?;
    let state = state.inner().clone();
    let cfg_state = state.clone();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        ensure_config_writable(&cfg_state)?;
        // 单写者（§2.1）：与 save_config 的整份覆盖串行，「读-改-写盘」全程持锁
        let _write_guard = crate::state::lock(&cfg_state.config_write);
        let mut cfg = crate::state::write(&cfg_state.config);
        cfg.widget.enabled = false;
        cfg.save(&cfg_state.config_path).map_err(|e| format!("保存配置失败: {e:#}"))?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())??;
    // 销毁窗口必须主线程，sync 内部会派发（先落盘再销毁：重启后保持关闭）；
    // 锁已在落盘块结束时释放，widget::sync 不进写锁
    crate::widget::sync(&app, &state);
    let _ = app.emit("bark://widget-closed", ());
    Ok(())
}

/// 设置页预览：临时点亮某个流光状态，几秒后自动恢复。
///
/// 无视总开关——用户点预览就是想先看效果，不该被开关挡住。
/// 恢复靠 `end_preview(seq)`：预览期间来了真实事件或被保存配置/熄灭顶掉时，
/// 序号已经失效，旧定时器不会再把流光改回去（这是「预览完停不下来」的根因）。
#[tauri::command]
pub async fn glow_preview(
    webview: Webview,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    state_name: String,
) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    let target = parse_glow_state(&state_name)?;
    let state = state.inner().clone();
    // "idle" = 熄灭：立刻收起，不需要恢复定时器
    if target == glow::GlowState::Idle {
        glow::off(&app, &state);
        return Ok(());
    }
    let seq = glow::preview(&app, &state, target);
    // 预览停留时长：跟「完成」一致，但不少于 3s，太短看不清
    let hold = glow::GlowState::Completed.hold_ms().max(3_000);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
        glow::end_preview(&app, &state, seq);
    });
    Ok(())
}

/// 熄灭：结束预览并立刻收起光效（下一次真实事件会重新点亮）。
/// 覆盖窗口不销毁，点一下就能灭，不用等预览的恢复定时器。
#[tauri::command]
pub async fn glow_off(webview: Webview, app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    let state = state.inner().clone();
    glow::off(&app, &state);
    Ok(())
}

/// 设置页组合预览：边缘亮 `edge` 状态、全屏以 `burst` 状态的颜色补放一次。
///
/// 用于预览双通道并存的效果——「一个完成、其余还在跑」这类多会话场景：
/// 边缘保持思考色呼吸，全屏按事件角色（完成 / 失败）闪一下。预览的临时接管与
/// 自动恢复语义与 `glow_preview` 一致。
#[tauri::command]
pub async fn glow_preview_burst(
    webview: Webview,
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    edge_name: String,
    burst_name: String,
) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    let edge = parse_glow_state(&edge_name)?;
    let burst = parse_glow_state(&burst_name)?;
    if edge == glow::GlowState::Idle || burst == glow::GlowState::Idle {
        return Err("组合预览的边缘与全屏状态都不能是 idle".into());
    }
    let state = state.inner().clone();
    let seq = glow::preview_burst(&app, &state, edge, burst);
    let hold = glow::GlowState::Completed.hold_ms().max(3_000);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(hold)).await;
        glow::end_preview(&app, &state, seq);
    });
    Ok(())
}

fn parse_glow_state(s: &str) -> Result<glow::GlowState, String> {
    match s {
        "running" => Ok(glow::GlowState::Running),
        "waiting" => Ok(glow::GlowState::Waiting),
        "completed" => Ok(glow::GlowState::Completed),
        "failed" => Ok(glow::GlowState::Failed),
        "idle" => Ok(glow::GlowState::Idle),
        other => Err(format!("未知流光状态: {other}")),
    }
}

/// 「屏幕光效」页的「生效显示器」下拉：枚举当前所有显示器。
///
/// 插拔显示器后用户会重新打开这个页面，因此每次调用都实时枚举，不做缓存
/// （缓存会让「刚插上的屏」永远不出现，而用户没有任何刷新入口）。
#[tauri::command]
pub async fn list_monitors(app: AppHandle) -> Vec<glow::MonitorInfo> {
    // available_monitors 内部要把调用派发到主线程并等结果，会阻塞当前线程
    tokio::task::spawn_blocking(move || glow::list_monitors(&app))
        .await
        .unwrap_or_default()
}

/// 事件历史（最近 500 条，新的在前）
#[tauri::command]
pub fn list_events(state: State<'_, Arc<AppState>>, limit: Option<usize>) -> Vec<NormalizedEvent> {
    let history = crate::state::lock(&state.history);
    let limit = limit.unwrap_or(HISTORY_CAP).min(HISTORY_CAP);
    history.iter().take(limit).cloned().collect()
}

/// 清空事件历史（事件流页的「清空显示」/「清空当前筛选」），返回被删除的事件 id。
///
/// 必须落在后端：页面每次挂载都会用 `list_events` 补历史，只删前端副本的话，
/// 切走菜单再切回来被清掉的事件会被历史整批合并回来。`kind = None` 清空全部。
/// 类型由 serde 反序列化保证合法（前端选项来自 EVENT_LABELS，未知值直接报错，
/// 不会静默清错类型）。
///
/// 返回 id 而不是条数：前端按「后端确实删了哪些」删本地副本，才是两边一致的口径
/// （按点击瞬间的本地快照删，在清空请求在途时会与后端产生反向偏差）。
#[tauri::command]
pub fn clear_events(state: State<'_, Arc<AppState>>, kind: Option<EventKind>) -> Vec<String> {
    state.clear_history(kind)
}

/// 运行中的 agent 会话（实时状态）。含惰性 GC，并过滤掉已关闭接入的 agent。
#[tauri::command]
pub fn list_active_sessions(state: State<'_, Arc<AppState>>) -> Vec<SessionStatus> {
    crate::state::sessions_for_ui(state.inner())
}

/// 渠道测试用的本次生效配置（跨模块契约，前端 api.ts 的 testChannel 已按此传参）。
enum TestTarget {
    Bark(BarkChannelConfig),
    Webhook(WebhookChannelConfig),
}

/// 合成渠道测试的本次生效配置（纯函数，§2.16 / code-review §1.14）：
/// - `url`：`Some(非空)` 用传入值构造**本次**渠道配置——「测试」测的必须是表单里
///   未保存的输入，否则新填 URL 没保存就点测试，发去的还是落盘的旧地址；
///   **空串视同 None** 回退落盘配置（表单把「清空」序列化成 ""）；
/// - `template`：`Some` 用传入值；`None` 回退落盘配置的 template，再回退默认模板
///   （漏配 template 发空 body 会被服务端 4xx）；
/// - 两者都 `None` = 用已落盘配置（旧行为）。
/// 测试发送不受 `enabled` 开关影响（点「测试」就是要发一条看看）。
fn resolve_test_target(
    channel: &str,
    saved: &ChannelsConfig,
    url: Option<String>,
    template: Option<String>,
) -> Result<TestTarget, String> {
    let url = url.filter(|u| !u.trim().is_empty());
    match channel {
        "bark" => {
            let saved = saved.bark.clone().unwrap_or_default();
            let url = url.unwrap_or(saved.url);
            if url.trim().is_empty() {
                return Err("Bark 未配置".into());
            }
            Ok(TestTarget::Bark(BarkChannelConfig { enabled: true, url }))
        }
        "webhook" => {
            let saved = saved.webhook.clone().unwrap_or_default();
            let url = url.unwrap_or(saved.url);
            if url.trim().is_empty() {
                return Err("Webhook 未配置".into());
            }
            let template = template.unwrap_or_else(|| {
                if saved.template.trim().is_empty() {
                    WebhookChannelConfig::default().template
                } else {
                    saved.template
                }
            });
            Ok(TestTarget::Webhook(WebhookChannelConfig {
                enabled: true,
                url,
                method: saved.method,
                template,
            }))
        }
        other => Err(format!("未知渠道: {other}")),
    }
}

/// 渠道测试：发一条测试通知走指定渠道。
/// `url`/`template` 的语义见 [`resolve_test_target`]（契约参数名 `{ channel, url, template }`）。
#[tauri::command]
pub async fn test_channel(
    webview: Webview,
    state: State<'_, Arc<AppState>>,
    channel: String,
    url: Option<String>,
    template: Option<String>,
) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    let saved = crate::state::read(&state.config).channels.clone();
    let target = resolve_test_target(&channel, &saved, url, template)?;
    let n = Notification {
        title: "agent-bark 测试通知".into(),
        body: format!("渠道 {channel} 工作正常"),
        event: EventKind::RunCompleted,
        agent: "agent-bark".into(),
        project: None,
    };
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        match target {
            TestTarget::Bark(c) => BarkChannel(c).send(&n),
            TestTarget::Webhook(c) => WebhookChannel(c).send(&n),
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("{e:#}"))
}

/// 「通知」页「声音」的测试按钮：立即把选定的音效播放 `plays` 次。
/// 走与真实事件同一条播放路径（bark_channels::play_sound），测的就是将来会响的那个声音。
#[tauri::command]
pub async fn test_sound(webview: Webview, effect: String, plays: u32) -> Result<(), String> {
    require_window(&webview, MAIN)?;
    tokio::task::spawn_blocking(move || bark_channels::play_sound(&effect, plays))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

// ---- 托盘菜单 / 悬浮窗的窗口动作 ----
// 自定义命令不需要 capabilities 里的权限项（ACL 只管 core:*/plugin:* 命令），
// 窗口能发起 IPC 即可调用；各窗口能调哪些动作在此按窗口 label 白名单收口
// （见文件头的白名单注释）。

/// 显示主面板：托盘菜单「显示主窗口」与悬浮窗双击共用。
/// 隐藏时先居中再显示，已可见只聚焦（口径见 lib.rs show_main）。
/// 任意窗口可调（App.vue 启动引导、widget.ts 双击、menu.ts 菜单项都用它）。
#[tauri::command]
pub fn show_main_panel(app: AppHandle) {
    crate::show_main(&app);
}

/// 托盘菜单「重置流光」：无条件熄灭（销毁覆盖窗 + 状态机复位），
/// 卡住的异常颜色一键驱散，下一次事件重新点亮。
#[tauri::command]
pub fn reset_glow(webview: Webview, app: AppHandle) -> Result<(), String> {
    require_window(&webview, MAIN_OR_TRAY)?;
    glow::reset(&app, &app.state::<Arc<AppState>>());
    Ok(())
}

/// 托盘菜单「静音」：切换静音（事件仍入历史，仅暂停推送），返回切换后的状态供菜单勾选。
#[tauri::command]
pub fn toggle_mute(webview: Webview, app: AppHandle) -> Result<bool, String> {
    require_window(&webview, MAIN_OR_TRAY)?;
    Ok(app.state::<Arc<AppState>>().toggle_mute())
}

/// 托盘菜单「退出」：整个应用退出（主窗口关闭只是隐藏到托盘，不在这里）。
#[tauri::command]
pub fn quit_app(webview: Webview, app: AppHandle) -> Result<(), String> {
    require_window(&webview, MAIN_OR_TRAY)?;
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::agent_integrated;
    use bark_adapters::{AgentStatus, VerifyReport};
    use bark_core::AdapterMode;

    fn status(enabled: bool, registered: bool) -> AgentStatus {
        AgentStatus {
            kind: "claude-code",
            display_name: "Claude Code",
            mode: AdapterMode::Hook,
            installed: true,
            registered,
            enabled,
            trust_hint: None,
            config_paths: Vec::new(),
            verify: Some(if registered {
                VerifyReport::Ok
            } else {
                VerifyReport::NotRegistered
            }),
            last_error: None,
        }
    }

    /// 「接上了」= 配置里开着 **且** hook 真的写进去了。
    ///
    /// 半死状态（开关开着、hook 被目标应用重写掉）必须仍算「没接上」：这种机器上
    /// 用户收不到任何通知，正是最需要被引导到 Agents 接入页的情形。
    #[test]
    fn integrated_needs_switch_and_registration() {
        assert!(agent_integrated(&status(true, true)));
        assert!(!agent_integrated(&status(true, false)));
        assert!(!agent_integrated(&status(false, true)));
        assert!(!agent_integrated(&status(false, false)));
    }

    /// §2.18 回归：命令调用方窗口白名单与各窗口入口的实际 invoke 调用面一致。
    /// 托盘菜单（menu.ts 直接调 reset_glow / toggle_mute / quit_app）不能被弄坏，
    /// 悬浮窗专属命令不许主窗口（反之亦然）。
    #[test]
    fn command_window_whitelist_matches_frontend_call_sites() {
        // 写命令：仅主窗口
        for label in ["main"] {
            assert!(label_allowed(label, MAIN));
        }
        assert!(!label_allowed("widget", MAIN) && !label_allowed("tray-menu", MAIN));
        // 托盘菜单动作：main 与 tray-menu（menu.ts）共用，widget/glow 拒绝
        assert!(label_allowed("main", MAIN_OR_TRAY) && label_allowed("tray-menu", MAIN_OR_TRAY));
        assert!(!label_allowed("widget", MAIN_OR_TRAY) && !label_allowed("glow-0", MAIN_OR_TRAY));
        // 悬浮窗专属：仅 widget（widget.ts 右键菜单）
        assert!(label_allowed("widget", WIDGET_ONLY));
        assert!(!label_allowed("main", WIDGET_ONLY) && !label_allowed("tray-menu", WIDGET_ONLY));
    }

    /// §2.16 跨模块契约：test_channel 的 url/template 语义
    /// （Some 用传入值构造本次配置；url 空串视同 None；template None 回退落盘→默认）
    #[test]
    fn test_target_uses_form_input_and_falls_back_to_saved() {
        let empty = ChannelsConfig::default();
        let saved = ChannelsConfig {
            bark: Some(BarkChannelConfig { enabled: true, url: "https://old/bark".into() }),
            webhook: Some(WebhookChannelConfig {
                enabled: true,
                url: "https://old/hook".into(),
                method: "PUT".into(),
                template: r#"{"t": "{title}"}"#.into(),
            }),
        };

        // url Some（非空）= 测表单里未保存的新地址
        let TestTarget::Bark(b) = resolve_test_target("bark", &saved, Some("https://new/bark".into()), None).unwrap() else {
            panic!("bark 渠道");
        };
        assert_eq!(b.url, "https://new/bark");
        // url 空串视同 None → 回退落盘配置
        let TestTarget::Bark(b) = resolve_test_target("bark", &saved, Some("  ".into()), None).unwrap() else {
            panic!("bark 渠道");
        };
        assert_eq!(b.url, "https://old/bark");
        // 全 None = 用已落盘配置（旧行为）
        let TestTarget::Bark(b) = resolve_test_target("bark", &saved, None, None).unwrap() else {
            panic!("bark 渠道");
        };
        assert_eq!(b.url, "https://old/bark");
        // 都没配过 → 明确报「未配置」
        assert!(resolve_test_target("bark", &empty, None, None).is_err());

        // template Some 用传入值；None 回退落盘 template；落盘为空再回退默认模板
        let TestTarget::Webhook(w) =
            resolve_test_target("webhook", &saved, Some("https://new/hook".into()), Some(r#"{"x": 1}"#.into())).unwrap()
        else {
            panic!("webhook 渠道");
        };
        assert_eq!(w.url, "https://new/hook");
        assert_eq!(w.template, r#"{"x": 1}"#);
        assert_eq!(w.method, "PUT", "契约不覆盖 method：沿用落盘值");
        let TestTarget::Webhook(w) = resolve_test_target("webhook", &saved, None, None).unwrap() else {
            panic!("webhook 渠道");
        };
        assert_eq!(w.template, r#"{"t": "{title}"}"#);
        let cleared = ChannelsConfig {
            webhook: Some(WebhookChannelConfig {
                url: "https://old/hook".into(),
                template: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let TestTarget::Webhook(w) = resolve_test_target("webhook", &cleared, None, None).unwrap() else {
            panic!("webhook 渠道");
        };
        assert_eq!(w.template, WebhookChannelConfig::default().template, "落盘 template 为空 → 默认模板");
        assert!(resolve_test_target("nope", &saved, None, None).is_err(), "未知渠道报错");
    }

    /// §2.1 回归：整份保存必须保留悬浮窗几何记忆（窗口侧实时上报的 x/y/pinned 为准），
    /// 否则用户保存设置前拖过悬浮窗会被旧快照回退
    #[test]
    fn whole_config_save_keeps_live_widget_geometry() {
        let mut current = BarkConfig::default();
        current.widget.x = -1920.0;
        current.widget.y = 60.0;
        current.widget.pinned = true;
        let mut incoming = BarkConfig::default();
        incoming.widget.x = 10.0;
        incoming.widget.y = 20.0;
        incoming.widget.pinned = false;
        incoming.widget.enabled = false; // 开关仍以页面为准
        preserve_widget_geometry(&mut incoming, &current);
        assert_eq!(incoming.widget.x, -1920.0, "刚拖到副屏的位置不得被旧快照覆盖");
        assert_eq!(incoming.widget.y, 60.0);
        assert!(incoming.widget.pinned);
        assert!(!incoming.widget.enabled, "开关不受几何保留逻辑影响");
    }
}
