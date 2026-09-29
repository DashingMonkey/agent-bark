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
  /** 全屏特效类型（#burst 的 data-effect）："fog"（雾散）| "scan"（HUD 扫描）| "rain"（矩阵雨） */
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
  // 矩阵雨画在 canvas 上，CSS 重放掐不掉它——任何新触发都先收掉上一场雨
  stopRain();
  burstEl.style.setProperty("--burst", `${ms}ms`);
  burstEl.classList.remove("on");
  void burstEl.offsetWidth;
  burstEl.classList.add("on");
}

// ---------------------------------------------------------------------------
// 全屏特效（矩阵雨）：canvas 引擎，移植自 doc/glitch-designs.html 的 06
// ---------------------------------------------------------------------------

/** 正在播的矩阵雨的 RAF 句柄：同一时刻最多一场雨 */
let rainRaf = 0;

/** 收掉正在播的矩阵雨并清屏（雨是画在 canvas 上的，不清会冻住最后一帧） */
function stopRain() {
  if (rainRaf) {
    cancelAnimationFrame(rainRaf);
    rainRaf = 0;
  }
  const cv = burstEl.querySelector<HTMLCanvasElement>(".burst-rain");
  const ctx = cv?.getContext("2d");
  if (ctx && cv) {
    // 播放时 ctx 带 dpr 变换，先归一再按画布物理尺寸整面清掉
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, cv.width, cv.height);
  }
}

/**
 * 播一场矩阵雨（`ms` = payload.burst_ms，全部时序按它等比伸缩）。
 *
 * 首尾都不做硬切、也不做整体淡出：
 * - 开局整屏是空的，各列在进场窗口内错开出生、**从顶部逐列落下**（级联进场）；
 * - 收尾靠「预算」：每次补雨按剩余时间反推最低速度，赶在收尾前落不完的列
 *   不再补雨，存量雨各自落到屏底自然离场，最后一滴约在 ms 前 100ms 出画。
 *
 * 相比设计稿 playRain 仍保留的修正：
 * 1. 设计稿在补雨窗口（2000ms）后雨滴**冻结在原地**只做淡出，可见降雨只有
 *    2 秒——这里全程持续下落，整场雨铺满 burst_ms；
 * 2. 设计稿按「每帧固定像素」步进（60fps 基准），120Hz 高刷屏雨速直接翻倍
 *    ——这里按 dt（ms）时间步进，帧率无关。
 */
function playRain(ms: number, rgb: string) {
  const cv = burstEl.querySelector<HTMLCanvasElement>(".burst-rain");
  if (!cv || ms <= 0) return;
  stopRain();
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const W = cv.clientWidth;
  const H = cv.clientHeight;
  if (W <= 0 || H <= 0) return;
  cv.width = Math.round(W * dpr);
  cv.height = Math.round(H * dpr);
  const ctx = cv.getContext("2d");
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

  // 尺度归一：1 ≈ 1080p 整屏；字号随尺度等比（沿用设计稿公式）
  const u = Math.min(W / 1920, H / 1080);
  const fs = Math.max(11, (17 * Math.sqrt(u * 2.2)) | 0);
  const cw = fs * 1.1;
  const cols = Math.ceil(W / cw);
  ctx.font = `bold ${fs}px Consolas, "Courier New", monospace`;
  ctx.textBaseline = "top";

  const GLYPHS = "ｱｲｳｴｵｶｷｸｹｺｻｼｽｾｿﾀﾁﾂﾃﾄﾅﾆﾇﾈﾉﾊﾋﾌﾍﾎﾏﾐﾑﾒﾓ0123456789ABCDEF";
  const [r, g, b] = rgb.split(",").map((s) => parseInt(s, 10));
  // 尾迹色按 alpha 分 20 档预生成：设计稿逐字符拼 rgba 字符串，整屏上千次/帧
  const TRAIL_STEPS = 20;
  const trail: string[] = [];
  for (let i = 0; i < TRAIL_STEPS; i++) {
    trail.push(`rgba(${r},${g},${b},${(i / TRAIL_STEPS).toFixed(3)})`);
  }
  const trailIdx = (a: number) =>
    Math.min(TRAIL_STEPS - 1, Math.max(0, Math.round(a * TRAIL_STEPS)));

  // 自然速度档（px/ms，时间口径，帧率无关）：0.21~0.54 × 尺度
  const speedScale = Math.max(0.55, u * 2.4);
  const spMax = 0.54 * speedScale;
  // 开局级联窗口：各列在窗口内错开出生（~百列随机错开不会出现可感知的空档）
  const ENTRY = Math.max(400, Math.min(800, ms * 0.22));
  // 收尾安全余量：所有雨滴保证在 ms-100ms 前落出屏底（帧量化 ~16ms 的富余）
  const SAFETY = 100;

  // 出生参数（tS = 出生时刻）。按「剩余时间」反推最低速度 need（绝对 px/ms）：
  // 比自然档上限还高出一截仍赶不上的雨滴不生（sp=0，该列就此收场）——这条预算
  // 约束同时就是收尾编排：越晚出生被迫越快，最后一批各自冲出屏底，整屏自然落空。
  const mkDrop = (x: number, tS: number) => {
    const len = 6 + ((Math.random() * 13) | 0);
    const y0 = -Math.random() * 0.12 * H; // 出生在屏上方，滑入画面
    const budget = ms - SAFETY - tS;
    const need = budget <= 0 ? Infinity : (H + (len + 1) * fs - y0) / budget;
    // 自然速度先按尺度缩放成绝对 px/ms，再与 need 取 max（need 已是绝对口径，
    // 不能再乘一次尺度）
    const sp =
      need > spMax * 1.15
        ? 0
        : Math.max((0.21 + Math.random() * 0.33) * speedScale, need);
    return { x, y: y0, sp, len, enter: tS, ch: (Math.random() * GLYPHS.length) | 0 };
  };
  const drops: ReturnType<typeof mkDrop>[] = [];
  for (let i = 0; i < cols; i++) {
    drops.push(mkDrop((i * cw) | 0, Math.random() * ENTRY));
  }

  // 时间轴锚在**第一帧真正渲染**的 vsync 上，而不是脚本执行时刻：首次预览要
  // 现建特效窗 + 加载页面，脚本跑完到首帧可能差几百毫秒——挂在脚本时刻的话，
  // 首帧一亮相级联窗口（enter）已全部过期、黑幕也已升到全值，观感就是「整排
  // 雨头和黑幕一起蹦出来」。CSS 款特效（雾散 / 扫描）的动画起点本来就在首帧
  // 渲染，这里对齐同一口径。
  let t0 = 0;
  let last = 0;
  const tick = (frameTs: number) => {
    if (!t0) {
      t0 = frameTs;
      last = frameTs;
    }
    const t = frameTs - t0;
    // 切后台 rAF 会被冻结：dt 钳到 50ms，醒来那帧不会瞬移一大截
    const dt = Math.min(50, frameTs - last);
    last = frameTs;
    ctx.clearRect(0, 0, W, H);
    // 极淡黑幕垫底（自己的软入软出包络）：浅色桌面上白炽雨头不至于消失——
    // 设计稿页脚的「静态暗晕垫层」手法，画进 canvas 免去多一层 DOM
    const veil = Math.min(1, t / 400) * (t > ms - 700 ? Math.max(0, (ms - t) / 700) : 1);
    if (veil > 0.01) {
      ctx.fillStyle = "#000";
      ctx.globalAlpha = 0.12 * veil;
      ctx.fillRect(0, 0, W, H);
      ctx.globalAlpha = 1;
    }
    for (const d of drops) {
      if (d.sp === 0) continue; // 死列：赶不上收尾预算，就此收场
      if (t < d.enter) continue; // 开局级联：还没轮到这列
      d.y += d.sp * dt;
      // 整滴（含尾）落出屏底：预算还够就在顶部重生，否则标记死列
      if (d.y - d.len * fs > H + fs) {
        const nd = mkDrop(d.x, t);
        if (nd.sp === 0) {
          d.sp = 0;
          continue;
        }
        Object.assign(d, nd);
      }
      for (let k = 0; k < d.len; k++) {
        const y = d.y - k * fs;
        if (y < -fs || y > H) continue;
        if (k === 0) {
          ctx.fillStyle = "#fff";
        } else {
          ctx.fillStyle = trail[trailIdx(0.9 * (1 - k / d.len))];
        }
        // 字符约每 90ms 换一批，雨身保持「数据在变」的质感
        const gi = (d.ch + k * 7 + ((t / 90) | 0) * 3) % GLYPHS.length;
        ctx.fillText(GLYPHS[gi], d.x, y | 0);
      }
    }
    if (t < ms) {
      rainRaf = requestAnimationFrame(tick);
    } else {
      // 跑完即停：清屏收场，平时零帧开销
      rainRaf = 0;
      ctx.clearRect(0, 0, W, H);
    }
  };
  rainRaf = requestAnimationFrame(tick);
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
  // 全屏特效类型：标记到 burst 元素上，CSS 按 data-effect 分派
  // （fog=雾散 / scan=HUD 扫描 / rain=矩阵雨走 canvas，见 playRain）
  burstEl.dataset.effect = p.fullscreen_effect;

  // 全屏特效只在**新的一次触发**时放：状态没变的重发（终态重发、窗口重建后的
  // 对齐推送）不该让整屏再闪一遍。
  const isNewTrigger = p.burst !== lastBurst;
  lastBurst = p.burst;
  if (p.fullscreen && isNewTrigger && p.state !== "idle") {
    playBurst(p.burst_ms);
    // 矩阵雨的绘制在 canvas 引擎里，.on 只负责亮层（逐列级联进场 / 逐列落底
    // 收场由 playRain 自己编排，不靠 CSS 动画）
    if (p.fullscreen_effect === "rain") {
      playRain(p.burst_ms, rgbTriplet(p.burst_color || p.color));
    }
  }

  window.clearTimeout(hideTimer);
  if (p.state === "idle") {
    root.classList.remove("on");
    burstEl.classList.remove("on");
    stopRain();
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
