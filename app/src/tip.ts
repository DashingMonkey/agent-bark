/**
 * 行悬停详情的独立窗口入口（tip.html）。
 *
 * 悬浮窗把行悬停意图（内容＋卡片/屏幕矩形）经事件发过来，这里驱动 widget-tip.ts
 * 做意图延时/渲染/计时，并用**自身窗口**几何落位——tip 窗口只覆盖详情矩形本身，
 * 绝不盖住卡片，悬浮窗本体因此永不因详情而移动（根治贴边抖动）。
 */
import { getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { initTheme } from "./theme";
import { initI18n, locale, onLocaleChanged, t } from "./i18n";
import { durationUnits } from "./session-time";
import { placeTip } from "./widget-layout";
import {
  hideRowTip, initRowTip, refreshRowTip, scheduleRowTip, scheduleTipHide,
  setRowTipLabels, tipHover, type RowTipLabels, type TipRequest,
} from "./widget-tip";

initTheme();
const win = getCurrentWindow();

function tipLabels(): RowTipLabels {
  return { ranFor: (d) => t("events.ranFor", { d }), units: durationUnits(locale()) };
}

initRowTip({
  place: async (box, data, current) => {
    if (!current()) return false;
    const { card, screen } = data;
    box.style.width = "220px";
    box.style.maxHeight = "112px";
    // placeTip 直接给出详情窗自身矩形（已钳进屏幕且不与卡片重叠）；box 填满窗口
    const g = placeTip(card, screen, data.rowTop, box.offsetHeight);
    box.style.left = "0px";
    box.style.top = "0px";
    box.style.width = g.width + "px";
    box.style.maxHeight = g.height + "px";
    box.dataset.placement = g.side;
    await win.setSize(new LogicalSize(g.width, g.height));
    if (!current()) return false;
    await win.setPosition(new LogicalPosition(g.x, g.y));
    if (!current()) return false;
    await win.show();
    return current();
  },
  close: async () => { await win.hide(); },
});

void Promise.all([
  listen<TipRequest>("bark://tip-row", (e) => scheduleRowTip(e.payload)),
  listen("bark://tip-leave", () => scheduleTipHide()),
  listen("bark://tip-hide", () => hideRowTip()),
  // macOS：AppKit 转发的详情窗进入/离开（Windows 走 box 的 DOM mouseenter/mouseleave）
  listen<boolean>("bark://tip-hover", (e) => tipHover(e.payload)),
]).catch((e) => console.warn("订阅详情事件失败：", e));

// 语言就绪后再定文案（窗口先加载后显示，这趟 IPC 用户看不见）；切语言就地重取词刷新
void (async () => {
  await initI18n();
  setRowTipLabels(tipLabels());
  onLocaleChanged(() => { setRowTipLabels(tipLabels()); refreshRowTip(); });
})();