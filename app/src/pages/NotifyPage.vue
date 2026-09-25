<script setup lang="ts">
import { onDeactivated, onMounted, onUnmounted, ref } from "vue";
import {
  api,
  BURST_EFFECTS,
  EDGE_EFFECTS,
  monitorLabel,
  SOUND_EFFECTS,
  type BarkConfig,
  type MonitorInfo,
  type StateEffects,
  type StateSound,
} from "../api";
import { toast } from "../toast";

const config = ref<BarkConfig | null>(null);
const monitors = ref<MonitorInfo[]>([]);
const monitorError = ref("");
const loadingMonitors = ref(false);
const loadError = ref("");
const errorMsg = ref("");

/**
 * 全页即时落盘：页面没有「保存」按钮，任何控件改一下就保存整份配置（后端口径）。
 *
 * 成功不弹「已保存」——开关/下拉本身的变化就是反馈；失败才 toast，并把刚改的
 * 值回滚（界面不能撒谎）。总开关、各开关、下拉、数字框全走这一条路。
 */
async function persist(): Promise<boolean> {
  if (!config.value) return false;
  sanitizeSoundPlays(config.value);
  try {
    await api.saveConfig(config.value);
    return true;
  } catch (e) {
    toast(`保存失败：${e}`, "error");
    return false;
  }
}

/** 提交 glow 段的一个字段：先改内存再落盘，失败回滚 */
async function commitGlow<K extends keyof BarkConfig["glow"]>(key: K, value: BarkConfig["glow"][K]) {
  const cfg = config.value;
  if (!cfg || cfg.glow[key] === value) return;
  const before = cfg.glow[key];
  cfg.glow[key] = value;
  if (!(await persist())) cfg.glow[key] = before;
}

/** 提交某状态的效果类型（边缘 / 全屏两列共用）：先改内存再落盘，失败回滚 */
async function commitStateEffect(
  kind: "edge_effects" | "burst_effects",
  state: (typeof SOUND_ROWS)[number],
  value: string,
) {
  const cfg = config.value;
  if (!cfg) return;
  const row = cfg.glow[kind];
  const before = row[state];
  if (before === value) return;
  row[state] = value;
  if (!(await persist())) row[state] = before;
}

/** 提交声音里某个状态的一项设置：先改内存再落盘，失败回滚 */
async function commitSound(key: (typeof SOUND_ROWS)[number], patch: Partial<StateSound>) {
  const cfg = config.value;
  if (!cfg) return;
  const row = cfg.notify[key];
  const before = { effect: row.effect, plays: row.plays };
  const next = { ...before, ...patch };
  if (next.effect === before.effect && next.plays === before.plays) return;
  row.effect = next.effect;
  row.plays = next.plays;
  if (!(await persist())) {
    row.effect = before.effect;
    row.plays = before.plays;
  }
}

/** 提交 notify 段的一个字段（目前只有总开关）：先改内存再落盘，失败回滚 */
async function commitNotify<K extends keyof BarkConfig["notify"]>(key: K, value: BarkConfig["notify"][K]) {
  const cfg = config.value;
  if (!cfg || cfg.notify[key] === value) return;
  const before = cfg.notify[key];
  cfg.notify[key] = value;
  if (!(await persist())) cfg.notify[key] = before;
}

/**
 * 播放次数：解析并夹回 1~10 再提交（数字框可能被清空成 NaN 或手填越界值，
 * 直接提交会让后端 serde 拒收整份配置）；输入框当场归位到夹好的值。
 */
function commitPlays(key: (typeof SOUND_ROWS)[number], el: HTMLInputElement) {
  const p = Math.round(Number(el.value));
  const clamped = Number.isFinite(p) ? Math.min(10, Math.max(1, p)) : 1;
  el.value = String(clamped);
  commitSound(key, { plays: clamped });
}

async function load() {
  loadError.value = "";
  try {
    config.value = await api.getConfig();
    normalizeSoundEffects(config.value);
    normalizeGlowEffects(config.value);
  } catch (e) {
    loadError.value = `加载配置失败：${e}`;
  }
  // 显示器枚举单独跑：它失败不该让整个页面变成错误态，只在「生效显示器」那一行提示
  await loadMonitors();
}

async function loadMonitors() {
  loadingMonitors.value = true;
  monitorError.value = "";
  try {
    monitors.value = await api.listMonitors();
    if (!monitors.value.length) {
      monitorError.value = "未检测到任何显示器，流光将回退到主显示器";
    }
    normalizeMonitorTarget();
  } catch (e) {
    monitors.value = [];
    monitorError.value = `检测显示器失败：${e}`;
  } finally {
    loadingMonitors.value = false;
  }
}

/**
 * 把配置里的生效显示器对齐到下拉里真实存在的选项。
 *
 * `monitors` 存的是显示器下标，但历史配置里可能是旧版的 "primary"，也可能因为
 * 拔掉某块屏而越界——不归一化的话下拉会显示成空白（用户以为设置丢了）。
 * 只改内存值，不主动落盘；之后任何一次修改提交都会把归一结果带上。
 */
function normalizeMonitorTarget() {
  const cfg = config.value;
  if (!cfg || !monitors.value.length) return;
  const raw = cfg.glow.monitors.trim();
  if (raw.toLowerCase() === "all") {
    cfg.glow.monitors = "all";
    return;
  }
  const idx = Number(raw);
  if (raw !== "" && Number.isInteger(idx) && idx >= 0 && idx < monitors.value.length) {
    cfg.glow.monitors = String(idx);
    return;
  }
  const primary = monitors.value.find((m) => m.is_primary) ?? monitors.value[0];
  cfg.glow.monitors = String(primary.index);
  if (raw !== "primary" && raw !== "") {
    errorMsg.value = `原先生效的显示器（${raw}）已不存在，已自动改为「${monitorLabel(primary)}」`;
  }
}

/**
 * 保存前把播放次数夹回 1~10：数字输入框可能被清空（NaN）或手填越界值，
 * 直接提交会让后端 serde 拒收整份配置。
 */
function sanitizeSoundPlays(cfg: BarkConfig) {
  for (const key of SOUND_ROWS) {
    const s = cfg.notify[key];
    const p = Math.round(Number(s.plays));
    s.plays = Number.isFinite(p) ? Math.min(10, Math.max(1, p)) : 1;
  }
}

/** 音效目录更新后，配置里可能残留已下架的 id：下拉会显示空白，这里归回「不响」 */
function normalizeSoundEffects(cfg: BarkConfig) {
  const known = new Set(SOUND_EFFECTS.map((s) => s.id));
  for (const key of SOUND_ROWS) {
    if (!known.has(cfg.notify[key].effect)) cfg.notify[key].effect = "";
  }
}

/**
 * 效果类型归一（与后端 GlowConfig::sanitize 同口径）：目录更新、旧配置迁移或
 * 手改后可能残留空 / 未知 id——空与未知回落家族默认（迁移源），「无」保留。
 * 整段缺失（老配置没有每状态段）先补出四态再归一。
 */
function normalizeGlowEffects(cfg: BarkConfig) {
  const edgeKnown = new Set(EDGE_EFFECTS.map((e) => e.id));
  const burstKnown = new Set(BURST_EFFECTS.map((e) => e.id));
  cfg.glow.edge_effects = normalizeStateEffects(cfg.glow.edge_effects, edgeKnown, "breathing");
  cfg.glow.burst_effects = normalizeStateEffects(cfg.glow.burst_effects, burstKnown, "fog");
}

function normalizeStateEffects(map: StateEffects | undefined, known: Set<string>, fallback: string): StateEffects {
  const m = map ?? { thinking: "", waiting: "", completed: "", failed: "" };
  for (const key of SOUND_ROWS) {
    if (!known.has(m[key])) m[key] = fallback;
  }
  return m;
}

// 预览：无视总开关临时点亮，几秒后由后端自动恢复
const previewing = ref("");

async function previewGlow(state: string) {
  previewing.value = state;
  errorMsg.value = "";
  try {
    await api.glowPreview(state);
  } catch (e) {
    errorMsg.value = `预览失败：${e}`;
  } finally {
    previewing.value = "";
  }
}

// 熄灭：立刻结束预览/收起光效，不必等预览的恢复定时器
const stopping = ref(false);

async function stopNow() {
  stopping.value = true;
  errorMsg.value = "";
  try {
    await api.glowOff();
  } catch (e) {
    errorMsg.value = `熄灭失败：${e}`;
  } finally {
    stopping.value = false;
  }
}

// ---- 声音 ----

/** 「声音」与「屏幕光效」两区的四行：与流光四色状态同语义（思考 / 等待 / 完成 / 失败） */
const SOUND_ROWS = ["thinking", "waiting", "completed", "failed"] as const;

const SOUND_ROW_LABELS: Record<(typeof SOUND_ROWS)[number], string> = {
  thinking: "思考",
  waiting: "等待",
  completed: "完成",
  failed: "失败",
};

const testing = ref("");
const soundTestError = ref("");

async function testSound(key: (typeof SOUND_ROWS)[number]) {
  const cfg = config.value;
  if (!cfg) return;
  testing.value = key;
  soundTestError.value = "";
  try {
    await api.testSound(cfg.notify[key].effect, cfg.notify[key].plays);
  } catch (e) {
    soundTestError.value = `播放失败：${e}`;
  } finally {
    testing.value = "";
  }
}

onMounted(load);

// 离开页面时兜底熄灭：预览残留不能完全押注后端的定时恢复——正在预览时切走/卸载，
// 直接 glowOff 收干净（下一次真实事件会重新点亮光效）。页面被 KeepAlive 保活
// （App.vue），切走是 onDeactivated、真正销毁才是 onUnmounted，两条路都要接
function glowOffSafely() {
  void api.glowOff().catch((e) => console.warn("熄灭光效失败：", e));
}

onDeactivated(glowOffSafely);
onUnmounted(glowOffSafely);
</script>

<template>
  <div v-if="config">
    <h1 class="page-title">通知</h1>
    <div v-if="errorMsg" class="warn-box mb-14">{{ errorMsg }}</div>

    <!-- 屏幕光效：整组一张卡。页面没有「保存」按钮，所有修改即时落盘生效；
         总开关放在分组标题右侧 -->
    <div class="card">
      <div class="between">
        <span class="agent-name">屏幕光效</span>
        <div class="row">
          <span class="hint">{{ config.glow.enabled ? "运行中" : "已停止" }}</span>
          <label class="switch">
            <input
              type="checkbox"
              :checked="config.glow.enabled"
              @change="commitGlow('enabled', ($event.target as HTMLInputElement).checked)"
            />
            <span class="track"></span>
          </label>
        </div>
      </div>

      <!-- 生效显示器：小节标题右边直接跟下拉框和重新检测，一行排完；
           标题占固定 96px 标签列，与下方各行控件同列对齐 -->
      <div class="row mt-12">
        <span class="section-label field-label">生效显示器</span>
        <select
          :value="config.glow.monitors"
          class="grow select-wide"
          @change="commitGlow('monitors', ($event.target as HTMLSelectElement).value)"
        >
          <button><selectedcontent></selectedcontent></button>
          <option value="all">全部显示器{{ monitors.length > 1 ? `（${monitors.length} 块）` : "" }}</option>
          <option v-for="m in monitors" :key="m.index" :value="String(m.index)">
            {{ monitorLabel(m) }}{{ m.is_primary ? "（主显示器）" : "" }}
          </option>
        </select>
        <button class="ghost" :disabled="loadingMonitors" @click="loadMonitors">
          {{ loadingMonitors ? "检测中…" : "重新检测" }}
        </button>
      </div>
      <div v-if="monitorError" class="warn-box mt-8">{{ monitorError }}</div>

      <!-- 边缘光效 / 全屏特效：内嵌子面板双列（macOS / Win11 设置的分组套路）。
           四态竖排（与「声音」区同款），每行选**该状态**的类型，「无」= 该状态
           不亮边缘 / 不放全屏；列开关关着时整列下拉禁用 -->
      <div class="glow-grid">
        <div class="glow-panel">
          <div class="between">
            <span class="panel-title">边缘光效</span>
            <label class="switch">
              <input
                type="checkbox"
                :checked="config.glow.edge"
                @change="commitGlow('edge', ($event.target as HTMLInputElement).checked)"
              />
              <span class="track"></span>
            </label>
          </div>
          <div class="glow-rows">
            <div v-for="key in SOUND_ROWS" :key="key" class="row">
              <span class="hint field-label">{{ SOUND_ROW_LABELS[key] }}</span>
              <select
                class="grow"
                :value="config.glow.edge_effects[key]"
                :disabled="!config.glow.edge"
                @change="commitStateEffect('edge_effects', key, ($event.target as HTMLSelectElement).value)"
              >
                <button><selectedcontent></selectedcontent></button>
                <option v-for="e in EDGE_EFFECTS" :key="e.id" :value="e.id">{{ e.label }}</option>
              </select>
            </div>
          </div>
        </div>

        <div class="glow-panel">
          <div class="between">
            <span class="panel-title">全屏特效</span>
            <label class="switch">
              <input
                type="checkbox"
                :checked="config.glow.fullscreen"
                @change="commitGlow('fullscreen', ($event.target as HTMLInputElement).checked)"
              />
              <span class="track"></span>
            </label>
          </div>
          <div class="glow-rows">
            <div v-for="key in SOUND_ROWS" :key="key" class="row">
              <span class="hint field-label">{{ SOUND_ROW_LABELS[key] }}</span>
              <select
                class="grow"
                :value="config.glow.burst_effects[key]"
                :disabled="!config.glow.fullscreen"
                @change="commitStateEffect('burst_effects', key, ($event.target as HTMLSelectElement).value)"
              >
                <button><selectedcontent></selectedcontent></button>
                <option v-for="e in BURST_EFFECTS" :key="e.id" :value="e.id">{{ e.label }}</option>
              </select>
            </div>
          </div>
        </div>
      </div>

      <!-- 预览：按钮边缘用对应状态色；「预览」与「熄灭」两组互斥 disable——
           两个操作都作用在同一套光效上，并发会互相打架 -->
      <div class="mt-16">
        <span class="section-label">预览</span>
        <div class="row mt-8">
          <button class="ghost state-thinking" :disabled="!!previewing || stopping" @click="previewGlow('running')">思考</button>
          <button class="ghost state-waiting" :disabled="!!previewing || stopping" @click="previewGlow('waiting')">等待</button>
          <button class="ghost state-completed" :disabled="!!previewing || stopping" @click="previewGlow('completed')">完成</button>
          <button class="ghost state-failed" :disabled="!!previewing || stopping" @click="previewGlow('failed')">失败</button>
          <button class="ghost" :disabled="stopping || !!previewing" @click="stopNow">熄灭</button>
        </div>
      </div>
    </div>

    <!-- 声音：总开关放分组标题右侧（与屏幕光效同构），关了全部不响；
         每种状态一个音效下拉 + 播放次数 + 测试按钮；修改即时生效 -->
    <div class="card">
      <div class="between">
        <span class="agent-name">声音</span>
        <div class="row">
          <span class="hint">{{ config.notify.enabled ? "已启用" : "已关闭" }}</span>
          <label class="switch">
            <input
              type="checkbox"
              :checked="config.notify.enabled"
              @change="commitNotify('enabled', ($event.target as HTMLInputElement).checked)"
            />
            <span class="track"></span>
          </label>
        </div>
      </div>
      <div class="mt-12 col">
        <div v-for="key in SOUND_ROWS" :key="key" class="row">
          <span class="hint field-label">{{ SOUND_ROW_LABELS[key] }}</span>
          <select
            :value="config.notify[key].effect"
            class="select-wide"
            :disabled="!config.notify.enabled"
            @change="commitSound(key, { effect: ($event.target as HTMLSelectElement).value })"
          >
            <button><selectedcontent></selectedcontent></button>
            <option value="">不响</option>
            <option v-for="s in SOUND_EFFECTS" :key="s.id" :value="s.id">{{ s.label }}</option>
          </select>
          <div class="input-group input-tiny">
            <input
              type="number"
              min="1"
              max="10"
              :value="config.notify[key].plays"
              :disabled="!config.notify.enabled"
              @change="commitPlays(key, $event.target as HTMLInputElement)"
            />
            <span class="input-unit">次</span>
          </div>
          <button
            class="ghost"
            :disabled="!config.notify.enabled || !config.notify[key].effect || testing === key"
            @click="testSound(key)"
          >
            {{ testing === key ? "播放中…" : "测试" }}
          </button>
        </div>
        <div v-if="soundTestError" class="warn-box">{{ soundTestError }}</div>
        <span class="hint">默认全部不响；总开关关闭、托盘「静音」与免打扰时段内同样不响。播放次数范围 1~10。</span>
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
/* ---- 「屏幕光效」双子面板（macOS / Win11 设置的 inset grouped panel 套路）----
   每组一块比卡片亮/暗一档的圆角面板，组与组物理分开，比「标题 + 发丝线」的
   分组（VS Code 设置的做法）区分更强 */
.glow-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; margin-top: 16px; }
.glow-panel {
  background: var(--panel-2);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 12px 14px 14px;
}
/* 浅色下面板底用极浅灰而不是 panel-2（#e9e9ee 一摞灰太深、压过卡片），
   对齐 macOS 浅色分组的 #f5f5f7 观感 */
:root[data-theme="light"] .glow-panel { background: #f5f5f8; }
.panel-title { font-size: 13px; font-weight: 600; }
.glow-rows { margin-top: 10px; display: flex; flex-direction: column; gap: 8px; }
/* 状态名 4 个字：标签列收窄到 52px（全局 field-label 的 96px 是给整页对齐用的） */
.glow-rows .field-label { width: 52px; }
/* 面板内的下拉用卡片底色：面板已是 panel-2，控件再同底就糊成一块（Fluent / macOS 通用做法） */
.glow-panel select { background-color: var(--panel); }
:root[data-theme="light"] .glow-panel select { background-color: #fff; }
</style>
