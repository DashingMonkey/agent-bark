/**
 * 悬浮窗的悬停提示（单例 DOM）。
 *
 * 为什么不用原生 `title`：弹出要等近 1 秒、样式跟系统走，在无边框置顶的小卡片上很突兀。
 * 也不引 `tooltip.ts`（主面板那份）：它的 `.tooltip` 样式在 style.css 里，而悬浮窗是独立
 * 入口（只加载 widget.html，完全不引 style.css），配色也不一样（卡片是半透明深/浅底）。
 * 这里只保留最小能力：定文案、跟目标定位、放不下翻到上方。
 *
 * **不用 `v-tooltip` 那种「每个元素挂监听」的写法**：卡片的行是每次 render 重建 innerHTML 的，
 * 逐元素挂监听会在每帧泄漏。改为**在容器上事件委托 + `data-tip` 属性**（见 widget.ts），
 * 行怎么重建都不影响。
 */

/** 显示延迟：原生 title 近 1s 太慢，这里对齐主面板的 300ms */
const DELAY_MS = 300;
const EDGE = 6;
const GAP = 6;

let tip: HTMLDivElement | null = null;
let showTimer = 0;
let target: HTMLElement | null = null;

function ensureEl(): HTMLDivElement {
  if (!tip) {
    tip = document.createElement("div");
    tip.id = "row-tip";
    tip.setAttribute("role", "tooltip");
    document.body.appendChild(tip);
  }
  return tip;
}

/** 立即收起（目标行被重建 / 指针离开 / 卡片淡出时调用） */
export function hideRowTip() {
  if (showTimer) {
    clearTimeout(showTimer);
    showTimer = 0;
  }
  target = null;
  tip?.classList.remove("show");
}

function show(text: string, el: HTMLElement) {
  const box = ensureEl();
  if (box.textContent !== text) box.textContent = text;
  target = el;
  const rect = el.getBoundingClientRect();
  const w = box.offsetWidth;
  const h = box.offsetHeight;
  // 水平：与行左对齐（卡片只有 240px，居中反而容易顶到窗口边），再夹进窗口内
  let left = rect.left;
  left = Math.max(EDGE, Math.min(left, window.innerWidth - w - EDGE));
  // 垂直：优先放在行下方；窗口只有卡片那么高，放不下就翻到上方
  let top = rect.bottom + GAP;
  if (top + h > window.innerHeight - EDGE) top = rect.top - h - GAP;
  box.style.left = `${Math.round(Math.max(EDGE, left))}px`;
  box.style.top = `${Math.round(Math.max(EDGE, top))}px`;
  box.classList.add("show");
}

/** 指针进入某行（带 `data-tip` 才提示；空文案不提示） */
export function scheduleRowTip(text: string, el: HTMLElement) {
  if (!text) {
    hideRowTip();
    return;
  }
  if (target === el && tip?.classList.contains("show")) return;
  if (showTimer) clearTimeout(showTimer);
  showTimer = window.setTimeout(() => show(text, el), DELAY_MS);
}
