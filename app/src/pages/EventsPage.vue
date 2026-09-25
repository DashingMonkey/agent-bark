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
  PHASE_LABELS,
  phaseBadgeClass,
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
      <!-- 运行中的 agent 会话（实时状态，心跳推送） -->
      <div v-if="store.sessions.length" class="card card-tight mb-14">
        <div class="event-item" v-for="s in store.sessions" :key="`${s.agent}|${s.session_id}`">
          <div class="row">
            <span :class="phaseBadgeClass(s.phase)">{{ PHASE_LABELS[s.phase] }}</span>
            <span class="agent-icon" v-html="agentIcon(s.agent)"></span>
            <span class="agent-name">{{ agentName(s.agent) }}</span>
            <span v-if="s.project" class="hint">{{ s.project }}</span>
            <span class="grow"></span>
            <span class="event-time">{{ durationLabel(s) }}</span>
          </div>
          <div v-if="s.prompt" class="event-msg">{{ s.prompt }}</div>
          <div class="hint mt-8">{{ statsLabel(s) }}</div>
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
      <div v-if="!filteredEvents.length && (filter || !store.sessions.length)" class="empty">
        {{
          filter
            ? "当前保留的最近事件中没有该类型"
            : "暂无事件 —— 接入 agent 后，任务完成/打断时事件会实时出现在这里"
        }}
      </div>

      <!-- 事件卡片只在真有条目时渲染：清空后若仍有会话在跑，空态与卡片都不出现，
           不能留下一个没有内容的空面板 -->
      <div v-else-if="filteredEvents.length" class="card card-tight">
        <div class="event-item" v-for="e in filteredEvents" :key="e.id">
          <div class="row">
            <span :class="eventBadgeClass(e.type)">{{ EVENT_LABELS[e.type] }}</span>
            <span class="agent-icon" v-html="agentIcon(e.agent)"></span>
            <span class="agent-name">{{ agentName(e.agent) }}</span>
            <span v-if="e.project" class="hint">{{ e.project }}</span>
            <span v-if="e.is_subagent" class="badge">子代理</span>
            <span class="grow"></span>
            <span class="event-time">{{ formatTime(e.timestamp) }}</span>
          </div>
          <div v-if="e.message" class="event-msg">{{ e.message }}</div>
        </div>
      </div>
    </template>
  </div>
</template>
