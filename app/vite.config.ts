import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "es2021",
    // 五个入口：主窗口（index.html）、屏幕边缘流光覆盖层（glow.html）、
    // 桌面悬浮窗（widget.html）、悬浮窗行详情（tip.html）、托盘菜单（menu.html）。
    // 后四者由 Rust 侧 WebviewUrl::App 加载，不加进 input 会被 Vite 当成无人引用的资源而根本不产出。
    rollupOptions: {
      input: {
        main: "index.html",
        glow: "glow.html",
        widget: "widget.html",
        tip: "tip.html",
        menu: "menu.html",
      },
    },
  },
});
