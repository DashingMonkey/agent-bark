//! 桌面悬浮窗——当前活动会话的常驻小卡片（左图标 / 右状态，可拖动、可固定位置）。
//!
//! 与 glow 同一套窗口套路：透明、无边框、置顶、不进任务栏、**不可聚焦**
//! （绝不抢用户编辑器的焦点，但鼠标事件正常接收——这是与 glow 唯一的区别，
//! glow 是彻底点击穿透）。页面本体是 `app/widget.html` + `src/widget.ts`，
//! 拖动 / 右键菜单 / 位置上报都在前端完成；Rust 侧只负责：
//! - 按配置建窗 / 销毁（`sync`，配置开关翻转与启动时调用）；
//! - 按记住的位置落位，初始参数经 initialization_script 注入
//!   （窗口建好时 emit 的事件会丢，和 glow 同理）。
//!
//! 位置记忆的口径：`config.widget.{x,y}` 存卡片左上角（逻辑像素），
//! `pinned` 存「固定位置」（固定后前端不响应拖动，右键菜单里切换）。
//!
//! 自动隐藏（`widget.auto_hide`）：隐藏/弹出是**前端可见性状态机**的事
//! （widget.ts 的 `evaluateVisibility`，安静 2 秒后 hide、有关注行立即 show），
//! Rust 侧只做两件事：auto_hide 开着时**建窗即隐藏**（登录不闪一下卡片），
//! 以及配置热更新时把 `bark://widget-config` 推给已存在的窗口（不重建）。

use crate::state::{read, AppState};
use bark_core::WidgetConfig;
use serde::Serialize;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// 悬浮窗窗口 label（capabilities/widget.json 按它授权）
pub const WIDGET_LABEL: &str = "widget";

/// 卡片宽度（逻辑像素，前端渲染同宽，两边必须一致）
pub const CARD_W: f64 = 240.0;
/// 空闲态（无进行中会话）高度（逻辑像素，首帧兜底；内容变化后由前端 set_size 跟随）
pub const IDLE_H: f64 = 38.0;

/// 建窗时注入的初始参数（见模块注释的「位置记忆口径」）
#[derive(Serialize)]
struct WidgetInit {
    x: f64,
    y: f64,
    width: f64,
    pinned: bool,
    auto_hide: bool,
}

/// 对齐窗口与配置（配置开关翻转 / 保存配置后调用）：
/// 关闭 → 销毁；开启 → 不在就按记住的位置建窗，在就把配置变化推给它。
pub fn sync(app: &AppHandle, state: &Arc<AppState>) {
    let enabled = read(&state.config).widget.enabled;
    let app = app.clone();
    let state = state.clone();
    // 建窗/销毁必须主线程（WebviewWindowBuilder 非线程安全，同 glow）
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let exists = app.get_webview_window(WIDGET_LABEL).is_some();
        if !enabled {
            if exists {
                if let Some(w) = app.get_webview_window(WIDGET_LABEL) {
                    let _ = w.destroy();
                }
            }
            return;
        }
        if !exists {
            let cfg = read(&state.config).widget.clone();
            if let Err(e) = create(&app, &cfg) {
                tracing::warn!("悬浮窗创建失败: {e}");
            }
            return;
        }
        // 窗口已在：只推配置变化（目前是 auto_hide 热更新），不重建窗口——
        // 重建会闪一下、还会丢右键菜单等前端瞬时状态
        let auto_hide = read(&state.config).widget.auto_hide;
        let _ = app.emit_to(
            WIDGET_LABEL,
            "bark://widget-config",
            serde_json::json!({ "auto_hide": auto_hide }),
        );
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
fn monitor_containing(app: &AppHandle, x: f64, y: f64) -> (f64, f64, f64, f64) {
    if let Ok(mons) = app.available_monitors() {
        for m in mons {
            let s = crate::glow::scale_or_one(m.scale_factor());
            let (lx, ly) = (m.position().x as f64 / s, m.position().y as f64 / s);
            let (lw, lh) = (m.size().width as f64 / s, m.size().height as f64 / s);
            if x >= lx && x < lx + lw && y >= ly && y < ly + lh {
                return (lx, ly, lw, lh);
            }
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
            )
        }
        _ => (0.0, 0.0, 1920.0, 1080.0),
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
    let (mx, my, mw, mh) = monitor_containing(app, px, py);
    // 位置夹回所在屏（屏分辨率变了不至于整卡落在屏外）
    let x = px.clamp(mx, (mx + mw - CARD_W).max(mx));
    let y = py.clamp(my, (my + mh - IDLE_H).max(my));
    let init = serde_json::to_string(&WidgetInit {
        x,
        y,
        width: CARD_W,
        pinned: cfg.pinned,
        auto_hide: cfg.auto_hide,
    })
    .unwrap_or_else(|_| "null".to_string());

    WebviewWindowBuilder::new(app, WIDGET_LABEL, WebviewUrl::App("widget.html".into()))
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
        // 自动隐藏开着就建窗即隐藏：安静时登录不该闪一下卡片，
        // 有需要关注的事件时由前端状态机（widget.ts）show 出来
        .visible(!cfg.auto_hide)
        .disable_drag_drop_handler()
        .position(x, y)
        .inner_size(CARD_W, IDLE_H)
        .initialization_script(format!("window.__WIDGET_INIT__={init};"))
        .build()?;
    tracing::info!("悬浮窗已创建（x={x}, y={y}, pinned={}, auto_hide={}）", cfg.pinned, cfg.auto_hide);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
