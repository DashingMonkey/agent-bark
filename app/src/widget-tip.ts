/** One detail panel, with delegated row events and serialized native window geometry. */
interface TipHost {
  place: (box: HTMLElement, row: HTMLElement, current: () => boolean) => Promise<boolean>;
  close: () => Promise<void>;
}

let host: TipHost;
let tip: HTMLDivElement;
let target: HTMLElement | null = null;
let showTimer = 0;
let hideTimer = 0;
let generation = 0;

export function initRowTip(next: TipHost) {
  host = next;
  tip = document.createElement("div");
  tip.id = "row-tip";
  tip.setAttribute("role", "tooltip");
  const heading = document.createElement("div");
  heading.className = "tip-heading";
  const body = document.createElement("div");
  body.className = "tip-body";
  tip.append(heading, body);
  document.body.appendChild(tip);
  tip.addEventListener("mouseenter", cancelTipHide);
  tip.addEventListener("mouseleave", scheduleTipHide);
}

function cancelTipHide() {
  window.clearTimeout(hideTimer);
  hideTimer = 0;
}

export function hideRowTip() {
  window.clearTimeout(showTimer);
  showTimer = 0;
  cancelTipHide();
  generation++;
  target?.removeAttribute("aria-describedby");
  target = null;
  tip?.classList.remove("show");
  if (host) void host.close().catch((e) => console.warn("收起悬浮窗详情失败：", e));
}

/** Leave enough time to cross the 8px gap into the detail panel. */
export function scheduleTipHide() {
  window.clearTimeout(showTimer);
  showTimer = 0;
  cancelTipHide();
  hideTimer = window.setTimeout(hideRowTip, 160);
}

async function show(row: HTMLElement) {
  const gen = ++generation;
  const current = () => gen === generation && row.isConnected && target === row;
  tip.querySelector(".tip-heading")!.textContent = row.dataset.tipHeading ?? "";
  tip.querySelector(".tip-body")!.textContent = row.dataset.tip ?? "";
  try {
    const placed = await host.place(tip, row, current);
    if (placed && current()) {
      tip.classList.add("show");
      row.setAttribute("aria-describedby", "row-tip");
    }
  } catch (e) {
    if (current()) hideRowTip();
    console.warn("显示悬浮窗详情失败：", e);
  }
}

export function scheduleRowTip(row: HTMLElement) {
  cancelTipHide();
  if (target === row && (showTimer || tip.classList.contains("show"))) return;
  hideRowTip();
  target = row;
  showTimer = window.setTimeout(() => {
    showTimer = 0;
    if (target === row && row.isConnected) void show(row);
  }, 220);
}

/** Existing row nodes survive snapshots, so live details need not flash closed. */
export function refreshRowTip() {
  if (!target) return;
  const list = target.closest(".session-list");
  const rect = target.getBoundingClientRect();
  const bounds = list?.getBoundingClientRect();
  if (!target.isConnected || !bounds || rect.bottom <= bounds.top || rect.top >= bounds.bottom) {
    hideRowTip();
  } else if (tip.classList.contains("show")) {
    void show(target);
  }
}
