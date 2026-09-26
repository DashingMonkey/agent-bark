// 屏幕光效覆盖层的渲染逻辑（对应 app/glow.html）。
//
// 这是一个独立入口（与主窗口的 Vue 应用无关），只做一件事：
// 收 Rust 侧推来的 GlowPayload，翻译成 CSS 变量 + data-* 属性。
//
// **颜色不在这里定义**：payload 的 color / burst_color 来自 `src-tauri/src/glow.rs`
// 的 `GlowState::color`（唯一来源）。本文件里的 hex 只是坏色值时的兜底，
// 状态一律用角色名（思考 / 等待 / 完成 / 失败）；改配色时同步下面的兜底分量。

import { listen } from "@tauri-apps/api/event";

export type GlowState = "idle" | "running" | "waiting" | "completed" | "failed";

/** 边缘光效类型（glow.html 的 data-effect）：呼吸 / 流光 */
export type GlowEffect = "breathing" | "comet";

export interface GlowPayload {
  state: GlowState;
  color: string;
  /** 边缘光效：是否显示边缘灯带（关掉后全屏特效照常，两层各自独立） */
  edge: boolean;
  /** 边缘光效类型（glow.html 的 data-effect） */
  effect: GlowEffect;
  /** 边缘光效位置："top"=只画顶部（重绘成顶边一条横条，窗口本身也是顶部条带） */
  sides: "top" | "all";
  /**
   * 本次下发要不要让边缘灯带「熄灭重放」（掐掉过渡立即熄一帧再重新淡入）。
   * 只有设置页预览点按为 true——连点预览时第二下要看得见是新的一次播放；
   * 真实事件 / 预览恢复 / 页面就绪补发为 false（平滑换色 / 幂等重推）
   */
  restart_edge: boolean;
  /** 灯效速度倍率（已格式化的 CSS 值）：CSS 里写成 `calc(2400ms / var(--speed))` */
  speed: string;
  /** 辉光强度倍率（已格式化的 CSS 值）：CSS 里写成 `calc(参考半径 * var(--glow))` */
  intensity: string;
  /** 停留时长（ms），0 = 常驻直到下一个状态覆盖 */
  hold_ms: number;
  width: number;
  radius: number;
  opacity: number;
  /** 是否启用全屏特效 */
  fullscreen: boolean;
  /** 全屏特效类型（#burst 的 data-effect）："fog"（雾散）| "scan"（HUD 扫描） */
  fullscreen_effect: string;
  /** 本次颜色特效的触发序号（Rust 侧单调递增） */
  burst: number;
  /** 一次全屏特效的时长（ms） */
  burst_ms: number;
  /** 全屏特效的颜色，与边缘色解耦：多会话并行时边缘保持思考色，全屏按事件角色补放 */
  burst_color: string;
}

declare global {
  interface Window {
    /** 建窗时由 Rust 的 initialization_script 注入的初始状态 */
    __GLOW_INIT__?: GlowPayload;
    /**
     * 这份初始态只是**重建窗口时的状态快照**（显示器数量变化后 `align` 的
     * destroy + create），不是一次新的颜色触发：序号要直接同步掉，不许重放全屏雾散。
     * 懒创建（首次建窗）时不带这个标记——那次建窗本身就是颜色触发的产物。
     */
    __GLOW_INIT_SNAPSHOT__?: boolean;
  }
}

const root = document.getElementById("glow") as HTMLElement;
const burstEl = document.getElementById("burst") as HTMLElement;
let hideTimer: number | undefined;
/** 上一次放过的触发序号：用来区分「新的一次颜色特效」和「同一状态的重复推送」 */
let lastBurst = 0;
/** 最近一次 payload：分辨率变化时窗口只被 resize、不会重推，线宽靠它重算 */
let lastPayload: GlowPayload | undefined;
/** 线宽参考口径：payload.width 按 1080p 屏高校准，各窗口按自身高度等比换算 */
const WIDTH_REF_HEIGHT = 1080;

/**
 * 本窗口首帧绑定的通道形态（sides），之后只接受同形态的 payload。
 *
 * 为什么必须锁：实测（2026-09-26）存在**跨窗口投递错位**——特效窗的
 * burst_view（side=all、edge=0）被投进条带窗口，`data-edge` 随之置 off、灯条
 * 整层 display:none，表现就是「灯条出现一下就消失」。顶部模式下条带（side=top）
 * 与特效窗（side=all）恰好按 sides 一分为二，按它锁通道后任何错投都变成无害
 * 空转；四周模式所有 payload 恒为 side=all，本守卫永不触发，行为一字不变。
 */
let boundSides: "top" | "all" | undefined;

/**
 * 流光线宽 = payload.width × (本屏逻辑高度 / 1080)。
 *
 * 每块屏一个窗口、各算各的：4K 上细线等比变粗，与屏高保持同一比例；混合
 * 分辨率的多屏各自成立。DPI 缩放不用另算——逻辑高度里已经体现了（150% 屏的
 * innerHeight 是物理高 ÷ 1.5）。取整到整数逻辑 px，避免半像素糊线。
 *
 * 参考高度的口径按「位置」分两档：四周窗口盖满整屏，innerHeight 就是屏高
 * （含 rect_of 底边外扩的那 1px，现状口径不变）；「顶部」的窗口只有 150px
 * 条带高，innerHeight 不能用，改取 window.screen.height（= 该屏逻辑高度）。
 * 因此本函数必须在 render 里 dataset.sides 写好之后调用。
 */
function applyWidth(p: GlowPayload) {
  const refH =
    root.dataset.sides === "top" ? window.screen.height || window.innerHeight : window.innerHeight;
  const w = Math.max(1, Math.round((p.width * refH) / WIDTH_REF_HEIGHT));
  root.style.setProperty("--w", `${w}px`);
}

/**
 * `#rrggbb` → `"r, g, b"`。
 *
 * 必须是**逗号分隔**：样式里用的是 `rgba(var(--rgb), .5)` 这种传统写法，
 * 而 `rgba(255 204 0, .5)` 是非法语法（空格分隔的分量必须配 `/` 再接 alpha），
 * 一旦写成空格，整条声明会被浏览器丢掉，特效会静默消失。
 *
 * 坏色值的兜底是**思考色的分量**（与 glow.rs 的 `color()` 保持同值）：让整层特效
 * 宁可错色也不要整块消失。
 */
function rgbTriplet(hex: string): string {
  const m = /^#?([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) return "68, 147, 248";
  let h = m[1];
  if (h.length === 3) {
    h = h
      .split("")
      .map((c) => c + c)
      .join("");
  }
  const n = parseInt(h, 16);
  return `${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}`;
}

/**
 * 补放一次全屏特效。
 *
 * 必须先摘 class 并强制回流：连续两次触发之间只改 class 的话，浏览器会认为
 * 动画名没变而不重启动画，第二次（甚至后面每一次）就都看不见了。
 */
function playBurst(ms: number) {
  burstEl.style.setProperty("--burst", `${ms}ms`);
  burstEl.classList.remove("on");
  void burstEl.offsetWidth;
  burstEl.classList.add("on");
}

function render(p: GlowPayload) {
  // 过期/乱序的推送直接丢弃（序号倒退 = 不是最新一次触发，比如补发撞上后发的
  // 状态）：宁可丢弃也不许把旧状态盖回去——旧状态带着旧的停留计时，会把刚点亮
  // 的灯条掐掉。序号不变的重推（终态重发、预览恢复、页面就绪补发）照常处理。
  if (p.burst < lastBurst) return;
  // 通道锁（见 boundSides 注释）：本窗口只认自己通道的 payload
  if (boundSides === undefined) {
    boundSides = p.sides;
  } else if (p.sides !== boundSides) {
    return;
  }
  root.style.setProperty("--c", p.color);
  root.style.setProperty("--rgb", rgbTriplet(p.color));
  // 全屏特效走独立的颜色通道：多会话并行时，边缘保持思考色持续呼吸，
  // 全屏按事件角色（完成 / 失败）补放——两层互不覆盖。
  // burst_color 缺失时回退边缘色（等价于状态切换触发的同色全屏）。
  burstEl.style.setProperty("--burst-rgb", rgbTriplet(p.burst_color || p.color));
  lastPayload = p;
  root.style.setProperty("--r", `${p.radius}px`);
  root.style.setProperty("--op", String(p.opacity));
  root.style.setProperty("--speed", p.speed);
  root.style.setProperty("--glow", p.intensity);
  root.dataset.state = p.state;
  root.dataset.effect = p.effect;
  // 边缘灯带显隐是独立开关（设置页「边缘光效」）：按 data-edge 只收起灯带层。
  // 不能用不加 .on 的方式实现——#burst 在 #glow 里，整层压暗会把全屏雾散一起压掉。
  root.dataset.edge = p.edge ? "on" : "off";
  // 位置（设置页「位置」单选）：CSS 按 data-sides 走顶部横条的独立渲染路径
  // （"top"，重绘成顶边一条线，不是四周渲染的裁剪）；"all"（四周）不命中
  // 任何新规则，渲染路径与旧版完全一致
  root.dataset.sides = p.sides;
  // 线宽的参考口径依赖刚写好的 data-sides（见 applyWidth 注释），必须在它之后调用
  applyWidth(p);
  // 全屏特效类型：标记到 burst 元素上，CSS 按 data-effect 分派（fog=雾散 / scan=HUD 扫描）
  burstEl.dataset.effect = p.fullscreen_effect;

  // 全屏特效只在**新的一次触发**时放：状态没变的重发（终态重发、窗口重建后的
  // 对齐推送）不该让整屏再闪一遍。
  const isNewTrigger = p.burst !== lastBurst;
  lastBurst = p.burst;
  if (p.fullscreen && isNewTrigger && p.state !== "idle") {
    playBurst(p.burst_ms);
  }

  window.clearTimeout(hideTimer);
  if (p.state === "idle") {
    root.classList.remove("on");
    burstEl.classList.remove("on");
    return;
  }
  /*
    顶部条带：预览点按这类「熄灭重放」触发（restart_edge）要看得见地重新播放——
    连点第二下先把灯条熄掉一帧再重新淡入，否则同色重触发毫无反应，用户以为
    第二下没生效。实现必须是干净的「立即关 → 重新淡入」：先把 opacity 过渡掐掉
    让熄灭立即生效，恢复过渡后由下面的 add("on") 重新淡入。**不能**用「摘 class +
    强制回流 + 重挂」去反转进行中的过渡——WebView2 里反转不可靠，0.45s 的淡出
    会走完，灯停在灭的状态（实测「闪一下就没」）。
    只在 sides="top" + 新序号时动手：四周模式渲染路径不变，同序号补发（页面
    就绪补发）也不会把灯重新打断一次。
  */
  if (p.sides === "top" && p.restart_edge && isNewTrigger) {
    root.style.transition = "none";
    root.classList.remove("on");
    void root.offsetWidth;
    root.style.transition = "";
  }
  root.classList.add("on");
  // 终态（绿/红）到点自己淡出；运行中 / 等待中由下一个事件驱散，
  // 否则「还在跑但灯灭了」比不亮更糟。
  if (p.hold_ms > 0) {
    hideTimer = window.setTimeout(() => root.classList.remove("on"), p.hold_ms);
  }
}

// 页面加载完成的时刻可能晚于窗口创建：先消费注入的初始态，再挂监听
const init = window.__GLOW_INIT__;
if (init) {
  // 重建窗口注入的是快照：先把序号对齐，`render` 里就不会把这次注入当成新触发，
  // 也不会重放一次整屏雾散（颜色照常显示——还在跑的会话必须看得见）。
  if (window.__GLOW_INIT_SNAPSHOT__) lastBurst = init.burst;
  render(init);
}

listen<GlowPayload>("glow://state", (e) => render(e.payload)).catch((e: unknown) => {
  console.error("光效事件监听注册失败：", e);
});
// 显示器热插拔 / 分辨率变化时 Rust 侧 align 只改窗口尺寸、不重推 payload
// （不 emit 是为了不重放全屏特效），线宽得自己跟着窗口走：resize 后按新高度重算
window.addEventListener("resize", () => {
  if (lastPayload) applyWidth(lastPayload);
});
