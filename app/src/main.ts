import { createApp } from "vue";
import App from "./App.vue";
import { vTooltip } from "./tooltip";
import { initTheme } from "./theme";
import { guardStaleHover } from "./hover";
import "./style.css";

// 主题先落地（首帧已由 index.html 的内联脚本定色，这里补上系统变化与跨窗口同步的监听）
initTheme();

// 隐藏再显示后 ：hover 残留的挡板（见模块注释）
guardStaleHover();

createApp(App).directive("tooltip", vTooltip).mount("#app");
