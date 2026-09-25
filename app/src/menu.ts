// 托盘菜单（对应 app/menu.html）。
//
// 独立入口，与主窗口的 Vue 应用无关。窗口由 tray_menu.rs 管理：点击托盘图标
// 时落位 + 显示 + 聚焦，平时常驻隐藏。职责只有两件事：
// 1. 静音勾选态渲染：建窗首帧用注入参数兜底，之后每次打开收 Rust 推送的最新值；
// 2. 菜单项动作（经 commands.rs）：显示主窗口 / 重置流光 / 静音（勾选项，
//    切换后菜单保持打开，同原生 CheckMenuItem）/ 退出。
//
// 关闭：点窗口外 → Rust 侧失焦即收（主口径）；点菜单项 / Esc → 自收；
// 光标离开窗口也自收——聚焦失败（Windows 前台锁）时失焦事件不会来，靠它兜底。
// 两个自动收起路径都有「刚打开豁免」：懒建窗的那次打开，WebView2 初始化 +
// 首导航会抖出虚假失焦/合成边界事件（光标明明还在托盘上），不豁免的话
// 第一次右键菜单刚弹出来就被收掉，第二次才正常。

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { initTheme } from "./theme";
import { guardStaleHover } from "./hover";

const win = getCurrentWindow();

// 首帧已由 menu.html 的内联脚本定色；这里补上跨窗口同步（主面板里切主题时，
// 已建好的菜单窗口跟着换色，不必等下次打开）
initTheme();

// 菜单常驻隐藏复用，重新弹出时 ：hover 残留的挡板（见模块注释）
guardStaleHover();

/** Rust 建窗时经 initialization_script 注入的初始静音态 */
const init =
  (window as unknown as { __MENU_INIT__?: { muted: boolean } }).__MENU_INIT__ ?? { muted: false };

let muted = init.muted;

function renderMute() {
  document.getElementById("menu-mute")!.classList.toggle("checked", muted);
}

async function hideSelf() {
  try {
    await win.hide();
  } catch {
    /* 窗口可能已被 Rust 侧收起 */
  }
}

document.getElementById("menu-show")!.addEventListener("click", async () => {
  try {
    await invoke("show_main_panel");
  } catch (e) {
    console.warn("显示主窗口失败：", e);
  }
  await hideSelf();
});

document.getElementById("menu-glow")!.addEventListener("click", async () => {
  try {
    await invoke("reset_glow");
  } catch (e) {
    console.warn("重置流光失败：", e);
  }
  await hideSelf();
});

// 静音是勾选项：切换后菜单保持打开（原生 CheckMenuItem 的行为），勾选态就地更新
document.getElementById("menu-mute")!.addEventListener("click", async () => {
  try {
    muted = await invoke<boolean>("toggle_mute");
    renderMute();
  } catch (e) {
    console.warn("切换静音失败：", e);
  }
});

document.getElementById("menu-quit")!.addEventListener("click", async () => {
  // app.exit 不返回，hideSelf 走不到也无妨
  try {
    await invoke("quit_app");
  } catch (e) {
    console.warn("退出失败：", e);
  }
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") void hideSelf();
});

/** 最近一次「打开」的时刻；页面加载本身算第一次（首开时 STATE_EVENT 早于监听注册） */
let lastOpenAt = performance.now();

/** 光标真实进过窗口才允许 mouseleave 收起：载入时合成的边界事件没有 enter 前置 */
let entered = false;

document.addEventListener("mouseenter", () => {
  entered = true;
});

// 光标离开窗口也收：与悬浮窗右键菜单同款兜底（见文件头「关闭」说明）。
// 刚打开的短时间内不收——那段时间的 leave 是载入期合成的，不是用户移出的。
document.addEventListener("mouseleave", () => {
  if (!entered || performance.now() - lastOpenAt < 800) return;
  void hideSelf();
});

// 每次打开（show + focus）Rust 都会推最新静音态
void listen<boolean>("tray-menu://state", (e) => {
  muted = e.payload;
  lastOpenAt = performance.now();
  renderMute();
}).catch((e) => console.warn("托盘菜单状态订阅失败：", e));

renderMute();
