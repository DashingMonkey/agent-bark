import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { reactive } from "vue";

// ---- agent 品牌图标（内联 SVG，见 assets/agents/README.md 的来源与对应关系）----
// mono 图标（opencode / generic）用 currentColor，跟随文字颜色；
// 彩色图标自带品牌色，深浅主题下都可直接用。
import iconClaudeCode from "./assets/agents/claude-code.svg?raw";
import iconTraeCode from "./assets/agents/trae-code.svg?raw";
import iconTraeWork from "./assets/agents/trae-work.svg?raw";
import iconCodebuddy from "./assets/agents/codebuddy.svg?raw";
import iconQoder from "./assets/agents/qoder.svg?raw";
import iconCodex from "./assets/agents/codex.svg?raw";
import iconZcode from "./assets/agents/zcode.svg?raw";
import iconWorkbuddy from "./assets/agents/workbuddy.svg?raw";
import iconOpencode from "./assets/agents/opencode.svg?raw";
import iconDsh from "./assets/agents/dsh.svg?raw";
import iconGemini from "./assets/agents/gemini-color.svg?raw";
import iconQwen from "./assets/agents/qwen-color.svg?raw";
import iconKimi from "./assets/agents/kimi.svg?raw";
import iconDoubao from "./assets/agents/doubao-color.svg?raw";
import iconCursor from "./assets/agents/cursor.svg?raw";
import iconCopilot from "./assets/agents/copilot-color.svg?raw";
import iconWindsurf from "./assets/agents/windsurf.svg?raw";
import iconGrok from "./assets/agents/grok.svg?raw";
import iconAntigravity from "./assets/agents/antigravity-color.svg?raw";
import iconGeneric from "./assets/agents/generic.svg?raw";

/** agent id → 品牌 SVG（内联标记）。未收录的 agent 回落到 generic 兜底图标 */
const AGENT_ICONS: Record<string, string> = {
  "claude-code": iconClaudeCode,
  "trae-code": iconTraeCode,
  "trae-work": iconTraeWork,
  codebuddy: iconCodebuddy,
  qoder: iconQoder,
  codex: iconCodex,
  zcode: iconZcode,
  workbuddy: iconWorkbuddy,
  opencode: iconOpencode,
  dsh: iconDsh,
  "gemini-cli": iconGemini,
  "qwen-code": iconQwen,
  "kimi-code": iconKimi,
  "kimi-work": iconKimi,
  "doubao-work": iconDoubao,
  cursor: iconCursor,
  "copilot-cli": iconCopilot,
  windsurf: iconWindsurf,
  grok: iconGrok,
  antigravity: iconAntigravity,
};

export function agentIcon(id: string): string {
  return AGENT_ICONS[id] ?? iconGeneric;
}

// ---- 与后端对应的数据结构 ----

export type EventKind =
  | "session_start"
  /// 工具级失败：agent 会自行重试（如编辑前没先读文件），回合仍在跑——不是 run_failed
  | "tool_failed"
  | "permission_required"
  | "input_required"
  | "run_completed"
  | "run_failed"
  /// 用户主动中止（自己按的停止）：不通知，流光在没有别的会话在跑时直接收起
  | "run_aborted";

export interface NormalizedEvent {
  id: string;
  agent: string;
  type: EventKind;
  session_id: string;
  cwd: string;
  project: string | null;
  message: string;
  timestamp: number;
  is_subagent?: boolean;
  /**
   * 派生出这个会话的**父会话 id**（主会话没有这个字段）。
   *
   * 与 [`SessionStatus.parent`] 同一份血缘来源，只是事件侧多带一份：终态驻留行要按它
   * 插回父行的子树下面，否则子代理的「任务完成」行会落在列表末尾那行会话的子树里，
   * 看着像挂到了别的会话下面（实测踩过）。
   */
  parent_session_id?: string | null;
  /// 纯保活心跳（不是活动信号）：daemon 侧只续命、绝不建档。
  /// 前端不消费（心跳本来就不入事件流），这里只为与后端结构保持一致。
  keepalive?: boolean;
}

// 会话实时状态（后端 SessionPhase / SessionStatus 的 snake_case 序列化形态）
export type SessionPhase = "thinking" | "tool_running" | "waiting_permission" | "waiting_input";

export interface SessionStatus {
  agent: string;
  session_id: string;
  project: string | null;
  cwd: string;
  phase: SessionPhase;
  /** 当前/最近回合的 prompt（已截断） */
  prompt: string | null;
  last_tool: string | null;
  turn_count: number;
  tool_calls: number;
  /** 当前回合开始时刻（unix ms） */
  started_at: number;
  last_activity: number;
  /**
   * 父会话的**完整 id**（不是尾号）；**没有这个字段 = 主会话**（后端 `skip_serializing_if`）。
   * 有值即「这是子代理」——与事件侧的 `is_subagent` 同一份血缘来源。
   * 认亲一律拿它与 `session_id` 直接比对，别用截断值（见 [`parentTail`]）。
   */
  parent?: string;
}

/** 会话是否是子代理（悬浮窗、事件流的「运行中会话」区共用这一条判据） */
export function isSubagentSession(s: SessionStatus): boolean {
  return typeof s.parent === "string" && s.parent.length > 0;
}

/**
 * 父会话 id 的**尾 6 位**（仅用于展示血缘小字）。匹配父子一律用完整 id
 * （[`SessionStatus.parent`] 与 `session_id` 直接比对），别拿截断值去认亲。
 */
export function parentTail(parent: string): string {
  return parent.slice(-6);
}

/** 排序 + 挂树后的会话行 */
export interface SessionRow {
  session: SessionStatus;
  /** 缩进层级：0 = 主会话 / 占位父行；子代理挂在它的父行下面 */
  depth: number;
  /**
   * 父链**整条**都不在本批快照里（回合已结束、被摘档或判死淘汰）。
   *
   * 这类子代理**不缩进**（`depth` 恒为 0）：父行不在，缩进就失去参照物，它会紧跟在上一行
   * 的子树后面，看起来像「挂在了那个会话下面」（实测踩过：两个工作区各开一个 DSH 会话时，
   * A 会话的子代理显示在 B 会话的子代理行下面）。父链只缺中间几节时走
   * [`SessionRow.placeholder`]：缺的链节逐级补占位行，子树仍挂在占位行下面。
   */
  parentMissing: boolean;
  /**
   * 占位父行：快照里没有这个会话（已结束/被淘汰），是为了让还活着的子代理有地方可挂
   * 而**合成**的一行。没有状态、没有 prompt，只显示 `会话 …尾号`；前端按此弱化渲染，
   * 不要拿它当真实会话（它不会出现在 `store.sessions` 里）。
   */
  placeholder: boolean;
}

/** 缩进展示上限：再深的层级只保留 ↳ 记号、不再继续右移（卡片只有 240px / 列表也放不下） */
export const SESSION_INDENT_MAX = 2;

/** 占位父行的展示名：`会话 …8ec9f`（与血缘小字同一套尾号口径） */
export function placeholderName(sessionId: string): string {
  return `会话 …${sessionId.slice(-6)}`;
}

/**
 * 把会话快照排成「父行 + 它的子行紧跟其后」的树序（深度优先，同级按开始时间倒序）。
 *
 * 后端快照是按 `started_at` 倒序的平铺列表，直接渲染会让子代理与父会话交错——
 * 一个 fan-out 出去十几次派生的会话会把父行挤到列表深处，「谁派生了谁」完全看不出来。
 *
 * **父链上有节点不在本批快照里**（回合已结束 / 被摘档）时，缺的链节逐级合成**占位行**
 * （`placeholder: true`），子代理照常挂在它下面：平铺到列表末尾会让它贴在别的会话子树
 * 后面，被读成「挂到了那个会话下面」（实测事故）。合成行不承载状态、没有 prompt，
 * 只显示 `会话 …尾号`；它的 `session_id` 就是缺的那个父 id，所以同一父的多个子代理会聚到
 * 同一行下。若**整条父链**都不在快照里，缩进失去参照物 → 该子树顶格平铺（`parentMissing`）。
 *
 * 两条硬约束（都是实测踩过的坑，见 .tmp-svg/check-session-rows.mjs 的用例）：
 * 1. 「缺不缺」一律拿 `byId` 判，**不能**用 `visited` 反推。子行的 `started_at` 更晚、
 *    倒序下常常排在父行前面，先处理子行时会误判「父不在」而合成占位行——占位行与真父行
 *    同 key（`agent|id`），真父行随后被 `visited` 静默吃掉，界面上只剩一条没有徽标、
 *    没有 prompt 的假行。真行先全部走完（父在就挂父下），再收尾剩下的悬空链。
 * 2. 悬空链只从**链首**挂一次。每个悬空子代理各自补链会补出重复占位行（同一父多个子代理时
 *    直接炸成多行）。这里先把缺的 id 登记成 ghost 行、再按「父不在表里」收口，链只补一次。
 *
 * 环（血缘来自外部输入，理论上可被伪造）用 `visited` 兜底：宁可平铺，不死循环。
 */
export function sessionRows(list: SessionStatus[]): SessionRow[] {
  const byParent = new Map<string, SessionStatus[]>();
  const byId = new Map<string, SessionStatus>();
  for (const s of list) {
    byId.set(s.session_id, s);
    if (isSubagentSession(s)) {
      const kids = byParent.get(s.parent!);
      if (kids) kids.push(s);
      else byParent.set(s.parent!, [s]);
    }
  }
  const byStart = (a: SessionStatus, b: SessionStatus) => b.started_at - a.started_at;
  const childrenOf = (id: string) => (byParent.get(id) ?? []).slice().sort(byStart);
  /**
   * 合成占位父行：只在这个 id **确实不在快照里**时才建（`byId` 判，**不能**用 `visited`
   * 反推——子行的 `started_at` 更晚、倒序下常排在父行前面，用它判「父不在」会合成一行与
   * 真父行同 key（`agent|id`）的假行，真父行随后被 `visited` 静默吃掉，界面上只剩一条
   * 没有徽标、没有 prompt 的假行）。同一 id 反复请求返回同一对象，于是「同一父的多个
   * 子代理」共享一行。
   */
  const ghosts = new Map<string, SessionStatus>();
  const pendingGhost = (id: string, kid: SessionStatus, parent: string | undefined): SessionStatus => {
    const found = ghosts.get(id);
    if (found) return found;
    const g = ghostSession(kid, id, parent);
    ghosts.set(id, g);
    return g;
  };
  /**
   * 沿 `parent` 往上补齐缺的链节，返回自顶向下的链。
   *
   * 每一节的 `parent` 都尽量接上**快照里的真实行**（查 `parentOfId` 反查出来的祖先）：
   * 接得上时整条链就是那个真实行的子树，`walk` 自然把它挂进去；接不上（父链整条都不在
   * 快照里）时才顶格平铺——缩进没有参照物。父已在表里 / 撞上已登记的链节 / 超上限就停
   * （上限只是防御性的：快照 ≤64 行，长链只可能来自伪造血缘）。
   */
  const ghostChain = (kid: SessionStatus, ancestorOf: (id: string) => string | undefined): SessionStatus[] => {
    const chain: SessionStatus[] = [];
    let cur: string | undefined = kid.parent;
    while (chain.length < 64 && cur && !byId.has(cur) && !ghosts.has(cur)) {
      const parent = ancestorOf(cur);
      const ghost = pendingGhost(cur, kid, parent !== cur ? parent : undefined);
      chain.unshift(ghost);
      cur = ghost.parent;
    }
    return chain;
  };
  const visited = new Set<string>();
  const out: SessionRow[] = [];
  const keyOf = (s: SessionStatus) => `${s.agent}|${s.session_id}`;
  const walk = (s: SessionStatus, depth: number, parentMissing: boolean, placeholder: boolean) => {
    const key = keyOf(s);
    if (visited.has(key)) return;
    visited.add(key);
    out.push({ session: s, depth, parentMissing, placeholder });
    // 孩子按「父会话 id == 本行 session_id」精确认领（完整 id，不做截断比对）
    for (const kid of childrenOf(s.session_id)) walk(kid, depth + 1, false, false);
  };
  /** 悬空链的下游：顶格平铺，不再累加缩进——缩进没有参照物 */
  const flatten = (s: SessionStatus, depth: number) => {
    if (visited.has(keyOf(s))) return;
    visited.add(keyOf(s));
    out.push({ session: s, depth, parentMissing: true, placeholder: false });
    for (const kid of childrenOf(s.session_id)) flatten(kid, depth);
  };
  // 第一步：快照里的真实行沿真实血缘走完（父在就挂父下，与处理顺序无关）
  for (const s of list.slice().sort(byStart)) if (!isSubagentSession(s)) walk(s, 0, false, false);
  // 第二步：收尾剩下的悬空子代理，给缺的链节登记占位行。
  // **先登记再挂**：边遍历边补会为每个子代理各补一次，同一父补出重复占位行。
  // 「缺的链节的父是谁」只能从真实行的 `parent` 反查——那一节不在快照里，别无来源
  const parentOfId = new Map<string, string | undefined>();
  for (const s of list) if (isSubagentSession(s)) parentOfId.set(s.session_id, s.parent);
  const ancestorOf = (id: string) => parentOfId.get(id);
  for (const kids of byParent.values()) {
    for (const kid of kids.slice().sort(byStart)) if (!visited.has(keyOf(kid))) ghostChain(kid, ancestorOf);
  }
  // 第三步：把占位行挂上。链首顶格、往下逐级缩进，占位行下面接它的真实子树。
  // 只从「自己上面没有占位行」的链首进——其余链节是它的孩子，`walk` 会顺着 `parent` 递归到
  const ghostIdSet = new Set(ghosts.keys());
  for (const [id, g] of ghosts) {
    if (g.parent && ghostIdSet.has(g.parent)) continue;
    walk(g, 0, false, true);
  }
  // 第四步：兜底——血缘成环或链长超上限时，上面几步都碰不到的行顶格平铺（宁可平铺，不丢行）
  for (const s of list.slice().sort(byStart)) flatten(s, 0);
  return out;
}

/**
 * 合成一行占位父行（快照里没有这个会话）。
 *
 * `agent` 取子代理的：子代理与父会话必然是同一个 agent（血缘来自同一个插件的会话头），
 * 这样同一父的多个子代理会聚到同一个 id 下，占位行只建一次。
 *
 * `parent` 尽量接上「缺的父自己的父」——由调用方沿血缘往上读。它能接上真实行时，
 * 占位行本身就是那个真实行的一个孩子（直接挂进去，不用顶格平铺）。
 */
function ghostSession(kid: SessionStatus, id: string, parent: string | undefined): SessionStatus {
  return {
    agent: kid.agent,
    session_id: id,
    project: null,
    cwd: kid.cwd,
    phase: kid.phase,
    prompt: null,
    last_tool: null,
    turn_count: 0,
    tool_calls: 0,
    // 用子代理的开始时刻排序：占位行紧挨着它下面的子代理
    started_at: kid.started_at,
    last_activity: kid.last_activity,
    parent,
  };
}

export type VerifyReport =
  | { state: "not_registered" }
  | { state: "ok" }
  | { state: "stale_path"; path: string }
  | { state: "config_unreadable"; path: string; reason: string };

export interface AgentStatus {
  kind: string;
  display_name: string;
  mode: "hook" | "watch" | "planned";
  installed: boolean;
  registered: boolean;
  enabled: boolean;
  trust_hint: string | null;
  config_paths: string[];
  verify: VerifyReport | null;
  last_error: string | null;
}

export interface Diagnostics {
  config_path: string;
  config_error: string | null;
  server_error: string | null;
  /** 配置里的端口/token 与运行中的服务器不一致 → 需重启才生效 */
  restart_required: boolean;
  /** 启动自检发现的问题（接入自愈失败 / 因配置损坏被置为未启用） */
  startup_warnings: string[];
}

/** 「关于」页检查更新的结果（对应后端 update::UpdateInfo） */
export interface UpdateInfo {
  /** 本地版本号（安装包版本，与「关于」页展示的一致） */
  current: string;
  /** 远端最新版本号（已去 v 前缀） */
  latest: string;
  /** 远端是否比本地新 */
  newer: boolean;
  /** 实际应答的源："gitee" | "github" */
  source: string;
  /** 应答源的发布页（「下载新版本」按钮打开它） */
  page_url: string;
}

/** 「屏幕光效」每状态的效果类型（对应后端 StateEffects） */
export interface StateEffects {
  thinking: string;
  waiting: string;
  completed: string;
  failed: string;
}

export interface GlowConfig {
  enabled: boolean;
  /** 边缘光效总开关：关掉后只隐藏边缘灯带，全屏特效照常 */
  edge: boolean;
  /**
   * 边缘光效位置："top" = 只亮顶部一条（默认，覆盖窗缩成顶部条带，
   * 避免被判定为全屏）；"all" = 四周（旧行为）。后端按 edge_sides() 读时归一
   */
  edge_position: string;
  /** 旧版全局边缘类型：只作迁移源，运行时不再读（改它没有效果） */
  effect: string;
  /** 全屏特效总开关：每次颜色亮起时整屏补一次特效 */
  fullscreen: boolean;
  /** 旧版全局全屏类型：只作迁移源 */
  fullscreen_effect: string;
  /**
   * 生效显示器："all" = 全部；数字 = `listMonitors()` 返回的 index；
   * 旧版的 "primary" 也能读（后端按主显示器处理）
   */
  monitors: string;
  /** 各状态的边缘光效（"none"=无 / "breathing" / "comet"） */
  edge_effects: StateEffects;
  /** 各状态的全屏特效（"none"=无 / "fog" / "scan" / "rain"） */
  burst_effects: StateEffects;
}

/** 「屏幕光效」页「生效显示器」下拉的一项 */
export interface MonitorInfo {
  /** 写进 `glow.monitors` 的值 */
  index: number;
  /** 友好名，如「显示器1」 */
  name: string;
  /** 物理分辨率 */
  width: number;
  height: number;
  scale_factor: number;
  is_primary: boolean;
  x: number;
  y: number;
}

/** 显示器在下拉里的文案：`1.显示器1(1920*1080)`（序号比 index 大 1，分辨率为物理像素）。
 *  调用方按需追加后缀（如「（主显示器）」），这里只出基础文案 */
export function monitorLabel(m: MonitorInfo): string {
  return `${m.index + 1}.${m.name}(${m.width}*${m.height})`;
}

/** 桌面悬浮窗配置（对应后端 WidgetConfig；位置口径见 widget.rs 模块注释） */
export interface WidgetConfig {
  enabled: boolean;
  /** 卡片左上角（逻辑像素）；-1 = 尚未放置过，按默认位置（主屏右上角）打开 */
  x: number;
  y: number;
  /** 固定位置：true 时卡片不响应拖动（悬浮窗右键菜单里切换） */
  pinned: boolean;
  /** 自动隐藏：安静即隐、有事即现（行为口径见 widget.ts 的「自动隐藏」一节） */
  auto_hide: boolean;
}

/** 「通知」页「声音」里单个状态的音效设置（对应后端 StateSound） */
export interface StateSound {
  /** 音效 id，见 SOUND_EFFECTS；空串 = 该状态不响（默认） */
  effect: string;
  /** 播放次数（1~10） */
  plays: number;
}

/** 可选音效目录（与后端 bark-channels 的 SOUND_EFFECTS 保持一致；全部为随应用内置的 CC0 素材） */
export const SOUND_EFFECTS: { id: string; label: string }[] = [
  { id: "chime", label: "风铃" },
  { id: "confirm", label: "确认" },
  { id: "success", label: "成功" },
  { id: "drop", label: "水滴" },
  { id: "pluck", label: "弹拨" },
  { id: "glass", label: "玻璃" },
  { id: "error", label: "错误" },
  { id: "deep", label: "低叮" },
];

/** 「屏幕光效」每状态「边缘光效 → 类型」下拉的选项（对应后端 canon_edge_effect；默认项紧跟「无」） */
export const EDGE_EFFECTS: { id: string; label: string }[] = [
  { id: "none", label: "无" },
  { id: "comet", label: "流光" },
  { id: "breathing", label: "呼吸" },
];

/** 「屏幕光效」每状态「全屏特效 → 类型」下拉的选项（对应后端 canon_burst_effect；默认项紧跟「无」） */
export const BURST_EFFECTS: { id: string; label: string }[] = [
  { id: "none", label: "无" },
  { id: "scan", label: "扫描" },
  { id: "fog", label: "雾散" },
  { id: "rain", label: "矩阵雨" },
];

export interface BarkConfig {
  server: { port: number; token: string };
  /** 每个状态（思考/等待/完成/失败）各自的音效；全部默认不响。enabled 为「声音」区总开关 */
  notify: {
    /** 总开关：关闭后所有状态音效都不响（各状态自己的设置保留） */
    enabled: boolean;
    thinking: StateSound;
    waiting: StateSound;
    completed: StateSound;
    failed: StateSound;
  };
  rules: { aggregate_window_ms: number; quiet_hours: string[] };
  channels: {
    bark: { enabled: boolean; url: string } | null;
    webhook: { enabled: boolean; url: string; method: string; template: string } | null;
  };
  agents: { kind: string; enabled: boolean }[];
  glow: GlowConfig;
  widget: WidgetConfig;
}

// ---- 全局事件流 store ----

export const store = reactive({
  events: [] as NormalizedEvent[],
  /// 会话实时状态快照（bark://sessions 推送全量替换）
  sessions: [] as SessionStatus[],
  /// 会话快照代际：每次 bark://sessions 推送递增。轮询拉取
  /// （list_active_sessions）落地前先核对——慢的旧拉取不得把推送的新快照覆盖回旧数据
  sessionsSeq: 0,
  listening: false,
  /// 事件监听注册失败的原因；非空即表示实时通道未连接，UI 据此显示告警与重试入口
  listeningError: "",
});

// 全局单例事件监听：应用生命周期内只注册一次，**有意不 unlisten**（进程退出时
// 监听随之销毁，因此成功路径不保留返回值引用）。唯一的例外是下面的**失败回滚**——
// 部分注册成功后必须把已注册的 unlisten 逐个兑现，否则「重试」会把两条监听
// 全部再注册一遍（每条事件翻倍，见 code-review §1.17）。
export async function ensureListening() {
  if (store.listening) return;
  store.listening = true;
  // 逐个注册并登记已获得的 unlisten：任一失败都能精确回滚到「一条都没注册」
  const registered: Array<() => void> = [];
  try {
    registered.push(
      await listen<NormalizedEvent>("bark://event", (e) => {
        store.events.unshift(e.payload);
        if (store.events.length > 500) store.events.pop();
      }),
    );
    // 会话实时状态：后端推送全量快照，直接替换；负载异常（null/非数组）忽略，
    // 别让坏推送把 store.sessions 打成非数组、冻结事件流页的会话区
    registered.push(
      await listen<SessionStatus[]>("bark://sessions", (e) => {
        if (!Array.isArray(e.payload)) {
          console.warn("会话快照负载异常，已忽略：", e.payload);
          return;
        }
        store.sessions = e.payload;
        store.sessionsSeq += 1;
      }),
    );
    store.listeningError = "";
  } catch (e) {
    // 整体失败：回滚已注册的监听（这里 unlisten 是失败回滚用途，见函数头注释），
    // 再复位标志并记录原因，允许后续重试且让 UI 能观察到失败
    for (const unlisten of registered) {
      try {
        unlisten();
      } catch (err) {
        console.warn("回滚事件监听失败：", err);
      }
    }
    store.listening = false;
    store.listeningError = `${e}`;
    console.error("事件监听注册失败：", e);
  }
}

// ---- IPC 封装 ----

export const api = {
  agentStatuses: () => invoke<AgentStatus[]>("agent_statuses"),
  setAgentEnabled: (kind: string, enabled: boolean) =>
    invoke<string | null>("set_agent_enabled", { kind, enabled }),
  getConfig: () => invoke<BarkConfig>("get_config"),
  saveConfig: (config: BarkConfig) => invoke<void>("save_config", { config }),
  listEvents: (limit?: number) => invoke<NormalizedEvent[]>("list_events", { limit }),
  /// 清空后端事件历史（事件流页的「清空显示」）：不传 kind 清空全部，否则只清该类型。
  /// 必须清后端——页面重挂载会补拉历史，只删前端副本的话被清掉的事件会复活。
  /// 返回**后端确实删掉的事件 id**：前端按它删本地副本，两边口径才一致。
  clearEvents: (kind?: string) => invoke<string[]>("clear_events", { kind: kind ?? null }),
  listActiveSessions: () => invoke<SessionStatus[]>("list_active_sessions"),
  /// 渠道测试：url / template 传 null 表示用已落盘配置；非 null 用传入值——
  /// 「测试」测的应当是**当前表单里未保存的输入**（bark 传 url，webhook 传 url+template），
  /// 否则新填的地址没保存就点测试，发去的还是旧地址（见 code-review §1.14）
  testChannel: (channel: string, url: string | null, template: string | null) =>
    invoke<void>("test_channel", { channel, url, template }),
  /// 「通知」页「声音」的测试按钮：立即把选定音效播放若干次
  testSound: (effect: string, plays: number) => invoke<void>("test_sound", { effect, plays }),
  diagnostics: () => invoke<Diagnostics>("diagnostics"),
  /// 启动引导判定：本机一个 agent 都没接上（配置开关与磁盘上的接入都没到位）时返回 true，
  /// 前端据此自动弹出面板并停在「Agents 接入」页（见 App.vue 的 guideToAgents）
  needsAgentSetup: () => invoke<boolean>("needs_agent_setup"),
  /// 「关于」页「检查更新」：后端同时问 Gitee（主源）与 GitHub（备源）的最新版本，
  /// 单边失败静默、两边都失败才报错。请求必须走后端——主窗口 CSP 的 connect-src
  /// 只允许 'self'，页面直连外网会被拦掉
  checkUpdate: () => invoke<UpdateInfo>("check_update"),
  /// 「关于」页发现新版本后的「下载新版本」：用系统浏览器打开发布页。
  /// URL 由后端按上次检查的应答源决定，前端不传——IPC 面越小越好
  openReleasePage: () => invoke<void>("open_release_page"),
  /// 设置页预览：临时点亮某个流光状态（"running" | "waiting" | "completed" | "failed" | "idle"）
  glowPreview: (state: string) => invoke<void>("glow_preview", { stateName: state }),
  /// 熄灭：结束进行中的预览并立刻收起光效（下一次真实事件会重新点亮）
  glowOff: () => invoke<void>("glow_off"),
  /// 「屏幕光效」页：实时枚举当前所有显示器（插拔后重新进页面即可刷新）
  listMonitors: () => invoke<MonitorInfo[]>("list_monitors"),
  /// 悬浮窗位置记忆（拖动松手后上报，低频）：只写 widget 段，不整份落盘
  saveWidgetPosition: (x: number, y: number) => invoke<void>("save_widget_position", { x, y }),
  /// 悬浮窗右键菜单「固定位置」开关（落盘到 widget.pinned，重启后保持）
  setWidgetPinned: (pinned: boolean) => invoke<void>("set_widget_pinned", { pinned }),
  /// 悬浮窗右键菜单「关闭悬浮窗」：关掉开关并销毁窗口；
  /// 主窗口的悬浮窗设置页经 bark://widget-closed 同步勾选态
  closeWidget: () => invoke<void>("close_widget"),
  /// 打开主面板（悬浮窗双击 / 托盘菜单「显示主窗口」共用）：隐藏时居中显示并聚焦
  showPanel: () => invoke<void>("show_main_panel"),
};

// ---- 工具 ----

export const AGENT_NAMES: Record<string, string> = {
  "claude-code": "Claude Code",
  "trae-code": "TraeCode",
  codebuddy: "CodeBuddy",
  qoder: "Qoder",
  codex: "Codex",
  zcode: "ZCode",
  opencode: "OpenCode",
  dsh: "DeepSeek Harness",
  "trae-work": "TraeWork",
  workbuddy: "WorkBuddy",
  "doubao-work": "豆包工作",
};

export function agentName(id: string): string {
  return AGENT_NAMES[id] ?? id;
}

/**
 * 简称（跨工作区时行内用，见 widget.ts 的 `rowLabel`）。
 *
 * 行内要同时放下 agent 名与工作区名，全称 + 后缀（`DeepSeek Harness · passport-vibemic`）
 * 在 240px 的卡片里必然被 ellipsis 从中间截掉——两个工作区同 agent 时截出来的前缀还可能
 * 一模一样，等于什么都没显示。只在「需要靠工作区名区分」这一档缩写：单一工作区时行内
 * 只有 agent 名，全称放得下，用简称反而丢信息。
 */
const AGENT_SHORT_NAMES: Record<string, string> = {
  dsh: "DSH",
};

export function agentShortName(id: string): string {
  return AGENT_SHORT_NAMES[id] ?? agentName(id);
}

export const EVENT_LABELS: Record<EventKind, string> = {
  session_start: "会话开始",
  tool_failed: "工具失败",
  permission_required: "需要确认",
  input_required: "等待输入",
  run_completed: "任务完成",
  run_failed: "任务失败",
  run_aborted: "已中止",
};

/**
 * 事件徽标四色规范，与屏幕光效（glow.rs `GlowState::color`）同一套色板：
 * 🔵 思考 · 🟠 等待 · 🟢 完成 · 🔴 失败。色值定义在 style.css 的 --state-*。
 * 工具失败例外用中性灰：agent 会自行重试，不需要用户行动——
 * 橙色「等待」留给「等你确认/输入」这类需要人的场景。
 */
export function eventBadgeClass(kind: EventKind): string {
  switch (kind) {
    // 会话开始 = 思考的起点
    case "session_start": return "badge thinking";
    case "run_completed": return "badge completed";
    // 失败与中止都归「失败」色
    case "run_failed":
    case "run_aborted": return "badge failed";
    case "permission_required":
    case "input_required": return "badge waiting";
    // 工具级失败：中性（可排查、不吓人）
    case "tool_failed": return "badge";
    default: return "badge";
  }
}

export const PHASE_LABELS: Record<SessionPhase, string> = {
  thinking: "思考中",
  tool_running: "执行工具",
  waiting_permission: "等待确认",
  waiting_input: "等待输入",
};

export function phaseBadgeClass(phase: SessionPhase): string {
  switch (phase) {
    case "thinking":
    case "tool_running": return "badge thinking";
    case "waiting_permission":
    case "waiting_input": return "badge waiting";
    // 未知 phase（后端新增/坏推送）：中性灰，别渲染成无样式裸字
    default: return "badge";
  }
}

/**
 * 子代理行的样式类（事件流 / 运行中会话区共用）：`is-sub` = 子代理行（名字与正文弱化），
 * `sub-indent-1` / `sub-indent-2` = 缩进层级（来自 [`sessionRows`]，封顶到
 * [`SESSION_INDENT_MAX`]，再深只保留记号不再右移）。
 *
 * 与悬浮窗的 `.row-item.sub` / `.tree` 是同一套视觉语言：↳ 记号表达层级、
 * 弱化正文把视线让给父会话。类名抽在这里，消费方不会各写一份判据。
 */
export function subagentRowClass(row: SessionRow): string {
  if (!isSubagentSession(row.session)) return "";
  // 缩进层级照常给出：父行缺席时上面有一行**占位父行**兜着（见 sessionRows），
  // 所以「子代理缩进到别人子树下」这件事不会发生
  return `is-sub sub-indent-${Math.min(row.depth, SESSION_INDENT_MAX)}`;
}

/** 运行时长展示（如 "3分20秒" / "1小时02分"） */
export function formatDuration(fromMs: number, toMs: number): string {
  const secs = Math.max(0, Math.floor((toMs - fromMs) / 1000));
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  const p = (n: number) => String(n).padStart(2, "0");
  if (h > 0) return `${h}小时${p(m)}分`;
  if (m > 0) return `${m}分${p(s)}秒`;
  return `${s}秒`;
}

export function formatTime(ts: number): string {
  const d = new Date(ts);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}
