/**
 * 全局悬停提示（参考 VS Code 的 hover）：替代原生 `title`。
 *
 * 原生 title 有两个硬伤：弹出延迟近 1 秒、样式跟随系统无法与应用主题统一。
 * 这里用单例 DOM + `v-tooltip` 指令实现：
 * - fixed 定位按 target 实时计算，不会被滚动容器裁剪，放不下自动翻转到上方；
 * - 300ms 延迟显示（原生太慢），滚动 / 窗口缩放 / 点击即收起；
 * - 文本可选中复制（提示里常有配置路径，用户排障时需要拷走）。
 */
import type { Directive } from "vue";

const DELAY_MS = 300;
const EDGE = 8;
const GAP = 6;

let tip: HTMLDivElement | null = null;
let showTimer = 0;
let currentTarget: HTMLElement | null = null;

function ensureEl(): HTMLDivElement {
  if (!tip) {
    tip = document.createElement("div");
    tip.className = "tooltip";
    tip.setAttribute("role", "tooltip");
    document.body.appendChild(tip);
  }
  return tip;
}

function hideNow() {
  if (showTimer) {
    clearTimeout(showTimer);
    showTimer = 0;
  }
  currentTarget = null;
  tip?.classList.remove("show");
}

function show(text: string, target: HTMLElement) {
  const el = ensureEl();
  el.textContent = text;
  currentTarget = target;
  // 先摆到位再淡入：透明状态下也能量出尺寸
  const rect = target.getBoundingClientRect();
  const w = el.offsetWidth;
  const h = el.offsetHeight;
  let left = rect.left + rect.width / 2 - w / 2;
  left = Math.max(EDGE, Math.min(left, window.innerWidth - w - EDGE));
  let top = rect.bottom + GAP;
  if (top + h > window.innerHeight - EDGE) top = rect.top - h - GAP;
  el.style.left = `${Math.round(Math.max(EDGE, left))}px`;
  el.style.top = `${Math.round(Math.max(EDGE, top))}px`;
  el.classList.add("show");
}

// 滚动时提示会悬空错位，直接收起最省事（capture 捕获所有滚动容器的滚动）
function onScroll() {
  if (currentTarget) hideNow();
}

let globalBound = false;
function bindGlobal() {
  if (globalBound) return;
  globalBound = true;
  window.addEventListener("scroll", onScroll, true);
  window.addEventListener("resize", hideNow);
}

interface TooltipHost extends HTMLElement {
  __tooltipText?: string;
  __tooltipOff?: () => void;
}

export const vTooltip: Directive<TooltipHost, string | null | undefined> = {
  mounted(el, binding) {
    el.__tooltipText = binding.value ?? "";
    const enter = () => {
      const text = el.__tooltipText;
      if (!text) return;
      if (showTimer) clearTimeout(showTimer);
      showTimer = window.setTimeout(() => show(text, el), DELAY_MS);
    };
    const leave = () => {
      if (showTimer) {
        clearTimeout(showTimer);
        showTimer = 0;
      }
      if (currentTarget === el) hideNow();
    };
    el.addEventListener("mouseenter", enter);
    el.addEventListener("mouseleave", leave);
    // 键盘可达：聚焦同样弹出（focusin 会从内部 input 冒泡上来）
    el.addEventListener("focusin", enter);
    el.addEventListener("focusout", leave);
    // 点击后提示往往已过时（如开关翻转），立即收起
    el.addEventListener("click", leave);
    bindGlobal();
    el.__tooltipOff = () => {
      el.removeEventListener("mouseenter", enter);
      el.removeEventListener("mouseleave", leave);
      el.removeEventListener("focusin", enter);
      el.removeEventListener("focusout", leave);
      el.removeEventListener("click", leave);
    };
  },
  updated(el, binding) {
    el.__tooltipText = binding.value ?? "";
    // 悬停期间文本被响应式更新（如开关状态翻转）就地把内容换掉
    if (currentTarget === el) {
      if (!binding.value) hideNow();
      else if (tip) tip.textContent = binding.value;
    }
  },
  unmounted(el) {
    el.__tooltipOff?.();
    if (currentTarget === el) hideNow();
  },
};
