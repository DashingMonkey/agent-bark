<script setup lang="ts">
import { computed, onActivated, onDeactivated, onMounted, onUnmounted, ref } from "vue";
import { api, type BarkConfig, type Diagnostics } from "../api";
import { toast } from "../toast";
import { enable, disable, isEnabled } from "@tauri-apps/plugin-autostart";

const config = ref<BarkConfig | null>(null);
const diag = ref<Diagnostics | null>(null);
const diagError = ref("");
const saving = ref(false);
const autostart = ref(false);
const autostartError = ref("");
const loadError = ref("");
const errorMsg = ref("");

/** 「免打扰时段」标签旁感叹号的悬停提示（与悬浮窗页的自动隐藏说明同一套） */
const QUIET_TIP = "未设置，事件全天推送";

// ---- 未保存提示（本页表单只在「保存」时落盘；页面被 KeepAlive 保活，切页不丢但也不落盘）----

/** 已落盘表单的快照（加载 / 保存成功时更新），与当前表单比对得出 dirty */
const savedSnapshot = ref("");

function formSnapshot(): string {
  // 本页可编辑的表单 = 通知规则段 + 结构化的免打扰时段（含原样保留的无法识别值）
  return JSON.stringify({
    rules: config.value?.rules,
    quiet: quietSegments.value,
    unknown: quietUnrecognized.value,
  });
}

/** 有未保存的修改：表单与已落盘快照不一致 */
const dirty = computed(() => !!config.value && formSnapshot() !== savedSnapshot.value);

// ---- 免打扰时段：结构化录入（一行一段、两个原生时间选择器），防呆优先 ----
// 曾经是自由文本框：全角冒号 / 缺分钟 / 笔误全要靠前后端各写一遍解析来兜。
// <input type="time"> 的取值天然就是 HH:mm，格式类错误从根上消失，需要校验的
// 只剩「起止相同」（后端视为不生效）与「留空」。手改配置文件的值加载时仍按
// 旧规则解析（宽容全角冒号与空格，并补零归一成 HH:mm）；解析不了的按原样保留、
// 展示为可删除项，不静默丢弃用户手写的数据。
interface QuietSegment {
  /** 稳定标识（列表渲染 key）：删除/重排时不复用 DOM，两个时间选择器的状态不串行 */
  id: number;
  start: string;
  end: string;
}

const quietSegments = ref<QuietSegment[]>([]);
const quietUnrecognized = ref<string[]>([]);
let nextSegmentId = 1;

/** 手写的 "9:5" 之类补零归一成 "09:05"：<input type="time"> 只认 HH:mm，
 *  不归一就显示空白、用户一动那格就把值丢了（见 code-review §2.16） */
function normalizeHhmm(s: string): string {
  const [h, m] = s.split(":");
  return `${h.padStart(2, "0")}:${m.padStart(2, "0")}`;
}

function parseQuietHours(hours: string[]) {
  const segs: QuietSegment[] = [];
  const unknown: string[] = [];
  for (const raw of hours) {
    const s = raw.replace(/：/g, ":").replace(/\s+/g, "");
    const idx = s.indexOf("-");
    const start = idx > 0 ? s.slice(0, idx) : "";
    const end = idx > 0 && idx < s.length - 1 ? s.slice(idx + 1) : "";
    if (isValidHhmm(start) && isValidHhmm(end)) {
      segs.push({ id: nextSegmentId++, start: normalizeHhmm(start), end: normalizeHhmm(end) });
    } else {
      unknown.push(raw);
    }
  }
  quietSegments.value = segs;
  quietUnrecognized.value = unknown;
}

function addQuietSegment() {
  // 默认给一个最常见的「跨零点夜间」写法
  quietSegments.value.push({ id: nextSegmentId++, start: "22:00", end: "08:00" });
}

function removeQuietSegment(i: number) {
  quietSegments.value.splice(i, 1);
}

function removeUnrecognized(u: string) {
  quietUnrecognized.value = quietUnrecognized.value.filter((x) => x !== u);
}

function quietValidationError(): string {
  for (const s of quietSegments.value) {
    if (!s.start || !s.end) {
      return "免打扰时段有未填完整的起止时间，请补全或删除该行";
    }
    // 24:00 只在区间终点有定义（与后端 parse_hhmm 口径一致）：起点用 24:00 是无效值
    if (s.start === "24:00") {
      return `免打扰时段 ${s.start}-${s.end} 的开始时间不能是 24:00（24:00 只能作为结束时间），请修改或删除该行`;
    }
    if (s.start === s.end) {
      return `免打扰时段 ${s.start}-${s.end} 起止相同不会生效，请修改或删除该行`;
    }
  }
  return "";
}

async function load() {
  loadError.value = "";
  try {
    config.value = await api.getConfig();
    parseQuietHours(config.value.rules.quiet_hours);
    savedSnapshot.value = formSnapshot(); // 已落盘基线：之后的差异都算「未保存的修改」
  } catch (e) {
    loadError.value = `加载配置失败：${e}`;
  }
  // 开机自启动状态单独读取：它失败不能让整个页面变成错误态（config 已就绪时
  // 错误分支本就不可达），只在开关旁给出提示。
  await loadAutostart();
  // 诊断信息同样独立获取，失败时显示一行提示而非静默吞掉
  await loadDiagnostics();
}

async function loadAutostart() {
  autostartError.value = "";
  try {
    autostart.value = await isEnabled();
  } catch (e) {
    autostartError.value = `读取开机自启动状态失败：${e}`;
  }
}

async function loadDiagnostics() {
  try {
    diag.value = await api.diagnostics();
    diagError.value = "";
  } catch (e) {
    diag.value = null;
    diagError.value = `诊断信息获取失败：${e}`;
  }
}

// 运行期事件服务器可能挂掉（端口被别的程序抢走）或配置漂移，
// 只在 onMounted 取一次会让页面一直显示旧状态，故定时刷新。
let diagTimer: ReturnType<typeof setInterval> | null = null;

// ---- 免打扰时段的兜底解析（只用于**加载**手改配置的旧值）----
// 后端 parse_hhmm 按 ':' 拆两段取整数：小时 0..=23 且分钟 0..=59，或恰好 24:00
// （24:00 只在区间终点有定义，起点用 24:00 由 quietValidationError 拦下）。
// 录入控件保证新值合法，这里只负责把用户手写的值读进来（并补零归一）
const HHMM = /^\d{1,2}:\d{1,2}$/;

function isValidHhmm(s: string): boolean {
  if (!HHMM.test(s)) return false;
  const [h, m] = s.split(":").map(Number);
  if (h === 24) return m === 0;
  return h >= 0 && h <= 23 && m >= 0 && m <= 59;
}

/**
 * 只落盘，结果用右下角 toast 反馈：按钮保存与开关类的即时保存（悬浮窗页的
 * persist 与此同款约定）共用，成败都不再动「保存」按钮的文字
 * （旧版闪「保存中… / 已保存 ✓」，观感很差）。
 */
async function persist(): Promise<boolean> {
  if (!config.value) return false;
  try {
    await api.saveConfig(config.value);
    return true;
  } catch (e) {
    toast(`保存失败：${e}`, "error");
    return false;
  }
}

/**
 * 事件聚合窗口的校验/夹取（数字框可能被清空成 ""，也能填负数 / 小数 / 超界值）：
 * 必须是 0..600000 的整数，非法一律回退默认 1500 并给出提示——坏值直接提交会让
 * 后端 serde 拒收**整份**配置（不只这一个字段，见 code-review §2.16）。
 * 归一结果写回表单（输入框当场归位），返回提示文案（无问题为空串）。
 */
function normalizeAggregateWindow(): string {
  const cfg = config.value;
  if (!cfg) return "";
  const raw = cfg.rules.aggregate_window_ms as number | string;
  // ""（清空）要按类型先挡掉：Number("") 是 0，会被误当成合法值
  const n = typeof raw === "number" && Number.isFinite(raw) ? Math.round(raw) : NaN;
  const fixed = Number.isFinite(n) && n >= 0 && n <= 600000 ? n : 1500;
  cfg.rules.aggregate_window_ms = fixed;
  return fixed === raw
    ? ""
    : `事件聚合窗口「${raw}」无效（整数、0~600000），已修正为 ${fixed}ms 并按此保存`;
}

async function save() {
  if (!config.value) return;
  const err = quietValidationError();
  if (err) {
    errorMsg.value = err;
    return;
  }
  // 聚合窗口先校验/夹取再落盘（杜绝 ""/负数让整份 saveConfig 被 serde 拒收）
  const aggNotice = normalizeAggregateWindow();
  saving.value = true;
  try {
    // 无法识别的历史手写值按原样保留（后端会忽略它们并打日志），不静默删除
    config.value.rules.quiet_hours = [
      ...quietSegments.value.map((s) => `${s.start}-${s.end}`),
      ...quietUnrecognized.value,
    ];
    if (await persist()) {
      // 保存成功清空旧报错；本次校正提示（如果有）接着展示——值已按提示落盘
      errorMsg.value = aggNotice;
      savedSnapshot.value = formSnapshot(); // 已落盘基线跟上，「未保存」提示消失
      toast("已保存");
    }
  } finally {
    saving.value = false;
  }
}

async function toggleAutostart(on: boolean) {
  try {
    if (on) {
      await enable();
    } else {
      await disable();
    }
    errorMsg.value = ""; // 设置成功清空旧报错（失败提示不该一直挂着）
  } catch (e) {
    errorMsg.value = `开机自启动设置失败：${e}`;
  }
  // 无论成败都回读真实状态，避免勾选态与实际自启动状态相反
  await loadAutostart();
}

// 悬浮窗的启用 / 自动隐藏已移入「悬浮窗」页（WidgetPage），本页不再管理

function stopDiagTimer() {
  if (diagTimer !== null) {
    clearInterval(diagTimer);
    diagTimer = null;
  }
}

onMounted(() => {
  load();
  diagTimer = setInterval(loadDiagnostics, 15_000);
});

// 页面被 KeepAlive 保活（App.vue）：切走时暂停轮询，切回来再补一次即时刷新 + 重启轮询，
// 别让隐藏的页面一直空转，也别回来时显示十几秒前的旧诊断
onActivated(() => {
  if (diagTimer === null) {
    void loadDiagnostics();
    diagTimer = setInterval(loadDiagnostics, 15_000);
  }
});

onDeactivated(stopDiagTimer);
onUnmounted(stopDiagTimer);
</script>

<template>
  <div v-if="config">
    <div class="between mb-14">
      <h1 class="page-title">设置</h1>
      <div class="row">
        <button :disabled="saving" @click="save">保存</button>
      </div>
    </div>
    <!-- 未保存提示：表单只在「保存」时落盘（切页不丢——页面被 KeepAlive 保活），给个明示 -->
    <div v-if="dirty" class="warn-box mb-14">有未保存的修改：点右上角「保存」写入配置（切页不会丢失）</div>
    <div v-if="errorMsg" class="warn-box mb-14">{{ errorMsg }}</div>

    <!-- 开关类设置：左标题 + 一行说明、右开关，行间发丝线分隔（VS Code / Codex 设置页的排法） -->
    <div class="card">
      <div class="setting-row">
        <div class="setting-text">
          <span class="setting-label">开机自启动</span>
        </div>
        <label class="switch">
          <input type="checkbox" :checked="autostart" @change="toggleAutostart(($event.target as HTMLInputElement).checked)" />
          <span class="track"></span>
        </label>
      </div>
      <div v-if="autostartError" class="warn-box mt-10 mb-10">
        <div class="row">
          <span class="grow">{{ autostartError }}</span>
          <button class="ghost" @click="loadAutostart">重试</button>
        </div>
      </div>
      <!-- 悬浮窗的启用 / 自动隐藏已移入侧边栏「悬浮窗」页 -->
    </div>

    <div class="card">
      <span class="agent-name">通知规则</span>
      <div class="mt-14 col">
        <div class="row">
          <span class="hint field-label">事件聚合窗口</span>
          <div class="input-group input-narrow">
            <input type="number" v-model.number="config.rules.aggregate_window_ms" min="0" max="600000" step="100" />
            <span class="input-unit">ms</span>
          </div>
          <span class="hint">同 agent 同类事件在窗口内合并为一条</span>
        </div>
        <!-- 免打扰时段：一行一段，两个原生时间选择器各带自己的描边与焦点环，
             中间是「起 → 止」箭头，删除用图标按钮、添加用虚线框（VS Code 列表行的做法）。
             标签按右侧首行控件的中线对齐（.row.top），不再对着整段列表居中 -->
        <div class="row top">
          <span class="hint field-label label-line">
            <span class="label-text">免打扰时段</span>
            <!-- 口径说明收进悬停提示：感叹号样式与 Agents 接入页的「接入说明」同一套 -->
            <span v-tooltip="QUIET_TIP" class="help-mark" role="img" :aria-label="QUIET_TIP">
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <circle cx="8" cy="8" r="6.1" />
                <path d="M8 4.9v4" />
                <circle class="dot" cx="8" cy="11.1" r="0.9" />
              </svg>
            </span>
          </span>
          <div class="col grow">
            <div v-if="quietSegments.length || quietUnrecognized.length" class="quiet-list">
              <div v-for="(seg, i) in quietSegments" :key="seg.id" class="quiet-item">
                <input type="time" v-model="seg.start" />
                <svg class="quiet-arrow" viewBox="0 0 16 16"><path d="M2.5 8h10M9.5 4.5L13 8l-3.5 3.5" /></svg>
                <input type="time" v-model="seg.end" />
                <!-- 24:00 只在区间终点有定义（与后端 parse_hhmm 口径一致），起点用 24:00 报错 -->
                <span v-if="seg.start === '24:00'" class="quiet-warn">24:00 只能作为结束时间</span>
                <span v-else-if="seg.start && seg.end && seg.start === seg.end" class="quiet-warn">起止相同不会生效</span>
                <button class="icon-btn danger" title="删除该时段" @click="removeQuietSegment(i)">
                  <svg viewBox="0 0 16 16"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7" /></svg>
                </button>
              </div>
              <div v-for="u in quietUnrecognized" :key="u" class="quiet-item legacy">
                <span class="hint">无法识别的时段「{{ u }}」· 按原样保留</span>
                <button class="icon-btn danger" title="删除" @click="removeUnrecognized(u)">
                  <svg viewBox="0 0 16 16"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7" /></svg>
                </button>
              </div>
            </div>
            <div class="row">
              <button class="quiet-add" @click="addQuietSegment">＋ 添加时段</button>
            </div>
            <span class="hint">免打扰期间事件照常记录，但不推送通知、也不响状态音效；跨零点直接把开始设得比结束晚（如 22:00-08:00）。</span>
          </div>
        </div>
      </div>
    </div>

    <div class="card">
      <span class="agent-name">服务信息</span>
      <div v-if="diag?.server_error" class="warn-box mb-12">
        事件服务器启动失败：{{ diag.server_error }}（端口可能被占用，实时事件与 hook 上报将不可用）
      </div>
      <div v-if="diag?.restart_required" class="warn-box mb-12">
        端口或鉴权 token 已修改，但运行中的事件服务器仍使用启动时的值——重启 agent-bark 后生效
      </div>
      <div v-if="diagError" class="hint mt-8">{{ diagError }}</div>
      <div class="mt-10 col">
        <div class="row">
          <span class="hint field-label">本地事件端口</span>
          <span class="code-chip">127.0.0.1:{{ config.server.port }}/event</span>
        </div>
        <div class="row">
          <span class="hint field-label">鉴权 token</span>
          <span class="code-chip">{{ config.server.token.slice(0, 6) }}…</span>
          <span class="hint">存储于应用配置目录下的 config.json</span>
        </div>
        <span class="hint">hook 子命令从配置文件读取端口与 token，token 不会写入各 agent 的配置</span>
        <span class="hint">端口与鉴权 token 修改需重启生效（运行中的服务器保留启动时的值）</span>
      </div>
    </div>
  </div>

  <div v-else-if="loadError" class="empty">
    {{ loadError }}
    <div class="mt-12"><button class="ghost" @click="load">重试</button></div>
  </div>
  <div v-else class="empty">加载配置中…</div>
</template>

<style scoped>
/* ---- 设置行（开关类）：.setting-row / .setting-text / .setting-label ---- */
/* 已提升到 style.css 全局：本页与「悬浮窗」页共用同一套排法 */

/* ---- 免打扰时段（设置页专属；通用工具类仍在 style.css）---- */

.quiet-list {
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 8px;
}

/* 时段条：两个时间框都是标准控件（自带描边与焦点环，几何见 style.css 的
   --control-h / --control-radius），中间箭头、右侧删除按钮，整体靠 gap 排开。
   旧版让时间框去边框融进一条浅底容器、焦点环打在整条上，两个「22:00」读起来
   像一行散字；现在每个框自己就是可点的控件，聚焦哪个一眼能看见 */
.quiet-item { display: inline-flex; align-items: center; gap: 6px; }

.quiet-item input[type="time"] {
  width: 92px;
  padding: 0 4px 0 9px;
}

/* 历史手写的无法识别值：告警色虚线小条，弱化但一眼能看出「需要处理」 */
.quiet-item.legacy {
  gap: 8px;
  padding: 3px 4px 3px 10px;
  background: color-mix(in srgb, var(--warn) 8%, transparent);
  border: 1px dashed color-mix(in srgb, var(--warn) 45%, transparent);
  border-radius: var(--control-radius);
}

/* 起止之间的箭头：与标题栏图标同一套 1px 描边画法 */
.quiet-arrow {
  width: 14px;
  height: 14px;
  flex-shrink: 0;
  color: var(--muted);
  fill: none;
  stroke: currentColor;
  stroke-width: 1.2;
  stroke-linecap: round;
  stroke-linejoin: round;
}

.quiet-warn { font-size: 12px; color: var(--warn); margin: 0 6px; white-space: nowrap; }

/* 图标按钮：裸图标 + 悬停浅底；删除类悬停变红（对齐标题栏关闭钮的做法）。
   border 必须显式清掉：`.danger` 在全局是「描边按钮」，不覆盖就会给图标按钮
   套一圈红框（旧版就是这样，一排红框比图标还显眼） */
.icon-btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 26px;
  height: 26px;
  flex-shrink: 0;
  padding: 0;
  background: transparent;
  border: none;
  border-radius: 6px;
  color: var(--muted);
}
.icon-btn svg {
  width: 14px;
  height: 14px;
  fill: none;
  stroke: currentColor;
  stroke-width: 1.2;
  stroke-linecap: round;
}
.icon-btn:hover:not(:disabled) { background: var(--hover-bg); color: var(--text); }
.icon-btn:active:not(:disabled) { background: var(--hover-bg-strong); }
.icon-btn.danger:hover:not(:disabled) {
  background: color-mix(in srgb, var(--err) 14%, transparent);
  color: var(--err);
}

/* 添加按钮：虚线框弱化展示，悬停点亮为品牌色（比实心按钮轻，和「＋」的语义一致） */
.quiet-add {
  display: inline-flex;
  align-items: center;
  height: 28px;
  padding: 0 11px;
  background: transparent;
  border: 1px dashed var(--field-border);
  border-radius: var(--control-radius);
  font-size: 12px;
  color: var(--muted);
}
.quiet-add:hover:not(:disabled) {
  background: transparent;
  border-color: var(--brand);
  color: var(--brand);
}
</style>
