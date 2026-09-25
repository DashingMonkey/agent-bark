<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from "vue";
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { UnlistenFn } from "@tauri-apps/api/event";
import { onThemeChange, resolvedTheme, themePref, toggleTheme, type Theme, type ThemePref } from "../theme";
import logo from "../assets/logo.png";

// 自绘标题栏：主窗口在 tauri.conf.json 里关掉了原生装饰（decorations:false），
// 拖动 / 双击最大化靠根元素上的 data-tauri-drag-region="deep"（Tauri 注入的 drag.js），
// 三个按钮则走 @tauri-apps/api 的窗口命令，权限见 capabilities/default.json。
// isTauri() 兜底：直接用浏览器打开 vite dev 页面看样式时按钮退化为无操作，而不是整页白屏。
// 按钮一律 @mousedown.prevent：Chromium 点击会把焦点留在按钮上，窗口隐藏再 show 时
// 焦点被原样还原，面板一打开就「选中」上次的关闭/最小化按钮（回车还会再触发一次）。
const appWindow = isTauri() ? getCurrentWindow() : null;

// 第三个按钮的图标要在「最大化 / 向下还原」之间切：双击标题栏、Win+↑、拖到屏幕顶部贴靠
// 都会改窗口状态，统一在 resize 事件里回读，不自己猜状态。
const maximized = ref(false);
let unlisten: UnlistenFn | null = null;

async function syncMaximized() {
  try {
    maximized.value = (await appWindow?.isMaximized()) ?? false;
  } catch (e) {
    // 窗口可能已被销毁（应用退出中）；吞掉以免 onResized 回调里冒出未处理 rejection
    console.warn("读取最大化状态失败：", e);
  }
}

function minimize() {
  void appWindow?.minimize().catch((e) => console.warn("最小化失败：", e));
}

function toggleMaximize() {
  void appWindow?.toggleMaximize().catch((e) => console.warn("切换最大化失败：", e));
}

// 关闭 → 只隐藏到托盘（真正的退出走托盘菜单），由 Rust 侧的 CloseRequested 拦下
function close() {
  void appWindow?.close().catch((e) => console.warn("关闭窗口失败：", e));
}

// ---- 深浅主题切换（原在设置页标题行，移到窗口按钮最左侧）----
// 偏好存 localStorage（见 theme.ts）；图标/提示跟随当前实际主题，
// 本窗口切换、系统主题变化、其它窗口改主题都经 theme.ts 的 apply() → onThemeChange 同步到这里
const theme = ref<{ theme: Theme; pref: ThemePref }>({ theme: resolvedTheme(), pref: themePref() });

const themeTip = computed(() => {
  const cur = theme.value.theme === "dark" ? "深色" : "浅色";
  const following = theme.value.pref === "system" ? "（跟随系统）" : "";
  return `当前${cur}${following} · 点击切换到${theme.value.theme === "dark" ? "浅色" : "深色"}`;
});

function onToggleTheme() {
  // setThemePref → apply() 会同步回调 onThemeChange，状态刷新由订阅完成
  toggleTheme();
}

let unlistenTheme: (() => void) | null = null;

onMounted(async () => {
  await syncMaximized();
  unlisten = (await appWindow?.onResized(() => void syncMaximized())) ?? null;
  unlistenTheme = onThemeChange((s) => {
    theme.value = s;
  });
});

onUnmounted(() => {
  unlisten?.();
  unlistenTheme?.();
});
</script>

<template>
  <header class="titlebar" data-tauri-drag-region="deep">
    <div class="titlebar-brand">
      <img class="titlebar-logo" :src="logo" alt="" />
      <span class="titlebar-name">Agent Bark</span>
    </div>

    <div class="titlebar-controls">
      <button type="button" class="titlebar-btn theme-btn" v-tooltip="themeTip" :aria-label="themeTip" @mousedown.prevent @click="onToggleTheme">
        <svg class="titlebar-icon" viewBox="0 0 16 16" aria-hidden="true">
          <path
            fill-rule="evenodd"
            d="M8 1.00195C6.61553 1.00195 5.26216 1.4125 4.11101 2.18167C2.95987 2.95084 2.06266 4.04409 1.53285 5.32317C1.00303 6.60225 0.86441 8.00972 1.13451 9.36759C1.4046 10.7255 2.07129 11.9727 3.05026 12.9517C4.02922 13.9307 5.27651 14.5974 6.63437 14.8675C7.99224 15.1375 9.3997 14.9989 10.6788 14.4691C11.9579 13.9393 13.0511 13.0421 13.8203 11.8909C14.5895 10.7398 15 9.38642 15 8.00195C15 6.14544 14.2625 4.36496 12.9498 3.05221C11.637 1.73945 9.85652 1.00195 8 1.00195ZM8 14.002V2.00195C9.5913 2.00195 11.1174 2.63409 12.2426 3.75931C13.3679 4.88453 14 6.41065 14 8.00195C14 9.59325 13.3679 11.1194 12.2426 12.2446C11.1174 13.3698 9.5913 14.002 8 14.002Z"
          />
        </svg>
      </button>

      <button type="button" class="titlebar-btn" v-tooltip="'最小化'" aria-label="最小化" @mousedown.prevent @click="minimize">
        <svg class="titlebar-icon" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M3 8.5h10" />
        </svg>
      </button>

      <button
        type="button"
        class="titlebar-btn"
        v-tooltip="maximized ? '向下还原' : '最大化'"
        :aria-label="maximized ? '向下还原' : '最大化'"
        @mousedown.prevent
        @click="toggleMaximize"
      >
        <svg v-if="maximized" class="titlebar-icon" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M10.5 3.5H3.5v7" />
          <rect x="5.5" y="5.5" width="7" height="7" />
        </svg>
        <svg v-else class="titlebar-icon" viewBox="0 0 16 16" aria-hidden="true">
          <rect x="3.5" y="3.5" width="9" height="9" />
        </svg>
      </button>

      <button type="button" class="titlebar-btn close" v-tooltip="'关闭'" aria-label="关闭" @mousedown.prevent @click="close">
        <svg class="titlebar-icon" viewBox="0 0 16 16" aria-hidden="true">
          <path d="M3.5 3.5l9 9M12.5 3.5l-9 9" />
        </svg>
      </button>
    </div>
  </header>
</template>
