<script setup lang="ts">
import { computed, onDeactivated, onMounted, onUnmounted, ref } from "vue";
import { useI18n } from "vue-i18n";
import {
  api,
  BURST_EFFECTS,
  EDGE_EFFECTS,
  monitorLabel,
  SOUND_EFFECTS,
  type BarkConfig,
  type Diagnostics,
  type MonitorInfo,
  type StateEffects,
  type StateSound,
} from "../api";
import { toast } from "../toast";

const { t } = useI18n();

const config = ref<BarkConfig | null>(null);
const monitors = ref<MonitorInfo[]>([]);
const monitorError = ref("");
const loadingMonitors = ref(false);
const loadError = ref("");
const errorMsg = ref("");
/// 音量滑杆的实时值：`:value` 单向绑定 + `change` 才落盘，拖动过程中数字不会停在旧值
const volumeDraft = ref<number | null>(null);
/// 最近一次「该响却没响」的原因（进页面时取一次；超过 5 分钟的不展示）
const lastSkip = ref<Diagnostics["last_sound_skip"]>(null);

/**
 * 全页即时落盘：页面没有「保存」按钮，任何控件改一下就保存声音与光效分区。
 *
 * 成功不弹「已保存」——开关/下拉本身的变化就是反馈；失败才 toast，并把刚改的
 * 值回滚（界面不能撒谎）。总开关、各开关、下拉、数字框全走这一条路。
 */
async function persist(): Promise<boolean> {
  if (!config.value) return false;
  try {
    await api.saveConfig({ notify: config.value.notify, glow: config.value.glow });
    return true;
  } catch (e) {
    toast(t("common.saveFailed", { error: String(e) }), "error");
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

/**
 * 音量滑杆：`input` 只更新草稿（拖动过程中数字跟手），`change`（松手）才落盘。
 * 不能直接用 v-model——commitNotify 发现「新旧相等」会直接返回、不写盘。
 */
function commitVolume(el: HTMLInputElement) {
  const v = Number(el.value);
  volumeDraft.value = v;
  void commitNotify("volume", v).then(() => (volumeDraft.value = null));
}

/** 提交声音里某个状态的一项设置：先改内存再落盘，失败回滚 */
async function commitSound(key: (typeof SOUND_ROWS)[number], patch: Partial<StateSound>) {
  const cfg = config.value;
  if (!cfg) return;
  const row = cfg.notify[key];
  const before = { effect: row.effect, custom_path: row.custom_path };
  const next = { ...before, ...patch };
  if (next.effect === before.effect && next.custom_path === before.custom_path) return;
  row.effect = next.effect;
  row.custom_path = next.custom_path;
  if (!(await persist())) {
    row.effect = before.effect;
    row.custom_path = before.custom_path;
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

async function load() {
  loadError.value = "";
  try {
    config.value = await api.getConfig();
    normalizeSoundEffects(config.value);
    normalizeGlowEffects(config.value);
  } catch (e) {
    loadError.value = t("common.loadConfigFailed", { error: String(e) });
  }
  // 诊断单独跑：拿不到不影响页面（例如后端还没就绪）
  try {
    lastSkip.value = (await api.diagnostics()).last_sound_skip;
  } catch {
    lastSkip.value = null;
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
      monitorError.value = t("notify.noMonitors");
    }
    normalizeMonitorTarget();
  } catch (e) {
    monitors.value = [];
    monitorError.value = t("notify.detectMonitorsFailed", { error: String(e) });
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
    errorMsg.value = t("notify.monitorSwitched", { old: raw, new: monitorLabel(primary) });
  }
}

/** 音效目录更新后，配置里可能残留已下架的 id：下拉会显示空白，这里归回「不响」 */
function normalizeSoundEffects(cfg: BarkConfig) {
  // "custom" 不在内置目录里但合法；已下线的 chime 迁移到同源素材 confirm
  // （后端 play_sound 里也有同一层别名，两侧口径一致，不能让 UI 说「不响」而后端还在响）；
  // 其余未知 id 回落「不响」
  const known = new Set([...SOUND_EFFECTS.map((s) => s.id), "custom"]);
  for (const key of SOUND_ROWS) {
    const cur = cfg.notify[key].effect;
    if (cur === "chime") cfg.notify[key].effect = "confirm";
    else if (!known.has(cur)) cfg.notify[key].effect = "";
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
  cfg.glow.edge_effects = normalizeStateEffects(cfg.glow.edge_effects, edgeKnown, "comet");
  cfg.glow.burst_effects = normalizeStateEffects(cfg.glow.burst_effects, burstKnown, "scan");
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
    errorMsg.value = t("notify.previewFailed", { error: String(e) });
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
    errorMsg.value = t("notify.stopFailed", { error: String(e) });
  } finally {
    stopping.value = false;
  }
}

// ---- 声音 ----

/** 「声音」与「屏幕光效」两区的四行：与流光四色状态同语义（思考 / 等待 / 完成 / 失败） */
const SOUND_ROWS = ["thinking", "waiting", "completed", "failed"] as const;

/** 状态名指向词表 `notify.state.*`：常量只建一次，存 key 不存译文，切语言跟着更新 */
const SOUND_ROW_LABEL_KEYS: Record<(typeof SOUND_ROWS)[number], string> = {
  thinking: "notify.state.thinking",
  waiting: "notify.state.waiting",
  completed: "notify.state.completed",
  failed: "notify.state.failed",
};

/// 进页面时取到的那条记录，只有 5 分钟内的才展示
const recentSkip = computed(() => {
  const s = lastSkip.value;
  return s && Date.now() - s.at < 5 * 60_000 ? s : null;
});

const testing = ref("");
const soundTestError = ref("");

/** 自定义音效：原生对话框选文件后写进该状态的 custom_path（即时落盘）；取消不动配置 */
async function pickSound(key: (typeof SOUND_ROWS)[number]) {
  soundTestError.value = "";
  try {
    const path = await api.pickSoundFile();
    if (path) await commitSound(key, { custom_path: path });
  } catch (e) {
    // 对话框被系统拒绝（窗口校验失败 / 平台不支持）时给一句可见的话，
    // 不能让 rejection 只落到 console、用户以为按钮坏了
    soundTestError.value = t("notify.pickFailed", { error: String(e) });
  }
}

/** 按钮上只显示文件名，全路径收进悬停提示 */
function fileName(path: string): string {
  return path.split(/[\/]/).pop() ?? "";
}

async function testSound(key: (typeof SOUND_ROWS)[number]) {
  const cfg = config.value;
  if (!cfg) return;
  testing.value = key;
  soundTestError.value = "";
  const started = Date.now();
  try {
    await api.testSound(cfg.notify[key].effect, cfg.notify[key].custom_path || null);
  } catch (e) {
    soundTestError.value = t("notify.playFailed", { error: String(e) });
  } finally {
    // 命令把音频交给后台就返回了，按钮不能只闪一帧：至少留 600ms 让「播放中…」看得见。
    // 顺带挡住连点——两次测试叠在一起就听不出是哪个音效了。
    const rest = 600 - (Date.now() - started);
    if (rest > 0) await new Promise((r) => setTimeout(r, rest));
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
    <div class="page-head">
      <div class="page-head-left"><h1 class="page-title">{{ $t("notify.title") }}</h1></div>
    </div>
    <div class="stack">
      <div v-if="errorMsg" class="warn-box">{{ errorMsg }}</div>

      <!-- 屏幕光效：整组一张卡。页面没有「保存」按钮，所有修改即时落盘生效；
           总开关放在分组标题右侧 -->
      <div class="card">
        <div class="card-head">
          <span class="card-title">{{ $t("notify.glowTitle") }}</span>
          <div class="row">
            <span class="hint">{{ config.glow.enabled ? $t("notify.glowRunning") : $t("notify.glowStopped") }}</span>
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

        <div class="form-rows">
          <!-- 生效显示器：小节标题右边直接跟下拉框和重新检测，一行排完；
               标题占固定 96px 标签列，与下方各行控件同列对齐 -->
          <div class="row">
            <span class="section-label field-label">{{ $t("notify.monitors") }}</span>
            <select
              :value="config.glow.monitors"
              class="grow select-wide"
              @change="commitGlow('monitors', ($event.target as HTMLSelectElement).value)"
            >
              <button><selectedcontent></selectedcontent></button>
              <option value="all">{{ $t("notify.allMonitors") }}{{ monitors.length > 1 ? $t("notify.monitorCount", { n: monitors.length }) : "" }}</option>
              <option v-for="m in monitors" :key="m.index" :value="String(m.index)">
                {{ monitorLabel(m) }}{{ m.is_primary ? $t("notify.primaryMonitor") : "" }}
              </option>
            </select>
            <button class="ghost" :disabled="loadingMonitors" @click="loadMonitors">
              {{ loadingMonitors ? $t("notify.detecting") : $t("notify.redetect") }}
            </button>
          </div>
          <div v-if="monitorError" class="warn-box">{{ monitorError }}</div>

          <!-- 边缘光效 / 全屏特效：内嵌子面板双列（macOS / Win11 设置的分组套路）。
               四态竖排（与「声音」区同款），每行选**该状态**的类型，「无」= 该状态
               不亮边缘 / 不放全屏；列开关关着时整列下拉禁用 -->
          <div class="glow-grid">
            <div class="glow-panel">
              <div class="card-head">
                <span class="card-title">{{ $t("notify.edgeEffects") }}</span>
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
                  <span class="hint field-label">{{ $t(SOUND_ROW_LABEL_KEYS[key]) }}</span>
                  <select
                    class="grow"
                    :value="config.glow.edge_effects[key]"
                    :disabled="!config.glow.edge"
                    @change="commitStateEffect('edge_effects', key, ($event.target as HTMLSelectElement).value)"
                  >
                    <button><selectedcontent></selectedcontent></button>
                    <option v-for="e in EDGE_EFFECTS" :key="e.id" :value="e.id">{{ $t(e.labelKey) }}</option>
                  </select>
                </div>
                <!-- 位置：按钮式单选（radio-button 分段控件）。「顶部」（默认，只亮
                     顶部一条横线，覆盖窗缩成顶部条带，避免被系统/游戏软件判定为全屏）
                     /「四周」（旧行为）。原生 radio 同 name 互斥 + 方向键切换，外观做成
                     并排按钮；边缘开关关着时整组禁用，与上方四行下拉同口径。
                     旧值（如历史配置里的 "bottom"）后端 edge_sides() 归一成顶部，
                     这里按「不是 all 即顶部」回显，改选任一项都会写回合法值 -->
                <div class="row">
                  <span class="hint field-label">{{ $t("notify.position") }}</span>
                  <div class="radio-group" role="radiogroup" :aria-label="$t('notify.edgePositionLabel')">
                    <label class="radio-btn">
                      <input
                        type="radio"
                        name="glow-position"
                        value="top"
                        :checked="config.glow.edge_position !== 'all'"
                        :disabled="!config.glow.edge"
                        @change="commitGlow('edge_position', 'top')"
                      />
                      <span>{{ $t("notify.positionTop") }}</span>
                    </label>
                    <label class="radio-btn">
                      <input
                        type="radio"
                        name="glow-position"
                        value="all"
                        :checked="config.glow.edge_position === 'all'"
                        :disabled="!config.glow.edge"
                        @change="commitGlow('edge_position', 'all')"
                      />
                      <span>{{ $t("notify.positionAll") }}</span>
                    </label>
                  </div>
                </div>
              </div>
            </div>

            <div class="glow-panel">
              <div class="card-head">
                <span class="card-title">{{ $t("notify.fullscreenEffects") }}</span>
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
                  <span class="hint field-label">{{ $t(SOUND_ROW_LABEL_KEYS[key]) }}</span>
                  <select
                    class="grow"
                    :value="config.glow.burst_effects[key]"
                    :disabled="!config.glow.fullscreen"
                    @change="commitStateEffect('burst_effects', key, ($event.target as HTMLSelectElement).value)"
                  >
                    <button><selectedcontent></selectedcontent></button>
                    <option v-for="e in BURST_EFFECTS" :key="e.id" :value="e.id">{{ $t(e.labelKey) }}</option>
                  </select>
                </div>
              </div>
            </div>
          </div>

          <!-- 预览：按钮边缘用对应状态色；「预览」与「熄灭」两组互斥 disable——
               两个操作都作用在同一套光效上，并发会互相打架 -->
          <div class="preview-block">
            <span class="section-label">{{ $t("notify.preview") }}</span>
            <div class="row">
              <button class="ghost state-thinking" :disabled="!!previewing || stopping" @click="previewGlow('running')">{{ $t("notify.state.thinking") }}</button>
              <button class="ghost state-waiting" :disabled="!!previewing || stopping" @click="previewGlow('waiting')">{{ $t("notify.state.waiting") }}</button>
              <button class="ghost state-completed" :disabled="!!previewing || stopping" @click="previewGlow('completed')">{{ $t("notify.state.completed") }}</button>
              <button class="ghost state-failed" :disabled="!!previewing || stopping" @click="previewGlow('failed')">{{ $t("notify.state.failed") }}</button>
              <button class="ghost" :disabled="stopping || !!previewing" @click="stopNow">{{ $t("notify.turnOff") }}</button>
            </div>
          </div>
        </div>
      </div>

      <!-- 声音：总开关放分组标题右侧（与屏幕光效同构），关了全部不响；
           每种状态一个音效下拉 + 播放次数 + 测试按钮；修改即时生效 -->
      <div class="card">
        <div class="card-head">
          <span class="card-title">{{ $t("notify.soundTitle") }}</span>
          <div class="row">
            <span class="hint">{{ config.notify.enabled ? $t("notify.soundEnabled") : $t("notify.soundDisabled") }}</span>
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
        <div class="form-rows">
          <!-- 音量：与系统音量相乘，独立于系统音量才能「听清提示音但不吵」 -->
          <div class="row">
            <span class="hint field-label">{{ $t("notify.volume") }}</span>
            <input
              type="range"
              min="0"
              max="100"
              step="5"
              class="grow"
              :value="volumeDraft ?? config.notify.volume"
              :disabled="!config.notify.enabled"
              @input="volumeDraft = Number(($event.target as HTMLInputElement).value)"
              @change="commitVolume($event.target as HTMLInputElement)"
            />
            <span class="hint volume-val">{{ volumeDraft ?? config.notify.volume }}%</span>
          </div>

          <div v-for="key in SOUND_ROWS" :key="key" class="row">
            <span class="hint field-label">{{ $t(SOUND_ROW_LABEL_KEYS[key]) }}</span>
            <!-- 定宽中区：不选自定义时下拉铺满；选自定义时下拉缩短、
                 让位给文件按钮，总宽不变 → 测试按钮位置不动 -->
            <div class="sound-mid">
              <select
                :value="config.notify[key].effect"
                :class="{ narrow: config.notify[key].effect === 'custom' }"
                :disabled="!config.notify.enabled"
                @change="commitSound(key, { effect: ($event.target as HTMLSelectElement).value })"
              >
                <button><selectedcontent></selectedcontent></button>
                <option value="">{{ $t("notify.silent") }}</option>
                <option v-for="s in SOUND_EFFECTS" :key="s.id" :value="s.id">{{ $t(s.labelKey) }}</option>
                <option value="custom">{{ $t("notify.custom") }}</option>
              </select>
              <button
                v-if="config.notify[key].effect === 'custom'"
                class="ghost sound-file"
                v-tooltip="config.notify[key].custom_path || $t('notify.chooseFile')"
                :disabled="!config.notify.enabled"
                @click="pickSound(key)"
              >
                <span>{{ fileName(config.notify[key].custom_path) || $t("notify.chooseFile") }}</span>
              </button>
            </div>
            <button
              class="ghost"
              :disabled="!config.notify.enabled || !config.notify[key].effect || (config.notify[key].effect === 'custom' && !config.notify[key].custom_path) || testing === key"
              @click="testSound(key)"
            >
              {{ testing === key ? $t("notify.playing") : $t("common.test") }}
            </button>
          </div>
          <!-- 连续提示：多会话几乎同时终态时，窗口内只响第一声，
               避免叠成一片「糊」；想听出「完成了几次」可以选「每次都响」 -->
          <div class="row">
            <span class="hint field-label">{{ $t("notify.throttle") }}</span>
            <select
              class="throttle-select"
              :value="String(config.notify.throttle_ms)"
              :disabled="!config.notify.enabled"
              @change="commitNotify('throttle_ms', Number(($event.target as HTMLSelectElement).value))"
            >
              <button><selectedcontent></selectedcontent></button>
              <option value="0">{{ $t("notify.throttleOff") }}</option>
              <option value="250">{{ $t("notify.throttle250") }}</option>
              <option value="500">{{ $t("notify.throttle500") }}</option>
              <option
                v-if="![0, 250, 500].includes(config.notify.throttle_ms)"
                :value="String(config.notify.throttle_ms)"
              >
                {{ config.notify.throttle_ms }} ms
              </option>
            </select>
          </div>
          <div v-if="soundTestError" class="warn-box">{{ soundTestError }}</div>
          <!-- 「上一次为什么没响」：判定层给出的原因，只留 5 分钟内的，
               免得一条旧记录（比如子代理事件）长期挂在页面上 -->
          <p v-if="recentSkip" class="hint skip-note">
            {{ $t("notify.lastSkip", { reason: $t(`notify.skip.${recentSkip.reason}`) }) }}
          </p>
        </div>
      </div>
    </div>
  </div>

  <div v-else-if="loadError" class="empty">
    {{ loadError }}
    <div class="mt-12"><button class="ghost" @click="load">{{ $t("common.retry") }}</button></div>
  </div>
  <div v-else class="empty">{{ $t("common.loadingConfig") }}</div>
</template>

<style scoped>
/* ---- 「屏幕光效」双子面板（macOS / Win11 设置的 inset grouped panel 套路）----
   每组一块比卡片亮/暗一档的圆角面板，组与组物理分开，比「标题 + 发丝线」的
   分组（VS Code 设置的做法）区分更强；间距交给外层 .form-rows */
.glow-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
.glow-panel {
  background: var(--panel-2);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 12px 14px 14px;
}
/* 浅色下面板底用极浅灰而不是 panel-2（#e9e9ee 一摞灰太深、压过卡片），
   对齐 macOS 浅色分组的 #f5f5f7 观感 */
:root[data-theme="light"] .glow-panel { background: #f5f5f8; }
/* 面板内卡头比整页卡头略紧（面板自身内边距已小一档） */
.glow-panel .card-head { margin-bottom: 10px; }
.glow-rows { display: flex; flex-direction: column; gap: 8px; }
/* 状态名 4 个字：标签列收窄到 52px（全局 field-label 的 96px 是给整页对齐用的） */
.glow-rows .field-label { width: 52px; }
/* 面板内的下拉用卡片底色：面板已是 panel-2，控件再同底就糊成一块（Fluent / macOS 通用做法） */
.glow-panel select { background-color: var(--panel); }
:root[data-theme="light"] .glow-panel select { background-color: #fff; }
/* 预览小节：标题与按钮行 8px 间距 */
.preview-block { display: flex; flex-direction: column; gap: 8px; }
/* 声音行定宽中区（302px = 旧 220 下拉 + 10 间距 + 72 播放次数）：
   自定义态下拉缩短、文件按钮补足剩余，总宽恒定 → 测试按钮 x 与旧版一致、两态不跳 */
.sound-mid { display: flex; align-items: center; gap: 8px; width: 302px; flex-shrink: 0; }
.sound-mid select { width: 100%; min-width: 0; }
.sound-mid select.narrow { width: 110px; flex-shrink: 0; }
.sound-file { flex: 1; min-width: 0; padding: 0 10px; }
.sound-file span { flex: 1; min-width: 0; overflow: hidden; white-space: nowrap; text-overflow: ellipsis; text-align: left; }
/* 音量百分比：定宽 + 等宽数字，拖动滑杆时数字不左右跳 */
.volume-val { width: 40px; flex-shrink: 0; text-align: right; font-variant-numeric: tabular-nums; }
/* 「上一次为什么没响」：小字、贴左，不与表单行抢注意力 */
.skip-note { margin: 2px 0 0; }
/* 「连续提示」下拉：不铺满整行（与状态行的中区同起点即可） */
.throttle-select { width: 180px; flex-shrink: 0; }
</style>
