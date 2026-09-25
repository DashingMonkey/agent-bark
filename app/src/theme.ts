/**
 * 深浅主题。
 *
 * 偏好存 localStorage（三态：`system` / `dark` / `light`），**不放 config.json**：
 * 主面板 / 悬浮窗 / 托盘菜单是三个独立 webview，都要跟着换色，而 localStorage
 * 同源共享、且能在首帧之前同步读到。放配置文件就得等 IPC 回来才知道选的是哪个，
 * 每次启动都会先闪一下另一种主题——悬浮窗是常驻窗口，闪一下尤其明显。
 *
 * 生效方式：给 `<html>` 打 `data-theme="dark|light"`。样式里只有两块
 * （`:root` 深色 / `:root[data-theme="light"]` 浅色），媒体查询不再参与配色——
 * 「跟随系统」由这里的 matchMedia 解析成具体值，否则「系统浅色 + 用户选深色」
 * 时媒体查询和显式选择会互相打架（媒体查询里的浅色变量会盖掉显式深色）。
 *
 * 首帧定色另有一份等价逻辑，内联在 index.html / widget.html / menu.html 的 head 里
 * （见那里的注释）：它必须在样式与模块之前同步执行，所以不能 import 这个文件。
 */

export type ThemePref = "system" | "dark" | "light";
export type Theme = "dark" | "light";

/** localStorage 键：改了要同步改**四处**复制——index.html / menu.html / widget.html
 *  三份内联首帧脚本 + 本文件（readPref / setThemePref），漏一处就会出现「首帧一个色、
 *  落地另一个色」 */
const KEY = "agent-bark.theme";

const lightMedia = window.matchMedia("(prefers-color-scheme: light)");

function readPref(): ThemePref {
  // localStorage 读取要防御：隐私模式 / 被禁用 / 配额异常都会抛错，
  // 而这段跑在模块加载期，抛出去就是整个页面白屏——异常一律回退「跟随系统」
  try {
    const v = localStorage.getItem(KEY);
    return v === "dark" || v === "light" ? v : "system";
  } catch (e) {
    console.warn("读取主题偏好失败，按跟随系统处理：", e);
    return "system";
  }
}

let pref: ThemePref = readPref();
let watching = false;

export function themePref(): ThemePref {
  return pref;
}

/** 主题快照：实际显示的主题 + 用户偏好（订阅回调与显示提示文案共用） */
export type ThemeState = { theme: Theme; pref: ThemePref };

const listeners = new Set<(s: ThemeState) => void>();

/**
 * 订阅主题变化，返回解绑函数。
 * 本窗口切换、系统主题变化（`system` 偏好时）与其它窗口改主题（storage 事件）
 * 最终都走 apply()，在这里统一通知，订阅方不用自己重复挂那几路监听。
 */
export function onThemeChange(cb: (s: ThemeState) => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function notify() {
  const snapshot: ThemeState = { theme: resolvedTheme(), pref };
  for (const cb of listeners) cb(snapshot);
}

/** 偏好 → 实际该用的主题（`system` 时问系统） */
export function resolvedTheme(p: ThemePref = pref): Theme {
  if (p === "system") return lightMedia.matches ? "light" : "dark";
  return p;
}

/**
 * 切主题的那一两帧临时压掉所有过渡。
 *
 * 界面上不少元素挂着过渡（导航项 .15s、按钮 .12s、开关轨道 .15s…），而它们的颜色
 * 大多也随主题变。不压掉的话：卡片已经瞬变，这些元素还在慢慢渐变过去，看着像
 * "点了没反应、过一会儿才跟上"。双 rAF 后撤掉——等新主题算完并画出来再撤，
 * 撤早了过渡会又接上。
 */
let transitionGuard: HTMLStyleElement | null = null;

function suppressTransitionsOnce() {
  if (!transitionGuard) {
    transitionGuard = document.createElement("style");
    transitionGuard.textContent = "*,*::before,*::after{transition:none !important}";
    document.head.appendChild(transitionGuard);
  }
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      transitionGuard?.remove();
      transitionGuard = null;
    });
  });
}

/** 把主题写到 `<html>` 上；原生控件（滚动条 / 下拉弹层 / 时间选择器）靠 color-scheme 跟随 */
function apply() {
  const t = resolvedTheme();
  document.documentElement.dataset.theme = t;
  document.documentElement.style.colorScheme = t;
  suppressTransitionsOnce();
  notify();
}

export function setThemePref(next: ThemePref) {
  pref = next;
  // 先 apply() 让本窗口当场换色，再写存储：写入失败只丢跨窗口同步
  // （storage 事件），不能让切主题「点了没反应」
  apply();
  try {
    // 删掉键而不是存 "system"：内联脚本只认 dark/light，缺键即跟随系统
    if (next === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, next);
  } catch (e) {
    console.warn("保存主题偏好失败（本次切换仍生效，重启后回到跟随系统）：", e);
  }
}

/** 深浅切换按钮用：按**当前实际显示**的主题取反，返回切换后的主题 */
export function toggleTheme(): Theme {
  const next: Theme = resolvedTheme() === "dark" ? "light" : "dark";
  setThemePref(next);
  return next;
}

/**
 * 启动时调用一次：应用当前偏好，并挂上两个监听——
 * 系统主题变化（仅 `system` 偏好时跟随）与其它窗口改主题（storage 事件跨窗口同步，
 * 主面板点一下切换，常驻的悬浮窗当场换色）。
 */
export function initTheme() {
  apply();
  if (watching) return;
  watching = true;
  lightMedia.addEventListener("change", () => {
    if (pref === "system") apply();
  });
  window.addEventListener("storage", (e) => {
    // key 为 null 表示整个 storage 被清空
    if (e.key !== null && e.key !== KEY) return;
    pref = readPref();
    apply();
  });
}
