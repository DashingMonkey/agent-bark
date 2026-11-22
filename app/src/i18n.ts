/**
 * 界面语言（中/英）。
 *
 * 偏好与解析都在 Rust 侧（config.json 顶层 `language` + bark-core/src/i18n.rs 的
 * sys-locale 解析），前端启动时经 `get_locale` 命令拿一次解析结果。不走
 * `navigator.language`：Tauri 各 webview 的检测不可靠且可能互相不一致，
 * 悬浮窗与推送通知必须拿同一个语言。
 *
 * 主窗口是 Vue 应用（`app.use(i18n)`，模板里直接 `$t`，脚本里 `useI18n()`）；
 * 悬浮窗 / 托盘菜单是原生 TS 入口，直接 import 本文件的 `t`，并在文案填充后
 * 订阅 [`onLocaleChanged`] —— 设置页切换语言时 Rust 广播 `bark://locale-changed`，
 * Vue 侧由响应式自动重渲染，原生入口靠这里的监听重跑各自的文案填充函数。
 *
 * 词表在 locales/zh-CN.json 与 en.json，两份键位必须完全一致（typecheck 没法查
 * JSON 内容，靠 Phase 验收时对照）。顶层命名空间按界面划分：主窗口各页 / 悬浮窗
 * / 托盘菜单各占一段，新增 key 先看注释里的归属。
 */

import { createI18n } from "vue-i18n";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import zhCN from "./locales/zh-CN.json";
import en from "./locales/en.json";

/** 与 Rust `Lang::id()` 与词表文件的语言键一致的两种取值 */
export type LocaleId = "zh" | "en";

export const i18n = createI18n({
  // Composition 模式：i18n.global 是 Composer（t 可直接给原生 TS 用），
  // globalInjection 让模板里不写 useI18n 也能用 $t
  legacy: false,
  globalInjection: true,
  // 默认中文：get_locale 返回前的一瞬（理论上被下面的 await 挡住，见 initI18n）
  // 与 get_locale 调用失败时的兜底
  locale: "zh",
  fallbackLocale: "zh",
  messages: { zh: zhCN, en },
});

/** 原生 TS 入口（悬浮窗 / 托盘菜单）用的快捷翻译函数 */
export const t = i18n.global.t;

export function locale(): LocaleId {
  return i18n.global.locale.value as LocaleId;
}

// ---- 原生入口的语言变化订阅（Vue 侧靠响应式，不走这里） --------------------

const listeners = new Set<() => void>();

/** 订阅语言切换（含其它窗口切语言后广播过来的），返回解绑函数 */
export function onLocaleChanged(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function applyLocale(id: LocaleId) {
  if (locale() === id) return;
  i18n.global.locale.value = id;
  // <html lang> 跟随：影响字体选择与系统合成语音的读法
  document.documentElement.lang = id === "zh" ? "zh-CN" : "en";
  for (const cb of listeners) cb();
}

let initPromise: Promise<void> | null = null;

/**
 * 启动时调用一次：拿解析后的语言并挂上跨窗口切换监听。
 *
 * **必须在渲染任何文案之前 await**（main.ts 在 mount 前 await；悬浮窗 / 托盘菜单
 * 在首个文案填充前 await）——窗口都是先加载后显示，这一趟 IPC 的延迟用户看不见；
 * 不等的话英文用户会先看到一帧中文。重复调用只初始化一次（返回同一个 Promise）。
 */
export function initI18n(): Promise<void> {
  initPromise ??= (async () => {
    try {
      const id = await invoke<string>("get_locale");
      if (id === "zh" || id === "en") applyLocale(id);
    } catch (e) {
      console.warn("获取界面语言失败，按中文兜底：", e);
    }
    try {
      // 设置页切语言时 Rust 对全部窗口广播（commands.rs save_config）
      await listen<LocaleId>("bark://locale-changed", (e) => {
        if (e.payload === "zh" || e.payload === "en") applyLocale(e.payload);
      });
    } catch (e) {
      console.warn("界面语言切换监听失败（仅影响跨窗口实时切换）：", e);
    }
  })();
  return initPromise;
}
