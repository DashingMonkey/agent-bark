//! 桌面悬浮窗——当前活动会话的常驻小卡片（左图标 / 右状态，可拖动、可固定位置）。
//!
//! 与 glow 同一套窗口套路：透明、无边框、置顶、不进任务栏、**不可聚焦**
//! （绝不抢用户编辑器的焦点，但鼠标事件正常接收——这是与 glow 唯一的区别，
//! glow 是彻底点击穿透）。页面本体是 `app/widget.html` + `src/widget.ts`，
//! 右键菜单 / 位置上报都在前端完成；Rust 侧负责：
//! - 发起原生拖动并等待 Windows 左键释放，回报最终位移；
//! - 按配置建窗 / 销毁（`sync`，配置开关翻转与启动时调用）；
//! - 按记住的位置落位，初始参数经 initialization_script 注入
//!   （窗口建好时 emit 的事件会丢，和 glow 同理）。
//!
//! 位置记忆的口径：`config.widget.{x,y}` 存卡片左上角（逻辑像素），
//! `pinned` 存「固定位置」（固定后前端不响应拖动，右键菜单里切换）。
//! `dock_side` 记住靠边隐藏方向；x/y 始终是展开位置，窄边几何由前端计算。
//!
//! 自由悬浮时常驻；停靠恢复时先隐藏窗口，等前端布置好窄边再显示。
//! 托盘单击通过可领取的请求临时展开，前端负责五秒计时和悬停保持。

use crate::state::{read, AppState};
use bark_core::{WidgetConfig, WidgetDockSide};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// 悬浮窗窗口 label（capabilities/widget.json 按它授权）
pub const WIDGET_LABEL: &str = "widget";

/// 卡片宽度（逻辑像素，前端渲染同宽，两边必须一致）
pub const CARD_W: f64 = 280.0;
/// 空闲态（无进行中会话）高度（逻辑像素，首帧兜底；内容变化后由前端 set_size 跟随）
pub const IDLE_H: f64 = 44.0;

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

/// Tauri start_dragging 只投递开始移动的消息，并不等待 Windows 的移动循环退出。
/// 在后台等待物理左键释放，再读取最终位置；WebView2 是否收到 mouseup 不影响收尾。
pub async fn drag(window: tauri::WebviewWindow) -> Result<bool, String> {
    let before = window.outer_position().map_err(|e| e.to_string())?;
    window.start_dragging().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        while unsafe { GetAsyncKeyState(0x01) } < 0 {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
        // 松手消息先交给原生窗口处理，再取位置；同时让最后一批 moved 事件到达前端。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let after = window.outer_position().map_err(|e| e.to_string())?;
    Ok(before != after)
}

/// 建窗时注入的初始参数（见模块注释的「位置记忆口径」）
#[derive(Serialize)]
struct WidgetInit {
    x: f64,
    y: f64,
    width: f64,
    pinned: bool,
    dock_side: Option<WidgetDockSide>,
}

fn request_reveal(state: &AppState) -> bool {
    if !read(&state.config).widget.enabled { return false; }
    state.widget_reveal_pending.store(true, Ordering::SeqCst);
    true
}

pub fn take_reveal(state: &AppState) -> bool {
    let pending = state.widget_reveal_pending.swap(false, Ordering::SeqCst);
    pending && read(&state.config).widget.enabled
}

/// 只找回已启用的悬浮窗；事件负责即时送达，pending 负责覆盖前端尚未就绪的时段。
pub fn reveal(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    if !request_reveal(&state) { return; }
    sync(app, &state);
    if let Err(error) = app.emit_to(WIDGET_LABEL, "bark://widget-reveal", ()) {
        tracing::warn!("发送悬浮窗展开请求失败: {error}");
    }
}

/// 对齐窗口与配置（配置开关翻转 / 保存配置后调用）：
/// 关闭 → 销毁；开启 → 不在就按记住的位置建窗。
pub fn sync(app: &AppHandle, state: &Arc<AppState>) {
    let app = app.clone();
    let state = state.clone();
    // 建窗/销毁必须主线程（WebviewWindowBuilder 非线程安全，同 glow）
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let cfg = read(&state.config).widget.clone();
        let exists = app.get_webview_window(WIDGET_LABEL).is_some();
        if !cfg.enabled {
            state.widget_reveal_pending.store(false, Ordering::SeqCst);
            if exists {
                if let Some(w) = app.get_webview_window(WIDGET_LABEL) {
                    let _ = w.destroy();
                }
            }
            return;
        }
        if !exists {
            if let Err(e) = create(&app, &cfg) {
                tracing::warn!("悬浮窗创建失败: {e}");
            }
        }
    }) {
        tracing::warn!("悬浮窗同步失败（主线程不可用？）: {e}");
    }
}

/// 从未放置过（x/y = -1，文档哨兵）时的默认位置：主显示器右上角
fn default_position(app: &AppHandle) -> (f64, f64) {
    if let Ok(Some(mon)) = app.primary_monitor() {
        // scale_factor 防御（§2.18）：0/非有限值当 1.0，对齐 glow.rs 的 scale_or_one
        let s = crate::glow::scale_or_one(mon.scale_factor());
        let (mx, my) = (mon.position().x as f64, mon.position().y as f64);
        let (mw, _mh) = (mon.size().width as f64, mon.size().height as f64);
        return (mx / s + mw / s - CARD_W - 16.0, my / s + 60.0);
    }
    (60.0, 60.0)
}

/// 包含逻辑坐标点 (x,y) 的显示器的逻辑矩形 (x, y, w, h)。找不到回落主显示器。
///
/// Tauri 的 Monitor 全是物理像素，按各屏 scale_factor 折算成逻辑桌面空间后
/// 再做包含判定——配置里存的 x/y 是逻辑坐标，两边必须同口径。
/// scale_factor 统一经 [`crate::glow::scale_or_one`] 防御（§2.18）。
fn monitor_containing(app: &AppHandle, x: f64, y: f64, preferred: Option<&str>) -> (f64, f64, f64, f64, f64) {
    if let Ok(mons) = app.available_monitors() {
        // 混合缩放时各屏的逻辑坐标范围可能重叠，优先用保存的显示器名称。
        let selected = preferred.and_then(|name| mons.iter().find(|m| m.name().map(String::as_str) == Some(name)))
            .or_else(|| mons.iter().find(|m| {
                let s = crate::glow::scale_or_one(m.scale_factor());
                let (lx, ly) = (m.position().x as f64 / s, m.position().y as f64 / s);
                let (lw, lh) = (m.size().width as f64 / s, m.size().height as f64 / s);
                x >= lx && x < lx + lw && y >= ly && y < ly + lh
            }));
        if let Some(m) = selected {
            let s = crate::glow::scale_or_one(m.scale_factor());
            let (lx, ly) = (m.position().x as f64 / s, m.position().y as f64 / s);
            let (lw, lh) = (m.size().width as f64 / s, m.size().height as f64 / s);
            return (lx, ly, lw, lh, s);
        }
    }
    match app.primary_monitor() {
        Ok(Some(m)) => {
            let s = crate::glow::scale_or_one(m.scale_factor());
            (
                m.position().x as f64 / s,
                m.position().y as f64 / s,
                m.size().width as f64 / s,
                m.size().height as f64 / s,
                s,
            )
        }
        _ => (0.0, 0.0, 1920.0, 1080.0, 1.0),
    }
}

/// 「记住的位置」判定：哨兵是**精确的 -1.0**（文档明说，见 WidgetConfig::x）。
///
/// 旧实现写成 `x >= 0.0 && y >= 0.0`，把负坐标当「从未放置」——副屏在主屏
/// 左侧/上方时逻辑坐标就是负的（x=-1920 等），拖过去落盘后重启跳回主屏右上角（§1.5）。
/// 负坐标副屏是合法位置，只有 (-1, -1) 这一对哨兵才表示「从未放置」。
fn has_saved_position(cfg: &WidgetConfig) -> bool {
    cfg.x != -1.0 || cfg.y != -1.0
}

fn create(app: &AppHandle, cfg: &WidgetConfig) -> tauri::Result<()> {
    // 记住的位置；没放置过则用主屏右上角
    let (px, py) = if has_saved_position(cfg) { (cfg.x, cfg.y) } else { default_position(app) };
    let preferred = cfg.dock_monitor.as_deref().filter(|_| cfg.dock_side.is_some() && !cfg.pinned);
    let (mx, my, mw, mh, scale) = monitor_containing(app, px, py, preferred);
    // 位置夹回所在屏（屏分辨率变了不至于整卡落在屏外）
    let x = px.clamp(mx, (mx + mw - CARD_W).max(mx));
    let y = py.clamp(my, (my + mh - IDLE_H).max(my));
    let init = serde_json::to_string(&WidgetInit {
        x,
        y,
        width: CARD_W,
        pinned: cfg.pinned,
        dock_side: if cfg.pinned { None } else { cfg.dock_side },
    })
    .unwrap_or_else(|_| "null".to_string());

    let window = WebviewWindowBuilder::new(app, WIDGET_LABEL, WebviewUrl::App("widget.html".into()))
        .title("agent-bark 悬浮窗")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .closable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        // 不可聚焦：悬停/点击都不把焦点从用户的编辑器里抢走（鼠标事件照常）
        .focusable(false)
        // 记住的停靠状态也先隐藏，等前端布置好窄边再显示，避免整卡闪现。
        .visible(cfg.pinned || cfg.dock_side.is_none())
        .disable_drag_drop_handler()
        .position(x, y)
        .inner_size(CARD_W, IDLE_H)
        .initialization_script(format!("window.__WIDGET_INIT__={init};"))
        .build()?;
    if cfg.dock_side.is_some() && !cfg.pinned {
        window.set_position(tauri::PhysicalPosition::new((x * scale).round() as i32, (y * scale).round() as i32))?;
    }
    tracing::info!("悬浮窗已创建（x={x}, y={y}, pinned={}, dock_side={:?}）", cfg.pinned, cfg.dock_side);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_survives_frontend_loading_and_is_consumed_once() {
        let state = AppState::new(bark_core::BarkConfig::default(), "config.json".into());
        assert!(!take_reveal(&state));
        assert!(request_reveal(&state));
        assert!(take_reveal(&state));
        assert!(!take_reveal(&state));
        assert!(request_reveal(&state));
        crate::state::write(&state.config).widget.enabled = false;
        assert!(!take_reveal(&state));
        assert!(!request_reveal(&state));
        crate::state::write(&state.config).widget.enabled = true;
        assert!(!take_reveal(&state), "disabled clicks must not carry over to the next enable");
    }

    fn placed(x: f64, y: f64) -> bool {
        let mut cfg = WidgetConfig::default(); // 默认即哨兵 (-1, -1)
        cfg.x = x;
        cfg.y = y;
        has_saved_position(&cfg)
    }

    /// §1.5 回归：负坐标是副屏（主屏左侧/上方）的合法位置，不能被当「从未放置」——
    /// 否则拖到副屏落盘后重启跳回主屏右上角
    #[test]
    fn negative_coordinates_are_a_remembered_position() {
        assert!(!placed(-1.0, -1.0), "哨兵 (-1,-1) = 从未放置");
        assert!(placed(-1920.0, 60.0), "副屏在主屏左侧：x 为负是合法位置");
        assert!(placed(60.0, -1080.0), "副屏在主屏上方：y 为负是合法位置");
        assert!(placed(-1920.0, -1080.0));
        assert!(placed(0.0, 0.0));
        assert!(placed(100.0, 100.0));
        assert!(placed(-1.0, 2.0), "哨兵是 (-1,-1) 整对，单边 -1 不算「从未放置」");
    }
}
