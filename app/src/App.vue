<script setup lang="ts">
import { computed, nextTick, onMounted, ref } from "vue";
import { api, ensureListening, store } from "./api";
import TitleBar from "./components/TitleBar.vue";
import AgentsPage from "./pages/AgentsPage.vue";
import EventsPage from "./pages/EventsPage.vue";
import ChannelsPage from "./pages/ChannelsPage.vue";
import NotifyPage from "./pages/NotifyPage.vue";
import SettingsPage from "./pages/SettingsPage.vue";
import WidgetPage from "./pages/WidgetPage.vue";
import AboutPage from "./pages/AboutPage.vue";

// 菜单顺序：事件流排第一——它装好就能看（离线兜底与托盘常驻都会往里记事件），
// 是「现在有没有事发生」的第一入口；「Agents 接入」是使用前的动作，排第二，
// 新用户一眼就能看到要先去哪一页开接入（本机一个都没接上时启动还会直接停在这一页，
// 见 guideToAgents）。
// 「第三方通知」排在后面：它是可选的推送通道，装在第三方 App 里才有意义，
// 而前面几项是装好就能用的本机功能。
// 「关于」跟在最后（低频入口：图标 / 名称 / 版本号 / 检查更新），不钉底部、不占首页视野。
const tabs = [
  { id: "events", label: "事件流", comp: EventsPage },
  { id: "agents", label: "Agents 接入", comp: AgentsPage },
  { id: "settings", label: "设置", comp: SettingsPage },
  // 「悬浮窗」紧跟「设置」：同类偏好设置，独立成页放启用开关与自动隐藏
  { id: "widget", label: "悬浮窗", comp: WidgetPage },
  { id: "notify", label: "通知", comp: NotifyPage },
  { id: "channels", label: "第三方通知", comp: ChannelsPage },
  { id: "about", label: "关于", comp: AboutPage },
] as const;

const active = ref<(typeof tabs)[number]["id"]>("events");
// computed 直接返回组件，避免 ref 持组件被 reactive 代理
const ActiveComp = computed(() => tabs.find((t) => t.id === active.value)?.comp ?? tabs[0].comp);

function select(id: (typeof tabs)[number]["id"]) {
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
    active.value = "agents";
    // 先切页再显示窗口，否则会先闪一帧默认的「事件流」
    await nextTick();
    await api.showPanel();
  } catch (e) {
    // 失败一律静默：引导只是锦上添花，不该因此冒出错误提示或拦住启动
    console.error("启动引导失败：", e);
  }
}

onMounted(() => {
  ensureListening();
  guideToAgents();
});
</script>

<template>
  <!-- 自绘标题栏（logo + 软件名 + 窗口按钮），取代 Windows 原生标题栏 -->
  <TitleBar />
  <div class="layout">
    <aside class="sidebar">
      <button
        v-for="t in tabs"
        :key="t.id"
        type="button"
        class="nav-item"
        :class="{ active: active === t.id }"
        :aria-current="active === t.id ? 'page' : undefined"
        @click="select(t.id)"
      >
        {{ t.label }}
      </button>
    </aside>
    <main class="content">
      <!-- 实时事件通道未连接：非阻塞横幅，点重试重新注册，成功即消失 -->
      <div v-if="store.listeningError" class="warn-box mb-14">
        <div class="row">
          <span class="grow">实时事件通道未连接：{{ store.listeningError }}</span>
          <button class="ghost" :disabled="retrying" @click="retryListening">
            {{ retrying ? "重试中…" : "重试" }}
          </button>
        </div>
      </div>
      <!-- 页面组件一律 KeepAlive 保活：设置页 / 第三方通知页的表单只在「保存」时落盘，
           切页即销毁会把没保存的修改全部丢掉、零提示（见 code-review §1.13）。
           保活后各页自行负责「有未保存的修改」提示（SettingsPage / ChannelsPage） -->
      <KeepAlive>
        <component :is="ActiveComp" />
      </KeepAlive>
    </main>
  </div>
</template>
