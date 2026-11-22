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

/// 行悬停详情面板的独立窗口 label（capabilities/tip.json 按它授权）。
/// 与卡片几何解耦：详情撑大/挪动只发生在自己的窗口里，悬浮窗本体永不移动。
pub const TIP_LABEL: &str = "widget-tip";

/// 卡片宽度（逻辑像素，前端渲染同宽，两边必须一致）
pub const CARD_W: f64 = 280.0;
/// 空闲态（无进行中会话）高度（逻辑像素，首帧兜底；内容变化后由前端 set_size 跟随）
pub const IDLE_H: f64 = 44.0;

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

// macOS：悬浮窗转 NSPanel（Sonoma+ 的「点壁纸显示桌面」/调度中心「显示桌面」
// 只推走普通应用窗口，utility/auxiliary 面板存活）。面板不成为 key（focusable
// false 的延续——点击/悬停都不激活应用、不抢焦点）。
#[cfg(target_os = "macos")]
tauri_nspanel::tauri_panel! {
    panel!(WidgetPanel {
        config: {
            can_become_key_window: false,
            can_become_main_window: false,
            is_floating_panel: true
        }
    })
}

/// 详情面板同样转 NSPanel：不抢焦点、与悬浮窗一致的「显示桌面」存活语义。
/// 单独子模块：tauri_panel! 宏会在作用域内生成一批 use 导入，与 WidgetPanel
/// 同模块展开会重复定义（E0252），隔离开各自干净。
#[cfg(target_os = "macos")]
mod tip_panel {
    tauri_nspanel::tauri_panel! {
        panel!(TipPanel {
            config: {
                can_become_key_window: false,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }
}

/// macOS 手动跟手拖动：不进入系统拖动会话，轮询全局光标 + 主线程
/// `setFrameOrigin` 移动窗口。原因：系统拖动会话自带「拖到菜单栏进全屏/平铺」
/// 手势——那是全局系统行为（桌面与 Dock 的「拖窗口到菜单栏以填充屏幕」），
/// 没有按 App 关闭的公开 API，FullScreenNone 与私有闸门都只能挡住部分路径；
/// 没有会话就没有手势，constrainFrameRect 钳制与 currentEvent 重建等 hack
/// 也一并消失（tauri start_dragging 的「窗口不跟手」问题根源在会话本身）。
#[cfg(target_os = "macos")]
pub mod macos {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSPoint, NSRect};

    /// 全局光标位置（点单位，左上原点）。CoreGraphics 直读——tao 的
    /// cursorPosition 在 Retina 上 y 值错一整屏，不能复用；CGEventCreate(nil)
    /// 合成的事件自带当前光标状态，是 macOS 上读全局光标的惯用法。
    pub mod cursor {
        #[repr(C)]
        pub struct CGPoint { pub x: f64, pub y: f64 }

        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
            fn CGEventGetLocation(event: *const std::ffi::c_void) -> CGPoint;
        }
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            fn CFRelease(cf: *mut std::ffi::c_void);
        }

        pub fn location() -> CGPoint {
            unsafe {
                let event = CGEventCreate(std::ptr::null());
                let point = CGEventGetLocation(event);
                if !event.is_null() { CFRelease(event); }
                point
            }
        }
    }

    /// 跟手会话：起始 Cocoa 原点与 CG 光标、桌面联合边界、窗口尺寸。
    /// 窗口指针以 usize 存放（跨线程持有所有权，解引用只在主线程）。
    struct Session {
        window: usize,
        origin0: (f64, f64),
        cg0: (f64, f64),
        last: (f64, f64),
        desktop: (f64, f64, f64, f64),
        size: (f64, f64),
    }
    static SESSION: std::sync::Mutex<Option<Session>> = std::sync::Mutex::new(None);
    fn lock() -> std::sync::MutexGuard<'static, Option<Session>> {
        SESSION.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 主线程调用：记录起始原点/光标与桌面联合边界（所有 NSScreen frame 的并集）。
    pub unsafe fn begin(ns_window: usize) {
        let win = ns_window as *mut AnyObject;
        let frame: NSRect = msg_send![win, frame];
        let cg0 = cursor::location();
        let screens: *mut AnyObject = msg_send![objc2::class!(NSScreen), screens];
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        if !screens.is_null() {
            let count: usize = msg_send![screens, count];
            for i in 0..count {
                let s: *mut AnyObject = msg_send![screens, objectAtIndex: i];
                let f: NSRect = msg_send![s, frame];
                min_x = min_x.min(f.origin.x);
                min_y = min_y.min(f.origin.y);
                max_x = max_x.max(f.origin.x + f.size.width);
                max_y = max_y.max(f.origin.y + f.size.height);
            }
        }
        if min_x > max_x {
            (min_x, min_y, max_x, max_y) = (0.0, 0.0, 1920.0, 1080.0);
        }
        *lock() = Some(Session {
            window: ns_window,
            origin0: (frame.origin.x, frame.origin.y),
            cg0: (cg0.x, cg0.y),
            last: (f64::NAN, f64::NAN),
            desktop: (min_x, min_y, max_x - min_x, max_y - min_y),
            size: (frame.size.width, frame.size.height),
        });
    }

    /// 跟手一帧：CG 光标增量换算 Cocoa 原点增量（两空间仅 y 方向相反），主线程
    /// setFrameOrigin。每轴保留 6px 在桌面联合边界内，防把卡片整个拖丢。
    pub fn follow(app: &tauri::AppHandle) {
        let Some(state) = lock().as_ref().map(|s| (s.window, s.origin0, s.cg0, s.last, s.desktop, s.size)) else {
            return;
        };
        let (window, origin0, cg0, last, desktop, size) = state;
        let cg = cursor::location();
        let mut ox = origin0.0 + (cg.x - cg0.0);
        let mut oy = origin0.1 - (cg.y - cg0.1);
        const MIN_VISIBLE: f64 = 6.0;
        ox = ox.clamp(
            desktop.0 - (size.0 - MIN_VISIBLE),
            desktop.0 + desktop.2 - MIN_VISIBLE,
        );
        oy = oy.clamp(
            desktop.1 - (size.1 - MIN_VISIBLE),
            desktop.1 + desktop.3 - MIN_VISIBLE,
        );
        if ox == last.0 && oy == last.1 {
            return;
        }
        if let Some(s) = lock().as_mut() {
            s.last = (ox, oy);
        }
        if let Err(e) = app.run_on_main_thread(move || unsafe {
            let win = window as *mut AnyObject;
            let origin = NSPoint { x: ox, y: oy };
            let _: () = msg_send![win, setFrameOrigin: origin];
        }) {
            tracing::warn!("悬浮窗跟手挪窗失败（主线程不可用？）: {e}");
        }
    }

    /// 结束跟手会话（松手后调用）。
    pub fn end() {
        *lock() = None;
    }

    /// 左键是否按着（NSEvent.pressedMouseButtons 的 bit0）。
    /// 类方法读事件状态，命令线程轮询不影响主线程的模态拖动循环。
    pub unsafe fn left_mouse_down() -> bool {
        let buttons: u64 = msg_send![objc2::class!(NSEvent), pressedMouseButtons];
        buttons & 1 != 0
    }
}

/// macOS 悬浮窗 hover 修复。
///
/// AppKit 默认只把 mousemove 投递给 key window（`acceptsMouseMovedEvents` 默认
/// NO，tao 建窗时已打开），但 WKWebView 对**非激活窗口**仍抑制 hover 类事件
/// （mouseenter/mouseover 的 DOM 派发跟 WebKit 的 active 状态走）——悬浮窗
/// focusable=false 永远不激活，贴边收起后移入触发条、悬停行上看详情，前端
/// 收不到任何事件。Windows 的 WebView2 非激活窗口照样派发，所以原作者没遇到。
///
/// 这里绕过 WebKit，在 AppKit 层拿 hover 再转发给前端：
/// - [`install_hover_tracking`]：NSTrackingArea（`ActiveAlways`）给窗口级的
///   进入/离开，负责「移入展开、移出收回」；
/// - [`install_mouse_moved_monitor`]：NSEvent 本地监视器把鼠标坐标流转发给
///   前端，前端 `elementFromPoint` 找出行元素，负责「悬停行看详情」。
#[cfg(target_os = "macos")]
pub mod hover {
    use objc2::define_class;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol};
    use objc2::MainThreadOnly;
    use objc2_app_kit::{NSEvent, NSTrackingArea, NSTrackingAreaOptions};
    use objc2_foundation::{MainThreadMarker, NSRect};
    use std::sync::OnceLock;
    use tauri::{AppHandle, Emitter};

    /// 静态持有 tracker：NSTrackingArea 对 owner 只持弱引用，必须活过窗口生命周期。
    /// ObjC 对象标非 Send/Sync 是运行时纪律（只在主线程解引用），static 只是
    /// 跨线程持有所有权，不跨线程使用。
    struct SendRetained<T>(#[allow(dead_code)] Retained<T>);
    unsafe impl<T> Send for SendRetained<T> {}
    unsafe impl<T> Sync for SendRetained<T> {}

    static HOVER_TRACKER: OnceLock<SendRetained<HoverTracker>> = OnceLock::new();
    /// 详情窗口的进入/离开 tracker（只需 enter/leave，不做行级坐标转发）。
    static TIP_TRACKER: OnceLock<SendRetained<TipHoverTracker>> = OnceLock::new();
    /// 回调里发事件用的 AppHandle。悬浮窗单实例，安装时写入一次。
    static APP: OnceLock<AppHandle> = OnceLock::new();

    define_class!(
        // SAFETY:
        // - NSObject 没有子类化要求。
        // - 不实现 Drop；Retained 由 HOVER_TRACKER 静态持有到进程退出。
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = ()]
        struct HoverTracker;

        impl HoverTracker {
            // tracking area 的事件派发与窗口 key/active 状态无关——WKWebView 对
            // 非激活窗口抑制 hover 类 DOM 事件，这里的 AppKit 层事件是唯一可靠来源。

            // 移入悬浮窗：上报进入时刻（hover(true)），前端用「entered 与最近一次
            // exited 的间隔」区分真实移入与合成对——窗口移动/收缩（吸附收缩、展开
            // 动画起止）会让 tracking area 几何重算并合成一对紧邻的 exited/entered，
            // 间隔只有几毫秒；真实移入之前必然有真实的离开，间隔是离开时长。合成对
            // 不算移入，指针停在原地继续动也不会展开。
            #[unsafe(method(mouseEntered:))]
            unsafe fn mouse_entered(&self, _event: &NSEvent) {
                if let Some(app) = APP.get() {
                    if let Err(e) = app.emit_to(super::WIDGET_LABEL, "bark://widget-hover", true) {
                        tracing::warn!("转发悬浮窗 enter 事件失败: {e}");
                    }
                }
            }

            #[unsafe(method(mouseExited:))]
            unsafe fn mouse_exited(&self, _event: &NSEvent) {
                if let Some(app) = APP.get() {
                    if let Err(e) = app.emit_to(super::WIDGET_LABEL, "bark://widget-hover", false) {
                        tracing::warn!("转发悬浮窗 hover 事件失败: {e}");
                    }
                }
            }

            // 窗口内鼠标移动：转发坐标给前端做行级 hover（elementFromPoint），
            // 同时作为「真实移入」信号驱动展开。
            #[unsafe(method(mouseMoved:))]
            unsafe fn mouse_moved(&self, event: &NSEvent) {
                let Some(app) = APP.get() else { return };
                let loc = event.locationInWindow();
                // locationInWindow 是左下原点的点坐标；无边框窗口 frame == content，
                // macOS 上 1pt == 1 CSS px，只翻转 y 轴就是前端视口坐标。
                if let Some(window) = event_window(event) {
                    let frame: NSRect = msg_send![&*window, frame];
                    let payload = serde_json::json!({
                        "x": loc.x,
                        "y": frame.size.height - loc.y,
                    });
                    if let Err(e) = app.emit_to(super::WIDGET_LABEL, "bark://widget-mouse", payload) {
                        tracing::warn!("转发悬浮窗鼠标坐标失败: {e}");
                    }
                }
            }
        }

        unsafe impl NSObjectProtocol for HoverTracker {}
    );

    impl HoverTracker {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            unsafe {
                let this = Self::alloc(mtm).set_ivars(());
                msg_send![super(this), init]
            }
        }
    }

    define_class!(
        // SAFETY: 同 HoverTracker——NSObject 无子类化要求、不实现 Drop、Retained 由
        // TIP_TRACKER 静态持有到进程退出，只在主线程解引用。
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = ()]
        struct TipHoverTracker;

        impl TipHoverTracker {
            // 详情窗口的进入/离开转发给前端做「移入保持、移出收起」。与悬浮窗同理：
            // WKWebView 对非激活窗口抑制 hover 类 DOM 事件，AppKit 层是唯一可靠来源。
            #[unsafe(method(mouseEntered:))]
            unsafe fn mouse_entered(&self, _event: &NSEvent) {
                if let Some(app) = APP.get() {
                    if let Err(e) = app.emit_to(super::TIP_LABEL, "bark://tip-hover", true) {
                        tracing::warn!("转发详情窗 enter 事件失败: {e}");
                    }
                }
            }

            #[unsafe(method(mouseExited:))]
            unsafe fn mouse_exited(&self, _event: &NSEvent) {
                if let Some(app) = APP.get() {
                    if let Err(e) = app.emit_to(super::TIP_LABEL, "bark://tip-hover", false) {
                        tracing::warn!("转发详情窗 hover 事件失败: {e}");
                    }
                }
            }
        }

        unsafe impl NSObjectProtocol for TipHoverTracker {}
    );

    impl TipHoverTracker {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            unsafe {
                let this = Self::alloc(mtm).set_ivars(());
                msg_send![super(this), init]
            }
        }
    }

    /// mouseMoved 事件的 window 字段（多数为所属窗口；nil 时丢弃）。
    unsafe fn event_window(event: &NSEvent) -> Option<Retained<objc2_app_kit::NSWindow>> {
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        event.window(mtm)
    }

    /// 给悬浮窗 contentView 挂 ActiveAlways 的 tracking area（含 MouseMoved），
    /// 并禁用「拖到菜单栏触发全屏」手势。主线程调用（AppKit 要求），在窗口创建完成后执行。
    pub unsafe fn install(ns_window: *mut std::ffi::c_void, app: AppHandle) {
        let mtm = MainThreadMarker::new_unchecked();
        let window = &*(ns_window.cast::<objc2_app_kit::NSWindow>());
        // 拖窗口到菜单栏停留会触发 macOS 的平铺/全屏手势——手势接管拖动，松手
        // 窗口被弹回原位（实测「拖到顶部回屏幕中间」）。悬浮窗这种贴边工具条
        // 不参与全屏：在现有 collectionBehavior（canJoinAllSpaces 等）上叠加
        // FullScreenNone + DisallowsTiled，手势不再触发。
        let behavior: objc2_app_kit::NSWindowCollectionBehavior = msg_send![window, collectionBehavior];
        window.setCollectionBehavior(
            behavior
                | objc2_app_kit::NSWindowCollectionBehavior::FullScreenNone
                | objc2_app_kit::NSWindowCollectionBehavior::FullScreenDisallowsTiling,
        );
        let _ = APP.set(app);

        let tracker = HoverTracker::new(mtm);
        let content_view: Retained<objc2_app_kit::NSView> = msg_send![window, contentView];
        let area = NSTrackingArea::initWithRect_options_owner_userInfo(
            <NSTrackingArea as objc2::AnyThread>::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(&*tracker),
            None,
        );
        content_view.addTrackingArea(&area);
        let _ = HOVER_TRACKER.set(SendRetained(tracker));
        let _ = mtm;
    }

    /// 给详情窗口 contentView 挂 ActiveAlways 的进入/离开 tracking area。
    /// 主线程调用，在详情窗口创建完成后执行（collection behavior 由建窗处设置）。
    pub unsafe fn install_tip(ns_window: *mut std::ffi::c_void, app: AppHandle) {
        let mtm = MainThreadMarker::new_unchecked();
        let window = &*(ns_window.cast::<objc2_app_kit::NSWindow>());
        let _ = APP.set(app);
        let tracker = TipHoverTracker::new(mtm);
        let content_view: Retained<objc2_app_kit::NSView> = msg_send![window, contentView];
        let area = NSTrackingArea::initWithRect_options_owner_userInfo(
            <NSTrackingArea as objc2::AnyThread>::alloc(),
            NSRect::ZERO,
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(&*tracker),
            None,
        );
        content_view.addTrackingArea(&area);
        let _ = TIP_TRACKER.set(SendRetained(tracker));
    }
}

/// 原生拖动回执：moved 是否发生位移；before 是拖动开始前的窗口位置（物理像素）。
///
/// moved 在命令内部由 before 与松手后的实际位置对比得出（macOS 手动跟手挪窗后
/// 位移可测，不依赖拖动期间可能丢失的 moved 事件）。before 仍随回执带回：前端
/// 拖动期间的 moved 事件可能丢失，松手后用 before 对比实际位置补判「真的拖动了」。
#[derive(Serialize)]
pub struct DragReport {
    pub moved: bool,
    pub before: (i32, i32),
    /// 松手时的全局鼠标位置（点单位，左上原点）。吸附判定的意图信号：
    /// 拖动时卡片跟着抓取点走，鼠标出不了屏，卡片边缘永远停在屏内几十像素——
    /// 用卡片几何判定「贴边」永远差一口气；鼠标能精确贴到 x=0。
    #[cfg(target_os = "macos")]
    pub mouse: (f64, f64),
}

/// 原生拖动：拖动期间跟手挪窗，松手后回执 moved 与拖动前位置。
///
/// Windows：start_dragging 即返回，后台轮询左键状态等释放；
/// macOS：[`macos::begin`] 记起始几何，轮询期间 [`macos::follow`] 手动跟手（见
/// 模块注释：不进系统拖动会话），松手后 [`macos::end`] 清理会话；
/// 其它平台：Tauri 的 start_dragging 自身语义已够。
pub async fn drag(window: tauri::WebviewWindow, app: tauri::AppHandle) -> Result<DragReport, String> {
    let before = window.outer_position().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        let _ = &app;
        window.start_dragging().map_err(|e| e.to_string())?;
        while unsafe { GetAsyncKeyState(0x01) } < 0 {
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
        // 松手消息先交给原生窗口处理，再取位置；同时让最后一批 moved 事件到达前端。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    #[cfg(target_os = "macos")]
    {
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        app.clone()
            .run_on_main_thread(move || unsafe { macos::begin(ns_window) })
            .map_err(|e| e.to_string())?;
        // 手动跟手：不进系统拖动会话（会话自带「拖到菜单栏进全屏」手势，
        // 全局系统行为、无按 App 关闭的 API），轮询光标在主线程直接挪窗口
        while unsafe { macos::left_mouse_down() } {
            macos::follow(&app);
            tokio::time::sleep(std::time::Duration::from_millis(16)).await;
        }
        macos::end();
        // 松手消息先交给原生窗口处理，再取位置；同时让最后一批 moved 事件到达前端。
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = &app;
        window.start_dragging().map_err(|e| e.to_string())?;
    }
    let after = window.outer_position().map_err(|e| e.to_string())?;
    let moved = before != after;
    tracing::info!("悬浮窗拖动结束（位移 {moved}，{before:?} → {after:?}）");
    #[cfg(target_os = "macos")]
    {
        let mouse = macos::cursor::location();
        return Ok(DragReport { moved, before: (before.x, before.y), mouse: (mouse.x, mouse.y) });
    }
    #[cfg(not(target_os = "macos"))]
    Ok(DragReport { moved, before: (before.x, before.y) })
}

/// 建窗时注入的初始参数（见模块注释的「位置记忆口径」）
#[derive(Serialize)]
struct WidgetInit {
    x: f64,
    y: f64,
    width: f64,
    pinned: bool,
    dock_side: Option<WidgetDockSide>,
    detail_tip: bool,
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
/// 关闭 → 销毁；开启 → 不在就按记住的位置建窗。详情窗口随悬浮窗同生共死。
pub fn sync(app: &AppHandle, state: &Arc<AppState>) {
    let app = app.clone();
    let state = state.clone();
    // 建窗/销毁必须主线程（WebviewWindowBuilder 非线程安全，同 glow）
    if let Err(e) = app.clone().run_on_main_thread(move || {
        let cfg = read(&state.config).widget.clone();
        let exists = app.get_webview_window(WIDGET_LABEL).is_some();
        if !cfg.enabled {
            state.widget_reveal_pending.store(false, Ordering::SeqCst);
            destroy_window(&app, TIP_LABEL);
            if exists {
                destroy_window(&app, WIDGET_LABEL);
            }
            return;
        }
        if !exists {
            if let Err(e) = create(&app, &cfg, state.lang()) {
                tracing::warn!("悬浮窗创建失败: {e}");
            }
        }
        // 详情窗口常驻（隐藏待命），缺失则补建——与悬浮窗解耦，开关详情只影响前端是否发起请求
        if app.get_webview_window(TIP_LABEL).is_none() {
            if let Err(e) = create_tip(&app, state.lang()) {
                tracing::warn!("详情窗创建失败: {e}");
            }
        }
    }) {
        tracing::warn!("悬浮窗同步失败（主线程不可用？）: {e}");
    }
}

/// 销毁指定窗口。macOS 面板先还原成普通窗口再销毁（面板上的异步销毁路径会抛
/// NSException，见 glow.rs create 的转换注释）；非面板窗口 restore_window 原样返回。
fn destroy_window(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        #[cfg(target_os = "macos")]
        let w = crate::glow::restore_window(w);
        let _ = w.destroy();
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

fn create(app: &AppHandle, cfg: &WidgetConfig, lang: bark_core::Lang) -> tauri::Result<()> {
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
        detail_tip: cfg.detail_tip,
    })
    .unwrap_or_else(|_| "null".to_string());

    let window = WebviewWindowBuilder::new(app, WIDGET_LABEL, WebviewUrl::App("widget.html".into()))
        .title(match lang {
            bark_core::Lang::Zh => "AgentBark 悬浮窗",
            bark_core::Lang::En => "AgentBark Widget",
        })
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
    #[cfg(target_os = "macos")]
    {
        // WKWebView 对非激活窗口抑制 hover 类 DOM 事件（WebKit active 状态），
        // 悬浮窗 focusable=false 永远不激活——由 AppKit 层转发 hover 与鼠标坐标
        if let Ok(ns_window) = window.ns_window() {
            unsafe { hover::install(ns_window, app.clone()) };
        }
        // 转 NSPanel：「点壁纸显示桌面」/调度中心「显示桌面」不再推走本窗口。
        // 转换是 object_setClass 换类，同一 ObjC 对象——tauri 的窗口 API、
        // onMoved 事件、tracking area 全部继续有效。
        use tauri_nspanel::{ManagerExt as _, WebviewWindowExt as _};
        if let Err(e) = window.to_panel::<WidgetPanel>() {
            tracing::warn!("悬浮窗 NSPanel 转换失败（继续以普通窗口运行）: {e}");
        } else if let Ok(panel) = app.get_webview_panel(WIDGET_LABEL) {
            // 面板的 utility 层级显式提到 screen-saver 级：置顶语义的延续
            panel.as_panel().setLevel(1000);
            // 「点壁纸显示桌面」/调度中心「显示桌面」把普通窗口移到 side space，
            // Sonoma+ 连 level 1000 的普通窗口也不放过——面板要显式声明
            // Stationary（不随 space 移动）+ Auxiliary（不参与窗口编排）才存活
            panel.set_collection_behavior(
                objc2_app_kit::NSWindowCollectionBehavior::CanJoinAllSpaces
                    | objc2_app_kit::NSWindowCollectionBehavior::Stationary
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenAuxiliary
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenNone
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenDisallowsTiling,
            );
        }
    }
    if cfg.dock_side.is_some() && !cfg.pinned {
        window.set_position(tauri::PhysicalPosition::new((x * scale).round() as i32, (y * scale).round() as i32))?;
    }
    tracing::info!("悬浮窗已创建（x={x}, y={y}, pinned={}, dock_side={:?}）", cfg.pinned, cfg.dock_side);
    Ok(())
}

/// 建详情面板窗口：透明、不可聚焦、置顶、初始隐藏，几何完全由前端（tip.ts）按需
/// 设置——只覆盖详情矩形本身，绝不盖住卡片，悬浮窗本体因此永不因详情而移动。
fn create_tip(app: &AppHandle, lang: bark_core::Lang) -> tauri::Result<()> {
    // window 句柄只在下面 macOS 分支用（NSPanel 转换 / hover 转发），其余平台允许未使用
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    let window = WebviewWindowBuilder::new(app, TIP_LABEL, WebviewUrl::App("tip.html".into()))
        .title(match lang {
            bark_core::Lang::Zh => "AgentBark 详情",
            bark_core::Lang::En => "AgentBark Details",
        })
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
        .focusable(false)
        .visible(false)
        .disable_drag_drop_handler()
        .inner_size(220.0, 80.0)
        .build()?;
    #[cfg(target_os = "macos")]
    {
        // 与悬浮窗同理：非激活窗口收不到 WebKit hover，由 AppKit 转发进入/离开
        if let Ok(ns_window) = window.ns_window() {
            unsafe { hover::install_tip(ns_window, app.clone()) };
        }
        use tauri_nspanel::{ManagerExt as _, WebviewWindowExt as _};
        if let Err(e) = window.to_panel::<tip_panel::TipPanel>() {
            tracing::warn!("详情窗 NSPanel 转换失败（继续以普通窗口运行）: {e}");
        } else if let Ok(panel) = app.get_webview_panel(TIP_LABEL) {
            panel.as_panel().setLevel(1000);
            panel.set_collection_behavior(
                objc2_app_kit::NSWindowCollectionBehavior::CanJoinAllSpaces
                    | objc2_app_kit::NSWindowCollectionBehavior::Stationary
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenAuxiliary
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenNone
                    | objc2_app_kit::NSWindowCollectionBehavior::FullScreenDisallowsTiling,
            );
        }
    }
    tracing::info!("详情窗已创建");
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
