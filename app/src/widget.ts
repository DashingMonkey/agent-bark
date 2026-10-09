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
// 自动隐藏（widget.auto_hide）：可见性状态机也在这里，完整规则与口径见
// 下方「自动隐藏」一节。auto_hide 开着时 Rust 建窗即隐藏（登录不闪卡片），
// 首批数据到达后才决定要不要弹出。

import { listen } from "@tauri-apps/api/event";
import { currentMonitor, getCurrentWindow, LogicalPosition, LogicalSize } from "@tauri-apps/api/window";
import {
  api,
  agentIcon,
  agentName,
  agentShortName,
  isSubagentSession,
  PHASE_LABELS,
  placeholderName,
  SESSION_INDENT_MAX,
  sessionRows,
  type NormalizedEvent,
  type SessionRow,
  type SessionStatus,
} from "./api";
import { initTheme } from "./theme";
import { hideRowTip, scheduleRowTip } from "./widget-tip";
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
/**
 * 子代理行「首级」的左内边距。
 *
 * 基准是主会话行里**正文起点**的偏移（行内边距 2 + 图标 15 + 行 gap 8 = 25，见 widget.html
 * 的 `.row-item` / `.agent-icon`），再加 6px 让子代理整体比父行的名字**略右一点**：
 * 完全齐平时（实测差 2px 内）只剩 ↳ 和小一号字体在表达层级，扫一眼分不出谁是父；
 * 略右移既不浪费宽度，又把「这是派生出来的」写在缩进上。
 *
 * 主会话行有图标、子代理行没有，按层级数直接缩进（2 起）会整体偏左一大截，所以首级
 * 直接顶到上面算出的位置，再深的层级才按级右移。
 */
const SUB_ALIGN_PX = 31;
const IDLE_H = 38;
/** 完成/失败事件的驻留时长：与屏幕光效的完成色停留同量级 */
const TERMINAL_HOLD_MS = 6000;
/** 拖动停止多久后再读窗口位置落盘（等 OS 拖拽循环安静下来） */
const SAVE_DELAY_MS = 220;
/** 转入安静后多久隐藏（需求给定的固定值，不做配置项） */
const HIDE_DELAY_MS = 5000;
/** 淡入 / 淡出时长（CSS 过渡同值，见 widget.html） */
const FADE_MS = 160;

type StateColor = "thinking" | "waiting" | "completed" | "failed";

let pinned = init.pinned;
let dragging = false;
/** 卡片高度（随行数变化），窗口尺寸跟随它 */
let expandedH = IDLE_H;

/**
 * 上一帧「是否存在多个工作区」的判定（由 [`currentRows`] 每次渲染更新）。
 *
 * 终态驻留行在 `onEvent` 里就要定名字，那时没有整份快照可遍历。它只决定终态行的行内
 * 标签带不带 ` · 工作区名` 后缀——不带的话，跨工作区时它跟另一个工作区的行长得一样，
 * 会被读成「挂在别人下面」（实测踩过）。
 */
let lastMixedWorkspaces = false;

interface Row {
  key: string;
  agent: string;
  /** 主标签：agent 名；跨工作区时带 ` · 工作区名` 后缀，占位父行是 `会话 …尾号`（见 [`rowLabel`]） */
  name: string;
  /**
   * 悬停提示文案（见 [`rowTip`]）；空串 = 该行不给提示（行上已经写着了）。
   * 用 `data-tip` 属性下发：行是每次 render 重建的，挂在元素上的监听会泄漏
   * （见 widget-tip.ts 的说明）。
   */
  tip: string;
  label: string;
  state: StateColor;
  /**
   * 需要关注（自动隐藏状态机的「有事」档）：等待确认 / 等待输入（等待）、
   * 任务完成、任务失败。思考类（思考中 / 执行工具）与手动中止（已中止）不算——
   * 中止是用户自己按的停止，不值得把卡片弹回来（与光效同口径）
   */
  attention: boolean;
  /** 会话行（进行中的事件流）：新会话行出现时弹出「一下」；驻留行（完成/中止）不算 */
  isSession: boolean;
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
    if (seq === sessionsSeq) sessions = s; // 期间已有新推送接管：丢弃这份旧拉取
    render();
  } catch {
    /* 静默：推送通道可能仍在 */
  }
}

/**
 * 会话行的**主标签**：默认是 agent 名（图标同源，见 `agentIcon`）；跨工作区时加
 * ` · 工作区名` 后缀，且那时改用**简称**。
 *
 * 为什么跨工作区必须把工作区名写在行上：同一个 agent 在两个工作区各开一个会话时，两行
 * 都是「DeepSeek Harness」，光看行分不出谁是谁（实测踩过：用户把另一工作区的会话当成了
 * 「凭空多出来的会话」）。只有一个工作区时不写——纯冗余，还占宽度。
 *
 * 为什么那时改用简称：全称 + 后缀（`DeepSeek Harness · passport-vibemic`）在 240px 的
 * 卡片里必然被 ellipsis 从中间截掉，两个工作区截出来的可能都是 `DeepSeek Harness · p…`，
 * 等于白写。简称（`DSH · passport-vibemic`）才放得下能区分的那一段。
 */
function rowLabel(agent: string, project: string | null, mixedWorkspaces: boolean): string {
  return mixedWorkspaces && project ? `${agentShortName(agent)} · ${project}` : agentName(agent);
}

/**
 * 子代理行的主标签：`DSH 子代理` / `DeepSeek Harness 子代理`。
 *
 * **不写工作区名**：子代理跟它的父会话必然在同一个工作区，行上再写一遍既是冗余、又跟父行
 * 抢宽度（父行写工作区名是为了区分「同 agent 的多个工作区」，子代理没有这个歧义）。
 * 写「子代理」三个字反而解决了真正的问题——一眼看出这行是派生的，而不是又一个会话。
 *
 * 缩写在跨工作区时用（与父行的口径一致）：那时行上要放下「agent 名 + 子代理」。
 */
function subagentLabel(agent: string, mixedWorkspaces: boolean): string {
  return `${mixedWorkspaces ? agentShortName(agent) : agentName(agent)} 子代理`;
}

/**
 * 悬停提示的文案（指针停在该行上时浮出），`""` = 不给提示、不渲染 `data-tip`。
 *
 * 内容就是**工作区名本身**。为什么不带 agent 名：行上已经写着 agent（单一工作区）或
 * `DSH · 工作区`（跨工作区），提示里再拼一遍是重复；用户要的就是「这行是哪个工作区」。
 *
 * 工作区为什么放在悬停里而不是行内：**子代理与主会话在同一个工作区**，行内再写一遍纯属
 * 冗余（一张卡上同一个名字重复五六次）；它真正有用的场景是「同一个 agent 开了多个工作区」
 * ——鼠标一停就能确认这行是哪个会话，不用常驻占位。
 *
 * 子代理行不给提示（调用点直接跳过）：它的标签已经是 `X 子代理`，工作区与父行相同。
 */
function rowTip(project: string | null): string {
  return project ? project : "";
}

/**
 * 已「手动终止」的会话行：`agent|session_id` → 标记时刻。
 *
 * 用户按停止时**不合成新行**，而是把已经显示着的那一行就地标成「手动终止」（红点，
 * 与失败同色但文案不同）。为什么不用驻留行：那会多出一行「主的」，或者（父行已不在快照
 * 里时）合成一行 `会话 …尾号` —— 一串 id 尾号既难认、也不是用户想看的（实测被点名）。
 *
 * daemon 收到终态就把条目摘掉，所以这一行可能只存在一帧。为了让它**看得清**，标记会
 * 保留 [`ABORT_HOLD_MS`]：行从快照里消失后，用最后见过的样子把这一行补在列表末尾
 * （见 `currentRows`），到期自然退场。
 */
const abortedRows = new Map<string, number>();
/**
 * 手动终止行的保留时长。取 5s：与安静自动隐藏（`HIDE_DELAY_MS`）同量级，够看清、
 * 又不会在卡片上赖着不走。
 */
const ABORT_HOLD_MS = 5000;
/**
 * 手动终止的文案。主面板事件流的 `EVENT_LABELS.run_aborted` 是「已中止」，这里刻意不同：
 * 卡片上的行要回答「这行怎么了」，而 `run_aborted` 只会由用户自己按停止产生，
 * 写「手动终止」比「已中止」更没有歧义（说清楚是谁中止的）。
 */
const ABORT_LABEL = "手动终止";
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
 * 这份是纯展示缓存，凡渲染过的行都进（父行正常结束 / 被判死摘档也走这条路）。
 */
const lastSeenRow = new Map<string, { name: string; tip: string }>();

/** 完成/失败事件 → 驻留行（思考/等待由会话表表达，终态靠这里短暂展示） */function onEvent(ev: NormalizedEvent) {
  // 推送负载兜底：null / 异常负载直接忽略，别把整个渲染循环冻在旧帧
  if (!ev || typeof ev !== "object") {
    console.warn("事件推送负载异常，已忽略：", ev);
    return;
  }
  const map: Partial<Record<NormalizedEvent["type"], [StateColor, string]>> = {
    run_completed: ["completed", "任务完成"],
    run_failed: ["failed", "任务失败"],
    run_aborted: ["failed", ABORT_LABEL],
  };
  const hit = map[ev.type];
  if (!hit) return;
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
          ? subagentLabel(ev.agent, lastMixedWorkspaces)
          : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
        tip: "",
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
    return;
  }
  abortedRows.delete(sessionRowKey(ev.agent, ev.session_id));
  abortedMemo.delete(sessionRowKey(ev.agent, ev.session_id));
  // 子代理的终态只驻留展示、**不算「有事」**：一个任务拆出十几个子代理，
  // 逐个收尾都把卡片弹回来会吵得没法干活（与通知/音效/全屏特效的 is_subagent 门
  // 同一条口径）。行本身照常出现——用户要的就是「看得见子代理在做什么」。
  terminal = {
    key: ev.id,
    agent: ev.agent,
    // 驻留行与它**取代的那一行同名**：跨工作区时会话行带 ` · 工作区名`、子代理行写
    // 「子代理」。名字不一致就会被读成「挂在别人下面」（实测踩过：子代理收尾那一瞬间，
    // 它的「任务完成」行看着挂到了另一个工作区的会话下）。单一工作区时才用全称——
    // 那时没有歧义，缩写反而是丢信息
    name: ev.is_subagent
      ? subagentLabel(ev.agent, lastMixedWorkspaces)
      : rowLabel(ev.agent, ev.project, lastMixedWorkspaces),
    // 驻留行是短命的一条，不给提示（免得高度/焦点刚变就浮一个框）
    tip: "",
    label: hit[1],
    state: hit[0],
    attention: !ev.is_subagent,
    isSession: false,
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
  // 不截断：所有会话都展示（用户明确要求），卡片高度随行数增长
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
    const state: StateColor =
      s.phase === "waiting_permission" || s.phase === "waiting_input" ? "waiting" : "thinking";
    const sub = isSubagentSession(s);
    // 手动终止：把**已有的这一行**就地改成「手动终止」（见 abortedRows）。它是终态，
    // 所以不再算「等待中」的等待色；子代理行照旧不置 attention
    const aborted = abortedRows.has(sessionRowKey(s.agent, s.session_id));
    // 这一行还在快照里：把「上一帧见到的样子」记准（常驻父行会用到）
    if (aborted && !placeholder) {
      abortedMemo.set(sessionRowKey(s.agent, s.session_id), {
        name: sub ? subagentLabel(s.agent, mixedWorkspaces) : rowLabel(s.agent, s.project, mixedWorkspaces),
        tip: sub ? "" : rowTip(s.project),
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
        ? subagentLabel(s.agent, mixedWorkspaces)
        : rowLabel(s.agent, s.project, mixedWorkspaces);
    const tip = placeholder || sub ? "" : rowTip(s.project);
    // 每一行都留一份「上一帧见到的样子」：父行消失后要靠它把占位父行还原成真名字
    lastSeenRow.set(sessionRowKey(s.agent, s.session_id), { name, tip });
    return {
      key: s.session_id,
      agent: s.agent,
      name,
      // 子代理行不给提示：它的标签已经说清了「是什么」，工作区名与父行重复
      tip,
      // 未知 phase 兜底显示原值：undefined 漏进 escapeHtml 会抛 TypeError、悬浮窗冻结在旧帧
      label: placeholder ? "已结束" : aborted ? ABORT_LABEL : (PHASE_LABELS[s.phase] ?? String(s.phase)),
      state: aborted ? "failed" : state,
      // 等待（等待确认 / 等待输入）要人管 = 有事；思考类（思考中 / 执行工具）不用。
      // 子代理行恒不置位：它做的事是父回合的一部分，弹卡片这件事留给父会话
      // （父会话在等子代理期间零事件，但心跳经 touch_ancestors 续着，不会被判死）。
      // 占位父行同理：它只是给子代理一个落脚点，不该把卡片弹回来
      attention: !placeholder && !aborted && state === "waiting" && !sub,
      isSession: !placeholder,
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
        state: aborted ? "failed" : "completed",
        // 手动终止是用户自己按的、结束是自然的：都不该把卡片弹回来
        attention: false,
        isSession: false,
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

function render() {
  // 行要整段重建（innerHTML）：提示框记的是旧行的元素，重建后它的位置就没人维护了。
  // 重建即收起——下次 mouseover 会按新行重新排（见 widget-tip.ts 的时序说明）
  hideRowTip();
  const list = currentRows();
  if (!list.length) {
    appEl.innerHTML = `<div class="card"><div class="row-item idle"><img src="${logo}" alt="" /><span>空闲 · 没有进行中的会话</span></div></div>`;
  } else {
    appEl.innerHTML =
      `<div class="card">` +
      list
        .map((r) => {
          // 子代理行：行首 ↳（层级缩进内联给出——深度是数据，不是样式），
          // 名字与状态由 CSS 弱化（见 widget.html 的 .row-item.sub）。
          // 父会话不在快照里的子代理**不顶格**：api 会给它补一行占位父行（见 sessionRows），
          // 它照常缩进挂在占位行下面——顶格平铺会被读成「挂到了别的会话下面」（实测事故）
          // 缩进：首级顶到正文起点（与上一行的会话名对齐，见 SUB_ALIGN_PX），再深的层级
          // 按级右移；层级封顶（与事件流同一口径，见 SESSION_INDENT_MAX）——卡片只有
          // 240px，一直右移会把名字挤没。
          // 注意：缩进看 `depth`，而「是不是子代理」看 `r.subagent`——子代理可能因为
          // 父行被摘掉而 `depth = 0`（见 currentRows），那时它仍是子代理行
          const step = Math.min(r.depth, SESSION_INDENT_MAX) - 1;
          // 占位父行没有 ↳ 记号，补回记号占的宽度，才与别的行的正文起点一致
          const pad = r.subagent ? SUB_ALIGN_PX + Math.max(step, 0) * 11 : r.placeholder ? SUB_ALIGN_PX - 3 : 0;
          const indent = pad > 0 ? ` style="padding-left:${pad}px"` : "";
          const mark = r.subagent ? `<span class="tree">↳</span>` : "";
          // 占位父行与子代理行都不放图标：子代理的父行就在正上方，同一棵树必然同 agent；
          // 占位行则根本不知道是哪个 agent 的会话（它不在快照里）
          const icon =
            r.subagent || r.placeholder ? "" : `<span class="agent-icon">${agentIcon(r.agent)}</span>`;
          // 工作区名走悬停提示（data-tip，由 widget-tip.ts 的委托监听消费）：
          // 空串 = 该行没什么可补充的，不提示
          const tip = r.tip ? ` data-tip="${escapeHtml(r.tip)}"` : "";
          const cls = `${r.subagent ? " sub" : ""}${r.placeholder ? " placeholder" : ""}`;
          return (
            `<div class="row-item${cls}"${indent}${tip}>` +
            mark +
            icon +
            `<span class="name">${escapeHtml(r.name)}</span>` +
            `<span class="state ${r.state}"><i class="dot"></i>${escapeHtml(r.label)}</span>` +
            `</div>`
          );
        })
        .join("") +
      `</div>`;
  }
  // 全部会话行都展示（不折叠）：卡片高度 = 行数 × 行高，行多了卡片就长
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
// 规则（隐藏时长唯一出处是 HIDE_DELAY_MS，注释不重复具体秒数）：
// - 有事（任一行 attention）→ 立即淡入弹出，并取消待隐藏计时；
// - 新会话行（= 新的事件流）出现 → 弹出「一下」：不做特殊驻留、照常随
//   安静去抖隐藏，只当「任务被接手」的回执，不与有事档的驻留抢戏；
// - 安静（无行 / 全部行不 attention）→ 去抖 HIDE_DELAY_MS 后淡出隐藏，
//   期间回到有事则取消；安静期间的行数抖动（思考中 ↔ 执行工具）不重排
//   计时，别把隐藏无限推迟；
// - 悬停 / 拖动 / 右键菜单打开期间挂起隐藏（正在读卡、拖动、点菜单时窗口不能跑），
//   解除后重新起算；
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
/** 安静去抖定时器 */
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
  const newStream = rows.some((r) => r.isSession && !knownRowKeys.has(r.key));
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
  // 新流的「一下」= 照常随安静去抖隐藏；已可见时不抢已在走的计时
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

/** 排一次「安静后隐藏」（已排过就沿用旧计时，见规则第三条） */
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
  hideRowTip(); // 提示框是独立 DOM，卡片淡出时必须一起收，否则会留在桌面上
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
  evaluateVisibility(); // 拖动结束，解除挂起
});

// 双击悬浮窗的动作并入上面的 mousedown（e.detail >= 2）——见那里的原因说明。

// ---- 行悬停提示（工作区名）----
//
// 事件委托而不是逐行挂监听：行是每次 render 重建 innerHTML 的，挂在元素上的监听会随帧泄漏
// （见 widget-tip.ts）。`mouseover` / `mouseout` 会冒泡，正好用来在容器上接。
appEl.addEventListener("mouseover", (e) => {
  const row = (e.target as Element | null)?.closest?.(".row-item[data-tip]") as HTMLElement | null;
  // 移到同卡片里**没有**提示的行（多工作区时行内已写工作区名、占位行、驻留行）也要收起：
  // 不处理的话上一条提示会 stale 地挂在桌上——`mouseout` 那边只在真正离开卡片时才收，
  // 行与行之间移动的 `relatedTarget` 非空，收不到这里
  if (!row) {
    if ((e.target as Element | null)?.closest?.(".row-item")) hideRowTip();
    return;
  }
  scheduleRowTip(row.dataset.tip ?? "", row);
});
appEl.addEventListener("mouseout", (e) => {
  // 只在真正离开卡片时收起：行与行之间移动会连发 out/in，靠 in 那边接管即可
  if (!(e as MouseEvent).relatedTarget) hideRowTip();
});
appEl.addEventListener("mouseleave", hideRowTip);
// 拖动 / 右键菜单 / 双击打开面板时提示都该消失（不然会挂在桌面上）
appEl.addEventListener("mousedown", hideRowTip);

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
// 悬停挂起自动隐藏：鼠标在卡片上（正在读 / 拖 / 点菜单）时不许窗口跑掉。
// 淡出途中鼠标进来了也算——showCard 会把淡出拉回
document.addEventListener("mouseenter", () => {
  hovering = true;
  cancelHide();
  if (fadingOut()) showCard();
});
// 光标离开窗口也收起：窗口外点击我们收不到，菜单会一直悬在别的应用上面。
// 同时解除悬停挂起（悬停期间不计时）
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
