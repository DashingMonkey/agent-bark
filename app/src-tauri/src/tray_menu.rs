//! 托盘菜单——点击托盘图标弹出的自绘菜单（与悬浮窗右键菜单同一套面板样式）。
//!
//! 原生托盘菜单（tauri::menu）是系统绘制的，无法换肤；这里用一个小 WebView
//! 窗口渲染 menu.html，面板样式直接搬 widget.html 的 `#ctx-menu`（深浅主题
//! 跟随系统）。菜单项与旧原生菜单一致：显示主窗口 / 重置流光 / 静音 / 退出，
//! 动作经 commands.rs 的命令回传（show_main_panel / reset_glow / toggle_mute / quit_app）。
//!
//! 生命周期：**懒创建 + 常驻**。第一次点托盘才建窗（同 glow 的懒创建思路，
//! 不白吃一个 WebView），之后一直隐藏复用——每次点击只做落位 + 显示 + 聚焦，
//! 不再有建窗延迟。静音勾选态在建窗时经 initialization_script 注入兜底，
//! 之后每次打开 emit 最新值（菜单里切换后本地也会即时更新）。
//!
//! 关闭口径（三层兜底）：
//! - **失焦即收**（主口径）：Rust 侧 `WindowEvent::Focused(false)` → hide（lib.rs），
//!   点窗口外任何地方、切到别的应用都走这里，与原生菜单「点外面就消失」同款；
//!   刚 show 完的 FOCUS_GRACE 宽限期内例外——建窗首帧的虚假失焦不收（见常量注释）；
//! - 菜单项点完自收、Esc 收起（menu.ts 调 `win.hide()`）；
//! - 光标离开窗口也收（menu.ts mouseleave）：万一聚焦失败（Windows 前台锁），
//!   失焦事件不会来，靠它保证菜单不会悬空关不掉。它同样要求光标真实进过窗口
//!   且不在刚打开的宽限期内，载入时合成的边界事件不收。
//!
//! 窗口操作全部主线程派发（同 glow/widget：WebviewWindowBuilder 非线程安全）。

use crate::state::AppState;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

/// 窗口 label（capabilities/tray-menu.json 按它授权）
pub const LABEL: &str = "tray-menu";

/// show 后视为「焦点尚未稳定」的宽限期。懒建窗的那次打开里，WebView2 初始化 +
/// menu.html 首导航会搬运焦点、抖出一对 GotFocus/LostFocus，而 Windows 单 webview
/// 模式下 Tauri 的 Focused 事件正是由它们合成的——刚 show 完就收到虚假失焦，
/// 被 lib.rs 的「失焦即收」当成用户点了窗口外，菜单刚弹出来就被收掉，表现即
/// 「第一次右键托盘没反应，第二次才正常」。宽限期内的失焦一律忽略。
pub const FOCUS_GRACE: Duration = Duration::from_millis(1500);

/// 最近一次 show 的时刻（epoch 毫秒；0 = 从未打开）。
///
/// 用 `AtomicU64` 而不是 `Mutex<Option<Instant>>`：这里只需要记一个时间点，
/// 原子量免掉锁中毒分支——原先那两处 `Mutex::lock().unwrap()` 是全仓库唯二
/// 不做锁中毒恢复的锁（§4.21），线程一 panic 就永久卡死「失焦即收」的豁免判定。
static LAST_OPEN: AtomicU64 = AtomicU64::new(0);

/// 每次 show 后记录时刻（open 的主线程闭包里调用）
fn note_opened() {
    LAST_OPEN.store(bark_core::now_millis().max(0) as u64, Ordering::Relaxed);
}

/// 菜单是否刚在宽限期内打开过（lib.rs 失焦即收的豁免判定）
pub fn opened_recently() -> bool {
    let last = LAST_OPEN.load(Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    within_focus_grace(bark_core::now_millis().max(0) as u64, last)
}

/// 「刚打开」判定（纯函数，便于测试）：距上次 show 不足 [`FOCUS_GRACE`]。
/// 时间倒流（NTP 回拨）按「刚打开」保守处理（saturating 差值为 0）——宁可宽限多
/// 一会儿，也不把刚弹出的菜单误收掉。
fn within_focus_grace(now_ms: u64, last_ms: u64) -> bool {
    now_ms.saturating_sub(last_ms) < FOCUS_GRACE.as_millis() as u64
}

/// 静音态推送事件名（menu.ts 监听）
const STATE_EVENT: &str = "tray-menu://state";

/// 菜单窗口逻辑尺寸。menu.html 的面板铺满窗口，两边必须严格一致：
/// 高 = 4 项 × 28px + 面板上下 padding 4px×2；宽容纳最长项「静音（暂停通知）」。
const MENU_W: f64 = 176.0;
const MENU_H: f64 = 120.0;
/// 菜单与托盘图标的间距、与屏幕边缘的最小留白（逻辑像素）
const GAP: f64 = 6.0;
const EDGE: f64 = 4.0;

/// 建窗时注入的初始参数（建窗首帧兜底，之后走 STATE_EVENT）
#[derive(Serialize)]
struct MenuInit {
    muted: bool,
}

/// 在托盘图标旁弹出菜单。
///
/// `cursor` 是点击位置、`icon_rect` 是托盘图标矩形（托盘事件给的都是物理像素）。
/// 菜单贴图标上沿弹出（Windows 惯例），任务栏在顶部时翻到图标下方；
/// 横向以图标为中心，整体夹回所在屏幕。若菜单已开着，重新落位并聚焦
/// （点托盘想收起请点窗口外任意处 / Esc——点击托盘会让菜单失焦后又被
/// 这里的 show 拉起，行为上等价于「保持打开」）。
pub fn open(app: &AppHandle, cursor: PhysicalPosition<f64>, icon_rect: tauri::Rect) {
    let app = app.clone();
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let win = match ensure_window(&app) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("托盘菜单窗口创建失败: {e}");
                return;
            }
        };
        let Some(m) = monitor_at(&app, cursor) else {
            return;
        };
        // scale_factor 防御（§2.18）：0/非有限值当 1.0，对齐 glow.rs 的 scale_or_one——
        // 换算除数异常时坐标会变成 NaN/Inf，菜单直接落进打不开的位置
        let s = crate::glow::scale_or_one(m.scale_factor());
        // 图标矩形：物理 → 逻辑（position/size 可能是物理或逻辑变体，分别折算）
        let ipos = match icon_rect.position {
            tauri::Position::Physical(p) => (p.x as f64, p.y as f64),
            tauri::Position::Logical(p) => (p.x, p.y),
        };
        let isize_ = match icon_rect.size {
            tauri::Size::Physical(sz) => (sz.width as f64, sz.height as f64),
            tauri::Size::Logical(sz) => (sz.width, sz.height),
        };
        let (ix, iy) = (ipos.0 / s, ipos.1 / s);
        let (iw, ih) = (isize_.0 / s, isize_.1 / s);
        // 所在屏幕的逻辑矩形
        let (mon_x, mon_y) = (m.position().x as f64 / s, m.position().y as f64 / s);
        let (mon_w, mon_h) = (m.size().width as f64 / s, m.size().height as f64 / s);

        let mut x = ix + iw / 2.0 - MENU_W / 2.0;
        let mut y = iy - MENU_H - GAP;
        if y < mon_y {
            y = iy + ih + GAP;
        }
        // 夹回屏幕（多屏 / 竖屏 / 任务栏贴边时不至于整块出屏）
        x = x.clamp(mon_x + EDGE, (mon_x + mon_w - MENU_W - EDGE).max(mon_x + EDGE));
        y = y.clamp(mon_y + EDGE, (mon_y + mon_h - MENU_H - EDGE).max(mon_y + EDGE));

        let _ = win.set_position(tauri::LogicalPosition::new(x, y));
        let _ = win.show();
        note_opened();
        let _ = win.set_focus();
        let muted = app.state::<Arc<AppState>>().is_muted();
        let _ = app.emit_to(LABEL, STATE_EVENT, muted);
    }) {
        tracing::warn!("托盘菜单弹出失败（主线程不可用？）: {e}");
    }
}

/// 取已有菜单窗，没有就建（隐藏）。仅主线程调用。
fn ensure_window(app: &AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    if let Some(w) = app.get_webview_window(LABEL) {
        return Ok(w);
    }
    let muted = app.state::<Arc<AppState>>().is_muted();
    let init = serde_json::to_string(&MenuInit { muted }).unwrap_or_else(|_| "null".to_string());
    // 可聚焦：失焦即收靠它（glow/widget 不可聚焦是为了不抢焦点，菜单正相反）
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("menu.html".into()))
        .title("agent-bark 托盘菜单")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .minimizable(false)
        .maximizable(false)
        .closable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .visible(false)
        .disable_drag_drop_handler()
        .inner_size(MENU_W, MENU_H)
        .initialization_script(format!("window.__MENU_INIT__={init};"))
        .build()
}

/// 包含物理坐标点的显示器；找不到回落主显示器（与 widget.rs 的口径互补：
/// 那边在逻辑空间判定，托盘事件给的是物理坐标，这里直接按物理矩形包含判定）。
fn monitor_at(app: &AppHandle, cursor: PhysicalPosition<f64>) -> Option<tauri::Monitor> {
    let mons = app.available_monitors().ok()?;
    let hit = mons.iter().find(|m| {
        let (mx, my) = (m.position().x as f64, m.position().y as f64);
        let (mw, mh) = (m.size().width as f64, m.size().height as f64);
        cursor.x >= mx && cursor.x < mx + mw && cursor.y >= my && cursor.y < my + mh
    });
    match hit {
        Some(m) => Some(m.clone()),
        None => app.primary_monitor().ok().flatten(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 宽限期豁免（纯逻辑）：刚 show 的 1.5s 内失焦不收（防懒建窗首帧虚假失焦
    /// 把刚弹出的菜单收掉）；到点后豁免失效。未打开过（last=0）不豁免。
    #[test]
    fn focus_grace_expires_after_open() {
        assert!(!within_focus_grace(10_000, 0), "从未打开不豁免");
        assert!(within_focus_grace(10_000, 10_000), "刚打开即豁免");
        assert!(within_focus_grace(10_000 + 1_499, 10_000), "宽限期内豁免");
        assert!(!within_focus_grace(10_000 + 1_500, 10_000), "到点后失焦照常收");
        assert!(within_focus_grace(9_000, 10_000), "时间倒流按「刚打开」保守豁免");
    }
}
