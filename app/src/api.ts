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
  warning: string;
  completed: string;
  terminated: string;
}

export interface GlowConfig {
  enabled: boolean;
  /** 边缘光效总开关：关掉后只隐藏边缘灯带，全屏特效照常 */
  edge: boolean;
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
  /** 各状态的全屏特效（"none"=无 / "fog" / "scan"） */
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
  /**
   * 自动隐藏：安静（无行 / 全部行属思考类）2 秒后隐藏卡片，
   * 出现需要关注的行（等待确认 / 等待输入 / 任务完成 / 任务失败）立即弹出；
   * 手动中止不弹出也不阻止隐藏（与光效「手动中止不提醒」同口径）
   */
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

/** 「屏幕光效」每状态「边缘光效 → 类型」下拉的选项（对应后端 canon_edge_effect） */
export const EDGE_EFFECTS: { id: string; label: string }[] = [
  { id: "none", label: "无" },
  { id: "breathing", label: "呼吸" },
  { id: "comet", label: "流光" },
];

/** 「屏幕光效」每状态「全屏特效 → 类型」下拉的选项（对应后端 canon_burst_effect） */
export const BURST_EFFECTS: { id: string; label: string }[] = [
  { id: "none", label: "无" },
  { id: "fog", label: "雾散" },
  { id: "scan", label: "扫描" },
];

export interface BarkConfig {
  server: { port: number; token: string };
  /** 每个状态（思考/警告/完成/终止）各自的音效；全部默认不响。enabled 为「声音」区总开关 */
  notify: {
    /** 总开关：关闭后所有状态音效都不响（各状态自己的设置保留） */
    enabled: boolean;
    thinking: StateSound;
    warning: StateSound;
    completed: StateSound;
    terminated: StateSound;
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
};

export function agentName(id: string): string {
  return AGENT_NAMES[id] ?? id;
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
 * 🔵 思考 · 🟠 警告 · 🟢 完成 · 🔴 终止。色值定义在 style.css 的 --state-*。
 * 工具失败例外用中性灰：agent 会自行重试，不需要用户行动——
 * 橙色「警告」留给「等你确认/输入」这类需要人的场景。
 */
export function eventBadgeClass(kind: EventKind): string {
  switch (kind) {
    // 会话开始 = 思考的起点
    case "session_start": return "badge thinking";
    case "run_completed": return "badge completed";
    // 失败与中止都归「终止」色
    case "run_failed":
    case "run_aborted": return "badge terminated";
    case "permission_required":
    case "input_required": return "badge warning";
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
    case "waiting_input": return "badge warning";
    // 未知 phase（后端新增/坏推送）：中性灰，别渲染成无样式裸字
    default: return "badge";
  }
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
