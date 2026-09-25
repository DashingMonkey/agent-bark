// 桌面悬浮窗（对应 app/widget.html）。
//
// 独立入口，与主窗口的 Vue 应用无关。职责只有两件事：
// 1. 展示「当前活动」：进行中的会话（图标 + 四态状态）+ 最近一次
//    完成/终止事件的短暂驻留——状态口径与事件流、屏幕光效完全一致；
// 2. 窗口行为：按住拖动（右键菜单「固定位置」可锁住）、位置记忆
//    （经 save_widget_position 落盘，重启原位恢复）、右键菜单
//    （固定位置 / 关闭悬浮窗）、双击打开主面板（主窗口启动时不自动显示，
//    这里是它的入口之一）。
//
// 窗口几何：内容恒为固定宽的卡片，窗口尺寸跟随内容高度（行数变化时
// setSize）；右键菜单比窗口高，打开时把窗口临时撑到能容纳菜单、
// 关闭时还原。坐标口径：配置里的 x/y 是卡片左上角（逻辑像素），
// Rust 建窗与这里的位置上报共用这一口径（见 widget.rs 模块注释）。
//
// 自动隐藏（widget.auto_hide）：可见性状态机也在这里——安静（无行 /
// 全部行属思考类）持续 2 秒 → 淡出后 win.hide()；出现需要关注的行
// （等待确认 / 等待输入 / 任务完成 / 任务失败）→ 立即 win.show() 淡入；
// **新会话行**（= 新的事件流）出现时也弹出「一下」（照常 2 秒后随安静隐藏），
// 给「刚发的任务被接手了」一个回执。手动中止（run_aborted）的「已中止」
// 驻留行不算新会话、不弹（与光效「手动中止不提醒」同口径）；
// 悬停 / 拖动 / 右键菜单打开期间挂起隐藏。auto_hide 开着时
// Rust 建窗即隐藏（登录不闪卡片），首批数据到达后才决定要不要弹出。

import { listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import { api, agentIcon, agentName, PHASE_LABELS, type NormalizedEvent, type SessionStatus } from "./api";
import { initTheme } from "./theme";
import logo from "./assets/logo.png";

const win = getCurrentWindow();

// 首帧已由 widget.html 的内联脚本定色；这里补上跨窗口同步——悬浮窗是常驻窗口，
// 主面板里一切主题它应当场换色，而不是等下次重建
initTheme();

/** Rust 建窗时经 initialization_script 注入的初始参数 */
interface WidgetInit {
  x: number;
  y: number;
  width: number;
  pinned: boolean;
  auto_hide: boolean;
}
const init: WidgetInit =
  (window as unknown as { __WIDGET_INIT__?: WidgetInit }).__WIDGET_INIT__ ??
  { x: 60, y: 60, width: 240, pinned: false, auto_hide: true };

const CARD_W = init.width;
const ROW_H = 26;
const PAD_V = 12;
const IDLE_H = 38;
/** 完成/终止事件的驻留时长：与屏幕光效的完成色停留同量级 */
const TERMINAL_HOLD_MS = 6000;
/** 拖动停止多久后再读窗口位置落盘（等 OS 拖拽循环安静下来） */
const SAVE_DELAY_MS = 220;
/** 转入安静后多久隐藏（需求给定的固定值，不做配置项） */
const HIDE_DELAY_MS = 2000;
/** 淡入 / 淡出时长（CSS 过渡同值，见 widget.html） */
const FADE_MS = 160;

type StateColor = "thinking" | "warning" | "completed" | "terminated";

let pinned = init.pinned;
let dragging = false;
/** 卡片高度（随行数变化），窗口尺寸跟随它 */
let expandedH = IDLE_H;

interface Row {
  key: string;
  agent: string;
  label: string;
  state: StateColor;
  /**
   * 需要关注（自动隐藏状态机的「有事」档）：等待确认 / 等待输入（警告）、
   * 任务完成、任务失败。思考类（思考中 / 执行工具）与手动中止（已中止）不算——
   * 中止是用户自己按的停止，不值得把卡片弹回来（与光效同口径）
   */
  attention: boolean;
  /** 会话行（进行中的事件流）：新会话行出现时弹出「一下」；驻留行（完成/中止）不算 */
  session: boolean;
}

let sessions: SessionStatus[] = [];
let terminal: (Row & { at: number }) | null = null;

const appEl = document.getElementById("app") as HTMLElement;

// ---- 数据 ----

/// 会话快照代际：bark://sessions 推送递增它，拉取落地前核对——
/// 慢的旧拉取不得把推送来的新快照覆盖回旧数据
let sessionsSeq = 0;

async function loadSessions() {
  const seq = sessionsSeq;
  try {
    const s = await api.listActiveSessions();
    if (!Array.isArray(s)) {
      console.warn("会话快照负载异常，已忽略：", s);
      return;
    }
    if (seq === sessionsSeq) sessions = s; // 期间已有新推送接管：丢弃这份旧拉取
    render();
  } catch {
    /* 静默：推送通道可能仍在 */
  }
}

/** 完成/终止事件 → 驻留行（思考/警告由会话表表达，终态靠这里短暂展示） */
function onEvent(ev: NormalizedEvent) {
  // 推送负载兜底：null / 异常负载直接忽略，别把整个渲染循环冻在旧帧
  if (!ev || typeof ev !== "object") {
    console.warn("事件推送负载异常，已忽略：", ev);
    return;
  }
  if (ev.is_subagent) return;
  const map: Partial<Record<NormalizedEvent["type"], [StateColor, string]>> = {
    run_completed: ["completed", "任务完成"],
    run_failed: ["terminated", "任务失败"],
    run_aborted: ["terminated", "已中止"],
  };
  const hit = map[ev.type];
  if (!hit) return;
  terminal = {
    key: ev.id,
    agent: ev.agent,
    label: hit[1],
    state: hit[0],
    attention: ev.type !== "run_aborted",
    session: false,
    at: Date.now(),
  };
  scheduleExpiry();
  render();
}

let expiryTimer = 0;
function scheduleExpiry() {
  window.clearTimeout(expiryTimer);
  if (!terminal) return;
  const left = TERMINAL_HOLD_MS - (Date.now() - terminal.at);
  if (left <= 0) {
    terminal = null;
    return;
  }
  expiryTimer = window.setTimeout(() => {
    terminal = null;
    render();
  }, left + 50);
}

function currentRows(): Row[] {
  const out: Row[] = sessions.map((s) => {
    const state: StateColor =
      s.phase === "waiting_permission" || s.phase === "waiting_input" ? "warning" : "thinking";
    return {
      key: `${s.agent}|${s.session_id}`,
      agent: s.agent,
      // 未知 phase 兜底显示原值：undefined 漏进 escapeHtml 会抛 TypeError、悬浮窗冻结在旧帧
      label: PHASE_LABELS[s.phase] ?? String(s.phase),
      state,
      // 警告（等待确认 / 等待输入）要人管 = 有事；思考类（思考中 / 执行工具）不用
      attention: state === "warning",
      session: true,
    };
  });
  if (terminal) {
    const left = TERMINAL_HOLD_MS - (Date.now() - terminal.at);
    if (left > 0) out.push(terminal);
    else terminal = null;
  }
  return out;
}

// ---- 渲染 ----

function escapeHtml(s: unknown): string {
  // 入参一律先转字符串：agentName / phase 兜底值都可能漏进 undefined，
  // 直接 .replace 会抛 TypeError（悬浮窗会冻在旧帧）
  return String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
}

function render() {
  const list = currentRows();
  if (!list.length) {
    appEl.innerHTML = `<div class="card"><div class="row-item idle"><img src="${logo}" alt="" /><span>空闲 · 没有进行中的会话</span></div></div>`;
  } else {
    appEl.innerHTML =
      `<div class="card">` +
      list
        .map(
          (r) =>
            `<div class="row-item">` +
            `<span class="agent-icon">${agentIcon(r.agent)}</span>` +
            `<span class="name">${escapeHtml(agentName(r.agent))}</span>` +
            `<span class="state ${r.state}"><i class="dot"></i>${escapeHtml(r.label)}</span>` +
            `</div>`,
        )
        .join("") +
      `</div>`;
  }
  expandedH = list.length ? PAD_V + list.length * ROW_H : IDLE_H;

  void syncWindowSize().catch((e) => console.warn("同步悬浮窗尺寸失败：", e));
  evaluateVisibility(list);
}

// ---- 窗口尺寸 ----

async function syncWindowSize() {
  if (menuOpen) return; // 菜单把窗口撑大过，尺寸由 closeMenu 还原，别在这儿缩回去
  await win.setSize(new LogicalSize(CARD_W, expandedH));
}

// ---- 自动隐藏（安静即隐、有事即现）----
//
// 规则（口径见文件头）：
// - 有事（任一行 attention）→ 立即淡入弹出，并取消待隐藏计时；
// - 新会话行（= 新的事件流）出现 → 弹出「一下」：照常计 2 秒安静隐藏，
//   只当「任务被接手」的回执，不与有事档的驻留抢戏；
// - 安静（无行 / 全部行不 attention）→ 2 秒去抖后淡出隐藏，期间回到有事则取消；
//   安静期间的行数抖动（思考中 ↔ 执行工具）不重排计时，别把隐藏无限推迟；
// - 悬停 / 拖动 / 右键菜单打开期间挂起隐藏（正在读卡、拖动、点菜单时窗口不能跑），
//   解除后重新计 2 秒；
// - 关闭自动隐藏 → 取消一切计时并立即恢复常驻（含当时正隐藏 / 淡出的情况）。
//
// 隐藏 = win.hide()（不占任务栏、彻底不见），弹出 = win.show()；窗口是
// focusable(false)，show 不会抢用户编辑器的焦点。淡出先走 CSS 过渡再 hide()
// （直接藏会「啪」地消失），弹出时先挂透明、show 后隔帧去掉淡入类，让过渡真跑一帧。
//
// 可见性跟踪在 hidden（我们造成的隐藏；auto_hide 开着时 Rust 建窗即隐藏，所以
// 初值跟随 init）。hide/show 都是异步且可能在途被反向操作打断，用 visGen 代号
// 让过期的完成回调作废；失败只记日志、不翻转 hidden（下次评估自动重试）。

/** 拖动结束后多久内仍按「正在拖动」挂起隐藏（mouseup 会被 OS 拖拽循环吞掉，只能看最近移动时刻） */
const DRAG_ACTIVE_MS = 1200;

let autoHide = init.auto_hide;
/** 窗口当前是否被状态机隐藏 */
let hidden = autoHide;
/** 鼠标是否悬停在卡片上 */
let hovering = false;
/** 最近一次窗口移动（拖动）的时刻，配合 dragging 判断拖动是否仍在进行 */
let lastMoveAt = 0;
/** hide / show 完成回调的代号：翻转可见性就递增，在途的旧回调作废 */
let visGen = 0;
/** 安静去抖（2 秒）定时器 */
let hideTimer = 0;
/** 淡出过渡（结束才真正 hide）定时器 */
let fadeTimer = 0;
/** 拖动挡下隐藏后的补评定时器（拖完没有新事件就不会再有评估时机） */
let recheckTimer = 0;

/** 卡片是否处于淡出过程（含 win.hide() 在途）：淡出类挂上后只由 showCard 摘掉 */
function fadingOut(): boolean {
  return appEl.classList.contains("fade");
}

/** 上次评估时的行 key：识别「新会话行」（= 新的事件流）用 */
let knownRowKeys = new Set<string>();

function evaluateVisibility(rows: Row[] = currentRows()) {
  // 新会话行出现（新事件流 → 新 key）：弹出「一下」。只认会话行——
  // 驻留行里完成/失败本就有事档，「已中止」不弹（见 Row::attention）
  const newStream = rows.some((r) => r.session && !knownRowKeys.has(r.key));
  knownRowKeys = new Set(rows.map((r) => r.key));
  if (!autoHide) {
    cancelHide();
    showCard(); // 刚关掉自动隐藏且正隐藏 → 立即恢复常驻
    return;
  }
  if (rows.some((r) => r.attention)) {
    cancelHide();
    showCard();
    return;
  }
  // 新流的「一下」= 照常 2 秒后随安静隐藏；已可见时不抢已在走的计时
  if (newStream && (hidden || fadingOut())) {
    showCard();
    scheduleHide();
    return;
  }
  // 安静：已在计时就不重排（见规则第三条）；交互中或已隐藏/淡出也不排
  if (hidden || hovering || menuOpen || fadingOut() || hideTimer) return;
  if (draggingActive()) {
    // 拖动把隐藏挡住了，但拖完不一定还有评估时机（mouseup 会被 OS 拖拽循环
    // 吞掉、也不一定再来事件）：到拖动判定窗口结束补评一次
    scheduleRecheck(DRAG_ACTIVE_MS - (Date.now() - lastMoveAt) + 100);
    return;
  }
  scheduleHide();
}

/** 排一次「安静 2 秒后隐藏」（已排过就沿用旧计时，见规则第三条） */
function scheduleHide() {
  if (hideTimer) return;
  hideTimer = window.setTimeout(() => {
    hideTimer = 0;
    hideCard();
  }, HIDE_DELAY_MS);
}

/** 延后重评一次可见性（拖动判定这类「没有事件收尾」的场景用） */
function scheduleRecheck(ms: number) {
  window.clearTimeout(recheckTimer);
  recheckTimer = window.setTimeout(() => {
    recheckTimer = 0;
    evaluateVisibility();
  }, Math.max(ms, 0));
}

function cancelHide() {
  window.clearTimeout(hideTimer);
  hideTimer = 0;
}

/** 拖动是否仍在进行：mousedown 到最近一次移动后 DRAG_ACTIVE_MS 内都算 */
function draggingActive(): boolean {
  return dragging && Date.now() - lastMoveAt < DRAG_ACTIVE_MS;
}

function hideCard() {
  if (hidden || !autoHide || fadingOut()) return;
  if (hovering || menuOpen) return; // 交互开始了：悬停/菜单的收尾路径都会重新评估
  if (draggingActive()) {
    scheduleRecheck(DRAG_ACTIVE_MS - (Date.now() - lastMoveAt) + 100); // 同 evaluateVisibility 的拖动补评
    return;
  }
  appEl.classList.add("fade");
  const gen = ++visGen;
  fadeTimer = window.setTimeout(() => {
    fadeTimer = 0;
    win
      .hide()
      .then(() => {
        if (gen !== visGen) return; // 中途被 showCard 拉回：以最新状态为准
        hidden = true;
        hovering = false; // 窗口没了就没有悬停；真指着卡片的用户会重新触发 mouseenter
      })
      .catch((e) => {
        if (gen !== visGen) return;
        appEl.classList.remove("fade"); // 没藏住就恢复显示，别停在半透明
        console.warn("隐藏悬浮窗失败：", e);
      });
  }, FADE_MS + 20);
}

function showCard() {
  cancelHide();
  window.clearTimeout(fadeTimer);
  fadeTimer = 0;
  const gen = ++visGen; // 作废在途 hide 的完成回调
  const wasFading = fadingOut();
  if (!hidden && !wasFading) return; // 已完全显示：不动，也不发 IPC（每次渲染都会走到这里）
  if (hidden) {
    // 隐藏 → 弹出：先挂透明（窗口还不可见），show 后隔帧去掉 → 淡入过渡真跑一帧
    appEl.classList.add("fade");
    win
      .show()
      .then(() => {
        if (gen !== visGen) return;
        hidden = false;
        requestAnimationFrame(() =>
          requestAnimationFrame(() => {
            if (gen === visGen) appEl.classList.remove("fade");
          }),
        );
      })
      .catch((e) => {
        appEl.classList.remove("fade");
        console.warn("显示悬浮窗失败：", e);
      });
  } else {
    // 淡出途中被打断（win.hide() 可能已在路上）：摘掉淡出类拉回，
    // 再补一次 show() 兜底——若 hide 在途，后到的 show 会把它盖回去
    hidden = false;
    appEl.classList.remove("fade");
    win
      .show()
      .catch((e) => {
        // show 失败：hidden 必须复位为 true——状态机认为窗口可见的话，
        // 后续 showCard 会「已完全显示」早退，卡片就永久不可见了；
        // 复位后下一次评估照常重试
        if (gen === visGen) hidden = true;
        console.warn("显示悬浮窗失败：", e);
      });
  }
}

// ---- 拖动与位置记忆 ----

/** 落盘去抖定时器：onMoved 高频触发，停稳后才读位置写配置 */
let saveTimer = 0;

async function saveCurrentPosition() {
  // 菜单会把窗口临时撑高/上移（见 openMenu），期间的位置不是用户放置的，不落盘
  if (menuOpen || menuMoving) return;
  try {
    const [pos, scale] = await Promise.all([win.outerPosition(), win.scaleFactor()]);
    await api.saveWidgetPosition(Math.round(pos.x / scale), Math.round(pos.y / scale));
  } catch (e) {
    console.warn("保存悬浮窗位置失败：", e);
  }
}

function schedulePositionSave() {
  window.clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => void saveCurrentPosition().catch((e) => console.warn("保存悬浮窗位置失败：", e)), SAVE_DELAY_MS);
}

// 位置落盘的主路径是 onMoved，拖动收尾走 startDragging() 的 promise 完成（见下面
// mousedown 处理器里的 .finally）：Windows 上 startDragging 进 OS 模态移动循环后，
// WebView2 收不到松手时的 mouseup（只有小幅点按才收得到）——旧实现只靠 mouseup
// 复位 dragging，会「拖完了却没存」（下次打开回到移动前的位置），且 dragging 永久
// 卡 true（菜单不再自动收起、onMoved 的落盘守卫失效）。现在 dragging=false /
// 补落盘 / 重评可见性统一在 startDragging 落定的 .finally 里收尾，document mouseup
// 保留当兜底。onMoved 在拖动过程中持续触发，停稳后的最后一次就是最终位置。
// 菜单挪窗（menuMoving）期间的移动不算拖动，天然被 dragging 挡住。
void win.onMoved(() => {
  if (!dragging) return;
  lastMoveAt = Date.now(); // 自动隐藏据此判断拖动是否仍在进行（mouseup 会被拖拽循环吞掉）
  schedulePositionSave();
}).catch((e) => console.warn("订阅悬浮窗移动事件失败：", e));

// 按住即拖（startDragging 必须在 mousedown 处理器里调用）；固定位置后不再响应拖动。
// 双击打开主面板也在 mousedown 里判（e.detail >= 2）：第一下的 startDragging
// 会进 OS 拖拽循环、吞掉第二次 mouseup，`dblclick` 事件根本不会来——
// Tauri 自带的标题栏拖动脚本（window/scripts/drag.js）就是靠 detail 区分单击/双击的。
appEl.addEventListener("mousedown", (e) => {
  if (menuOpen) {
    void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err)); // 点在菜单外（卡片上）：只收菜单，不开始拖
    return;
  }
  if (e.button !== 0) return;
  if (e.detail >= 2) {
    void api.showPanel().catch((err) => console.warn("打开主面板失败：", err)); // 双击：打开主面板（与托盘菜单「显示主窗口」同一条命令路径）
    return;
  }
  if (pinned) return;
  dragging = true;
  // startDragging 的 promise 要到 OS 拖拽循环结束才落定，天然就是「拖完了」信号
  // （mouseup 会被拖拽循环吞掉，不能当收尾）：finally 里统一复位 dragging、
  // 补一次落盘（onMoved 已在拖动中持续排，这里兜住「停稳后没有新事件」的场景）、
  // 重评可见性（拖动挡下的隐藏从现在起重新计时）。document mouseup 保留当兜底。
  win
    .startDragging()
    .catch((e) => console.warn("启动拖动失败：", e))
    .finally(() => {
      dragging = false;
      schedulePositionSave();
      evaluateVisibility();
    });
});
document.addEventListener("mouseup", () => {
  if (!dragging) return;
  dragging = false;
  // 兜底路径（主路径是 startDragging 的 .finally，见上面 mousedown 处理器）：
  // 等 OS 拖拽循环安静下来再读最终位置（松手瞬间 outerPosition 可能还没落定）
  schedulePositionSave();
  evaluateVisibility(); // 拖动结束：安静的话从现在起计 2 秒隐藏
});

// 双击悬浮窗的动作并入上面的 mousedown（e.detail >= 2）——见那里的原因说明。

// ---- 右键菜单（固定位置 / 关闭悬浮窗）----

const menuEl = document.getElementById("ctx-menu") as HTMLElement;
let menuOpen = false;
/** 菜单挪窗期间（上移→还原）为 true：这期间的位置不是用户放置的，不许落盘 */
let menuMoving = false;
/** 菜单把窗口上移过时记住的原 y（逻辑像素），关闭菜单时还原；没动过为 null */
let menuSavedY: number | null = null;

/**
 * 菜单操作串行化（互斥）：openMenu / closeMenu 各自整段排队执行，绝不交错。
 * 没有它时「菜单开着又右键」会让旧 closeMenu 的还原 setSize 晚于新 openMenu 的
 * 撑高 setSize 生效——菜单被裁、窗口高度错乱，menuMoving 守卫也被破坏（§1.18）。
 * 队列 FIFO 保证「还原」要么整体在新打开之前、要么整体在之后，不可能插进中间。
 */
let menuChain: Promise<void> = Promise.resolve();

function queueMenuOp(op: () => Promise<void>): Promise<void> {
  // 前一段的成败不影响后一段；op 自身不抛，这里再兜一层
  const run = menuChain.then(op, op);
  menuChain = run.catch(() => {});
  return run;
}

/** 打开右键菜单（对外入口：排队串行执行，见 queueMenuOp） */
function openMenu(cx: number, cy: number): Promise<void> {
  return queueMenuOp(() => openMenuImpl(cx, cy));
}

/** 收起右键菜单（对外入口：排队串行执行，见 queueMenuOp） */
function closeMenu(): Promise<void> {
  return queueMenuOp(closeMenuImpl);
}

function renderMenuState() {
  document.getElementById("ctx-pin")!.classList.toggle("checked", pinned);
}

async function openMenuImpl(cx: number, cy: number) {
  // 防重入：菜单开着时再次右键不重新打开——否则 menuSavedY 会被覆写成上移后的 y，
  // closeMenu 还原到错误位置（底边附近的卡片会向上漂移，见 §1.18）
  if (menuOpen) return;
  cancelHide(); // 菜单打开期间不许隐藏（指针不一定触发过 mouseenter，别只靠悬停挡）
  menuMoving = true; // 从这里到 closeMenu 还原完，窗口的移动都不是用户放置
  renderMenuState();
  menuEl.style.display = "block";
  const mw = menuEl.offsetWidth;
  const mh = menuEl.offsetHeight;
  // 菜单顶边贴光标（Windows 惯例），横向夹在窗口内
  const mx = Math.min(Math.max(cx, 4), Math.max(4, CARD_W - mw - 4));
  let my = cy + 2;
  let upShift = 0;
  let needH = Math.round(my + mh + 4);
  try {
    const mon = await currentMonitor();
    if (mon) {
      const scale = mon.scaleFactor;
      const pos = await win.outerPosition();
      const winTop = pos.y / scale;
      const cursorY = winTop + cy;
      const screenBottom = (mon.position.y + mon.size.height) / scale;
      if (cursorY + mh + 6 > screenBottom) {
        // 菜单朝下会伸出屏幕底 → 朝上翻：底边贴光标上沿，窗口顶边抬到菜单上方
        const wantTop = cursorY - mh - 6;
        upShift = Math.max(0, winTop - wantTop);
        // 别把窗口抬出屏幕顶
        upShift = Math.min(upShift, winTop - mon.position.y / scale);
        // 窗口上移 upShift 后，同一屏幕位置在窗口里的相对 y 变小
        my = Math.max(4, cursorY - 2 - mh - (winTop - upShift));
        needH = Math.round(my + mh + 4);
      }
    }
    if (upShift > 0 || needH > expandedH) {
      if (upShift > 0) {
        const pos = await win.outerPosition();
        const scale = await win.scaleFactor();
        // 只在还没记过时写入原 y（§1.18）：任何路径都不许把已上移的 y 当成原位置，
        // 否则 closeMenu 会还原到错处
        if (menuSavedY === null) menuSavedY = pos.y / scale;
        await win.setPosition(new LogicalPosition(pos.x / scale, menuSavedY - upShift));
      }
      if (needH > expandedH) {
        await win.setSize(new LogicalSize(CARD_W, needH));
      }
    }
  } catch {
    /* 窗口几何操作失败就按原尺寸显示，菜单可能被裁一点，可接受 */
  }
  menuEl.style.left = `${mx}px`;
  menuEl.style.top = `${my}px`;
  menuOpen = true;
}

async function closeMenuImpl() {
  // menuOpen 为 false 但 menuMoving 为 true = 上一次 openMenu 中途失败留下的半开状态，
  // 窗口可能已被上移/撑高：这里同样要做完还原，别把 menuMoving 卡在 true
  if (!menuOpen && !menuMoving) return;
  menuOpen = false;
  menuEl.style.display = "none";
  const savedY = menuSavedY;
  menuSavedY = null;
  try {
    if (savedY !== null) {
      const pos = await win.outerPosition();
      const scale = await win.scaleFactor();
      await win.setPosition(new LogicalPosition(pos.x / scale, savedY));
    }
    await win.setSize(new LogicalSize(CARD_W, expandedH));
  } catch {
    /* 窗口可能已被销毁（右键 → 关闭悬浮窗） */
  }
  menuMoving = false;
  evaluateVisibility(); // 菜单收起后才允许隐藏（打开期间挂着），重新计 2 秒
}

document.addEventListener("contextmenu", (e) => {
  // 拦掉 WebView2 自带的右键菜单，换我们自己的两项目录
  e.preventDefault();
  void openMenu(e.clientX, e.clientY).catch((err) => console.warn("打开悬浮窗菜单失败：", err));
});
// 点菜单之外（窗口内的任何地方）收起；点菜单项本身不收（由 click 处理器收）
document.addEventListener("mousedown", (e) => {
  if (menuOpen && !menuEl.contains(e.target as Node)) {
    void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err));
  }
});
// 悬停挂起自动隐藏：鼠标在卡片上（正在读 / 拖 / 点菜单）时不许窗口跑掉。
// 淡出途中鼠标进来了也算——showCard 会把淡出拉回
document.addEventListener("mouseenter", () => {
  hovering = true;
  cancelHide();
  if (fadingOut()) showCard();
});
// 光标离开窗口也收起：窗口外点击我们收不到，菜单会一直悬在别的应用上面。
// 同时解除悬停挂起：安静的话从离开起重新计 2 秒隐藏（悬停期间不计时）
document.addEventListener("mouseleave", () => {
  hovering = false;
  if (!dragging) void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err));
  evaluateVisibility();
});

document.getElementById("ctx-pin")!.addEventListener("click", async () => {
  pinned = !pinned;
  try {
    await api.setWidgetPinned(pinned);
  } catch (e) {
    console.warn("保存固定状态失败：", e);
  }
  await closeMenu();
});
document.getElementById("ctx-exit")!.addEventListener("click", async () => {
  try {
    await api.closeWidget();
  } catch (e) {
    console.warn("关闭悬浮窗失败：", e);
  }
  await closeMenu();
});

// ---- 启动 ----

void Promise.all([
  listen<SessionStatus[]>("bark://sessions", (e) => {
    // 推送负载兜底：null / 非数组忽略（并作废旧拉取），别让坏推送把渲染冻在旧帧
    if (!Array.isArray(e.payload)) {
      console.warn("会话快照负载异常，已忽略：", e.payload);
      return;
    }
    sessionsSeq += 1;
    sessions = e.payload;
    render();
  }),
  listen<NormalizedEvent>("bark://event", (e) => onEvent(e.payload)),
  // 悬浮窗设置页的「自动隐藏」开关热更新（Rust 侧 widget::sync 推送，不重建窗口）：
  // 关掉时即使正隐藏/淡出也立即恢复常驻
  listen<{ auto_hide: boolean }>("bark://widget-config", (e) => {
    if (!e.payload || typeof e.payload.auto_hide !== "boolean") {
      console.warn("悬浮窗配置推送负载异常，已忽略：", e.payload);
      return;
    }
    autoHide = e.payload.auto_hide;
    evaluateVisibility();
  }),
]).catch((e) => console.warn("订阅悬浮窗事件失败：", e));
void loadSessions().catch((e) => console.warn("拉取会话快照失败：", e));
render();
