/**
 * 右下角 toast 提醒：保存成功 / 保存失败这类操作结果的即时反馈。
 *
 * 替代旧版「保存中… / 已保存 ✓」的按钮文字闪烁——文字一来一回按钮宽度跟着跳，
 * 且反馈长在按钮上，用户点完往往已经不看那里了。与 tooltip.ts 同一套单例 DOM 思路：
 * - 视口右下角一个容器，多条向上堆叠；新的滑入，到时自动滑出；
 * - 成功 / 错误两种语义色，配色走 style.css 的主题变量（--ok / --err），跟随深浅主题；
 * - 点击任意一条立即关闭（错误文案长，等不及自动消失时可以手动关掉）。
 */

type ToastKind = "success" | "error";

const SUCCESS_MS = 2200; // 成功提示一闪即走
const ERROR_MS = 4200; // 错误要留时间读原因
const OUT_MS = 200; // 与 style.css 里 .toast.out 的离场过渡对齐

const ICONS: Record<ToastKind, string> = {
  // 对勾 / 叉：与标题栏、删除按钮同一套 1px 描边画法
  success: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 8.6l3.4 3.4L13 5"/></svg>',
  error: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg>',
};

interface ToastItem {
  el: HTMLDivElement;
  timer: number;
  closing: boolean;
}

let box: HTMLDivElement | null = null;

function ensureBox(): HTMLDivElement {
  if (!box) {
    box = document.createElement("div");
    box.className = "toast-box";
    // aria-live：读屏器也能听到保存结果
    box.setAttribute("role", "status");
    box.setAttribute("aria-live", "polite");
    document.body.appendChild(box);
  }
  return box;
}

function dismiss(item: ToastItem) {
  if (item.closing) return; // 点击关闭与定时器到时可能先后到达，只走一遍
  item.closing = true;
  clearTimeout(item.timer);
  item.el.classList.add("out");
  window.setTimeout(() => item.el.remove(), OUT_MS);
}

/**
 * 弹一条右下角 toast。文案用 textContent 注入——错误信息来自异常文本，
 * 不能拼进 innerHTML（图标是常量，走 innerHTML 没问题）。
 */
export function toast(message: string, kind: ToastKind = "success") {
  const el = document.createElement("div");
  el.className = `toast ${kind}`;
  el.innerHTML = ICONS[kind];
  const text = document.createElement("span");
  text.textContent = message;
  el.appendChild(text);

  const item: ToastItem = { el, timer: 0, closing: false };
  el.addEventListener("click", () => dismiss(item));
  ensureBox().appendChild(el);
  item.timer = window.setTimeout(() => dismiss(item), kind === "error" ? ERROR_MS : SUCCESS_MS);
}
