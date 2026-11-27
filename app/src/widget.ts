// 桌面悬浮窗（对应 app/widget.html）。
//
// 独立入口，与主窗口的 Vue 应用无关。职责只有两件事：
// 1. 展示「当前活动」：进行中的会话（图标 + 四态状态）+ 最近一次
//    完成/失败事件的短暂驻留——状态口径与事件流、屏幕光效完全一致；
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
// 自由悬浮时常驻；停靠时托盘单击或会话状态变化临时展开，悬停保持展开。
// 靠边隐藏（widget.dock_side）：拖到顶部/左右外缘后收为窄边，悬停展开；
// 与菜单/详情共用窗口操作队列，位置记忆始终保存横向卡片的展开位置。

import { emitTo, listen } from "@tauri-apps/api/event";
import { availableMonitors, cursorPosition, currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, type Monitor } from "@tauri-apps/api/window";
import {
  api,
  eventLabel,
  agentIcon,
  agentName,
  isSubagentEvent,
  isSubagentSession,
  phaseLabel,
  placeholderName,
  SESSION_INDENT_MAX,
  sessionRows,
  type NormalizedEvent,
  type SessionRow,
  type SessionStatus,
} from "./api";
import { initI18n, onLocaleChanged, t } from "./i18n";
import { initTheme } from "./theme";
import type { TipRequest } from "./widget-tip";
import { CARD_WIDTH, MAX_LIST_HEIGHT, fitPosition, splitName, SubagentLabels, type Rect } from "./widget-layout";
import { containsPoint, dockRect, edgeAtPoint, edgeFrame, edgeMotion, edgeTransform, edgeThreshold, isMacOS, type EdgeSide, type Point } from "./widget-edge";
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
  dock_side?: EdgeSide | null;
  detail_tip?: boolean;
}
const init: WidgetInit =
  (window as unknown as { __WIDGET_INIT__?: WidgetInit }).__WIDGET_INIT__ ??
  { x: 60, y: 60, width: CARD_WIDTH, pinned: false };

const CARD_W = CARD_WIDTH;
const PAD_V = 12;
const SUB_ALIGN_PX = 30;
const IDLE_H = 44;
const subagentLabels = new SubagentLabels((n) => t("floating.subagent", { n: String(n).padStart(2, "0") }));
/** 完成/失败事件的驻留时长：与屏幕光效的完成色停留同量级 */
const TERMINAL_HOLD_MS = 6000;
/** 拖动停止多久后再读窗口位置落盘（等 OS 拖拽循环安静下来） */
const SAVE_DELAY_MS = 220;
/** 状态变化后保持展开的时间。 */
const REVEAL_MS = 5000;
/** 托盘单击只需短暂展开，方便定位。 */
const TRAY_REVEAL_MS = 2000;

type StateColor = "thinking" | "waiting" | "completed" | "failed" | "aborted" | "paused";

let pinned = init.pinned;
/** 行悬停详情面板开关（右键菜单切换，落盘 widget.detail_tip）。关闭则不发起悬停请求。 */
let detailTip = init.detail_tip ?? true;
/** 详情独立窗口 label（与 Rust TIP_LABEL 一致）；行悬停意图经事件发给它 */
const TIP_LABEL = "widget-tip";
let dragging = false;
/** 卡片高度（随行数变化），窗口尺寸跟随它 */
let expandedH = IDLE_H;
let dockSide: EdgeSide | null = init.pinned ? null : init.dock_side ?? null;
let dockOrigin: Point = { x: init.x, y: init.y };
let dockScreen: Rect | null = null;
let dockScale = 1;
let dockMonitor: string | null = null;
let edgeProgress = dockSide ? 1 : 0;
let edgeTarget = edgeProgress;
let edgeMoving = false;
let interruptEdgeMotion: (() => void) | null = null;
let edgeTimer = 0;
let edgeHoldUntil = 0;
let edgeReady = !dockSide;
const EDGE_LEAVE_MS = 350;
/** 等浏览器提交一帧的超时兜底：后台/托盘唤醒时 rAF 可能停摆，但不能让展开卡住。
 *  80ms 覆盖 30Hz 下两帧（≈66ms）并留余量；正常 60Hz 下双 rAF 约 32ms 就会先返回。 */
const EDGE_PAINT_TIMEOUT_MS = 80;

/**
 * 上一帧「是否存在多个工作区」的判定（由 [`currentRows`] 每次渲染更新）。
 *
 * 终态驻留行在 `onEvent` 里就要定名字，那时没有整份快照可遍历。它只决定终态行的行内
 * 标签是否使用工作区名——不带的话，跨工作区时它跟另一个工作区的行长得一样，
 * 会被读成「挂在别人下面」（实测踩过）。
 */
let lastMixedWorkspaces = false;

interface Row {
  key: string;
  sessionId?: string;
  agent: string;
  /** 主标签：单工作区显示 agent，多工作区显示项目，子代理使用稳定编号。 */
  name: string;
  /**
   * 详情正文：完整工作区名。没有工作区时由渲染层回退为完整标签。
   * 行节点按身份复用，更新快照时保持已展开的详情与滚动位置。
   */
  tip: string;
  /** 仅活跃会话携带发起时间，终态和占位行不继续计时。 */
  startedAt?: number;
  label: string;
  state: StateColor;
  /**
   * 这一行是**子代理**（派生出来的会话），不做「深度 0 = 根会话」的推断。
   *
   * 为什么必须显式带：`depth` 会被改（「手动终止」摘掉占位父行后，它下面的子代理顶格变成
   * `depth = 0`，见 [`currentRows`]），而图标 / ↳ / 弱化样式原先都拿 `depth` 判断，
   * 于是降级后的子代理行会被渲染成**带根行图标、无 ↳、不缩进**的样子（实测被用户抓到：
   * 「子代理标签 + 根行图标」同时出现）。判断「是不是子代理」用这个标记，判断「缩进多少」
   * 才用 `depth`。
   */
  subagent: boolean;
  /** 子代理行的缩进层级（0 = 主会话 / 驻留行 / 孤儿行）；>0 时行首渲染 ↳ 并按级缩进 */
  depth: number;
  /**
   * 父会话不在快照里：这行仍是子代理，但它上面会有一行**占位父行**兜着，
   * 所以照常缩进也不会被误读成「挂在上一行那个会话下」（见 api.ts 的 sessionRows）
   */
  parentMissing: boolean;
  /** 占位父行（合成的，快照里没有这个会话）：弱化渲染，不当真实会话看待 */
  placeholder: boolean;
  /**
   * 驻留行（终态）的父会话 id：用来把这行插到父行子树的正下方。
   *
   * 追加到列表末尾会让子代理的「任务完成」行落在**最后一行会话**的子树里，看着像挂在
   * 别的会话（尤其另一个工作区）下面（实测踩过）。
   */
  parentId?: string | null;
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
    if (seq === sessionsSeq) receiveSessions(s); // 期间已有新推送接管：丢弃这份旧拉取
  } catch {
    /* 静默：推送通道可能仍在 */
  }
}

/** 与行的显示颜色共用分类；思考和执行工具都属于工作状态。 */
function sessionState(phase: SessionStatus["phase"]): StateColor {
  return phase === "waiting_permission" || phase === "waiting_input" ? "waiting"
    : phase === "waiting_children" || phase === "settling" ? "paused" : "thinking";
}

/** 只按会话增减和显示状态类别判断变化，忽略同类阶段切换、心跳和工具计数。 */
let knownSessionStates: Map<string, StateColor> | null = null;
function receiveSessions(next: SessionStatus[]) {
  const states = new Map(next.map((session) => [sessionRowKey(session.agent, session.session_id), sessionState(session.phase)]));
  const changed = knownSessionStates !== null && (states.size !== knownSessionStates.size
    || [...states].some(([key, state]) => knownSessionStates!.get(key) !== state));
  knownSessionStates = states;
  sessions = next;
  render();
  if (changed) revealTemporarily();
}

const seenTerminalEvents = new Set<string>();

/** Multiple workspaces use project names; icons and details retain the agent identity. */
function rowLabel(agent: string, project: string | null, mixedWorkspaces: boolean): string {
  return mixedWorkspaces && project ? project : agentName(agent);
}
function subagentLabel(agent: string, sessionId: string): string {
  return subagentLabels.get(sessionRowKey(agent, sessionId));
}
function rowTip(project: string | null): string { return project ?? ""; }

/**
 * 已「手动终止」的会话行：`agent|session_id` → 标记时刻。
 *
 * 用户按停止时**不合成新行**，而是把已经显示着的那一行就地标成「手动终止」（中性色）。
 * 为什么不用驻留行：那会多出一行「主的」，或者（父行已不在快照
 * 里时）合成一行 `会话 …尾号` —— 一串 id 尾号既难认、也不是用户想看的（实测被点名）。
 *
 * daemon 收到终态就把条目摘掉，所以这一行可能只存在一帧。为了让它**看得清**，标记会
 * 保留 [`ABORT_HOLD_MS`]：行从快照里消失后，用最后见过的样子把这一行补在列表末尾
 * （见 `currentRows`），到期自然退场。
 */
const abortedRows = new Map<string, number>();
/**
 * 手动终止行的保留时长。取 5s：与临时展开（`REVEAL_MS`）同量级，够看清、
 * 又不会在卡片上赖着不走。
 */
const ABORT_HOLD_MS = 5000;
/**
 * 父会话被手动终止、但它的子代理还在跑时，**子代理行**的文案。
 *
 * 不沿用它们各自的事件相位（执行工具 / 思考中）：那是**父回合**的状态，父都被终止了还写
 * 「执行工具」会让人以为父还在干活。这四个字说的才是实情——子代理收尾之前，主会话这边
 * 不再有别的动静（用户的原话：主会话会等子会话自然结束后再被唤起）。
 * 与「等待子代理」相位同词：复用词表 phases.waiting_children，按当前语言取词。
 */
function awaitLabel(): string {
  return t("phases.waiting_children");
}

/** 相位显示名：未知 phase（后端新增/坏推送）原样显示，不把 i18n key 漏给用户 */
function phaseText(phase: SessionStatus["phase"]): string {
  const label = phaseLabel(phase);
  return label === `phases.${phase}` ? String(phase) : label;
}

/** 会话行/事件的统一身份键：与 api.ts 的 `visited` 同口径（`agent|session_id`） */
export function sessionRowKey(agent: string, sessionId: string): string {
  return `${agent}|${sessionId}`;
}

/**
 * 「手动终止」行的展示信息快照（按 [`sessionRowKey`] 存）。
 *
 * daemon 摘掉会话之后就再也算不出这一行的名字了（`subagentLabel` 需要「是否多工作区」、
 * `rowLabel` 需要工作区名），所以在它**还在快照里**的那几帧把名字/提示存下来，
 * 供保留期内重建这一行时使用。
 */
const abortedMemo = new Map<string, { name: string; tip: string }>();

/**
 * 「上一帧见到的样子」——**每条真实行**都记（键同 [`sessionRowKey`]），不只记手动终止的。
 *
 * 用途：父行从快照里消失、而它的子代理还在跑时，`sessionRows` 会合成一行占位父行
 * （`会话 …尾号`）当落脚点。一串 id 尾号既认不出是哪个 agent、也不好看（用户点名过两次），
 * 而这个会话本 agent 名我们**上一帧刚见过**——直接拿来用，标签降级成「已结束」。
 *
 * 与 [`abortedMemo`] 分家的原因：那个只在「手动终止」时写、且语义是「这一行被谁终止了」；
 * 这份是纯展示缓存，真实行渲染过就进（父行正常结束 / 已从本地显示移除也走这条路）。
 * **占位行不进**：它的名字是合成的，写进来之后还原时读到的就是它自己，真名字与下面的
 * 兜底永远轮不到（实测：主会话在子代理跑着的那段时间一直显示 `会话 …6ee0ea`）。
 */
const lastSeenRow = new Map<string, { name: string; tip: string }>();

/** 完成/失败事件 → 驻留行（思考/等待由会话表表达，终态靠这里短暂展示） */
function onEvent(ev: NormalizedEvent) {
  // 推送负载兜底：null / 异常负载直接忽略，别把整个渲染循环冻在旧帧
  if (!ev || typeof ev !== "object") {
    console.warn("事件推送负载异常，已忽略：", ev);
    return;
  }
  // 「是不是子代理行」只有这一个判据：名字选 `子代理 NN`，缩进 / 连接线 / 图标（见 `render`）
  // 也全看它。两个口径会拼出「子代理标签 + 根行图标」这种自相矛盾的一行（实测抓到过）。
  const sub = isSubagentEvent(ev);
  const map: Partial<Record<NormalizedEvent["type"], [StateColor, string]>> = {
    run_completed: ["completed", eventLabel("run_completed")],
    run_failed: ["failed", eventLabel("run_failed")],
    run_aborted: ["aborted", eventLabel("run_aborted")],
  };
  if (ev.completion_deferred) return;
  const hit = map[ev.type];
  if (!hit || seenTerminalEvents.has(ev.id)) return;
  seenTerminalEvents.add(ev.id);
  if (seenTerminalEvents.size > 128) seenTerminalEvents.delete(seenTerminalEvents.values().next().value!);
  if (ev.type === "run_aborted") {
    const key = sessionRowKey(ev.agent, ev.session_id);
    // 手动终止：**只把已经显示着的那一行就地标成「手动终止」**（见 abortedRows），
    // 绝不因此多出一行。合成行（`会话 …尾号`）是给「父行已不在快照里」的孤儿兜底用的，
    // 拿它表达「你自己按了停止」既难认（一串 id 尾号）又多出一行主的（实测被用户点名）。
    abortedRows.set(key, Date.now());
    // 顺手记下这一行「长什么样」——**用事件自己算，不依赖当时的快照**。
    // 为什么不依赖：abort 事件与快照推送是两条独立通道，事件到达时 `sessions` 可能已经
    // 不含这一行了（daemon 收到终态立刻摘条目），那时算不出名字 → 合成不出「手动终止」行
    // → 卡片上只剩孤儿子代理顶着的那行占位父行（`会话 …尾号`，实测被用户点名）。
    // 只补空档：已经在快照里就地标过的那种，用 `currentRows` 存下来的更准
    if (!abortedMemo.has(key)) {
      abortedMemo.set(key, {
        name: sub ? subagentLabel(ev.agent, ev.session_id) : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
        tip: ev.project ?? "",
      });
    }
    scheduleExpiry();
    render();
    revealTemporarily();
    return;
  }
  abortedRows.delete(sessionRowKey(ev.agent, ev.session_id));
  abortedMemo.delete(sessionRowKey(ev.agent, ev.session_id));
  terminal = {
    key: ev.id,
    sessionId: ev.session_id,
    agent: ev.agent,
    // 驻留行与它**取代的那一行同名**：跨工作区时会话行带 ` · 工作区名`、子代理行写
    // 「子代理」。名字不一致就会被读成「挂在别人下面」（实测踩过：子代理收尾那一瞬间，
    // 它的「任务完成」行看着挂到了另一个工作区的会话下）。单一工作区时才用全称——
    // 那时没有歧义，缩写反而是丢信息
    name: sub ? subagentLabel(ev.agent, ev.session_id) : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
    tip: ev.project ?? "",
    label: hit[1],
    state: hit[0],
    subagent: sub,
    // 子代理的终态行跟着它的父会话**同一个工作区**（同一个 agent 的终态事件本就该按事件里
    // 的 agent 名显示），缩进与子代理行一致
    depth: sub ? 1 : 0,
    parentMissing: false,
    placeholder: false,
    parentId: ev.parent_session_id ?? null,
    at: Date.now(),
  };
  scheduleExpiry();
  render();
  revealTemporarily();
}

let expiryTimer = 0;
function scheduleExpiry() {
  window.clearTimeout(expiryTimer);
  const now = Date.now();
  const deadlines = [...abortedRows.values()].map((at) => at + ABORT_HOLD_MS).filter((at) => at > now);
  if (terminal) deadlines.push(terminal.at + TERMINAL_HOLD_MS);
  if (!deadlines.length) return;
  expiryTimer = window.setTimeout(() => {
    if (terminal && Date.now() - terminal.at >= TERMINAL_HOLD_MS) terminal = null;
    render();
    scheduleExpiry();
  }, Math.max(0, Math.min(...deadlines) - now) + 50);
}

/**
 * 找出挂在某些行**下面**的所有行（按深度算，不看血缘 id）。
 *
 * 用深度而不是 `parentId`：树的顺序是 api 排好的（父行紧跟其子树），悬浮窗这里只是按
 * 深度推进栈。返回 `keys` 以便调用方在这些行被摘掉后把子树顶格。
 */
function rowsUnder(rows: Row[], roots: Set<string>): Set<string> {
  const prefixes: number[] = [];
  const under = new Set<string>();
  for (const r of rows) {
    while (prefixes.length && r.depth <= prefixes[prefixes.length - 1]) prefixes.pop();
    if (prefixes.length) under.add(r.key);
    if (roots.has(r.key)) prefixes.push(r.depth);
  }
  return under;
}

function currentRows(): Row[] {
  // 排序 + 挂树交给 api（与事件流的「运行中会话」区共用一份口径）：后端快照按
  // started_at 倒序平铺，直接用会让子代理与父行交错、看不出谁派生了谁。
  // 不截断：全部会话都保留，超出可用高度时在卡片内滚动。
  const list = sessionRows(sessions);
  // 手动终止标记的过期：**还有活着的子代理引用这个父会话就不清**。
  //
  // 为什么不能按固定时长硬清：daemon 收到终态立刻摘掉父条目（`state.rs` 的 sessions.remove），
  // 而子代理还要跑很久。硬清之后父行只剩一行合成占位行 `会话 …尾号`——实测用户停止 51 秒后
  // 截图看到的正是它（事件本身到得了前端：DSH 的 `turn/end` 带 `reason.kind=aborted`，
  // 插件归一成 `run_aborted`，只是我把它 5 秒就丢了）。
  const now = Date.now();
  const stillHasChild = new Set<string>();
  for (const { session: s } of list) if (s.parent) stillHasChild.add(sessionRowKey(s.agent, s.parent));
  for (const [k, t] of abortedRows) {
    if (now - t > ABORT_HOLD_MS && !stillHasChild.has(k)) {
      abortedRows.delete(k);
      abortedMemo.delete(k);
    }
  }
  // 跨工作区判定（只数真实会话行）：≥2 个不同的工作区才把工作区名写进行内标签
  const projects = new Set<string>();
  for (const { session: s, placeholder } of list) {
    if (!placeholder && s.project) projects.add(s.project);
  }
  const mixedWorkspaces = projects.size > 1;
  // 驻留行在 `onEvent` 里就定好了名字，那时拿不到这里的判定（要遍历整份快照）——
  // 记在模块级供它取用。它只影响「跨工作区时行内要不要带工作区后缀」
  lastMixedWorkspaces = mixedWorkspaces;
  // 本轮快照里被标了「手动终止」的会话（键）——稍后用来把父行就地留下、并摘掉占位行
  // **直接从标记表填**，不要只在「这一行还在快照里」时填：daemon 摘条目的速度可能快过
  // 快照推送（事件与快照是两条独立通道），那时 `list` 里已经没有这一行，只在表格里补键
  // 就会漏掉整条常驻逻辑（实测踩过：父行消失、只剩合成占位行 `会话 …尾号`）
  const abortedKeys = new Set<string>(abortedRows.keys());
  const rows: Row[] = list.map(({ session: s, depth, parentMissing, placeholder }: SessionRow) => {
    const state = sessionState(s.phase);
    const sub = isSubagentSession(s);
    // 手动终止：把**已有的这一行**就地改成「手动终止」（见 abortedRows）。它是终态，
    // 所以不再使用「等待中」的等待色。
    const aborted = abortedRows.has(sessionRowKey(s.agent, s.session_id));
    // 这一行还在快照里：把「上一帧见到的样子」记准（常驻父行会用到）
    if (aborted && !placeholder) {
      abortedMemo.set(sessionRowKey(s.agent, s.session_id), {
        name: sub ? subagentLabel(s.agent, s.session_id) : rowLabel(s.agent, s.project, mixedWorkspaces),
        tip: rowTip(s.project),
      });
    }
    // 占位父行没有 agent 级信息可取（它不是快照里的会话）：显示 `会话 …尾号`；
    // 子代理行写「子代理」而不是工作区名（它必然与父同工作区，见 subagentLabel）
    const name = placeholder
      ? placeholderName(s.session_id)
      : sub
        ? subagentLabel(s.agent, s.session_id)
        : rowLabel(s.agent, s.project, mixedWorkspaces);
    const tip = rowTip(s.project);
    // 只记**真实行**：占位行的名字是合成的（`会话 …尾号`），记进「上一帧见到的样子」后，
    // 还原占位行时读回来的就是它自己——真名字与下面的兜底永远轮不到（实测：主会话在
    // 子代理跑着的那段时间一直显示 `会话 …6ee0ea`，正是被自己覆盖的结果）。
    if (!placeholder) lastSeenRow.set(sessionRowKey(s.agent, s.session_id), { name, tip });
    return {
      key: s.session_id,
      sessionId: s.session_id,
      agent: s.agent,
      name,
      // 子代理详情同样保留完整工作区，方便确认归属。
      tip,
      startedAt: placeholder || aborted ? undefined : s.started_at,
      // 未知 phase 兜底显示原值：undefined 漏进 escapeHtml 会抛 TypeError、悬浮窗冻结在旧帧
      label: placeholder ? t("events.ended") : aborted ? eventLabel("run_aborted") : phaseText(s.phase) + (s.phase === "waiting_children" && s.active_descendants ? ` · ${s.active_descendants}` : ""),
      state: aborted ? "aborted" : state,
      subagent: sub,
      depth,
      parentMissing,
      placeholder,
    };
  });
  // 占位父行一律还原成**真名字**：只要这一行还有子在跑，就没理由摆一串 id 尾号
  // （用户点名过两次）。名字取「上一帧见到的样子」；实在没见过（widget 刚启动、快照里
  // 从头到尾没出现过父行——典型是 daemon 重启后父会话只剩保活心跳）就按**真实行的同一
  // 口径**现算：单一工作区用 agent 名，跨工作区用「简称 · 工作区」。工作区来自子代理
  // （见 api.ts 的 ghostSession），所以这条兜底同样带得出 dsh-git-panel 这样的归属。
  {
    const seen = new Set<string>();
    for (let i = 0; i < rows.length; i++) {
      const row = rows[i];
      if (!row.placeholder || seen.has(row.key)) continue;
      const key = sessionRowKey(row.agent, row.key);
      // 先算出这一行的子树（必须在本行被替换之前算——树序信息在这一步还在）
      const subtree = rowsUnder(rows, new Set([row.key]));
      if (!subtree.size) continue; // 没有子在跑：占位行失去意义，交给下面收掉
      seen.add(row.key);
      const memo = abortedMemo.get(key);
      const last = lastSeenRow.get(key);
      const aborted = abortedKeys.has(key);
      rows[i] = {
        key: row.key,
        agent: row.agent,
        name: memo?.name ?? last?.name ?? rowLabel(row.agent, row.tip || null, lastMixedWorkspaces),
        tip: memo?.tip ?? last?.tip ?? row.tip,
        label: aborted ? eventLabel("run_aborted") : t("events.ended"),
        state: aborted ? "aborted" : "completed",
        subagent: false,
        depth: 0,
        parentMissing: false,
        placeholder: false,
      };
      if (aborted) {
        for (const r of rows) {
          if (!subtree.has(r.key) || !r.subagent) continue;
          r.label = awaitLabel();
          r.state = "thinking";
        }
      }
    }
  }
  if (terminal) {
    const left = TERMINAL_HOLD_MS - (Date.now() - terminal.at);
    if (left > 0) {
      // 插到父行**子树**的正下方，而不是列表末尾：追加到末尾会让子代理的「任务完成」
      // 行落在最后一行会话（可能是另一个工作区的）的子树里，看起来就像挂到了那个会话
      // 下面（实测踩过）。
      let pi = terminal.parentId ? rows.findIndex((r) => r.key === terminal!.parentId) : -1;
      // 父行连列表都不在（daemon 刚重启、父会话只剩保活心跳；或父回合已结束被摘档）：
      // 子代理的终态行会孤零零地落在末尾，卡片上只剩「子代理 01 · 任务完成」，看着像
      // 子代理没有主会话（用户实测点名）。这里按子代理终态事件自带的 agent / 工作区
      // 补一行「已结束」父行，让驻留行照常挂在它下面——与 sessionRows 的占位父行同一口径。
      if (pi < 0 && terminal.subagent && terminal.parentId) {
        // 名字与占位行还原同一条兜底链：上一帧见过就用它（父行刚被摘档时这是唯一的真名与
        // 工作区），没见过才按真实行的同一口径现算。
        const seenParent = lastSeenRow.get(sessionRowKey(terminal.agent, terminal.parentId));
        rows.push({
          key: terminal.parentId,
          agent: terminal.agent,
          name: seenParent?.name ?? rowLabel(terminal.agent, terminal.tip || null, lastMixedWorkspaces),
          tip: seenParent?.tip || terminal.tip,
          label: t("events.ended"),
          state: "completed",
          subagent: false,
          depth: 0,
          parentMissing: false,
          // 不标 `placeholder`：既有「还有子在跑的占位父行」在悬浮窗里还原后同样按正常行
          // 强度渲染（弱化会让人读成另一条临时会话），页脚口径也与之一致。
          placeholder: false,
        });
        pi = rows.length - 1;
      }
      if (pi < 0) {
        rows.push(terminal);
      } else {
        const pd = rows[pi].depth;
        let end = pi + 1;
        while (end < rows.length && rows[end].depth > pd) end++;
        // 驻留行跟着父行再深一级：父行本身就是子代理时（孙代），写死 1 级会和父行同缩进
        const inserted = terminal.subagent
          ? { ...terminal, depth: Math.min(pd + 1, SESSION_INDENT_MAX) }
          : terminal;
        rows.splice(end, 0, inserted);
      }
    } else terminal = null;
  }
  return rows;
}

// ---- 渲染 ----

function escapeHtml(s: unknown): string {
  // 入参一律先转字符串：agentName / phase 兜底值都可能漏进 undefined，
  // 直接 .replace 会抛 TypeError（悬浮窗会冻在旧帧）。
  // 不转单引号：本文件的拼接点全是**双引号属性或文本节点**（如 data-tip="…"）；
  // 哪天真往 `'…'` 属性里拼，这里要连 `'` → `&#39;` 一起加。
  return String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
}

const cardEl = document.createElement("div");
cardEl.className = "card";
const listEl = document.createElement("div");
listEl.className = "session-list";
const footerEl = document.createElement("div");
footerEl.className = "list-footer";
footerEl.hidden = true;
cardEl.append(listEl, footerEl);
appEl.append(cardEl);
const edgeHandle = document.createElement("div");
edgeHandle.id = "edge-handle";
// aria-label 由 applyStaticTexts 按当前语言填充（语言切换时就地重写）
document.body.append(edgeHandle);
const rowElements = new Map<string, HTMLElement>();
let maxListHeight = MAX_LIST_HEIGHT;
let listContentHeight = 32;

function render() {
  const list = currentRows();
  const keep = new Set<string>();
  const labelKeys = new Set(sessions.map((s) => sessionRowKey(s.agent, s.session_id)));
  for (const key of abortedRows.keys()) labelKeys.add(key);
  if (terminal?.sessionId) labelKeys.add(sessionRowKey(terminal.agent, terminal.sessionId));
  subagentLabels.retain(labelKeys);
  list.forEach((r, index) => {
    const key = sessionRowKey(r.agent, r.key);
    keep.add(key);
    let el = rowElements.get(key);
    if (!el) { el = document.createElement("div"); rowElements.set(key, el); }
    const group = index > 0 && !r.subagent && list[index - 1].subagent;
    const lastChild = r.subagent && (!list[index + 1] || list[index + 1].depth < r.depth || !list[index + 1].subagent);
    el.className = "row-item" + (r.subagent ? " sub" : "") + (r.placeholder ? " placeholder" : "") + (group ? " group-start" : "") + (lastChild ? " last-child" : "");
    el.dataset.state = r.state;
    el.dataset.key = key;
    el.dataset.tip = r.tip || r.name;
    el.dataset.tipHeading = agentName(r.agent) + (r.subagent ? " · " + r.name : "");
    el.dataset.startedAt = r.startedAt === undefined ? "" : String(r.startedAt);
    const pad = r.subagent ? SUB_ALIGN_PX + Math.max(0, Math.min(r.depth, SESSION_INDENT_MAX) - 1) * 11 : 7;
    el.style.paddingLeft = pad + "px";
    el.style.setProperty("--indent", pad + "px");
    const [start, tail] = splitName(r.name);
    const icon = r.subagent ? '<span class="tree" aria-hidden="true"></span>' : '<span class="agent-icon">' + agentIcon(r.agent) + '</span>';
    const pin = pinned && index === 0 ? '<span class="pin-mark" aria-label="' + escapeHtml(t("floating.pinnedAria")) + '">⌖</span>' : '';
    const markup = icon + '<span class="name"><span class="name-start">' + escapeHtml(start) + '</span><span class="name-tail">' + escapeHtml(tail) + '</span></span>' + pin + '<span class="state ' + r.state + '"><i class="dot" aria-hidden="true"></i>' + escapeHtml(r.label) + '</span>';
    if (el.innerHTML !== markup) el.innerHTML = markup;
    if (listEl.children[index] !== el) listEl.insertBefore(el, listEl.children[index] ?? null);
  });
  for (const [key, el] of rowElements) if (!keep.has(key)) { el.remove(); rowElements.delete(key); }
  const idle = listEl.querySelector(".idle");
  if (list.length) idle?.remove();
  else if (!idle) listEl.innerHTML = '<div class="row-item idle"><img src="' + logo + '" alt=""><span>' + escapeHtml(t("floating.idle")) + '</span></div>';
  listContentHeight = list.length ? list.reduce((height, row, index) => height + (row.subagent ? 29 : 32) + (index > 0 && !row.subagent && list[index - 1].subagent ? 6 : 0), 0) : 32;
  const attentionCount = list.filter((r) => r.state === "waiting" || r.state === "failed").length;
  footerEl.innerHTML = '<span>' + escapeHtml(t("floating.footerSessions", list.filter((r) => !r.placeholder).length)) + '</span><b>' + escapeHtml(t("floating.footerAttention", attentionCount)) + '</b>';
  updateCardHeight();
  void syncWindowSize().then(() => { refreshTip(); if (dockSide) evaluateVisibility(); }).catch((e) => console.warn("同步悬浮窗尺寸失败：", e));
  evaluateVisibility();
}

function updateCardHeight() {
  listEl.style.maxHeight = maxListHeight + "px";
  footerEl.hidden = listContentHeight <= maxListHeight;
  expandedH = PAD_V + Math.min(listContentHeight, maxListHeight) + (footerEl.hidden ? 0 : 31);
  document.body.style.setProperty("--card-h", expandedH + "px");
}

async function screenGeometry() {
  const [pos, scale, mon] = await Promise.all([win.outerPosition(), win.scaleFactor(), currentMonitor()]);
  const monitorScale = mon?.scaleFactor ?? scale;
  const ratio = Number.isFinite(monitorScale) && monitorScale > 0 ? monitorScale : 1;
  const work = mon?.workArea;
  return {
    scale: ratio,
    monitorName: mon?.name ?? null,
    position: { x: pos.x / ratio, y: pos.y / ratio },
    screen: {
      x: (work?.position.x ?? mon?.position.x ?? 0) / ratio,
      y: (work?.position.y ?? mon?.position.y ?? 0) / ratio,
      width: (work?.size.width ?? mon?.size.width ?? 1920 * ratio) / ratio,
      height: (work?.size.height ?? mon?.size.height ?? 1080 * ratio) / ratio,
    } satisfies Rect,
    // 完整显示器矩形（含菜单栏）：macOS 顶部吸附贴屏幕物理上沿用
    full: {
      x: (mon?.position.x ?? 0) / ratio,
      y: (mon?.position.y ?? 0) / ratio,
      width: (mon?.size.width ?? 1920 * ratio) / ratio,
      height: (mon?.size.height ?? 1080 * ratio) / ratio,
    } satisfies Rect,
  };
}

function syncWindowSize() {
  return queueMenuOp(async () => {
    if (menuOpen || menuMoving || dragging) return;
    const { position, screen, full, scale, monitorName } = await screenGeometry();
    if (menuOpen || menuMoving || dragging) return;
    maxListHeight = Math.min(MAX_LIST_HEIGHT, Math.max(32, screen.height - PAD_V - 31 - 120));
    updateCardHeight();
    if (dockSide) {
      // macOS 顶部吸附与 dockTarget 同口径：贴屏幕物理上沿（工作区上沿在菜单栏
      // 下方，否则吸附完成后这里会把触发条搬回菜单栏下面）；其余按工作区
      const snap = dockSide === "top" && isMacOS() ? full : screen;
      dockScreen = snap;
      dockScale = scale;
      dockMonitor = monitorName;
      dockOrigin = dockRect({ ...dockOrigin, width: CARD_W, height: expandedH }, snap, dockSide);
      await applyEdgeFrame();
      edgeReady = true;
      return;
    }
    const next = fitPosition({ ...position, width: CARD_W, height: expandedH }, screen);
    await win.setSize(new LogicalSize(CARD_W, expandedH));
    if (next.x !== position.x || next.y !== position.y) await win.setPosition(new LogicalPosition(next.x, next.y));
  });
}

// ---- 靠边隐藏：窗口只覆盖可见部分，内容横向不变 ----

async function applyEdgeFrame() {
  if (!dockSide) return;
  if (dockScreen) dockOrigin = dockRect({ ...dockOrigin, width: CARD_W, height: expandedH }, dockScreen, dockSide);
  const frame = edgeFrame({ ...dockOrigin, width: CARD_W, height: expandedH }, dockSide, edgeProgress);
  appEl.style.transform = `translate(${frame.offsetX}px, ${frame.offsetY}px)`;
  document.body.dataset.edge = dockSide;
  document.body.classList.toggle("edge-collapsed", edgeProgress === 1);
  await setEdgeWindow(frame);
}

async function setEdgeWindow(frame: ReturnType<typeof edgeFrame>) {
  // 跨不同缩放的显示器时按目标屏的物理坐标落位，不依赖尚未更新的窗口 DPI。
  const position = new PhysicalPosition(Math.round(frame.x * dockScale), Math.round(frame.y * dockScale));
  const size = new PhysicalSize(Math.round(frame.width * dockScale), Math.round(frame.height * dockScale));
  // 右侧收缩先缩小再右移，避免中间一帧越过屏幕边界。
  if (dockSide === "right" && frame.width < CARD_W) {
    await win.setSize(size);
    await win.setPosition(position);
  } else {
    await win.setPosition(position);
    await win.setSize(size);
  }
}

function paintEdgeMotion() {
  if (!dockSide) return;
  appEl.style.transform = edgeTransform(CARD_W, expandedH, dockSide, edgeProgress);
}

async function playEdgeMotion() {
  if (!dockSide) return;
  const from = edgeProgress;
  const to = edgeTarget;
  const timing = edgeMotion(from, to, matchMedia("(prefers-reduced-motion: reduce)").matches);
  const motion = appEl.animate([
    { transform: edgeTransform(CARD_W, expandedH, dockSide, from) },
    { transform: edgeTransform(CARD_W, expandedH, dockSide, to) },
  ], timing);
  let interrupted = false;
  interruptEdgeMotion = () => {
    interrupted = true;
    // Computed progress includes easing: reverse from the rendered position, not a linear estimate.
    const progress = motion.effect?.getComputedTiming().progress ?? 0;
    edgeProgress = from + (to - from) * progress;
    paintEdgeMotion();
    motion.cancel();
  };
  try {
    await motion.finished;
    edgeProgress = to;
  } catch (error) {
    if (!interrupted) throw error;
  } finally {
    interruptEdgeMotion = null;
    paintEdgeMotion();
    motion.cancel();
  }
}

function cancelEdgeHide() {
  window.clearTimeout(edgeTimer);
  edgeTimer = 0;
}

/**
 * 等浏览器真正提交一帧。改原生窗口几何之前先让「窄条已隐藏」上屏，
 * 否则 WebView 合成器可能还拿着带窄条的旧画面去套新窗口尺寸，
 * 窄条就会按旧视口出现在新窗口的左缘。
 * rAF 在后台/休眠窗口可能不触发，超时兜底避免展开卡住（Windows / macOS 通用）。
 */
function nextPaint(): Promise<void> {
  return new Promise((resolve) => {
    let done = false;
    let timer = 0;
    const finish = () => {
      if (done) return;
      done = true;
      window.clearTimeout(timer);
      resolve();
    };
    timer = window.setTimeout(finish, EDGE_PAINT_TIMEOUT_MS);
    requestAnimationFrame(() => requestAnimationFrame(finish));
  });
}

function requestEdge(collapsed: boolean) {
  cancelEdgeHide();
  const target = collapsed ? 1 : 0;
  if (!dockSide || (edgeTarget === target && (edgeMoving || edgeProgress === target))) return;
  edgeTarget = target;
  interruptEdgeMotion?.();
  void queueMenuOp(async () => {
    if (!dockSide || dragging || menuOpen || menuMoving || edgeProgress === edgeTarget) return;
    const side: EdgeSide = dockSide;
    const expanding = edgeTarget === 0;
    edgeMoving = true;
    try {
      // Only the start/end touch native geometry. The WebView compositor owns all slide frames.
      // Keep the collapsed card hidden while restoring the full viewport, preventing a full-card flash.
      paintEdgeMotion();
      // 展开时原生窗口要改尺寸/位置，但 WebView 的布局视口会晚一帧才重排：
      // 先藏起窄条并等这一帧提交，否则它会按旧视口 right:0 画在新窗口的左缘。
      //
      // 严格说只有右侧需要：右侧展开会移动窗口原点，窄条又锚在视口右缘
      // （right:0 依赖视口宽度）；左/顶的固定边是 left:0/top:0，视口长大不会
      // 把窄条带偏。这里统一走同一条展开路径、少一个分支，代价是左/顶也
      // 晚 1–2 帧、窄条短暂消失——有意取舍。
      if (expanding) {
        document.body.classList.add("edge-handle-hidden");
        await nextPaint();
        // 等帧期间鼠标又移出：别再展开，finally 会按收起收尾。
        if (edgeTarget !== 0) return;
      }
      const frame = edgeFrame({ ...dockOrigin, width: CARD_W, height: expandedH }, side, 0);
      await setEdgeWindow(frame);
      document.body.classList.remove("edge-collapsed");
      document.body.classList.add("edge-moving");
      while (side && !dragging && edgeProgress !== edgeTarget) {
        await playEdgeMotion();
      }
    } finally {
      try {
        // Also recover geometry if the browser cancels an animation unexpectedly.
        edgeProgress = edgeTarget;
        await applyEdgeFrame();
      } finally {
        edgeMoving = false;
        document.body.classList.remove("edge-moving");
        // edge-collapsed 已按当前进度恢复；这里统一收掉临时类，异常/中断也走得掉。
        document.body.classList.remove("edge-handle-hidden");
        // macOS 的坐标流在鼠标静止时不再来。展开动画期间鼠标往往已停在
        // 某一行上，动画一结束就用最后的坐标补一次行检测，否则要再动
        // 一下鼠标（或点一下）详情面板才出现。
        if (edgeProgress === 0 && lastNativeMouse) replayRowHover(lastNativeMouse);
      }
    }
  }).catch((e) => console.warn("悬浮窗靠边收缩失败：", e));
}

function revealTemporarily(duration = REVEAL_MS) {
  if (!dockSide) return;
  edgeHoldUntil = performance.now() + duration;
  evaluateVisibility();
}

function evaluateEdge() {
  // 首次恢复先布置窄窗；期间的展开请求通过 edgeHoldUntil 保留。
  if (!edgeReady) return;
  showCard();
  const interacting = hovering || dragging || menuOpen || menuMoving;
  if (interacting || performance.now() < edgeHoldUntil) requestEdge(false);
  if (interacting || edgeTimer || (edgeTarget === 1 && (edgeMoving || edgeProgress === 1))) return;
  edgeTimer = window.setTimeout(() => {
    edgeTimer = 0;
    if (!dockSide || hovering || dragging || menuOpen || menuMoving) return;
    if (performance.now() < edgeHoldUntil) { evaluateEdge(); return; }
    requestEdge(true);
  }, Math.max(EDGE_LEAVE_MS, edgeHoldUntil - performance.now()));
}

function clearDock() {
  cancelEdgeHide();
  dockSide = null;
  dockScreen = null;
  dockMonitor = null;
  edgeProgress = edgeTarget = 0;
  edgeHoldUntil = 0;
  edgeReady = true;
  appEl.style.transform = "";
  delete document.body.dataset.edge;
  document.body.classList.remove("edge-collapsed");
  document.body.classList.remove("edge-handle-hidden");
}

function monitorRect(monitor: Monitor, work = false): Rect {
  const area = work ? monitor.workArea : monitor;
  return { x: area.position.x, y: area.position.y, width: area.size.width, height: area.size.height };
}

async function dockTarget() {
  const [monitors, pos, winScale] = await Promise.all([availableMonitors(), win.outerPosition(), win.scaleFactor()]);
  // macOS 的吸附意图信号是「鼠标贴边」而非「卡片贴边」：拖动时卡片跟着抓取点走，
  // 鼠标出不了屏，卡片边缘永远停在屏内几十像素，卡片几何判定永远差一口气。
  // 拖动回执带松手时的全局鼠标位置（点单位、左上原点，CoreGraphics 直读，
  // 无 tao cursorPosition 的 Retina bug），乘窗口屏 scale 转成物理坐标与
  // monitorRect 同口径。鼠标能精确贴到 x=0，24px 判定带足够命中。
  const pointer = isMacOS() && dragMouse
    ? { x: dragMouse[0] * winScale, y: dragMouse[1] * winScale }
    : await cursorPosition();
  // 窗口被拖到越过屏幕边缘时，左上角会落在所有屏幕矩形之外——按「窗口与屏幕
  // 是否相交」兜底找所在屏（窗口可见部分所在的那块屏就是吸附目标屏）。
  const overlaps = (r: Rect) =>
    pos.x < r.x + r.width && pos.x + CARD_W > r.x && pos.y < r.y + r.height && pos.y + IDLE_H > r.y;
  const monitor = monitors.find((m) => containsPoint(monitorRect(m), pointer))
    ?? monitors.find((m) => overlaps(monitorRect(m)));
  if (!monitor) return null;
  const scale = monitor.scaleFactor > 0 && Number.isFinite(monitor.scaleFactor) ? monitor.scaleFactor : 1;
  const work = monitorRect(monitor, true);
  const card = { x: pos.x, y: pos.y, width: CARD_W * scale, height: expandedH * scale };
  // macOS 的探针点取卡片中心（side 判定由 card 条件触发，探针只参与邻屏排除）
  const probe = isMacOS() ? { x: card.x + card.width / 2, y: card.y + card.height / 2 } : pointer;
  const side = edgeAtPoint(probe, work, monitors.filter((m) => m !== monitor).map((m) => monitorRect(m)), edgeThreshold() * scale, card);
  // macOS 顶部吸附贴屏幕物理上沿（与流光一致，盖过菜单栏）：工作区上沿在菜单栏
  // 下方 ~25px，按工作区吸会悬在导航栏下面；悬浮窗面板与流光同为 level 1000，
  // 有能力盖在菜单栏上。左右仍按工作区（让开侧边 Dock）；Windows 保持工作区
  // 口径（任务栏可能在顶部，不应叠上去）。
  const snap = side === "top" && isMacOS() ? monitorRect(monitor) : work;
  return {
    side,
    scale,
    monitorName: monitor.name,
    position: { x: pos.x / scale, y: pos.y / scale },
    screen: { x: snap.x / scale, y: snap.y / scale, width: snap.width / scale, height: snap.height / scale },
  };
}

// ---- 可见性：只有停靠会收缩，自由悬浮始终常驻 ----

let hidden = !!dockSide;
let showing = false;
let hovering = false;
// macOS 坐标流的最后位置：鼠标静止时不再有 move 事件，展开动画结束后要靠它补一次行检测
let lastNativeMouse: { x: number; y: number } | null = null;

function evaluateVisibility() {
  if (dockSide) evaluateEdge();
  else showCard();
}

function showCard(force = false) {
  if ((!hidden && !force) || showing) return;
  showing = true;
  void win.show().then(() => { hidden = false; })
    .catch((error) => { hidden = true; console.warn("显示悬浮窗失败：", error); })
    .finally(() => { showing = false; });
}

async function consumeTrayReveal() {
  if (!(await api.takeWidgetReveal())) return;
  if (dockSide) revealTemporarily(TRAY_REVEAL_MS);
  else showCard(true);
}

// ---- 拖动与位置记忆 ----

/** 固定状态切换后，等菜单还原完再保存位置。拖动由 finishDrag 统一落盘。 */
let saveTimer = 0;
let dragMoved = false;
/** 拖动开始前的窗口位置（物理像素，来自拖动回执）；松手时对比判定真实位移 */
let dragStartBefore: [number, number] | null = null;
/** 松手时的全局鼠标位置（macOS 回执，点单位、左上原点）——吸附意图的判定信号 */
let dragMouse: [number, number] | null = null;
let justUndocked = false;
/** 本次拖动从哪个停靠边拖离（拖离 A 边后拖到 B 边视为新的吸附意图，应正常吸附） */
let undockedFrom: EdgeSide | null = null;
let dragGeneration = 0;
let checkingEdge = false;
let dragSettling = false;

/** 最近一次吸附收缩完成/开始的时刻：静默期内坐标流不触发展开（鼠标恰落在收缩后的触发条上时，坐标流会把刚收起的卡片又判成「移入」展开回去） */
let dockCollapsedAt = 0;
/** 最近一次 mouseEntered / mouseExited 的时刻（macOS 坐标流，AppKit 层转发）。
 * 展开的意图判定靠两者的间隔：几何重算（吸附收缩、展开动画起止）会合成一对
 * 紧邻的 exited/entered，间隔只有几毫秒；真实移入之前必然有真实的离开，间隔
 * 是离开时长。间隔过小 = 这次 entered 是几何重算的合成，不算移入——指针停在
 * 原地继续动也不会展开，跨过这条线的只有真实离开再进入。 */
let lastEnterAt = 0;
let lastExitAt = 0;

async function previewDock() {
  if (checkingEdge || !dragging) return;
  checkingEdge = true;
  const generation = dragGeneration;
  try {
    const target = await dockTarget();
    if (!dragging || generation !== dragGeneration) return;
    if (!target?.side) justUndocked = false;
    appEl.dataset.dockPreview = justUndocked ? "" : target?.side ?? "";
  } finally { checkingEdge = false; }
}

/** 应用吸附：落位到边缘、收成窄条（预览提示清掉）。finishDrag 与 previewDock 共用。 */
async function applySnap(side: EdgeSide, target: NonNullable<Awaited<ReturnType<typeof dockTarget>>>) {
  dockSide = side;
  dockScreen = target.screen;
  dockScale = target.scale;
  dockMonitor = target.monitorName;
  dockOrigin = dockRect({ ...target.position, width: CARD_W, height: expandedH }, target.screen, side);
  edgeProgress = edgeTarget = 0;
  edgeReady = true;
  try {
    await applyEdgeFrame();
    showCard();
  } catch (error) {
    // 布局/落位失败不打断吸附：收缩动画里的 applyEdgeFrame 会重试落位，
    // 这里若把 requestEdge(true) 吞掉，窗口就停在「吸附态但永不收缩」
    console.warn("吸附落位失败（收缩动画将重试）：", error);
  }
  hovering = false;
  dockCollapsedAt = Date.now();
  requestEdge(true);
}

async function finishDrag() {
  if (!dragging) return;
  dragging = false;
  dragSettling = true;
  dragGeneration++;
  delete appEl.dataset.dockPreview;
  window.clearTimeout(saveTimer);
  try {
    await queueMenuOp(async () => {
      // moved 事件与 IPC 回执都可能在 macOS 的模态拖动中丢失：用「松手位置 ≠ 拖动前
      // 位置」兜底判定真的拖动了——吸附判定本来就基于窗口位置，这个口径不会误判。
      if (!dragMoved && dragStartBefore) {
        const now = await win.outerPosition();
        if (now.x !== dragStartBefore[0] || now.y !== dragStartBefore[1]) dragMoved = true;
      }
      if (!dragMoved) return;
      const target = await dockTarget();
      // 拖离 A 边后拖到 B 边（或从自由态拖到任意边）都是新的吸附意图；
      // 只有「从 A 边拖离又拖回 A 边」才保留「拖离取消停靠」的原语义。
      if (target?.side && (!justUndocked || target.side !== undockedFrom) && !pinned) {
        await applySnap(target.side, target);
      }
    });
    await syncWindowSize();
    await saveCurrentPosition();
  } catch (e) { console.warn("拖动后调整悬浮窗失败：", e); }
  finally {
    dragSettling = false;
    justUndocked = false;
    evaluateVisibility();
  }
}

async function saveCurrentPosition() {
  // 菜单会把窗口临时撑高/上移（见 openMenu），期间的位置不是用户放置的，不落盘
  if (menuOpen || menuMoving || dragging) return;
  try {
    if (dockSide) {
      await api.saveWidgetPosition(Math.round(dockOrigin.x), Math.round(dockOrigin.y), dockSide, dockMonitor);
      return;
    }
    const [pos, scale] = await Promise.all([win.outerPosition(), win.scaleFactor()]);
    if (menuOpen || menuMoving || dragging || dockSide) return;
    await api.saveWidgetPosition(Math.round(pos.x / scale), Math.round(pos.y / scale));
  } catch (e) {
    console.warn("保存悬浮窗位置失败：", e);
  }
}

function schedulePositionSave() {
  window.clearTimeout(saveTimer);
  saveTimer = window.setTimeout(() => void saveCurrentPosition().catch((e) => console.warn("保存悬浮窗位置失败：", e)), SAVE_DELAY_MS);
}

// Windows 原生拖动会吞掉 mouseup，后端等待物理左键释放才回执。
// 移动时只做停靠预览；松手后统一吸附、调整尺寸，再保存展开位置。
// 停靠态拖起的第一下就取消停靠（即使指针还在边缘带内），「重新吸附」
// 要等指针离开边缘带再回来——previewDock 的 justUndocked 重置配合此语义。
void win.onMoved(() => {
  if (!dragging || menuMoving || edgeMoving) return;
  dragMoved = true;
  if (dockSide) { undockedFrom = dockSide; clearDock(); justUndocked = true; }
  void previewDock().catch((e) => console.warn("检测靠边位置失败：", e));
}).catch((e) => console.warn("订阅悬浮窗移动事件失败：", e));

// 按住即拖（在 mousedown 中发起原生拖动）；固定位置后不再响应拖动。
// 双击打开主面板也在 mousedown 里判（e.detail >= 2）：第一下的 startDragging
// 会进 OS 拖拽循环、吞掉第二次 mouseup，`dblclick` 事件根本不会来——
// Tauri 自带的标题栏拖动脚本（window/scripts/drag.js）就是靠 detail 区分单击/双击的。
appEl.addEventListener("mousedown", (e) => {
  if (dragSettling) return;
  if (menuOpen) {
    void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err)); // 点在菜单外（卡片上）：只收菜单，不开始拖
    return;
  }
  if (e.button !== 0) return;
  const scrollBar = listEl.getBoundingClientRect().right - (listEl.offsetWidth - listEl.clientWidth);
  if (listEl.scrollHeight > listEl.clientHeight && e.clientX >= scrollBar) return;
  if (e.detail >= 2) {
    void api.showPanel().catch((err) => console.warn("打开主面板失败：", err)); // 双击：打开主面板（与托盘菜单「显示主窗口」同一条命令路径）
    return;
  }
  if (pinned) return;
  if (edgeMoving || edgeProgress > 0) { requestEdge(false); return; }
  // 详情已独立成窗，拖动前只需收起它，悬浮窗本体不受详情几何影响
  hideTipNow();
  cancelEdgeHide();
  dragMoved = false;
  dragStartBefore = null;
  dragMouse = null;
  undockedFrom = null;
  dragGeneration++;
  dragging = true;
  // IPC 回执由后端在实际松手后发出；即使最后的 moved 没到，也能确认真实位移。
  // macOS 上 moved 恒为 false（start_dragging 即返回、模态拖动期间 onMoved 也可能
  // 不派发），所以回执里的拖动前位置必须记下来，finishDrag 与实际位置对比兜底。
  api
    .startWidgetDrag()
    .then((report) => {
      dragStartBefore = report.before;
      dragMouse = report.mouse ?? null;
      if (report.moved && !dragMoved) {
        dragMoved = true;
        if (dockSide) { undockedFrom = dockSide; clearDock(); justUndocked = true; }
      }
    })
    .catch((e) => console.warn("启动拖动失败：", e))
    .finally(() => void finishDrag());
});

// 双击悬浮窗的动作并入上面的 mousedown（e.detail >= 2）——见那里的原因说明。

// ---- 行悬停详情（独立 tip 窗口）----
// 详情已独立成窗（tip.html → tip.ts），悬浮窗只把行悬停意图经事件发过去，本体永不
// 因详情挪动/撑大（根治贴边时整窗被 fitPosition 挤来挤去的抖动）。这里只维护「当前
// 悬停行」做去重，意图延时/渲染/时长计时/落位都在 tip 窗口侧。
let tipTargetRow: HTMLElement | null = null;

async function sendTipRow(row: HTMLElement) {
  const { position, screen, full } = await screenGeometry();
  if (tipTargetRow !== row || !row.isConnected) return;
  // 与 syncWindowSize 同口径：macOS 顶部吸附贴屏幕物理上沿（盖菜单栏），详情落位
  // 也按全屏矩形钳位；其余按工作区
  const snap = dockSide === "top" && isMacOS() ? full : screen;
  const cardTop = cardEl.getBoundingClientRect().top;
  // 几何取整：亚像素坐标会让详情侧「内容/几何未变」去重失效，触发无谓重排（1–2px 抖动）
  const req: TipRequest = {
    key: row.dataset.key ?? "",
    heading: row.dataset.tipHeading ?? "",
    body: row.dataset.tip ?? "",
    startedAt: Number(row.dataset.startedAt) || 0,
    card: { x: Math.round(position.x), y: Math.round(position.y), width: CARD_W, height: Math.round(expandedH) },
    screen: { x: Math.round(snap.x), y: Math.round(snap.y), width: Math.round(snap.width), height: Math.round(snap.height) },
    rowTop: Math.round(row.getBoundingClientRect().top - cardTop),
  };
  void emitTo(TIP_LABEL, "bark://tip-row", req);
}

/** 行悬停意图：移到行上发 tip-row，移开发 tip-leave（tip 侧延迟收起，留出跨 8px 间隙的时间）。 */
function hoverRow(row: HTMLElement | null) {
  if (!detailTip) { hideTipNow(); return; }
  if (row) {
    if (row === tipTargetRow) return;
    tipTargetRow = row;
    void sendTipRow(row);
  } else {
    leaveTip();
  }
}

function leaveTip() {
  if (!tipTargetRow) return;
  tipTargetRow = null;
  void emitTo(TIP_LABEL, "bark://tip-leave", null);
}

function hideTipNow() {
  tipTargetRow = null;
  void emitTo(TIP_LABEL, "bark://tip-hide", null);
}

/** 快照/滚动后刷新：当前行还在就重发（更新内容与几何），不在就收起。 */
function refreshTip() {
  if (!detailTip || !tipTargetRow) return;
  if (!tipTargetRow.isConnected) { hideTipNow(); return; }
  void sendTipRow(tipTargetRow);
}

listEl.addEventListener("mouseover", (event) => {
  if (dragging || menuOpen || menuMoving || edgeMoving || edgeProgress > 0) return;
  const row = (event.target as Element | null)?.closest<HTMLElement>(".row-item[data-tip]");
  hoverRow(row ?? null);
});
listEl.addEventListener("mouseleave", () => hoverRow(null));
listEl.addEventListener("scroll", refreshTip);
appEl.addEventListener("mousedown", hideTipNow);

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

function queueMenuOp<T>(op: () => Promise<T>): Promise<T> {
  // 前一段的成败不影响后一段；op 自身不抛，这里再兜一层
  const run = menuChain.then(op, op);
  menuChain = run.then(() => {}, () => {});
  return run;
}

/** 打开右键菜单（对外入口：排队串行执行，见 queueMenuOp） */
function openMenu(cx: number, cy: number): Promise<void> {
  requestEdge(false);
  return queueMenuOp(() => openMenuImpl(cx, cy));
}

/** 收起右键菜单（对外入口：排队串行执行，见 queueMenuOp） */
function closeMenu(): Promise<void> {
  return queueMenuOp(closeMenuImpl);
}

function renderMenuState() {
  document.getElementById("ctx-pin")!.textContent = pinned ? t("floating.unpin") : t("floating.pin");
  document.getElementById("ctx-tip")!.textContent = detailTip ? t("floating.detailTipOff") : t("floating.detailTipOn");
  document.getElementById("ctx-undock")!.hidden = !dockSide;
}

async function openMenuImpl(cx: number, cy: number) {
  // 防重入：菜单开着时再次右键不重新打开——否则 menuSavedY 会被覆写成上移后的 y，
  // closeMenu 还原到错误位置（底边附近的卡片会向上漂移，见 §1.18）
  if (menuOpen) return;
  hideTipNow();
  cancelEdgeHide();
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
      const screenBottom = (mon.workArea.position.y + mon.workArea.size.height) / scale;
      if (cursorY + mh + 6 > screenBottom) {
        // 菜单朝下会伸出屏幕底 → 朝上翻：底边贴光标上沿，窗口顶边抬到菜单上方
        const wantTop = cursorY - mh - 6;
        upShift = Math.max(0, winTop - wantTop);
        // 别把窗口抬出屏幕顶
        upShift = Math.min(upShift, winTop - mon.workArea.position.y / scale);
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
  evaluateVisibility(); // 菜单收起，解除挂起
}

document.addEventListener("contextmenu", (e) => {
  // 拦掉 WebView2 自带的右键菜单，换成悬浮窗菜单
  e.preventDefault();
  void openMenu(e.clientX, e.clientY).catch((err) => console.warn("打开悬浮窗菜单失败：", err));
});
// 点菜单之外（窗口内的任何地方）收起；点菜单项本身不收（由 click 处理器收）
document.addEventListener("mousedown", (e) => {
  if (menuOpen && !menuEl.contains(e.target as Node)) {
    void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err));
  }
});
// 鼠标停在卡片上时持续展开，即使临时展开时间已到。
// macOS 的非激活窗口默认收不到 mousemove（AppKit 只投递给 key window），
// 由 widget.rs 建窗时的 setAcceptsMouseMovedEvents 打开——事件路径与 Windows 一致。
document.addEventListener("mouseenter", () => {
  hovering = true;
  requestEdge(false);
});
// 光标离开窗口也收起：窗口外点击我们收不到，菜单会一直悬在别的应用上面。
// 详情走延迟收起（leaveTip）——鼠标可能正跨过 8px 间隙移入独立详情窗，那边移入会取消。
document.addEventListener("mouseleave", () => {
  hovering = false;
  leaveTip();
  if (!dragging) void closeMenu().catch((err) => console.warn("收起悬浮窗菜单失败：", err));
  evaluateVisibility();
});

document.getElementById("ctx-panel")!.addEventListener("click", async () => {
  await closeMenu();
  void api.showPanel().catch((e) => console.warn("打开主面板失败：", e));
});
document.getElementById("ctx-pin")!.addEventListener("click", async () => {
  const button = document.getElementById("ctx-pin") as HTMLButtonElement;
  const next = !pinned;
  button.disabled = true;
  try {
    await api.setWidgetPinned(next);
    pinned = next;
    if (next && dockSide) clearDock();
    render();
  } catch (e) { console.warn("保存固定状态失败：", e); }
  finally { button.disabled = false; }
  await closeMenu();
  if (next) schedulePositionSave();
});
document.getElementById("ctx-tip")!.addEventListener("click", async () => {
  const button = document.getElementById("ctx-tip") as HTMLButtonElement;
  const next = !detailTip;
  button.disabled = true;
  try {
    await api.setWidgetDetailTip(next);
    detailTip = next;
    if (!next) hideTipNow();
    renderMenuState();
  } catch (e) { console.warn("保存详情面板开关失败：", e); }
  finally { button.disabled = false; }
  await closeMenu();
});
document.getElementById("ctx-undock")!.addEventListener("click", async () => {
  await closeMenu();
  clearDock();
  await syncWindowSize();
  await saveCurrentPosition();
  evaluateVisibility();
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

/**
 * 静态文案填充（首次启动 + 语言切换时重跑）：窗口标题、右键菜单标签、靠边
 * 触发条的 aria——这些不随会话事件重渲染的文本就地重写，不重建任何结构。
 */
function applyStaticTexts() {
  document.title = t("floating.windowTitle");
  edgeHandle.setAttribute("aria-label", t("floating.edgeHandleAria"));
  document.getElementById("ctx-panel")!.textContent = t("floating.openPanel");
  document.getElementById("ctx-undock")!.textContent = t("floating.undock");
  document.getElementById("ctx-exit")!.textContent = t("floating.exit");
  renderMenuState(); // ctx-pin 按当前固定态取词（取消固定位置 / 固定位置）
}

// 语言切换（设置页切换后 Rust 对全部窗口广播）：静态文案就地重写；会话行的
// 标签本就随快照/事件重渲染，render() 复用行节点按新语言重取词（SubagentLabels
// 前缀按编号重新生成、空闲行 / 页脚 / pin 角标都在 render 里取词）。详情面板的
// 文案由 tip 窗口自己的 onLocaleChanged 处理，这里只需刷新行内容。
onLocaleChanged(() => {
  applyStaticTexts();
  render();
});

void Promise.all([
  listen<SessionStatus[]>("bark://sessions", (e) => {
    // 推送负载兜底：null / 非数组忽略（并作废旧拉取），别让坏推送把渲染冻在旧帧
    if (!Array.isArray(e.payload)) {
      console.warn("会话快照负载异常，已忽略：", e.payload);
      return;
    }
    sessionsSeq += 1;
    receiveSessions(e.payload);
  }),
  listen<NormalizedEvent>("bark://event", (e) => onEvent(e.payload)),
  listen("bark://widget-reveal", () => {
    void consumeTrayReveal().catch((error) => console.warn("领取悬浮窗展开请求失败：", error));
  }),
  // macOS：非激活窗口收不到 WebKit 的 hover 类 DOM 事件（active 状态抑制），
  // widget.rs 用 NSTrackingArea（ActiveAlways）在 AppKit 层捕获后转发到这里。
  // entered/exited 都只记时刻，展开的意图判定在下面的坐标流里做：真实移入的
  // entered 与最近一次 exited 间隔是「离开时长」，几何重算（吸附收缩等）合成
  // 的对间隔只有几毫秒，间隔过小的不算移入。exited 同时负责收起。
  // Windows 上这两个事件不会发出，走上面的 mouseenter/mouseleave/over。
  listen<boolean>("bark://widget-hover", (e) => {
    if (e.payload) {
      lastEnterAt = performance.now();
      return;
    }
    lastExitAt = performance.now();
    hovering = false;
    leaveTip();
    evaluateVisibility();
  }),
  listen<{ x: number; y: number }>("bark://widget-mouse", (e) => {
    lastNativeMouse = e.payload;
    // 坐标在动 = 鼠标真实位于悬浮窗上：驱动展开（Windows 路径靠 mouseenter）。
    // 展开需同时满足两道门：entered 距最近一次 exited 超过 50ms——几何重算
    // 合成的 exited/entered 对间隔只有几毫秒，不算移入；距吸附落位超过 600ms
    // ——否则松手时落在收缩后触发条上的指针微小移动会把刚收起的卡片再展开回去。
    if (!hovering && lastEnterAt - lastExitAt > 50 && Date.now() - dockCollapsedAt > 600) {
      hovering = true;
      requestEdge(false);
    }
    replayRowHover(e.payload);
  }),
]).then(consumeTrayReveal).catch((e) => console.warn("订阅悬浮窗事件失败：", e));
void loadSessions().catch((e) => console.warn("拉取会话快照失败：", e));

// 界面语言必须在首帧文案（行标签 / 右键菜单 / 空闲行）之前定好（Rust 是唯一解析点）：
// 悬浮窗先加载后显示，这一趟 IPC 用户看不见（见 i18n.ts 的说明）。语言就绪后才做
// 首次文案填充与首帧 render。不用顶层 await——本模块被 tests/widget-edge.test.mjs
// 以 VM 沙箱按经典脚本加载，顶层 await 经 TS 的 CJS 编译会变成对未定义标识符
// `await` 的调用，沙箱跑不了。
void (async () => {
  await initI18n();
  applyStaticTexts();
  render();
})();

/** 用 AppKit 转发的坐标找行元素，触发或收回行详情——与 Windows 的 mouseover 路径同一条管线 */
function replayRowHover(at: { x: number; y: number }) {
  const el = document.elementFromPoint(at.x, at.y);
  const row = el?.closest<HTMLElement>(".row-item[data-tip]");
  hoverRow(row ?? null);
}
