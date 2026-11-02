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

import { listen } from "@tauri-apps/api/event";
import { availableMonitors, cursorPosition, currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, type Monitor } from "@tauri-apps/api/window";
import {
  api,
  EVENT_LABELS,
  agentIcon,
  agentName,
  isSubagentSession,
  PHASE_LABELS,
  placeholderName,
  SESSION_INDENT_MAX,
  sessionRows,
  type NormalizedEvent,
  type SessionRow,
  type SessionStatus,
} from "./api";
import { initTheme, onThemeChange } from "./theme";
import { hideRowTip, initRowTip, refreshRowTip, scheduleRowTip, scheduleTipHide } from "./widget-tip";
import { CARD_WIDTH, MAX_LIST_HEIGHT, fitPosition, placeTip, tipWidth, splitName, SubagentLabels, type Rect } from "./widget-layout";
import { containsPoint, dockRect, edgeAtPoint, edgeFrame, edgeMotion, edgeTransform, EDGE_THRESHOLD, type EdgeSide, type Point } from "./widget-edge";
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
}
const init: WidgetInit =
  (window as unknown as { __WIDGET_INIT__?: WidgetInit }).__WIDGET_INIT__ ??
  { x: 60, y: 60, width: CARD_WIDTH, pinned: false };

const CARD_W = CARD_WIDTH;
const PAD_V = 12;
const SUB_ALIGN_PX = 30;
const IDLE_H = 44;
const subagentLabels = new SubagentLabels();
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
 * 终止文案与主面板共用统一事件标签，各 agent 不单独决定展示名称。
 */
const ABORT_LABEL = EVENT_LABELS.run_aborted;
/**
 * 父会话被手动终止、但它的子代理还在跑时，**子代理行**的文案。
 *
 * 不沿用它们各自的事件相位（执行工具 / 思考中）：那是**父回合**的状态，父都被终止了还写
 * 「执行工具」会让人以为父还在干活。这四个字说的才是实情——子代理收尾之前，主会话这边
 * 不再有别的动静（用户的原话：主会话会等子会话自然结束后再被唤起）。
 */
const AWAIT_LABEL = "等待子代理";

/** 会话行/事件的统一身份键：与 api.ts 的 `visited` 同口径（`agent|session_id`） */
export function sessionRowKey(agent: string, sessionId: string): string {
  return `${agent}|${sessionId}`;
}

/**
 * 「手动终止」行的展示信息快照（按 [`sessionRowKey`] 存）。
 *
 * daemon 摘掉会话之后就再也算不出这一行的名字了（`subagentLabel` 需要「是否多工作区」、
 * `rowLabel` 需要工作区名），所以在它**还在快照里**的那几帧把名字/提示/缩进存下来，
 * 供保留期内重建这一行时使用。
 */
const abortedMemo = new Map<string, { name: string; tip: string; depth?: number; subagent: boolean }>();

/**
 * 「上一帧见到的样子」——**每一行**都记（键同 [`sessionRowKey`]），不只记手动终止的。
 *
 * 用途：父行从快照里消失、而它的子代理还在跑时，`sessionRows` 会合成一行占位父行
 * （`会话 …尾号`）当落脚点。一串 id 尾号既认不出是哪个 agent、也不好看（用户点名过两次），
 * 而这个会话本 agent 名我们**上一帧刚见过**——直接拿来用，标签降级成「已结束」。
 *
 * 与 [`abortedMemo`] 分家的原因：那个只在「手动终止」时写、且语义是「这一行被谁终止了」；
 * 这份是纯展示缓存，凡渲染过的行都进（父行正常结束 / 已从本地显示移除也走这条路）。
 */
const lastSeenRow = new Map<string, { name: string; tip: string }>();

/** 完成/失败事件 → 驻留行（思考/等待由会话表表达，终态靠这里短暂展示） */function onEvent(ev: NormalizedEvent) {
  // 推送负载兜底：null / 异常负载直接忽略，别把整个渲染循环冻在旧帧
  if (!ev || typeof ev !== "object") {
    console.warn("事件推送负载异常，已忽略：", ev);
    return;
  }
  const map: Partial<Record<NormalizedEvent["type"], [StateColor, string]>> = {
    run_completed: ["completed", EVENT_LABELS.run_completed],
    run_failed: ["failed", EVENT_LABELS.run_failed],
    run_aborted: ["aborted", ABORT_LABEL],
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
        name: ev.is_subagent
          ? subagentLabel(ev.agent, ev.session_id)
          : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
        tip: ev.project ?? "",
        // 子代理恒挂在父行下面一级；主会话顶格。`undefined` = **不知道**（事件没带
        // `is_subagent`）：这样的一行**不缩进**，也就不带根行的图标（见 render 的图标条件），
        // 免得出现「子代理标签 + 根行图标」这种自相矛盾的一行
        depth: ev.is_subagent === true ? 1 : ev.is_subagent === false ? 0 : undefined,
        // 名字已经按「是不是子代理」选过了（子代理→`X 子代理`），这个标记只影响渲染方式
        subagent: ev.is_subagent === true,
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
    name: ev.is_subagent
      ? subagentLabel(ev.agent, ev.session_id)
      : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
    tip: ev.project ?? "",
    label: hit[1],
    state: hit[0],
    subagent: ev.is_subagent === true,
    // 子代理的终态行跟着它的父会话**同一个工作区**（同一个 agent 的终态事件本就该按事件里
    // 的 agent 名显示），缩进与子代理行一致
    depth: ev.is_subagent ? 1 : 0,
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
        depth,
        // 真实行子代理与否是确定的（快照里的 `parent`），记下来供合成行复用
        subagent: sub,
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
    // 每一行都留一份「上一帧见到的样子」：父行消失后要靠它把占位父行还原成真名字
    lastSeenRow.set(sessionRowKey(s.agent, s.session_id), { name, tip });
    return {
      key: s.session_id,
      sessionId: s.session_id,
      agent: s.agent,
      name,
      // 子代理详情同样保留完整工作区，方便确认归属。
      tip,
      // 未知 phase 兜底显示原值：undefined 漏进 escapeHtml 会抛 TypeError、悬浮窗冻结在旧帧
      label: placeholder ? "已结束" : aborted ? ABORT_LABEL : (PHASE_LABELS[s.phase] ?? String(s.phase)) + (s.phase === "waiting_children" && s.active_descendants ? ` · ${s.active_descendants}` : ""),
      state: aborted ? "aborted" : state,
      subagent: sub,
      depth,
      parentMissing,
      placeholder,
    };
  });
  // 占位父行一律还原成**真名字**：只要这一行还有子在跑，就没理由摆一串 id 尾号
  // （用户点名过两次）。名字取「上一帧见到的样子」；实在没见过（widget 是刚启动的）
  // 就退化成 agent 名 +「已结束」——那个 agent 名是确定的：合成占位行时用的是子代理的
  // agent，而子代理与父会话必然是同一个 agent。
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
        name: memo?.name ?? last?.name ?? `${agentName(row.agent)} 已结束`,
        tip: memo?.tip ?? last?.tip ?? "",
        label: aborted ? ABORT_LABEL : "已结束",
        state: aborted ? "aborted" : "completed",
        subagent: false,
        depth: 0,
        parentMissing: false,
        placeholder: false,
      };
      if (aborted) {
        for (const r of rows) {
          if (!subtree.has(r.key) || !r.subagent) continue;
          r.label = AWAIT_LABEL;
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
      // 下面（实测踩过）。父行不在了就退回追加——至少还是「一条短命的驻留行」。
      const pi = terminal.parentId ? rows.findIndex((r) => r.key === terminal!.parentId) : -1;
      if (pi < 0) {
        rows.push(terminal);
      } else {
        const pd = rows[pi].depth;
        let end = pi + 1;
        while (end < rows.length && rows[end].depth > pd) end++;
        rows.splice(end, 0, terminal);
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
edgeHandle.setAttribute("aria-label", "移入展开悬浮窗，拖离边缘取消隐藏");
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
    el.dataset.tip = r.tip || r.name;
    el.dataset.tipHeading = agentName(r.agent) + (r.subagent ? " · " + r.name : "");
    const pad = r.subagent ? SUB_ALIGN_PX + Math.max(0, Math.min(r.depth, SESSION_INDENT_MAX) - 1) * 11 : 7;
    el.style.paddingLeft = pad + "px";
    el.style.setProperty("--indent", pad + "px");
    const [start, tail] = splitName(r.name);
    const icon = r.subagent ? '<span class="tree" aria-hidden="true"></span>' : '<span class="agent-icon">' + agentIcon(r.agent) + '</span>';
    const pin = pinned && index === 0 ? '<span class="pin-mark" aria-label="位置已固定">⌖</span>' : '';
    const markup = icon + '<span class="name"><span class="name-start">' + escapeHtml(start) + '</span><span class="name-tail">' + escapeHtml(tail) + '</span></span>' + pin + '<span class="state ' + r.state + '"><i class="dot" aria-hidden="true"></i>' + escapeHtml(r.label) + '</span>';
    if (el.innerHTML !== markup) el.innerHTML = markup;
    if (listEl.children[index] !== el) listEl.insertBefore(el, listEl.children[index] ?? null);
  });
  for (const [key, el] of rowElements) if (!keep.has(key)) { el.remove(); rowElements.delete(key); }
  const idle = listEl.querySelector(".idle");
  if (list.length) idle?.remove();
  else if (!idle) listEl.innerHTML = '<div class="row-item idle"><img src="' + logo + '" alt=""><span>空闲 · 没有进行中的会话</span></div>';
  listContentHeight = list.length ? list.reduce((height, row, index) => height + (row.subagent ? 29 : 32) + (index > 0 && !row.subagent && list[index - 1].subagent ? 6 : 0), 0) : 32;
  const attentionCount = list.filter((r) => r.state === "waiting" || r.state === "failed").length;
  footerEl.innerHTML = '<span>' + list.filter((r) => !r.placeholder).length + ' 个会话 · 可滚动</span><b>' + attentionCount + ' 项待处理</b>';
  updateCardHeight();
  void syncWindowSize().then(() => { refreshRowTip(); if (dockSide) evaluateVisibility(); }).catch((e) => console.warn("同步悬浮窗尺寸失败：", e));
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
  };
}

function syncWindowSize() {
  return queueMenuOp(async () => {
    if (menuOpen || menuMoving || tipOpen || tipMoving || dragging) return;
    const { position, screen, scale, monitorName } = await screenGeometry();
    if (menuOpen || menuMoving || tipOpen || tipMoving || dragging) return;
    maxListHeight = Math.min(MAX_LIST_HEIGHT, Math.max(32, screen.height - PAD_V - 31 - 120));
    updateCardHeight();
    if (dockSide) {
      dockScreen = screen;
      dockScale = scale;
      dockMonitor = monitorName;
      dockOrigin = dockRect({ ...dockOrigin, width: CARD_W, height: expandedH }, screen, dockSide);
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

function requestEdge(collapsed: boolean) {
  cancelEdgeHide();
  const target = collapsed ? 1 : 0;
  if (!dockSide || (edgeTarget === target && (edgeMoving || edgeProgress === target))) return;
  edgeTarget = target;
  interruptEdgeMotion?.();
  void queueMenuOp(async () => {
    if (!dockSide || dragging || menuOpen || menuMoving || tipOpen || tipMoving || edgeProgress === edgeTarget) return;
    edgeMoving = true;
    try {
      // Only the start/end touch native geometry. The WebView compositor owns all slide frames.
      // Keep the collapsed card hidden while restoring the full viewport, preventing a full-card flash.
      paintEdgeMotion();
      await setEdgeWindow(edgeFrame({ ...dockOrigin, width: CARD_W, height: expandedH }, dockSide, 0));
      document.body.classList.remove("edge-collapsed");
      document.body.classList.add("edge-moving");
      while (dockSide && !dragging && edgeProgress !== edgeTarget) {
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
  const interacting = hovering || dragging || menuOpen || menuMoving || tipOpen || tipMoving;
  if (interacting || performance.now() < edgeHoldUntil) requestEdge(false);
  if (interacting || edgeTimer || (edgeTarget === 1 && (edgeMoving || edgeProgress === 1))) return;
  edgeTimer = window.setTimeout(() => {
    edgeTimer = 0;
    if (!dockSide || hovering || dragging || menuOpen || menuMoving || tipOpen || tipMoving) return;
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
}

function monitorRect(monitor: Monitor, work = false): Rect {
  const area = work ? monitor.workArea : monitor;
  return { x: area.position.x, y: area.position.y, width: area.size.width, height: area.size.height };
}

async function dockTarget() {
  const [pointer, monitors, pos] = await Promise.all([cursorPosition(), availableMonitors(), win.outerPosition()]);
  const monitor = monitors.find((m) => containsPoint(monitorRect(m), pointer));
  if (!monitor) return null;
  const scale = monitor.scaleFactor > 0 && Number.isFinite(monitor.scaleFactor) ? monitor.scaleFactor : 1;
  const work = monitorRect(monitor, true);
  const card = { x: pos.x, y: pos.y, width: CARD_W * scale, height: expandedH * scale };
  const side = edgeAtPoint(pointer, work, monitors.filter((m) => m !== monitor).map((m) => monitorRect(m)), EDGE_THRESHOLD * scale, card);
  return {
    side,
    scale,
    monitorName: monitor.name,
    position: { x: pos.x / scale, y: pos.y / scale },
    screen: { x: work.x / scale, y: work.y / scale, width: work.width / scale, height: work.height / scale },
  };
}

// ---- 可见性：只有停靠会收缩，自由悬浮始终常驻 ----

let hidden = !!dockSide;
let showing = false;
let hovering = false;

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
let justUndocked = false;
let dragGeneration = 0;
let checkingEdge = false;
let dragSettling = false;

async function previewDock() {
  if (checkingEdge || !dragging) return;
  checkingEdge = true;
  const generation = dragGeneration;
  try {
    const target = await dockTarget();
    if (!dragging || generation !== dragGeneration) return;
    if (!target?.side) justUndocked = false;
    cardEl.dataset.dockPreview = justUndocked ? "" : target?.side ?? "";
  } finally { checkingEdge = false; }
}

async function finishDrag() {
  if (!dragging) return;
  dragging = false;
  dragSettling = true;
  dragGeneration++;
  delete cardEl.dataset.dockPreview;
  window.clearTimeout(saveTimer);
  try {
    await queueMenuOp(async () => {
      if (!dragMoved) return;
      const target = await dockTarget();
      if (target?.side && !justUndocked && !pinned) {
        dockSide = target.side;
        dockScreen = target.screen;
        dockScale = target.scale;
        dockMonitor = target.monitorName;
        dockOrigin = dockRect({ ...target.position, width: CARD_W, height: expandedH }, target.screen, dockSide);
        edgeProgress = edgeTarget = 0;
        edgeReady = true;
        await applyEdgeFrame();
        showCard();
        // 松手即收；后续鼠标移入再展开。
        hovering = false;
        requestEdge(true);
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
  if (menuOpen || menuMoving || tipOpen || tipMoving || dragging) return;
  try {
    if (dockSide) {
      await api.saveWidgetPosition(Math.round(dockOrigin.x), Math.round(dockOrigin.y), dockSide, dockMonitor);
      return;
    }
    const [pos, scale] = await Promise.all([win.outerPosition(), win.scaleFactor()]);
    if (menuOpen || menuMoving || tipOpen || tipMoving || dragging || dockSide) return;
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
void win.onMoved(() => {
  if (!dragging || menuMoving || tipMoving || edgeMoving) return;
  dragMoved = true;
  if (dockSide) { clearDock(); justUndocked = true; }
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
  // Use the current visual position as the drag origin, even if details shifted it upward.
  tipSavedPosition = null;
  hideRowTip();
  cancelEdgeHide();
  dragMoved = false;
  dragGeneration++;
  dragging = true;
  // IPC 回执由后端在实际松手后发出；即使最后的 moved 没到，也能确认真实位移。
  api
    .startWidgetDrag()
    .then((moved) => {
      if (moved && !dragMoved) {
        dragMoved = true;
        if (dockSide) { clearDock(); justUndocked = true; }
      }
    })
    .catch((e) => console.warn("启动拖动失败：", e))
    .finally(() => void finishDrag());
});

// 双击悬浮窗的动作并入上面的 mousedown（e.detail >= 2）——见那里的原因说明。

// ---- 行悬停详情 ----
listEl.addEventListener("mouseover", (event) => {
  if (dragging || menuOpen || menuMoving || edgeMoving || edgeProgress > 0) return;
  const row = (event.target as Element | null)?.closest<HTMLElement>(".row-item[data-tip]");
  if (row) scheduleRowTip(row);
});
listEl.addEventListener("mouseleave", scheduleTipHide);
listEl.addEventListener("scroll", refreshRowTip);
appEl.addEventListener("mousedown", hideRowTip);
onThemeChange(refreshRowTip);

let tipOpen = false;
let tipMoving = false;
let tipSavedPosition: { x: number; y: number } | null = null;

async function closeTipWindowImpl() {
  if (!tipOpen && !tipMoving && !tipSavedPosition) return;
  tipOpen = false;
  tipMoving = true;
  const saved = tipSavedPosition;
  tipSavedPosition = null;
  try {
    await win.setSize(new LogicalSize(CARD_W, expandedH));
    if (saved && !dragging) await win.setPosition(new LogicalPosition(saved.x, saved.y));
  } finally {
    tipMoving = false;
    if (dockSide) evaluateVisibility();
  }
}

initRowTip({
  close: () => queueMenuOp(closeTipWindowImpl),
  place: (box, row, current) => queueMenuOp(async () => {
    if (!current() || menuOpen || menuMoving || dragging || hidden || edgeMoving || edgeProgress > 0 || edgeTarget > 0) return false;
    tipMoving = true;
    try {
      const { position, screen } = await screenGeometry();
      if (!current() || dragging) return false;
      const origin = tipSavedPosition ?? position;
      const card = { ...origin, width: CARD_W, height: expandedH };
      const plan = tipWidth(card, screen);
      box.style.width = plan.width + "px";
      box.style.maxHeight = "112px";
      const geometry = placeTip(card, screen, row.getBoundingClientRect().top, box.offsetHeight);
      box.style.left = geometry.left + "px";
      box.style.top = geometry.top + "px";
      box.style.maxHeight = geometry.height + "px";
      box.dataset.placement = geometry.side;
      if (!tipSavedPosition) tipSavedPosition = origin;
      tipOpen = true;
      await win.setSize(new LogicalSize(geometry.windowWidth, geometry.windowHeight));
      if (!current() || dragging) return false;
      if (position.x !== geometry.x || position.y !== geometry.y) {
        await win.setPosition(new LogicalPosition(geometry.x, geometry.y));
      }
      return current();
    } finally {
      tipMoving = false;
    }
  }),
});

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
  document.getElementById("ctx-pin")!.classList.toggle("checked", pinned);
  document.getElementById("ctx-undock")!.hidden = !dockSide;
}

async function openMenuImpl(cx: number, cy: number) {
  // 防重入：菜单开着时再次右键不重新打开——否则 menuSavedY 会被覆写成上移后的 y，
  // closeMenu 还原到错误位置（底边附近的卡片会向上漂移，见 §1.18）
  if (menuOpen) return;
  hideRowTip();
  await closeTipWindowImpl();
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
// 鼠标停在卡片上时持续展开，即使临时展开时间已到。
document.addEventListener("mouseenter", () => {
  hovering = true;
  requestEdge(false);
});
// 光标离开窗口也收起：窗口外点击我们收不到，菜单会一直悬在别的应用上面。
// 同时解除悬停挂起，按当前截止时间收起。
document.addEventListener("mouseleave", () => {
  hovering = false;
  hideRowTip();
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
]).then(consumeTrayReveal).catch((e) => console.warn("订阅悬浮窗事件失败：", e));
void loadSessions().catch((e) => console.warn("拉取会话快照失败：", e));
render();
