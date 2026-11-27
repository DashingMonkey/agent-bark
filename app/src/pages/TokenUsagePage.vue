<script setup lang="ts">
import { computed, nextTick, onActivated, onDeactivated, onMounted, onUnmounted, ref, watch, watchEffect } from "vue";
import { useI18n } from "vue-i18n";
import { agentIcon, api, type AgentUsage, type DailyUsage, type HourlyUsage, type UsageCoverage } from "../api";

const { t, locale } = useI18n();

type Range = "today" | "d7" | "d30" | "d365";
const RANGE_KEY = "usage.range";
function isRange(v: string | null): v is Range {
  return v === "today" || v === "d7" || v === "d30" || v === "d365";
}
// 默认查「今天」，切换后记入 localStorage，重启应用也保持上次选择
const range = ref<Range>((() => {
  try {
    const saved = localStorage.getItem(RANGE_KEY);
    return isRange(saved) ? saved : "today";
  } catch {
    return "today";
  }
})());

function onRangeChange(next: Range) {
  range.value = next;
  try {
    localStorage.setItem(RANGE_KEY, next);
  } catch {
    // 存不进去就算了，本次会话内仍生效
  }
  void refresh();
}
const agents = ref<AgentUsage[]>([]);
const daily = ref<DailyUsage[]>([]);
const hourly = ref<HourlyUsage[]>([]);
const coverage = ref<UsageCoverage[]>([]);
const loadError = ref("");
const asOf = ref("");
let timer: number | null = null;
let resizeOb: ResizeObserver | null = null;

function dayStr(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function bounds(): [string, string] {
  const now = new Date();
  const to = dayStr(now);
  if (range.value === "today") return [to, to];
  // 近一年 = 库内全部：保留期 365 天，更早的已在开库时清掉（usage_store）
  if (range.value === "d365") return ["2000-01-01", to];
  const from = new Date(now);
  from.setDate(from.getDate() - (range.value === "d7" ? 6 : 29));
  return [dayStr(from), to];
}

async function refresh() {
  const [from, to] = bounds();
  try {
    const [a, d, h] = await Promise.all([
      api.usageByAgent(from, to),
      api.usageDaily(from, to),
      range.value === "today" ? api.usageHourly(from, to) : Promise.resolve([] as HourlyUsage[]),
    ]);
    agents.value = a;
    daily.value = d;
    hourly.value = h;
    loadError.value = "";
    asOf.value = new Date().toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });
  } catch (e) {
    loadError.value = t("usage.loadFailed", { error: String(e) });
  }
}

// 已安装但无完整本地用量源的 agent：只在页面进入时扫一次，不跟 30s 轮询。
// 失败就隐藏页脚，不能影响用量加载。
async function loadCoverage() {
  try {
    coverage.value = await api.usageCoverage();
  } catch {
    coverage.value = [];
  }
}

function stopTimer() {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}
// KeepAlive 保活：切走停轮询，切回补一次即时刷新（与设置页诊断轮询同款）
onMounted(() => {
  void refresh();
  void loadCoverage();
  timer = window.setInterval(() => void refresh(), 30_000);
  // 滚动时 fixed 提示会悬空错位，直接收起（capture 捕获所有滚动容器）
  window.addEventListener("scroll", barLeave, true);
  // 观察 body 而非 window resize：容器级宽度变化（含窗口缩放）都不漏事件
  resizeOb = new ResizeObserver(() => measureChart());
  resizeOb.observe(document.body);
});
onActivated(() => {
  if (timer === null) {
    void refresh();
    void loadCoverage();
    timer = window.setInterval(() => void refresh(), 30_000);
  }
  void nextTick(measureChart);
});
onDeactivated(() => {
  stopTimer();
  barLeave();
});
onUnmounted(() => {
  stopTimer();
  barLeave();
  window.removeEventListener("scroll", barLeave, true);
  resizeOb?.disconnect();
  resizeOb = null;
});

const totals = computed(() => {
  const s = { requests: 0, input: 0, cache: 0, output: 0 };
  for (const a of agents.value) {
    s.requests += a.requests;
    s.input += a.input_new;
    s.cache += a.cache_read;
    s.output += a.output;
  }
  return s;
});
const cacheRatio = computed(() => {
  const den = totals.value.input + totals.value.cache;
  return den ? totals.value.cache / den : 0;
});
// 只在表格里没有该 agent 行时展示；有行就以行内的 token 为准（含 Qoder 条件源）。
const coverageAgents = computed(() => {
  const used = new Set(agents.value.map((a) => a.agent));
  return coverage.value.filter((c) => !used.has(c.kind));
});
function coverageReason(c: UsageCoverage): string {
  return c.reason === "custom_only" ? t("usage.coverageQoder") : t("usage.coverageNoSource");
}
const rangeLabel = computed(() =>
  t({ today: "usage.rangeToday", d7: "usage.range7", d30: "usage.range30", d365: "usage.range365" }[range.value]),
);

const trimZeros = (s: string) => s.replace(/\.?0+$/, "");
function fmt(n: number): string {
  if (locale.value === "zh") {
    if (n >= 1e8) return trimZeros((n / 1e8).toFixed(2)) + " 亿";
    if (n >= 1e4) return trimZeros((n / 1e4).toFixed(1)) + " 万";
  } else {
    if (n >= 1e9) return trimZeros((n / 1e9).toFixed(2)) + "B";
    if (n >= 1e6) return trimZeros((n / 1e6).toFixed(1)) + "M";
    if (n >= 1e3) return trimZeros((n / 1e3).toFixed(1)) + "K";
  }
  return n.toLocaleString();
}
const fmtFull = (n: number) => n.toLocaleString();
const tokenSum = (a: AgentUsage) => a.input_new + a.cache_read + a.output;

// ---- 堆叠图（今天=小时 / 近一年=月 / 其余=日；hover 变形 + 跟随光标的提示）----
const svgEl = ref<SVGSVGElement | null>(null);
// 渲染宽度 / viewBox 宽：轴文字按它反算 font-size，保持屏幕 11px
const chartScale = ref(1);

let measureCtx: CanvasRenderingContext2D | null = null;
function textWidthPx(text: string, family: string): number {
  if (!measureCtx) measureCtx = document.createElement("canvas").getContext("2d");
  if (!measureCtx) return text.length * 6.5;
  measureCtx.font = `11px ${family}`;
  return measureCtx.measureText(text).width;
}

function measureChart() {
  const el = svgEl.value;
  if (!el) return;
  const s = el.getBoundingClientRect().width / 960;
  if (s > 0.05 && Math.abs(s - chartScale.value) > 0.005) chartScale.value = s;
}

interface Bar {
  key: string;
  x: number;
  w: number;
  segs: { cls: string; y: number; h: number }[];
  title: string;
  input: number;
  cache: number;
  output: number;
  requests: number;
}
const chart = computed(() => {
  interface Point {
    key: string;
    label: string;
    title: string;
    input: number;
    cache: number;
    output: number;
    requests: number;
  }
  const zero = { input: 0, cache: 0, output: 0, requests: 0 };
  let points: Point[];
  if (range.value === "today") {
    // 今天 → 24 根小时柱（本地时区，后端按 ts 聚合）
    const map = new Map<string, typeof zero>();
    for (const r of hourly.value) {
      const b = map.get(r.hour) ?? { ...zero };
      b.input += r.input_new;
      b.cache += r.cache_read;
      b.output += r.output;
      b.requests += r.requests;
      map.set(r.hour, b);
    }
    points = Array.from({ length: 24 }, (_, h) => {
      const key = String(h).padStart(2, "0");
      const b = map.get(key) ?? { ...zero };
      return {
        key,
        label: `${key}:00`,
        title: `${key}:00–${key}:59`,
        ...b,
      };
    });
  } else if (range.value === "d365") {
    // 近一年 → 按月聚合：365 根日柱太密，月柱一眼看趋势
    const map = new Map<string, typeof zero>();
    for (const r of daily.value) {
      const k = r.day.slice(0, 7);
      const b = map.get(k) ?? { ...zero };
      b.input += r.input_new;
      b.cache += r.cache_read;
      b.output += r.output;
      b.requests += r.requests;
      map.set(k, b);
    }
    const keys = [...map.keys()].sort();
    if (!keys.length) return null;
    // 轴至少覆盖最近 6 个月（只有一个月数据时不孤柱）；数据更宽则随数据。
    // 首末之间按月补零（保留期 ≤365 天，上限 14 个月）
    const now = new Date();
    const curKey = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}`;
    const minStart = new Date(now.getFullYear(), now.getMonth() - 5, 1);
    const minStartKey = `${minStart.getFullYear()}-${String(minStart.getMonth() + 1).padStart(2, "0")}`;
    const startKey = keys[0] < minStartKey ? keys[0] : minStartKey;
    const endKey = keys[keys.length - 1] > curKey ? keys[keys.length - 1] : curKey;
    const months: { month: string; input: number; cache: number; output: number; requests: number }[] = [];
    const cur = new Date(startKey + "-01T00:00:00");
    const end = new Date(endKey + "-01T00:00:00");
    while (cur <= end && months.length <= 14) {
      const k = `${cur.getFullYear()}-${String(cur.getMonth() + 1).padStart(2, "0")}`;
      months.push({ month: k, ...(map.get(k) ?? { ...zero }) });
      cur.setMonth(cur.getMonth() + 1);
    }
    points = months.map((m) => ({
      key: m.month,
      label: m.month,
      title: monthTitle(m.month),
      input: m.input,
      cache: m.cache,
      output: m.output,
      requests: m.requests,
    }));
  } else {
    const map = new Map<string, typeof zero>();
    for (const r of daily.value) {
      const b = map.get(r.day) ?? { ...zero };
      b.input += r.input_new;
      b.cache += r.cache_read;
      b.output += r.output;
      b.requests += r.requests;
      map.set(r.day, b);
    }
    const keys = [...map.keys()].sort();
    if (!keys.length) return null;
    // 首末之间按日补零（保留期 ≤365 天，跨度可控）
    const days: { day: string; input: number; cache: number; output: number; requests: number }[] = [];
    const cur = new Date(keys[0] + "T00:00:00");
    const end = new Date(keys[keys.length - 1] + "T00:00:00");
    while (cur <= end && days.length <= 366) {
      const k = dayStr(cur);
      days.push({ day: k, ...(map.get(k) ?? { ...zero }) });
      cur.setDate(cur.getDate() + 1);
    }
    points = days.map((d) => ({
      key: d.day,
      label: d.day.slice(5),
      title: d.day,
      input: d.input,
      cache: d.cache,
      output: d.output,
      requests: d.requests,
    }));
  }
  const T = 18, plotH = 248, W = 960;
  const max = Math.max(...points.map((d) => d.input + d.cache + d.output), 1);
  // 刻度粒度取 max/4 的十分之一量级再向上取整：顶格贴近最大值，不留半张空白
  const raw = max / 4;
  const mag = 10 ** Math.floor(Math.log10(raw)) / 10;
  const step = Math.ceil(raw / mag) * mag;
  const top = step * 4;
  const yTicks = [0, 1, 2, 3, 4].map((i) => ({ y: T + plotH - (plotH * i) / 4, label: fmt(step * i) }));
  // 轴文字固定屏幕 11px：窗口缩小时标签才真正变挤，触发斜放/竖放换档
  const s = Math.max(chartScale.value, 0.2);
  const fontSvg = 11 / s;
  const family = svgEl.value ? getComputedStyle(svgEl.value).fontFamily : "sans-serif";
  const yw = Math.max(...yTicks.map((t) => textWidthPx(t.label, family))) / s;
  const xw = Math.max(...points.map((p) => textWidthPx(p.label, family))) / s;
  const L = Math.ceil(yw + 12), R = 12;
  const plotW = W - L - R;
  const n = points.length;
  const slot = plotW / n;
  // 全标签阶梯：横排 → 40° → 65° → 竖放 90°；竖放仍挤才抽稀兜底
  let rotate = 0, every = 1;
  if (xw + 6 > slot) {
    rotate = 90;
    for (const a of [40, 65]) {
      const r = (a * Math.PI) / 180;
      if (xw * Math.cos(r) + fontSvg * Math.sin(r) + 4 <= slot) {
        rotate = a;
        break;
      }
    }
    if (rotate === 90 && fontSvg + 4 > slot) every = Math.ceil((fontSvg + 4) / slot);
  }
  const rad = (rotate * Math.PI) / 180;
  const labelY = T + plotH + (rotate ? fontSvg + 4 : fontSvg * 2 + 2);
  const below = rotate === 90 ? xw : rotate ? xw * Math.sin(rad) + fontSvg * Math.cos(rad) : fontSvg * 0.35;
  const H = Math.ceil(labelY + below + 4);
  const bw = (plotW / n) * 0.62;
  const bars: Bar[] = points.map((d, i) => {
    const x = L + (plotW * (i + 0.5)) / n - bw / 2;
    let y = T + plotH;
    const segs: Bar["segs"] = [];
    // 堆叠顺序：缓存(底) → 新输入 → 输出(顶)
    const parts: [string, number][] = [
      ["seg-cache", d.cache],
      ["seg-input", d.input],
      ["seg-output", d.output],
    ];
    for (const [cls, v] of parts) {
      const h = (plotH * v) / top;
      if (h > 0) {
        y -= h;
        segs.push({ cls, y, h: Math.max(h, 0.6) });
      }
    }
    return {
      key: d.key,
      x,
      w: bw,
      segs,
      title: d.title,
      input: d.input,
      cache: d.cache,
      output: d.output,
      requests: d.requests,
    };
  });
  const xLabels = points
    .map((d, i) => ({ key: d.key, label: d.label, x: L + (plotW * (i + 0.5)) / n, i }))
    // 抽稀时不再强留末标签：它会和前一保留标签贴在一起重叠
    .filter((d) => d.i % every === 0 || (every === 1 && d.i === n - 1));
  return { bars, yTicks, xLabels, H, L, R, fontSvg, labelY, rotate };
});
// 图出现 / 数据刷新后补测宽度：首帧 svg 还没挂上，量不到真实渲染宽
watch(chart, () => void nextTick(measureChart), { flush: "post" });
const chartTitle = computed(() =>
  t(
    range.value === "today"
      ? "usage.chartTitleHourly"
      : range.value === "d365"
        ? "usage.chartTitleMonthly"
        : "usage.chartTitle",
  ),
);

function monthTitle(key: string): string {
  const [y, m] = key.split("-").map(Number);
  if (locale.value === "zh") return `${y}年${m}月`;
  return new Date(y, m - 1, 1).toLocaleString("en", { month: "short", year: "numeric" });
}

// ---- 柱子 hover：选中变形 + 立即出现、跟随光标的详情提示 ----
// 不走全局 v-tooltip：那是 300ms 延迟 + 锚定元素下方，图表要即时且贴着鼠标
const hoverKey = ref<string | null>(null);
const cursor = ref<{ x: number; y: number } | null>(null);
const tipEl = ref<HTMLDivElement | null>(null);
const hoverBar = computed(() => chart.value?.bars.find((b) => b.key === hoverKey.value) ?? null);

function barEnter(bar: Bar, e: MouseEvent) {
  hoverKey.value = bar.key;
  cursor.value = { x: e.clientX, y: e.clientY };
}
function barMove(e: MouseEvent) {
  cursor.value = { x: e.clientX, y: e.clientY };
}
function barLeave() {
  hoverKey.value = null;
  cursor.value = null;
}

// 渲染后量尺寸再摆位：贴近光标，右/下放不下翻转到另一侧
watchEffect(
  () => {
    const el = tipEl.value;
    const p = cursor.value;
    if (!el || !p) return;
    const w = el.offsetWidth;
    const h = el.offsetHeight;
    let left = p.x + 14;
    let top = p.y + 16;
    if (left + w > window.innerWidth - 8) left = p.x - w - 14;
    if (top + h > window.innerHeight - 8) top = p.y - h - 12;
    el.style.left = `${Math.max(8, left)}px`;
    el.style.top = `${Math.max(8, top)}px`;
  },
  { flush: "post" },
);
</script>

<template>
  <div>
    <div class="page-head">
      <div class="page-head-left">
        <h1 class="page-title">{{ $t("usage.title") }}</h1>
        <span class="hint">{{ $t("usage.asOf", { time: asOf }) }}</span>
      </div>
      <div class="page-actions">
        <select :value="range" @change="onRangeChange(($event.target as HTMLSelectElement).value as Range)">
          <option value="today">{{ $t("usage.rangeToday") }}</option>
          <option value="d7">{{ $t("usage.range7") }}</option>
          <option value="d30">{{ $t("usage.range30") }}</option>
          <option value="d365">{{ $t("usage.range365") }}</option>
        </select>
        <button class="ghost" @click="refresh">{{ $t("usage.refresh") }}</button>
      </div>
    </div>

    <div class="stack">
      <div v-if="loadError" class="warn-box">{{ loadError }}</div>

      <template v-if="agents.length">
        <div class="stat-grid">
          <div class="stat">
            <div class="k">{{ $t("usage.totalTokens") }}</div>
            <div class="v">{{ fmt(totals.input + totals.cache + totals.output) }}</div>
          </div>
          <div class="stat">
            <div class="k">{{ $t("usage.outputTokens") }}</div>
            <div class="v">{{ fmt(totals.output) }}</div>
          </div>
          <div class="stat">
            <div class="k">{{ $t("usage.cacheHit") }}</div>
            <div class="v">{{ (cacheRatio * 100).toFixed(1) }}%</div>
          </div>
          <div class="stat">
            <div class="k">{{ $t("usage.requests") }}</div>
            <div class="v">{{ fmtFull(totals.requests) }}{{ locale === "zh" ? " 次" : "" }}</div>
          </div>
        </div>

        <div class="card">
          <div class="card-head">
            <span class="card-title">{{ chartTitle }}</span>
            <div class="legend hint">
              <span><i class="legend-dot" style="background: var(--c-cache)"></i>{{ $t("usage.legendCache") }}</span>
              <span><i class="legend-dot" style="background: var(--c-input)"></i>{{ $t("usage.legendInput") }}</span>
              <span><i class="legend-dot" style="background: var(--c-output)"></i>{{ $t("usage.legendOutput") }}</span>
            </div>
          </div>
          <svg v-if="chart" ref="svgEl" class="usage-chart" :class="{ 'has-hover': hoverBar }" :viewBox="`0 0 960 ${chart.H}`" role="img" :aria-label="chartTitle" @mousedown.prevent>
            <g v-for="tick in chart.yTicks" :key="tick.y">
              <line class="grid" :x1="chart.L" :y1="tick.y" :x2="960 - chart.R" :y2="tick.y" />
              <text :x="chart.L - 8" :y="tick.y + chart.fontSvg * 0.35" :font-size="chart.fontSvg" text-anchor="end">{{ tick.label }}</text>
            </g>
            <g
              v-for="bar in chart.bars"
              :key="bar.key"
              class="bar"
              :class="{ active: hoverKey === bar.key }"
              @mouseenter="barEnter(bar, $event)"
              @mousemove="barMove"
              @mouseleave="barLeave"
            >
              <rect v-for="(seg, si) in bar.segs" :key="si" :class="seg.cls" :x="bar.x" :y="seg.y" :width="bar.w" :height="seg.h" rx="1.5" />
              <rect :x="bar.x - 3" y="18" :width="bar.w + 6" height="248" fill="transparent" />
            </g>
            <text
              v-for="lb in chart.xLabels"
              :key="lb.key"
              :x="lb.x"
              :y="chart.labelY"
              :font-size="chart.fontSvg"
              :text-anchor="chart.rotate ? 'end' : 'middle'"
              :transform="chart.rotate ? `rotate(-${chart.rotate} ${lb.x} ${chart.labelY})` : undefined"
            >{{ lb.label }}</text>
          </svg>
        </div>

        <div class="card">
          <div class="card-head"><span class="card-title">{{ $t("usage.tableTitle", { range: rangeLabel }) }}</span></div>
          <table class="usage">
            <thead>
              <tr>
                <th>{{ $t("usage.colAgent") }}</th>
                <th>{{ $t("usage.colRequests") }}</th>
                <th>{{ $t("usage.colInput") }}</th>
                <th>{{ $t("usage.colCache") }}</th>
                <th>{{ $t("usage.colOutput") }}</th>
                <th>{{ $t("usage.colTotal") }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="a in agents" :key="a.agent">
                <td>
                  <div class="row">
                    <span class="agent-icon" v-html="agentIcon(a.agent)"></span>
                    <span class="label-line">
                      <span class="agent-name">{{ a.display_name }}</span>
                      <span
                        v-if="a.agent === 'qoder'"
                        v-tooltip="$t('usage.qoderCustomOnly')"
                        class="help-mark"
                        role="img"
                        :aria-label="$t('usage.qoderCustomOnly')"
                      >
                        <svg viewBox="0 0 16 16" aria-hidden="true">
                          <circle cx="8" cy="8" r="6.1" />
                          <path d="M8 4.9v4" />
                          <circle class="dot" cx="8" cy="11.1" r="0.9" />
                        </svg>
                      </span>
                    </span>
                  </div>
                </td>
                <td>{{ tokenSum(a) ? fmtFull(a.requests) : "—" }}</td>
                <td>{{ tokenSum(a) ? fmt(a.input_new) : "—" }}</td>
                <td>{{ tokenSum(a) ? fmt(a.cache_read) : "—" }}</td>
                <td>{{ tokenSum(a) ? fmt(a.output) : "—" }}</td>
                <td class="total">{{ tokenSum(a) ? fmt(tokenSum(a)) : "—" }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </template>
      <div v-else class="empty">{{ $t("usage.empty") }}</div>

      <div v-if="coverageAgents.length" class="usage-coverage">
        <span class="usage-coverage-label">{{ $t("usage.coverageTitle") }}</span>
        <span
          v-for="c in coverageAgents"
          :key="c.kind"
          v-tooltip="`${c.display_name} · ${coverageReason(c)}`"
          class="usage-coverage-agent"
          role="img"
          tabindex="0"
          :aria-label="`${c.display_name} · ${coverageReason(c)}`"
        >
          <span class="agent-icon" v-html="agentIcon(c.kind)"></span>
        </span>
      </div>
    </div>

    <Teleport to="body">
      <div v-if="hoverBar && cursor" ref="tipEl" class="chart-tip" role="tooltip">
        <div class="chart-tip-title">{{ hoverBar.title }}</div>
        <div class="chart-tip-row">
          <i class="legend-dot" style="background: var(--c-cache)"></i>
          <span>{{ $t("usage.legendCache") }}</span>
          <b>{{ fmtFull(hoverBar.cache) }}</b>
        </div>
        <div class="chart-tip-row">
          <i class="legend-dot" style="background: var(--c-input)"></i>
          <span>{{ $t("usage.legendInput") }}</span>
          <b>{{ fmtFull(hoverBar.input) }}</b>
        </div>
        <div class="chart-tip-row">
          <i class="legend-dot" style="background: var(--c-output)"></i>
          <span>{{ $t("usage.legendOutput") }}</span>
          <b>{{ fmtFull(hoverBar.output) }}</b>
        </div>
        <div class="chart-tip-total">
          {{ $t("usage.tipTotal") }} {{ fmtFull(hoverBar.input + hoverBar.cache + hoverBar.output) }} ·
          {{ $t("usage.tipRequests", { n: fmtFull(hoverBar.requests) }) }}
        </div>
      </div>
    </Teleport>
  </div>
</template>