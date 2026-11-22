/**
 * 行悬停详情面板——跑在**独立 tip 窗口**里（tip.html → tip.ts）。
 *
 * 与卡片几何彻底解耦：详情撑大/挪动只发生在 tip 自己的窗口，悬浮窗本体永不移动
 * （旧实现把详情画进悬浮窗、靠撑大挪窗容纳，贴边时整窗被 fitPosition 挤来挤去抖动）。
 * 悬浮窗侧只负责把行悬停意图（内容＋卡片/屏幕矩形）经事件发过来，本模块做意图延时、
 * 渲染、时长计时与「移入保持、移出收起」。host.place/close 由 tip.ts 用自身窗口几何实现。
 *
 * 本模块被 tests/widget-tip.test.mjs 以 VM 沙箱直接加载，**绝不 import i18n.ts 或
 * @tauri-apps**：取词结果由调用方注入，窗口几何由 host 注入。
 */
import { durationUnits, formatDuration, type DurationUnits } from "./session-time";
import type { Rect } from "./widget-layout";

/** 悬浮窗发来的一次行悬停请求：内容＋落位所需的几何（逻辑像素、屏幕坐标）。 */
export interface TipRequest {
  /** 行身份键（agent|session_id），用于去重与异步失效判定 */
  key: string;
  heading: string;
  body: string;
  /** 运行起始毫秒时间戳；0/非有限值表示不显示时长 */
  startedAt: number;
  /** 悬浮窗卡片的屏幕矩形 */
  card: Rect;
  /** 落位钳制矩形：macOS 顶部吸附用全屏（盖菜单栏），其余用工作区 */
  screen: Rect;
  /** 行顶相对卡片顶的偏移 */
  rowTop: number;
}

interface TipHost {
  place: (box: HTMLElement, data: TipRequest, current: () => boolean) => Promise<boolean>;
  close: () => Promise<void>;
}

/** 详情文案（tip.ts 按 t()/当前语言传入；缺省保持中文原样）。 */
export interface RowTipLabels {
  /** 时长前缀：参数是 formatDuration 的结果（如「5秒」/"5s"），返回如「已运行 5秒」 */
  ranFor: (d: string) => string;
  /** formatDuration 的单位词（按语言取 durationUnits(locale())） */
  units: DurationUnits;
}

const DEFAULT_LABELS: RowTipLabels = {
  ranFor: (d) => `已运行 ${d}`,
  units: durationUnits("zh"),
};

let labels: RowTipLabels = DEFAULT_LABELS;

/** 运行中换语言时由 tip.ts 重设（详情下次渲染即按新语言取词） */
export function setRowTipLabels(next: RowTipLabels) {
  labels = next;
}

let host: TipHost;
let tip: HTMLDivElement;
let duration: HTMLDivElement;
let target: TipRequest | null = null;
let showTimer = 0;
let hideTimer = 0;
let generation = 0;
let durationTimer = 0;

export function initRowTip(next: TipHost, tipLabels?: RowTipLabels) {
  if (tipLabels) labels = tipLabels;
  host = next;
  tip = document.createElement("div");
  tip.id = "row-tip";
  tip.setAttribute("role", "tooltip");
  const heading = document.createElement("div");
  heading.className = "tip-heading";
  const body = document.createElement("div");
  body.className = "tip-body";
  duration = document.createElement("div");
  duration.className = "tip-duration";
  duration.hidden = true;
  tip.append(heading, body, duration);
  document.body.appendChild(tip);
  // Windows 走 DOM hover；macOS 非激活窗口 DOM hover 被抑制，由 tip.ts 监听
  // AppKit 转发的 bark://tip-hover 调 tipHover()，两路不会同时触发。
  tip.addEventListener("mouseenter", cancelTipHide);
  tip.addEventListener("mouseleave", scheduleTipHide);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopDurationTimer();
    else if (tip.classList.contains("show")) startDurationTimer();
  });
}

function stopDurationTimer() {
  window.clearInterval(durationTimer);
  durationTimer = 0;
}

function renderDuration() {
  const startedAt = target?.startedAt ?? 0;
  duration.hidden = !Number.isFinite(startedAt) || startedAt <= 0;
  duration.textContent = duration.hidden ? "" : labels.ranFor(formatDuration(startedAt, Date.now(), labels.units));
}

function startDurationTimer() {
  stopDurationTimer();
  renderDuration();
  // 只更新可见详情里的文字，不触发原生窗口缩放。
  if (!duration.hidden && !document.hidden) durationTimer = window.setInterval(renderDuration, 1000);
}

function cancelTipHide() {
  window.clearTimeout(hideTimer);
  hideTimer = 0;
}

export function hideRowTip() {
  window.clearTimeout(showTimer);
  showTimer = 0;
  cancelTipHide();
  stopDurationTimer();
  generation++;
  target = null;
  tip?.classList.remove("show");
  if (host) void host.close().catch((e) => console.warn("收起详情失败：", e));
}

/** Leave enough time to cross the 8px gap into the detail panel. */
export function scheduleTipHide() {
  window.clearTimeout(showTimer);
  showTimer = 0;
  cancelTipHide();
  hideTimer = window.setTimeout(hideRowTip, 160);
}

/** 鼠标进入/离开详情窗口（macOS 由 AppKit 转发，Windows 由 DOM 事件直接调内部监听）。 */
export function tipHover(entered: boolean) {
  if (entered) cancelTipHide();
  else scheduleTipHide();
}

async function show(data: TipRequest) {
  const gen = ++generation;
  const current = () => gen === generation && target?.key === data.key;
  tip.querySelector(".tip-heading")!.textContent = data.heading;
  tip.querySelector(".tip-body")!.textContent = data.body;
  renderDuration();
  try {
    const placed = await host.place(tip, data, current);
    if (placed && current()) {
      tip.classList.add("show");
      startDurationTimer();
    }
  } catch (e) {
    if (current()) hideRowTip();
    console.warn("显示详情失败：", e);
  }
}

/** 内容/几何是否实质相同——快照频繁刷新但行未变时跳过重排，避免详情窗亚像素抖动。 */
function sameTip(a: TipRequest, b: TipRequest): boolean {
  return a.heading === b.heading && a.body === b.body && a.startedAt === b.startedAt && a.rowTop === b.rowTop
    && a.card.x === b.card.x && a.card.y === b.card.y && a.card.width === b.card.width && a.card.height === b.card.height
    && a.screen.x === b.screen.x && a.screen.y === b.screen.y && a.screen.width === b.screen.width && a.screen.height === b.screen.height;
}

export function scheduleRowTip(data: TipRequest) {
  cancelTipHide();
  if (target && target.key === data.key) {
    // 同一行：内容/几何没变就不重排（快照刷新会高频重发）；变了才就地更新重排
    const unchanged = sameTip(target, data);
    target = data;
    if (!unchanged && tip.classList.contains("show")) void show(data);
    return;
  }
  hideRowTip();
  target = data;
  showTimer = window.setTimeout(() => {
    showTimer = 0;
    if (target) void show(target);
  }, 220);
}

/** 内容/语言刷新：已显示则按当前 target 重渲染重排（行是否还在由悬浮窗侧判定）。 */
export function refreshRowTip() {
  if (target && tip.classList.contains("show")) void show(target);
}