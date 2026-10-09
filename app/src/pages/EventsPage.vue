<script setup lang="ts">
import { computed, onActivated, onDeactivated, onMounted, onUnmounted, ref } from "vue";
import {
  api,
  store,
  agentName,
  agentIcon,
  eventBadgeClass,
  EVENT_LABELS,
  formatTime,
  formatDuration,
  isSubagentSession,
  parentTail,
  PHASE_LABELS,
  phaseBadgeClass,
  placeholderName,
  sessionRows,
  subagentRowClass,
  type SessionStatus,
} from "../api";

const historyLoaded = ref(false);
const historyError = ref("");
const clearError = ref("");
const clearing = ref(false);
const filter = ref<string>("");

const filteredEvents = computed(() =>
  filter.value ? store.events.filter((e) => e.type === filter.value) : store.events
);

/**
 * 运行中会话：后端快照是 started_at 倒序的平铺列表，直接渲染会让子代理与父会话交错
 * （一个 fan-out 出去十几次派生会话，父行会被挤到列表深处）。sessionRows 排成
 * 「父行 + 它的子行紧跟其后」，与悬浮窗共用同一次排序、同一套记号。
 */
const runningRows = computed(() => sessionRows(store.sessions));

/**
 * 「显示 ID」开关：在每行下面显示 `agent|session_id` 与它声明的父会话 id。
 *
 * 为什么值得留一个开关（不只是排障用的临时物）：父子归属出问题时，界面上看到的是
 * 「子代理挂在某个会话下面」，而**看不见的是 daemon 实际下发的 parent 到底是哪个 id**——
 * 缺了它就只能猜（我为此猜错了两轮）。开着它，谁是谁的子会话、父 id 有没有对上，
 * 一眼就能核对，不用重编应用、也不用翻日志。
 */
const showIds = ref(false);

// 清空序号：一次历史拉取若跨越了清空，拿回来的就是「清空前」的快照，必须作废
let clearSeq = 0;

// 历史事件补齐：不能只在 store 为空时灌入——页面停留期间实时事件可能先到达，
// 那样历史就永远补不上了。按 id 去重后合并。
async function loadHistory() {
  historyError.value = "";
  // 最多两轮：一轮正常拉取；若这次拉取**跨越了清空**（我们等响应时用户点了清空），
  // 拿回来的就是「清空前」的快照（后端此刻已不含被清条目），必须作废重拉一轮。
  // 旧实现是直接递归调用自己，连点清空会把递归层层叠上去；改成有上限的循环后，
  // 重试也用尽时**不合并**那份过期快照——合并回去等于让刚被清掉的幽灵事件复活
  // （缺的那截历史会在下次挂载或下条实时事件时补上，比复活已删条目轻）。
  for (let attempt = 0; attempt < 2; attempt++) {
    const seq = clearSeq;
    try {
      const history = await api.listEvents(200);
      if (seq !== clearSeq) continue; // 跨越了清空：作废这份旧快照，重拉一轮
      const seen = new Set(store.events.map((e) => e.id));
      const missing = history.filter((e) => !seen.has(e.id));
      if (missing.length) {
        store.events.push(...missing);
        store.events.sort((a, b) => b.timestamp - a.timestamp);
        if (store.events.length > 500) store.events.splice(500);
      }
    } catch (e) {
      // 历史加载失败只作为提示展示，已到达的实时事件继续正常渲染
      historyError.value = `加载历史事件失败：${e}`;
    }
    // 合并成功或失败都收工；只有上面那条 continue 会进到第二轮
    break;
  }
  historyLoaded.value = true;
}

// 实时状态初始快照（后续靠 bark://sessions 推送）；失败静默——推送通道可能仍在。
// 15s 轮询拉取与 bark://sessions 推送存在竞态：慢的旧拉取不得把推送的新快照
// 覆盖回旧数据——拉取落地前核对**拉取代际 + 推送代际**（store.sessionsSeq），
// 期间来了更新的拉取或新的推送，这份旧响应直接作废（code-review §2.16）
let fetchGen = 0;

async function loadSessions() {
  const gen = ++fetchGen;
  const seq = store.sessionsSeq;
  try {
    const s = await api.listActiveSessions();
    if (!Array.isArray(s)) return; // 坏响应不当快照用
    if (gen !== fetchGen || seq !== store.sessionsSeq) return; // 旧拉取 / 期间已有新推送：丢弃
    store.sessions = s;
  } catch {
    /* 静默 */
  }
}

const now = ref(Date.now());
let tickTimer: number | undefined;
let reconcileTimer: number | undefined;

function stopTimers() {
  if (tickTimer) clearInterval(tickTimer);
  if (reconcileTimer) clearInterval(reconcileTimer);
  tickTimer = undefined;
  reconcileTimer = undefined;
}

function startTimers() {
  if (tickTimer) return;
  // 已运行时长需要本地时钟驱动（后端只在状态变化时推送，时长刻度在前端算）
  tickTimer = window.setInterval(() => {
    now.value = Date.now();
  }, 5000);
  // 周期对账：补推可能丢失的快照，也让僵死会话（心跳静止超时）被惰性淘汰
  reconcileTimer = window.setInterval(loadSessions, 15000);
}

onMounted(() => {
  loadHistory();
  loadSessions();
  startTimers();
});

// 页面被 KeepAlive 保活（App.vue）：切走时停掉本地计时与轮询，切回来重启并即刻对账一次
onActivated(() => {
  void loadSessions();
  startTimers();
});

onDeactivated(stopTimers);
onUnmounted(stopTimers);

function durationLabel(s: SessionStatus): string {
  const verb =
    s.phase === "waiting_permission" || s.phase === "waiting_input" ? "已等待" : "已运行";
  return `${verb} ${formatDuration(s.started_at, now.value)}`;
}

function statsLabel(s: SessionStatus): string {
  const parts = [`${s.turn_count} 回合`, `${s.tool_calls} 次工具调用`];
  if (s.last_tool && s.phase === "tool_running") parts.unshift(`工具：${s.last_tool}`);
  return parts.join(" · ");
}

/**
 * 清空显示：先清后端历史，成功后再按**后端返回的 id**删本地副本。
 *
 * 只删本地副本是旧实现的做法，而本页每次挂载都会用 `list_events` 补历史 ——
 * 切走菜单再切回来，被清掉的事件会被历史整批合并回来（看起来「清空没生效」）。
 * 有筛选时只清该类型（后端按 kind 过滤），避免误删其他类型事件。
 *
 * 为什么删的是后端回传的 id，而不是「点击瞬间的本地快照」：清空请求在途时可能
 * 有新事件到达（后端没删它们），按本地快照删会出现两边反向不一致——本地没了、
 * 历史里还在（重挂载又冒出来），或本地留着、后端已删（切页面前显示幽灵事件）。
 */
async function clear() {
  if (clearing.value) return;
  const kind = filter.value || undefined;
  clearing.value = true;
  clearError.value = "";
  let cleared: string[] = [];
  try {
    cleared = await api.clearEvents(kind);
  } catch (e) {
    // 后端没清掉就保持列表原样：本地清了也会被历史补回来，不如老实报错
    clearError.value = `清空事件失败：${e}`;
    return;
  } finally {
    clearing.value = false;
  }
  const gone = new Set(cleared);
  store.events.splice(0, store.events.length, ...store.events.filter((e) => !gone.has(e.id)));
  // 清空序号：让「跨越本次清空」的在途历史拉取作废重拉（见 loadHistory）
  clearSeq += 1;
}
</script>

<template>
  <div>
    <div class="between mb-14">
      <h1 class="page-title">事件流</h1>
      <div class="row">
        <select v-model="filter">
          <button><selectedcontent></selectedcontent></button>
          <option value="">全部类型</option>
          <option v-for="(label, kind) in EVENT_LABELS" :key="kind" :value="kind">{{ label }}</option>
        </select>
        <button class="ghost" :disabled="clearing" @click="clear">
          {{ clearing ? "清空中…" : filter ? "清空当前筛选" : "清空显示" }}
        </button>
      </div>
    </div>

    <!-- 清空失败：列表保持原样（后端没清掉，本地清了也会被历史补回来） -->
    <div v-if="clearError" class="warn-box mb-14">
      <div class="row">
        <span class="grow">{{ clearError }}</span>
        <button class="ghost" :disabled="clearing" @click="clear">重试</button>
      </div>
    </div>

    <div v-if="!historyLoaded" class="empty">加载历史事件…</div>

    <template v-else>
      <!-- 运行中的 agent 会话（实时状态，心跳推送）：
           子代理行打 ↳ + 缩进 + 灰徽标（与悬浮窗、下面的事件列表同一套视觉语言），
           正文弱化，视线自然落在「需要我的那个主会话」上 -->
      <div v-if="runningRows.length" class="card card-tight mb-14">
        <div class="row mb-8">
          <span class="hint grow">运行中会话（{{ runningRows.length }}）</span>
          <label class="hint id-toggle">
            <input type="checkbox" v-model="showIds" />
            显示 ID
          </label>
        </div>
        <div
          v-for="row in runningRows"
          :key="`${row.session.agent}|${row.session.session_id}`"
          class="event-item"
          :class="subagentRowClass(row)"
        >
          <div class="row">
            <span v-if="row.depth > 0" class="sub-tree">↳</span>
            <span v-if="!row.placeholder" :class="phaseBadgeClass(row.session.phase)">{{ PHASE_LABELS[row.session.phase] }}</span>
            <span v-else class="badge">已结束</span>
            <span class="agent-icon" v-html="agentIcon(row.session.agent)"></span>
            <!-- 占位父行：快照里没有这个会话（回合已结束/被淘汰），只是为了给还在跑的子代理
                 一个落脚点而合成的一行（见 api.ts 的 SessionRow.placeholder） -->
            <span class="agent-name" :class="{ 'is-sub': row.placeholder }">
              {{ row.placeholder ? placeholderName(row.session.session_id) : agentName(row.session.agent) }}
            </span>
            <!-- 两个工作区各开一个 DSH 会话时，光看 agent 名分不出是哪一行，
                 而「子代理挂在哪个会话下」全靠这个项目名来认 -->
            <span v-if="row.session.project" class="hint">{{ row.session.project }}</span>
            <span v-if="isSubagentSession(row.session) && !row.placeholder" class="badge">子代理</span>
            <!-- 子代理的血缘小字：父行在快照里时指向它，父行缺席时指向上面那行占位父行 -->
            <span v-if="isSubagentSession(row.session) && !row.placeholder" class="kin">← …{{ parentTail(row.session.parent!) }}</span>
            <span class="grow"></span>
            <span class="event-time">{{ row.placeholder ? "" : durationLabel(row.session) }}</span>
          </div>
          <div v-if="row.session.prompt" class="event-msg">{{ row.session.prompt }}</div>
          <!-- 占位行没有统计数据（它不在快照里，数字全是 0），别显示成「0 回合」 -->
          <div v-if="!row.placeholder" class="hint mt-8">{{ statsLabel(row.session) }}</div>
          <!-- 「显示 ID」：daemon 实际下发的会话 id 与它声明的父 id。
               父子归属有疑问时，这一行就是判据（父 id 对不上 = 插件报错了血缘） -->
          <div v-if="showIds" class="id-line">
            <span>{{ row.session.agent }}|{{ row.session.session_id }}</span>
            <span v-if="row.session.parent"> parent={{ row.session.parent }}</span>
            <span v-else-if="row.placeholder"> 父行已被摘档（快照里没有这个会话）</span>
          </div>
        </div>
      </div>
      <!-- 历史加载失败：非阻塞提示，实时事件列表照常渲染 -->
      <div v-if="historyError" class="warn-box mb-12">
        <div class="row">
          <span class="grow">{{ historyError }}</span>
          <button class="ghost" @click="loadHistory">重试</button>
        </div>
      </div>

      <!-- 空态：还有会话在跑时不再显示「暂无事件」——上面的运行中卡片已经说明
           有 agent 在工作（清空显示后尤甚：进行中的会话不产生历史事件，空态会一直挂着） -->
      <div v-if="!filteredEvents.length && (filter || !runningRows.length)" class="empty">
        {{
          filter
            ? "当前保留的最近事件中没有该类型"
            : "暂无事件 —— 接入 agent 后，任务完成/打断时事件会实时出现在这里"
        }}
      </div>

      <!-- 事件卡片只在真有条目时渲染：清空后若仍有会话在跑，空态与卡片都不出现，
           不能留下一个没有内容的空面板 -->
      <div v-else-if="filteredEvents.length" class="card card-tight">
        <div
          v-for="e in filteredEvents"
          :key="e.id"
          class="event-item"
          :class="{ 'is-sub': e.is_subagent }"
        >
          <div class="row">
            <span :class="eventBadgeClass(e.type)">{{ EVENT_LABELS[e.type] }}</span>
            <span class="agent-icon" v-html="agentIcon(e.agent)"></span>
            <span class="agent-name">{{ agentName(e.agent) }}</span>
            <span v-if="e.project" class="hint">{{ e.project }}</span>
            <!-- 「子代理」徽标走品牌色：四态色表达「它在干什么」，子代理表达「它属于谁」，
                 用中性灰会和「工具失败」撞色。历史事件不带血缘（后端历史里没有父会话边），
                 父子关系在上面「运行中会话」卡片里用 ↳ + ← …尾号 表达 -->
            <span v-if="e.is_subagent" class="badge subagent">子代理</span>
            <span class="grow"></span>
            <span class="event-time">{{ formatTime(e.timestamp) }}</span>
          </div>
          <div v-if="e.message" class="event-msg">{{ e.message }}</div>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
/* 「显示 ID」开关：与卡片内的小字同级，不抢视觉 */
.id-toggle {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  cursor: pointer;
  user-select: none;
}
/* 会话 id / 父 id 行：等宽 + 更暗，属于排障信息而非正文 */
.id-line {
  margin-top: 6px;
  font-family: Consolas, monospace;
  font-size: 11px;
  color: var(--muted);
  word-break: break-all;
  user-select: text;
}
.mb-8 { margin-bottom: 8px; }
</style>
