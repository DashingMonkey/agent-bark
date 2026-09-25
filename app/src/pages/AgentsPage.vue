<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { api, agentIcon, type AgentStatus, type Diagnostics } from "../api";

const statuses = ref<AgentStatus[]>([]);
const diag = ref<Diagnostics | null>(null);
const loading = ref(true);
const loadError = ref("");
const busy = ref<string | null>(null);
// 接入成功后的提示（如 CodeBuddy 需在 /hooks 面板审核）
const messages = ref<Record<string, string>>({});
// 一键开启/关闭：进行中标志与结果汇总
const bulkBusy = ref(false);
const bulkMsg = ref("");
const bulkFailed = ref(false);

/// silent：开关翻转后的回读。静默模式下不切 loading，
/// 否则每次点开关列表都会先变成「扫描中…」再回来，闪一下很难受。
///
/// 请求代际：慢的旧扫描落地前先核对代号，过期即弃——旧响应不得覆盖新响应
/// （连点「重新扫描」或开关回读插队时会出现反向倒退）。
/// 用户主动刷新（非静默）时顺带清掉上一轮的操作提示（messages），它描述的是
/// 已经过去的那次操作，对着新扫出来的状态继续挂着会误导。
let refreshGen = 0;

async function refresh(silent = false) {
  const gen = ++refreshGen;
  if (!silent) {
    loading.value = true;
    messages.value = {}; // 刷新即清：过期的操作提示不带到新状态上
  }
  loadError.value = "";
  try {
    const [list, d] = await Promise.all([api.agentStatuses(), api.diagnostics()]);
    if (gen !== refreshGen) return; // 有更新的扫描在途/已完成：丢弃这份旧响应
    statuses.value = list;
    diag.value = d;
  } catch (e) {
    if (gen !== refreshGen) return;
    statuses.value = [];
    loadError.value = `扫描失败：${e}`;
  } finally {
    if (!silent) loading.value = false;
  }
}

async function toggle(s: AgentStatus, enabled: boolean) {
  busy.value = s.kind;
  bulkMsg.value = ""; // 手动改过开关后，上一次一键操作的结果就不准了
  delete messages.value[s.kind]; // 输入变化即清：上一次操作提示先作废，下面用新结果覆盖
  try {
    const hint = await api.setAgentEnabled(s.kind, enabled);
    if (hint) {
      messages.value[s.kind] = hint;
    } else {
      delete messages.value[s.kind];
    }
    await refresh(true);
  } catch (e) {
    messages.value[s.kind] = `操作失败：${e}`;
    await refresh(true);
  } finally {
    busy.value = null;
  }
}

function statusBadge(s: AgentStatus): { text: string; cls: string } {
  if (!s.installed) return { text: "未安装", cls: "badge" };
  if (s.mode === "planned") return { text: "开发中", cls: "badge" };
  if (s.verify?.state === "config_unreadable") return { text: "配置损坏", cls: "badge err" };
  if (s.verify?.state === "stale_path") return { text: "hook 失效", cls: "badge err" };
  if (s.registered) return { text: s.mode === "watch" ? "监控中" : "已接入", cls: "badge ok" };
  if (s.enabled) return { text: s.mode === "watch" ? "监控未运行" : "hook 已丢失", cls: "badge warn" };
  return { text: "已安装", cls: "badge on" };
}

/// 开关的显示状态 = **实际接上了**（配置里开着 + 校验通过）。
///
/// 不直接用配置里的 `enabled`：目标应用重写配置时会把我们写的 hook 冲掉
/// （实测 Qoder CN 桌面应用启动时整份重写 `~/.qoder-cn/settings.json`），
/// 这时若开关仍显示"开"，用户得点两下（关→开）才能重写；显示成"关"则点一下即可。
function isOn(s: AgentStatus): boolean {
  return s.enabled && s.registered;
}

/// 需要用户处理的具体问题（优先于 trust_hint 展示）
function problem(s: AgentStatus): string | null {
  if (s.last_error) return s.last_error;
  if (s.verify?.state === "config_unreadable") {
    return `配置文件无法解析：${s.verify.path} —— ${s.verify.reason}`;
  }
  if (s.verify?.state === "stale_path") {
    return `hook 指向的程序已不存在（可能重装/移动过）：${s.verify.path}。重新勾选即可修复。`;
  }
  return null;
}

/// 「配置里开着但校验没过」时的操作提示（不是错误，只是告诉用户点哪里恢复）
function actionHint(s: AgentStatus): string | null {
  if (!s.installed || s.mode === "planned" || !s.enabled || s.registered) return null;
  // 上面 problem() 已经给出处理方式，这里不抢
  if (s.verify?.state === "config_unreadable" || s.verify?.state === "stale_path") return null;
  return s.mode === "watch"
    ? "监控未在运行（会话库不可用或轮询启动失败）：点右侧开关重新开启即可重试"
    : "配置文件里已经没有我们的 hook（可能被目标应用重写或清空过）：点右侧开关重新写入即可";
}

// ---- 排序与分组 ----
// 分组规则：本机检测到的（installed）排最前，没检测到的统一置底；
// 两组内部都按软件名首字母排序。
//
// 比较用 Intl.Collator 而非裸 localeCompare：显式钉死中文排序规则，
// 免得跟随系统区域设置出现同一份列表在两台机器上顺序不同。
const collator = new Intl.Collator(["zh-Hans-CN", "en"], { sensitivity: "base" });

/// 软件首字母：取显示名里第一个拉丁字母（现在每个显示名都以拉丁字母开头，
/// 这么写是为了将来万一有带中文前缀的名字也能排对）。
/// 整个名字里没有拉丁字母时用 \uffff 兜底，统一落到 A–Z 之后，不会插进字母中间。
function initial(name: string): string {
  const m = /[A-Za-z]/.exec(name);
  return m ? m[0].toUpperCase() : "\uffff";
}

/// 同首字母时的次级排序键：从第一个拉丁字母开始截断，丢掉可能的中文前缀
/// —— 否则前缀会被 collator 排到同组最前，出现「前缀名」压在同组其它名字上面的怪顺序。
function sortName(name: string): string {
  const m = /[A-Za-z]/.exec(name);
  return m ? name.slice(m.index) : name;
}

function byName(a: AgentStatus, b: AgentStatus): number {
  const ia = initial(a.display_name);
  const ib = initial(b.display_name);
  if (ia !== ib) return ia < ib ? -1 : 1;
  return collator.compare(sortName(a.display_name), sortName(b.display_name));
}

const groups = computed(() => {
  // filter 已产出新数组，再 sort 不会动到 statuses 本身
  const detected = statuses.value.filter((s) => s.installed).sort(byName);
  const missing = statuses.value.filter((s) => !s.installed).sort(byName);
  return [
    { key: "detected", title: "已检测到", items: detected },
    { key: "missing", title: "未检测到", items: missing },
  ].filter((g) => g.items.length > 0);
});

// ---- 一键开启 / 关闭 ----

/// 参与批量操作的 agent：未安装、开发中的开关本身不可用，这里同样跳过
const toggleable = computed(() =>
  statuses.value.filter((s) => s.installed && s.mode !== "planned"),
);

/// 需要「开」的：实际没接上的（含「配置里开着但 hook 被应用重写掉了」和
/// 「hook 指向的程序被移动过」这两种——重新开一次即重写/就地修复）
const pendingEnable = computed(() => toggleable.value.filter((s) => !isOn(s)));
const bulkTarget = computed(() => pendingEnable.value.length > 0);
/// 「全部关闭」只按配置里的开关算：hook 已丢失的也在内（顺手把陈旧的 enabled 关掉）
const bulkDisable = computed(() => toggleable.value.filter((s) => s.enabled));

async function bulkToggle() {
  const target = bulkTarget.value;
  const targets = target ? pendingEnable.value : bulkDisable.value;
  if (!targets.length) return;

  bulkBusy.value = true;
  bulkMsg.value = "";
  bulkFailed.value = false;
  messages.value = {}; // 输入变化即清：整表即将重写，上一轮的操作提示全部作废
  const failed: string[] = [];
  // 串行下发：每步都要改写 agent 配置文件与 config.json，并发容易在
  // 「写 hook / 失败回滚」之间交错，慢一点但结果确定
  for (const s of targets) {
    try {
      const hint = await api.setAgentEnabled(s.kind, target);
      if (hint) {
        messages.value[s.kind] = hint;
      } else {
        delete messages.value[s.kind];
      }
    } catch (e) {
      messages.value[s.kind] = `操作失败：${e}`;
      failed.push(s.display_name);
    }
  }
  bulkBusy.value = false;
  await refresh(true);

  const verb = target ? "开启" : "关闭";
  const ok = targets.length - failed.length;
  bulkFailed.value = failed.length > 0;
  bulkMsg.value = failed.length
    ? `已${verb} ${ok} 个 agent，${failed.length} 个失败（${failed.join("、")}）——原因见对应卡片`
    : `已${verb} ${ok} 个 agent`;
}

onMounted(refresh);
</script>

<template>
  <div>
    <div class="between mb-14">
      <h1 class="page-title">Agents 接入</h1>
      <div class="row">
        <button class="ghost" :disabled="loading || bulkBusy || !!busy" @click="refresh()">重新扫描</button>
        <!-- 一键总开关：开 = 全部开启（顺带修复 hook 失效的接入），关 = 全部关闭。
             显示态与 agent 开关同口径：只要还有没接上的就显示为关，翻开即全部开启。
             任意 setAgentEnabled 在途（单个 busy / 批量 bulkBusy）时整个开关区禁用：
             每步都要改写 agent 配置与 config.json，并发会互相交错（与 bulkToggle 的串行假设对齐） -->
        <label
          v-tooltip="
            toggleable.length
              ? '一键开关所有已检测到的 agent（未安装与暂不支持接入的自动跳过）；开启时会顺带修复 hook 失效的接入'
              : '本机暂无可接入的 agent'
          "
          class="switch"
        >
          <input
            type="checkbox"
            :checked="toggleable.length > 0 && !bulkTarget"
            :disabled="loading || bulkBusy || !!busy || !toggleable.length"
            aria-label="一键开关所有 agent"
            @change="bulkToggle"
          />
          <span class="track"></span>
        </label>
      </div>
    </div>

    <div v-if="bulkMsg" :class="[bulkFailed ? 'warn-box' : 'ok-box', 'mb-12']">{{ bulkMsg }}</div>

    <div v-if="diag?.startup_warnings?.length" class="warn-box mb-12">
      启动自检发现问题：
      <div v-for="(w, i) in diag.startup_warnings" :key="i" class="mt-4">· {{ w }}</div>
    </div>

    <div v-if="diag?.config_error" class="warn-box mb-12">
      配置读取失败：{{ diag.config_error }}<br />
      文件：{{ diag.config_path }}（当前使用默认配置运行，修复文件后重启应用即可）
    </div>

    <div v-if="loading" class="empty">扫描中…</div>

    <div v-else-if="loadError" class="empty">
      {{ loadError }}
      <div class="mt-12"><button class="ghost" @click="refresh()">重新扫描</button></div>
    </div>

    <div v-else-if="!statuses.length" class="empty">未检测到支持的 agent</div>

    <div v-else>
      <template v-for="g in groups" :key="g.key">
        <div class="group-title">{{ g.title }}（{{ g.items.length }}）</div>

        <div
          v-for="s in g.items"
          :key="s.kind"
          class="card"
          :class="{ disabled: !s.installed || s.mode === 'planned' }"
        >
          <div class="between">
            <div class="col grow">
              <div class="row">
                <span class="agent-icon" v-html="agentIcon(s.kind)"></span>
                <span class="agent-name">{{ s.display_name }}</span>
                <span :class="statusBadge(s).cls">{{ statusBadge(s).text }}</span>
                <span v-if="s.mode === 'watch' && s.installed" class="badge warn">监控型</span>
              </div>
              <!-- 多路径（如 DeepseekHarness/OpenCode 的配置+插件）各占一行：
                   用 · 拼一行时路径会在中途折断，哪段是哪个文件读不出来 -->
              <span v-if="s.config_paths.length" class="agent-path">
                <span v-for="(p, i) in s.config_paths" :key="i">{{ p }}</span>
              </span>
            </div>

            <!-- 接入说明改为悬停显示的感叹号，不再常驻占一版面 -->
            <span
              v-if="s.trust_hint"
              v-tooltip="s.trust_hint"
              class="help-mark"
              role="img"
              :aria-label="s.trust_hint"
            >
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <circle cx="8" cy="8" r="6.1" />
                <path d="M8 4.9v4" />
                <circle class="dot" cx="8" cy="11.1" r="0.9" />
              </svg>
            </span>

            <label
              v-tooltip="
                !s.installed
                  ? '未安装'
                  : s.mode === 'planned'
                    ? '开发中'
                    : isOn(s)
                      ? '已接入，点击关闭'
                      : '未接入，点击写入配置'
              "
              class="switch"
            >
              <input
                type="checkbox"
                :checked="isOn(s)"
                :disabled="!s.installed || s.mode === 'planned' || !!busy || bulkBusy"
                :aria-label="`${s.display_name} 接入开关`"
                @change="toggle(s, ($event.target as HTMLInputElement).checked)"
              />
              <span class="track"></span>
            </label>
          </div>

          <div v-if="messages[s.kind]" class="warn-box mt-8">{{ messages[s.kind] }}</div>
          <div v-else-if="problem(s)" class="warn-box mt-8">{{ problem(s) }}</div>
          <div v-else-if="actionHint(s)" class="hint mt-8">{{ actionHint(s) }}</div>
        </div>
      </template>
    </div>
  </div>
</template>
