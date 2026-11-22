import { createApp } from "vue";
import { invoke, isTauri } from "@tauri-apps/api/core";
import App from "./App.vue";
import { vTooltip } from "./tooltip";
import { initTheme, onThemeChange, resolvedTheme } from "./theme";
import { guardStaleHover } from "./hover";
import { i18n, initI18n } from "./i18n";
import "./style.css";

// 主题先落地（首帧已由 index.html 的内联脚本定色，这里补上系统变化与跨窗口同步的监听）
initTheme();

// Windows 无边框窗口的 DWM 默认描边是黑线：按主题同步成「浅色深一档 / 深色浅一档」。
// macOS 带原生红绿灯的窗口边框由系统绘制（公开 API 改不了色），命令在那边是空操作。
// initTheme 的首次 apply 已发过通知，这里订阅之外再补一次当前值，保证启动即落地。
if (isTauri()) {
  const syncBorder = (theme: string) => {
    void invoke("set_window_border_color", { theme }).catch((e) => console.warn("同步窗口描边失败：", e));
  };
  syncBorder(resolvedTheme());
  onThemeChange((s) => syncBorder(s.theme));
}

// 隐藏再显示后 ：hover 残留的挡板（见模块注释）
guardStaleHover();

// 界面语言必须在挂载前定好（Rust 是唯一解析点）：不等的话英文用户
// 会先看到一帧中文。主窗口默认隐藏，这一趟 IPC 用户看不见。
// 用异步 IIFE 而非顶层 await：构建目标（vite target）不含 es2022，
// 顶层 await 会吃 TOLERATED_TRANSFORM 警告（widget.ts 同款理由）。
void (async () => {
  await initI18n();
  createApp(App).use(i18n).directive("tooltip", vTooltip).mount("#app");
})();
