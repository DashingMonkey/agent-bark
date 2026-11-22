<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref } from "vue";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api, ensureListening, store } from "./api";
import TitleBar from "./components/TitleBar.vue";
import TokenUsagePage from "./pages/TokenUsagePage.vue";
import AgentsPage from "./pages/AgentsPage.vue";
import EventsPage from "./pages/EventsPage.vue";
import ChannelsPage from "./pages/ChannelsPage.vue";
import NotifyPage from "./pages/NotifyPage.vue";
import SettingsPage from "./pages/SettingsPage.vue";
import WidgetPage from "./pages/WidgetPage.vue";
import AboutPage from "./pages/AboutPage.vue";

// 菜单顺序：Token 用量置顶（装好即有本机历史可看）；事件流第二——它装好就能看
// （离线兜底与托盘常驻都会往里记事件），是「现在有没有事发生」的第一入口；
// 「Agents 接入」是使用前的动作，排第三，新用户一眼就能看到要先去哪一页开接入
// （本机一个都没接上时启动还会直接停在这一页，见 guideToAgents）。
// 「第三方通知」排在后面：它是可选的推送通道，装在第三方 App 里才有意义，
// 而前面几项是装好就能用的本机功能。
// 「关于」跟在最后（低频入口：图标 / 名称 / 版本号 / 检查更新），不钉底部、不占首页视野。
const TABS = [
  { id: "usage", labelKey: "nav.usage", comp: TokenUsagePage },
  { id: "events", labelKey: "nav.events", comp: EventsPage },
  { id: "agents", labelKey: "nav.agents", comp: AgentsPage },
  { id: "settings", labelKey: "nav.settings", comp: SettingsPage },
  // 「悬浮窗」紧跟「设置」：同类偏好设置，独立成页放启用开关与停靠说明
  { id: "widget", labelKey: "nav.widget", comp: WidgetPage },
  { id: "notify", labelKey: "nav.notify", comp: NotifyPage },
  { id: "channels", labelKey: "nav.channels", comp: ChannelsPage },
  { id: "about", labelKey: "nav.about", comp: AboutPage },
] as const;

// 用量统计开关（设置页）：关闭即隐藏菜单；后端 save_config 广播实时同步
const usageEnabled = ref(true);
const visibleTabs = computed(() => TABS.filter((t) => t.id !== "usage" || usageEnabled.value));

const active = ref<(typeof TABS)[number]["id"]>("events");
// computed 直接返回组件，避免 ref 持组件被 reactive 代理
const ActiveComp = computed(
  () => visibleTabs.value.find((t) => t.id === active.value)?.comp ?? visibleTabs.value[0].comp,
);

function select(id: (typeof TABS)[number]["id"]) {
  navigated = true;
  active.value = id;
}

const retrying = ref(false);

// 重试实时通道注册：成功后 store.listeningError 被清空，横幅随之消失
async function retryListening() {
  retrying.value = true;
  try {
    await ensureListening();
  } finally {
    retrying.value = false;
  }
}

/**
 * 启动引导：本机**一个 agent 都没接上**时，自动弹出主面板并停在「Agents 接入」页。
 *
 * 主窗口默认隐藏、只走托盘（例行启动不弹面板），新用户装完根本不知道要先开接入——
 * 不引导的话这一页可能永远不会被打开。判定在后端（`needs_agent_setup`，口径与
 * Agents 页的开关一致：配置里开着 **且** hook 真的写进去了）；接上任意一个 agent
 * 之后条件不再成立，启动就不再打扰。
 */
async function guideToAgents() {
  try {
    if (!(await api.needsAgentSetup())) return;
    navigated = true;
    active.value = "agents";
    // 先切页再显示窗口，否则会先闪一帧默认的「事件流」
    await nextTick();
    // 引导落点是 Agents 页，别让 panel-shown 的「回第一个菜单」把它盖掉
    skipPanelShown = true;
    await api.showPanel();
  } catch (e) {
    // 失败一律静默：引导只是锦上添花，不该因此冒出错误提示或拦住启动
    console.error("启动引导失败：", e);
  }
}

let skipPanelShown = false;
let navigated = false;
const unlisteners: (() => void)[] = [];

onMounted(() => {
  ensureListening();
  guideToAgents();
  // 用量菜单的显隐跟配置走；加载失败保持默认显示（统计页自己会报加载错误）
  api
    .getConfig()
    .then((c) => {
      usageEnabled.value = c.token_monitor.enabled;
      // 首帧落点跟配置走（窗口此时还隐藏，切页不可见）；用户/引导已选过页就不动
      if (!navigated) active.value = c.token_monitor.enabled ? "usage" : "events";
    })
    .catch(() => {});
  // 隐藏的那一刻就把落点切到第一个菜单：隐藏时切页不可见，再唤出直接是对的页，
  // 不会「先画一帧旧页再切」。panel-shown 只负责面板已可见时的跳转。
  getCurrentWindow()
    .onCloseRequested(() => {
      active.value = visibleTabs.value[0].id;
    })
    .then((fn) => unlisteners.push(fn));
  listen<boolean>("bark://token-monitor-changed", (e) => {
    usageEnabled.value = e.payload;
    // 正停在用量页时被关掉：回落事件流，避免留在一个已隐藏的菜单页上
    if (!e.payload && active.value === "usage") active.value = "events";
  }).then((fn) => unlisteners.push(fn));
  // 唤出主面板（托盘 / 悬浮窗双击 / 二次启动）：落第一个可见菜单
  listen("bark://panel-shown", () => {
    if (skipPanelShown) {
      skipPanelShown = false;
      return;
    }
    active.value = visibleTabs.value[0].id;
  }).then((fn) => unlisteners.push(fn));
});

onUnmounted(() => unlisteners.forEach((f) => f()));
</script>

<template>
  <!-- 自绘标题栏（logo + 软件名 + 窗口按钮），取代 Windows 原生标题栏 -->
  <TitleBar />
  <div class="layout">
    <aside class="sidebar">
      <button
        v-for="tab in visibleTabs"
        :key="tab.id"
        type="button"
        class="nav-item"
        :class="{ active: active === tab.id }"
        :aria-current="active === tab.id ? 'page' : undefined"
        @click="select(tab.id)"
      >
        {{ $t(tab.labelKey) }}
      </button>
    </aside>
    <main class="content">
      <div class="stack">
        <!-- 实时事件通道未连接：非阻塞横幅，点重试重新注册，成功即消失 -->
        <div v-if="store.listeningError" class="warn-box">
          <div class="row">
            <span class="grow">{{ $t("common.listeningError", { error: store.listeningError }) }}</span>
            <button class="ghost" :disabled="retrying" @click="retryListening">
              {{ retrying ? $t("common.retrying") : $t("common.retry") }}
            </button>
          </div>
        </div>
        <!-- 页面组件一律 KeepAlive 保活：设置页 / 第三方通知页的表单只在「保存」时落盘，
             切页即销毁会把没保存的修改全部丢掉、零提示（见 code-review §1.13）。
             保活后各页自行负责「有未保存的修改」提示（SettingsPage / ChannelsPage） -->
        <KeepAlive>
          <component :is="ActiveComp" />
        </KeepAlive>
      </div>
    </main>
  </div>
</template>
