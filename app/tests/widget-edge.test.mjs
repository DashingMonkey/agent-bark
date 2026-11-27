import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";
import { loadApi } from "./api-loader.mjs";

function compile(name, extra = "") {
  return ts.transpileModule(readFileSync(new URL(`../src/${name}.ts`, import.meta.url), "utf8") + extra, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
}
function module(code, imports = {}, context = {}) {
  const exports = {};
  runInNewContext(code, { exports, require: (id) => {
    assert.ok(id in imports, `Unexpected import: ${id}`);
    return imports[id];
  }, ...context });
  return exports;
}
const layout = module(compile("widget-layout"));
const edge = module(compile("widget-edge"), { "./widget-layout": layout });
const screen = { x: 0, y: 0, width: 1920, height: 1040 };

test("only top/left/right edges dock, with top taking precedence at corners", () => {
  for (const [point, expected] of [
    [{ x: 2, y: 2 }, "top"], [{ x: 2, y: 400 }, "left"],
    [{ x: 1918, y: 400 }, "right"], [{ x: 900, y: 1038 }, null],
    [{ x: 900, y: 50 }, null],
  ]) assert.equal(edge.edgeAtPoint(point, screen, [], 6), expected);
});

test("macOS widens the snap band to clear the menu-bar fullscreen gesture zone", () => {
  // 顶部菜单栏占约 25 逻辑 px，拖窗口顶到菜单栏停留会触发系统全屏手势并弹回；
  // macOS 的吸附意图信号是「鼠标贴边」（指针能精确贴到 x=0；卡片几何不行——
  // 拖动跟手时卡片边缘永远停在屏内几十像素），24px 对指针足够精确
  const macEdge = module(compile("widget-edge"), { "./widget-layout": layout }, {
    navigator: { userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15" },
  });
  assert.equal(macEdge.edgeThreshold(), 24);
  const winEdge = module(compile("widget-edge"), { "./widget-layout": layout }, {
    navigator: { userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" },
  });
  assert.equal(winEdge.edgeThreshold(), 6);
});

test("shared edges are ignored using physical coordinates across differently scaled monitors", () => {
  const left = { x: -2560, y: -300, width: 2560, height: 1440 };
  const above = { x: 0, y: -1080, width: 1920, height: 1080 };
  assert.equal(edge.edgeAtPoint({ x: 3, y: 600 }, screen, [left], 9), null);
  assert.equal(edge.edgeAtPoint({ x: 900, y: 3 }, screen, [above], 6), null);
  assert.equal(edge.edgeAtPoint({ x: -2555, y: 300 }, left, [screen], 9), "left");
  assert.equal(edge.edgeAtPoint({ x: -1280, y: -295 }, left, [screen], 9), "top");
  // A partially shared edge remains dockable where there is no neighboring screen.
  assert.equal(edge.edgeAtPoint({ x: 1918, y: 900 }, screen, [{ x: 1920, y: 0, width: 1920, height: 600 }], 6), "right");
});

test("every animation frame stays inside the work area and ends with a six-pixel handle", () => {
  for (const area of [screen, { x: -1707, y: -200, width: 1707, height: 920 }, { x: 60, y: 45, width: 1860, height: 995 }]) {
    for (const side of ["top", "left", "right"]) {
      const card = edge.dockRect({ x: area.x + 950, y: area.y + 850, width: 280, height: 235 }, area, side);
      for (const progress of [0, .25, .5, .75, 1]) {
        const frame = edge.edgeFrame(card, side, progress);
        assert.ok(frame.x >= area.x && frame.y >= area.y);
        assert.ok(frame.x + frame.width <= area.x + area.width);
        assert.ok(frame.y + frame.height <= area.y + area.height);
        if (progress === 1) assert.equal(side === "top" ? frame.height : frame.width, 6);
      }
    }
  }
});

// Run the real entry point with a deterministic DOM, clock and native-window boundary.
// This covers interactions between docking, temporary reveals, dragging and menus.
const widgetCode = compile("widget", `
export { requestEdge, saveCurrentPosition, syncWindowSize, openMenu, closeMenu };
export function snapshot() { return { dockSide, edgeProgress, edgeTarget, hidden, hovering, dragging, edgeMoving, menuOpen, dockOrigin }; }
`);
function setup(init = {}, options = {}) {
  let now = 0, nextTimer = 0, moved, endDrag, pendingReveal = !!options.pendingReveal;
  const timers = new Map(), listeners = new Map(), saves = [], calls = [], animations = [];
  const noop = () => {};
  function element() {
    const classes = new Set(), events = new Map();
    return {
      dataset: {}, style: { setProperty: noop }, children: [], innerHTML: "", hidden: false,
      animate(keyframes, timing) {
        const started = now;
        let resolve, reject;
        const animation = {
          finished: new Promise((yes, no) => { resolve = yes; reject = no; }),
          effect: { getComputedTiming: () => ({ progress: options.animationProgress ?? Math.min(1, (now - started) / timing.duration) }) },
          cancel() { window.clearTimeout(timer); reject(new Error("Animation cancelled")); },
        };
        const timer = window.setTimeout(resolve, timing.duration);
        animations.push({ target: this, keyframes, timing, animation });
        return animation;
      },
      offsetWidth: 154, offsetHeight: 180, clientWidth: 154, clientHeight: 44, scrollHeight: 44,
      classList: {
        add: (key) => classes.add(key), remove: (key) => classes.delete(key), contains: (key) => classes.has(key),
        toggle(key, value) { if (value ?? !classes.has(key)) classes.add(key); else classes.delete(key); },
      },
      append(...nodes) { this.children.push(...nodes); },
      insertBefore(node, before) { this.children = this.children.filter((n) => n !== node); const index = this.children.indexOf(before); this.children.splice(index < 0 ? this.children.length : index, 0, node); },
      addEventListener(key, fn) { events.set(key, [...(events.get(key) ?? []), fn]); },
      dispatch(key, event = {}) { return Promise.all((events.get(key) ?? []).map((fn) => fn(event))); },
      setAttribute: noop, remove: noop,
      querySelector: () => null,
      contains: () => false,
      getBoundingClientRect: () => ({ top: 0, bottom: 44, right: 280 }),
    };
  }
  const nodes = new Map();
  const document = Object.assign(element(), {
    body: element(), createElement: element,
    getElementById(id) { if (!nodes.has(id)) nodes.set(id, element()); return nodes.get(id); },
  });
  const initial = { x: 1640, y: 120, pinned: false, dock_side: "right", ...init };
  const window = {
    __WIDGET_INIT__: initial,
    setTimeout(fn, ms) { const id = ++nextTimer; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout: (id) => timers.delete(id),
    // 沙箱没有 macOS 的真实窗口环境，hover 轮询不启动
    setInterval: () => 0,
    clearInterval: () => {},
  };
  const hiddenDuringGeometry = [];
  const monitor = options.monitor ?? { position: { x: 0, y: 0 }, size: { width: 1920, height: 1080 }, workArea: { position: { x: 0, y: 0 }, size: { width: 1920, height: 1040 } }, scaleFactor: 1 };
  const native = {
    position: { x: initial.x, y: initial.y }, size: { width: 280, height: 44 }, pointer: { x: 800, y: 500 },
    outerPosition: async () => native.position, scaleFactor: async () => 1,
    async setSize(size) { hiddenDuringGeometry.push(document.body.classList.contains("edge-handle-hidden")); native.size = size; calls.push(["size", size.width, size.height]); },
    async setPosition(position) { hiddenDuringGeometry.push(document.body.classList.contains("edge-handle-hidden")); native.position = position; calls.push(["position", position.x, position.y]); moved?.({ payload: position }); },
    async show() { calls.push(["show"]); }, async hide() { calls.push(["hide"]); },
    onMoved: async (fn) => { moved = fn; return noop; },
    // Native startDragging only acknowledges dispatch; it resolves before movement/mouse release.
    startDragging: async () => { calls.push(["drag-started"]); },
  };
  class Size { constructor(width, height) { Object.assign(this, { width, height }); } }
  class Position { constructor(x, y) { Object.assign(this, { x, y }); } }
  class Clock extends Date { static now() { return now; } }
  const api = {
    async startWidgetDrag() {
      const before = { ...native.position };
      await native.startDragging();
      return new Promise((resolve) => {
        endDrag = () => resolve({
          moved: !options.ipcMovedFalse && (before.x !== native.position.x || before.y !== native.position.y),
          before: [before.x, before.y],
          mouse: [native.pointer.x, native.pointer.y],
        });
      });
    },
    listActiveSessions: async () => options.initialSessions ?? [], saveWidgetPosition: async (...args) => saves.push(args),
    takeWidgetReveal: async () => { const pending = pendingReveal; pendingReveal = false; return pending; },
    setWidgetPinned: async () => { if (options.failPin) throw Error("disk full"); },
    showPanel: async () => {},
  };
  const result = module(widgetCode, {
    // isMacOS 读 navigator.userAgent，mac 场景要按 options.navigator 重建 edge 模块
    "./widget-edge": options.navigator
      ? module(compile("widget-edge"), { "./widget-layout": layout }, { navigator: options.navigator })
      : edge,
    "./widget-layout": layout,
    "./assets/logo.png": "logo.png",
    "./api": {
      api, eventLabel: (kind) => kind, phaseLabel: (phase) => phase, placeholderName: (id) => `session …${id}`,
      SESSION_INDENT_MAX: 2,
      agentName: (name) => name, agentIcon: () => "", isSubagentSession: (s) => !!s.parent,
      // 有血缘才算子代理（与 src/api.ts 的同名判据一致）
      isSubagentEvent: (ev) => ev.is_subagent === true && typeof ev.parent_session_id === "string" && ev.parent_session_id !== "",
      sessionRows: (sessions) => sessions.map((session) => ({ session, depth: 0 })),
      // 个别用例要真实 api：占位父行只有真 sessionRows 才造得出来（默认桩是拍平的）
      ...(options.apiOverrides ?? {}),
    },
    "./i18n": {
      initI18n: async () => {}, t: (key) => key, locale: () => "zh", onLocaleChanged: () => noop,
    },
    "./session-time": { durationUnits: () => ({ hour: "h", minute: "m", second: "s" }) },
    "./theme": { initTheme: noop, onThemeChange: noop },
    "./widget-tip": Object.fromEntries(["hideRowTip", "initRowTip", "refreshRowTip", "scheduleRowTip", "scheduleTipHide", "setRowTipLabels"].map((key) => [key, noop])),
    "@tauri-apps/api/event": { listen: async (name, fn) => { listeners.set(name, fn); return noop; }, emitTo: async () => {} },
    "@tauri-apps/api/window": {
      getCurrentWindow: () => native, currentMonitor: async () => monitor, availableMonitors: async () => [monitor],
      cursorPosition: async () => native.pointer, LogicalPosition: Position, LogicalSize: Size, PhysicalPosition: Position, PhysicalSize: Size,
    },
  }, {
    document, window, console: { ...console, warn: (...args) => { if (!options.failPin) console.warn(...args); } }, Date: Clock, performance: { now: () => now },
    requestAnimationFrame: (fn) => options.frameMs === null ? 0 : window.setTimeout(fn, options.frameMs ?? 16), matchMedia: () => ({ matches: !!options.reducedMotion }),
    ...(options.navigator ? { navigator: options.navigator } : {}),
  });
  async function flush() { for (let i = 0; i < 100; i++) await Promise.resolve(); }
  async function advance(ms) {
    await flush();
    const until = now + ms;
    for (;;) {
      const next = [...timers].filter(([, timer]) => timer.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      now = next[1].at; timers.delete(next[0]); next[1].fn(); await flush();
    }
    now = until; await flush();
  }
  return {
    result, native, document, saves, calls, animations, hiddenDuringGeometry, advance,
    node: (id) => document.getElementById(id),
    sessions: (sessions) => listeners.get("bark://sessions")({ payload: sessions }),
    event: (event) => listeners.get("bark://event")({ payload: event }),
    tray() { pendingReveal = true; listeners.get("bark://widget-reveal")({ payload: null }); },
    move(x, y, pointer, emit = true) { native.position = { x, y }; native.pointer = pointer; if (emit) moved({ payload: { x, y } }); },
    endDrag: () => endDrag(),
  };
}

test("restart restores a visible handle, hover reverses an in-flight slide, and leaving collapses", async () => {
  const ui = setup();
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
  assert.equal(ui.result.snapshot().hidden, false);
  await ui.document.dispatch("mouseenter");
  await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  await ui.document.dispatch("mouseleave");
  await ui.advance(410);
  assert.equal(ui.result.snapshot().edgeMoving, true);
  assert.equal(ui.native.size.width, 280); // Only content moves while the compositor is animating.
  await ui.document.dispatch("mouseenter");
  await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  await ui.document.dispatch("mouseleave");
  await ui.advance(6000);
  assert.equal(ui.native.size.width, 6);
  assert.equal(ui.calls.some(([name]) => name === "hide"), false);
  await ui.result.saveCurrentPosition();
  assert.deepEqual(ui.saves.at(-1), [1640, 120, "right", null]);
});

test("the fake handle is hidden before the native window grows", async () => {
  const ui = setup();
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
  const before = ui.hiddenDuringGeometry.length;
  await ui.document.dispatch("mouseenter");
  await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  // 展开时第一次改原生几何之前必须已经藏好窄条。否则 WebView 合成器会拿旧 6px
  // 视口把 right:0 的窄条画到新窗口左缘——鼠标移入时闪一下的收缩条。
  const during = ui.hiddenDuringGeometry.slice(before);
  assert.ok(during.length >= 2, "expansion should touch both position and size");
  // 发生在动画结束、卡片已经露出来之后，不再要求 edge-handle-hidden。
  assert.equal(during[0], true, "the first native geometry call must already hide the handle");
  assert.equal(during[1], true, "the size call must already hide the handle");
  assert.equal(ui.document.body.classList.contains("edge-handle-hidden"), false);
});

test("expansion completes even if requestAnimationFrame never fires", async () => {
  const ui = setup({}, { frameMs: null });
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
  const before = ui.calls.length;
  await ui.document.dispatch("mouseenter");
  await ui.advance(79);
  // 双 rAF 永不回调：超时兜底（80ms）之前不许改原生几何。
  assert.equal(ui.native.size.width, 6);
  assert.equal(ui.document.body.classList.contains("edge-handle-hidden"), true);
  await ui.advance(30);
  assert.equal(ui.native.size.width, 280);
  assert.equal(ui.document.body.classList.contains("edge-collapsed"), false);
  assert.ok(ui.calls.length - before >= 2, "timeout must still apply native geometry");
  await ui.advance(300);
  assert.equal(ui.document.body.classList.contains("edge-handle-hidden"), false);
  assert.equal(ui.result.snapshot().edgeMoving, false);
});

test("slides use the compositor, including reduced motion, and shrink native bounds only after finishing", async () => {
  for (const side of ["left", "right", "top"]) {
    for (const reducedMotion of [false, true]) {
      const ui = setup({ dock_side: side }, { reducedMotion });
      await ui.advance(0);
      const callsBefore = ui.calls.length;
      await ui.document.dispatch("mouseenter");
      await ui.advance(16);
      assert.equal(ui.calls.length, callsBefore, "native geometry must wait for a paint");
      await ui.advance(16);
      const slide = ui.animations.find(({ target }) => target === ui.node("app"));
      assert.ok(slide.timing.duration >= 160);
      assert.match(slide.timing.easing, /^cubic-bezier/);
      assert.notEqual(slide.keyframes[0].transform, slide.keyframes[1].transform);
      const during = ui.calls.length;
      assert.equal(during - callsBefore, 2, "expanding touches native position and size once each");
      await ui.advance(50);
      assert.equal(ui.calls.length, during, "animation frames must not send window geometry IPC");
      assert.equal(ui.result.snapshot().edgeMoving, true);
      await ui.advance(250);
      assert.equal(ui.native.size.width, 280);
      assert.equal(ui.native.size.height, 44);
      await ui.document.dispatch("mouseleave"); await ui.advance(390);
      assert.equal(ui.result.snapshot().edgeMoving, true);
      assert.equal(ui.native.size.width, 280);
      assert.equal(ui.native.size.height, 44);
      await ui.advance(300);
      assert.equal(side === "top" ? ui.native.size.height : ui.native.size.width, 6);
    }
  }
});

test("reversing a slide starts at the compositor's eased position without resizing the window", async () => {
  const ui = setup({}, { animationProgress: .7 });
  await ui.advance(0);
  await ui.document.dispatch("mouseenter"); await ui.advance(60);
  const callsBefore = ui.calls.length;
  ui.result.requestEdge(true); await ui.advance(0);
  assert.ok(Math.abs(ui.result.snapshot().edgeProgress - .3) < 1e-8);
  const slides = ui.animations.filter(({ target }) => target === ui.node("app"));
  assert.equal(slides.length, 2);
  assert.equal(slides[1].keyframes[0].transform, edge.edgeTransform(280, 44, "right", ui.result.snapshot().edgeProgress));
  assert.equal(ui.calls.length, callsBefore);
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
});

test("context menus expand the handle, block collapse, and restore the card on close", async () => {
  const ui = setup({ x: 1640, y: 996 });
  await ui.advance(300);
  const opened = ui.result.openMenu(3, 20);
  await ui.advance(300); await opened;
  assert.equal(ui.native.size.width, 280);
  assert.equal(ui.result.snapshot().menuOpen, true);
  await ui.advance(6000);
  assert.equal(ui.native.size.width, 280);
  await ui.result.saveCurrentPosition();
  assert.equal(ui.saves.length, 0);
  await ui.result.closeMenu();
  await ui.advance(600);
  assert.equal(ui.native.size.width, 6);
  assert.equal(ui.native.position.y, 996);
});

test("displayed state changes reveal for five seconds, including waiting, without holding indefinitely", async () => {
  const ui = setup();
  await ui.advance(300);
  const session = { agent: "codex", session_id: "one", phase: "thinking" };
  ui.sessions([session]); await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  await ui.advance(4900);
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
  ui.sessions([{ ...session, phase: "waiting_input" }]); await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  await ui.advance(5500);
  assert.equal(ui.native.size.width, 6);
  ui.sessions([{ ...session, phase: "waiting_input", last_activity: 9000 }]); await ui.advance(600);
  assert.equal(ui.native.size.width, 6);
  ui.sessions([session]); await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
});

test("same-color phase changes neither reveal nor extend a tray reveal, including subagents", async () => {
  for (const [first, second] of [["thinking", "tool_running"], ["waiting_input", "waiting_permission"], ["waiting_children", "settling"]]) {
    for (const parent of [null, "parent-session"]) {
      const session = { agent: "codex", session_id: "one", phase: first, parent };
      const ui = setup({}, { initialSessions: [session] });
      await ui.advance(300);
      for (const phase of [second, first, second]) {
        ui.sessions([{ ...session, phase }]); await ui.advance(300);
        assert.equal(ui.native.size.width, 6, `${first}/${second} must stay collapsed`);
      }
      ui.tray(); await ui.advance(1400);
      ui.sessions([session]); await ui.advance(400);
      assert.equal(ui.native.size.width, 280);
      await ui.advance(500);
      assert.equal(ui.native.size.width, 6, "same-color transition must not renew the tray deadline");
    }
  }
});

test("dragging to an edge docks, clicks retain docking, and dragging inland undocks", async () => {
  const ui = setup({ x: 600, y: 400, dock_side: null });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.move(0, 150, { x: 2, y: 160 });
  await ui.advance(0); ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "left");
  assert.equal(ui.native.size.width, 6);
  assert.deepEqual(ui.saves.at(-1), [0, 150, "left", null]);
  await ui.document.dispatch("mouseenter"); await ui.advance(300);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.endDrag(); await ui.advance(0);
  assert.equal(ui.result.snapshot().dockSide, "left");
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.move(500, 300, { x: 530, y: 310 });
  await ui.advance(0); ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, null);
  assert.equal(ui.native.size.width, 280);
  assert.deepEqual(ui.saves.at(-1), [500, 300]);
});

test("macOS snaps by the pointer at the screen edge from the drag report", async () => {
  // 拖动时卡片跟着抓取点走，鼠标出不了屏，卡片边缘永远停在屏内几十像素——
  // 卡片几何判定永远差一口气。吸附意图信号是「鼠标贴边」：拖动回执带松手时
  // 的全局鼠标位置（CoreGraphics 直读，无 tao cursorPosition 的 Retina bug）
  const macNavigator = { userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15" };
  const ui = setup({ x: 600, y: 400, dock_side: null }, { navigator: macNavigator });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  // 窗口拖到左缘附近，鼠标贴住左边缘（x=0 ≤ 24 阈值）
  ui.move(5, 570, { x: 0, y: 570 });
  await ui.advance(0); ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "left");
  assert.equal(ui.native.size.width, 6);
  // 收起态先移入展开，第一次点击只展开不拖（与 Windows 同一交互序列）
  await ui.document.dispatch("mouseenter"); await ui.advance(300);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.endDrag(); await ui.advance(0);
  // 拖离边缘取消停靠——鼠标已离开边缘带，窗口位置远离边缘
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.move(500, 300, { x: 500, y: 310 });
  await ui.advance(0); ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, null);
  assert.equal(ui.native.size.height, 44);
});

test("pinning clears docking only after a successful save", async () => {
  for (const failPin of [false, true]) {
    const ui = setup({}, { failPin });
    await ui.advance(300);
    const opened = ui.result.openMenu(3, 20);
    await ui.advance(300); await opened;
    await ui.node("ctx-pin").dispatch("click"); await ui.advance(600);
    assert.equal(ui.result.snapshot().dockSide, failPin ? "right" : null);
    assert.equal(ui.native.size.width, failPin ? 6 : 280);
  }
});

test("growing contents reveal before returning to clipped geometry and save expanded coordinates", async () => {
  for (const dock_side of ["top", "left", "right"]) {
    const ui = setup({ dock_side, y: 996 });
    await ui.advance(300);
    ui.sessions(Array.from({ length: 12 }, (_, index) => ({ agent: "codex", session_id: String(index), phase: "thinking" })));
    await ui.advance(300);
    assert.equal(ui.native.size.width, 280);
    assert.equal(ui.native.size.height, 235);
    await ui.advance(5300);
    assert.equal(dock_side === "top" ? ui.native.size.height : ui.native.size.width, 6);
    assert.ok(ui.native.position.y + ui.native.size.height <= 1040);
    await ui.result.saveCurrentPosition();
    assert.equal(ui.saves.at(-1)[2], dock_side);
    assert.equal(ui.saves.at(-1)[0], dock_side === "left" ? 0 : 1640);
    assert.equal(ui.saves.at(-1)[1], dock_side === "top" ? 0 : 805);
  }
});

test("a fresh session extends the reveal deadline and repeated heartbeats do not", async () => {
  const ui = setup();
  await ui.advance(300);
  const first = { agent: "codex", session_id: "one", phase: "thinking" };
  const second = { ...first, session_id: "two" };
  ui.sessions([first]); await ui.advance(4000);
  ui.sessions([first, second]); await ui.advance(3000);
  assert.equal(ui.native.size.width, 280);
  ui.sessions([{ ...first, last_activity: 7000, tool_calls: 99 }, second]); await ui.advance(2400);
  assert.equal(ui.native.size.width, 6);
});

test("tray clicks reveal for two seconds, renew the deadline, and hovering keeps the card open", async () => {
  const ui = setup();
  await ui.advance(0);
  assert.equal(ui.native.size.width, 6);
  ui.tray(); await ui.advance(1800);
  assert.equal(ui.native.size.width, 280);
  ui.tray(); await ui.advance(1500);
  assert.equal(ui.native.size.width, 280);
  await ui.document.dispatch("mouseenter"); await ui.advance(7000);
  assert.equal(ui.native.size.width, 280);
  await ui.document.dispatch("mouseleave"); await ui.advance(300);
  assert.equal(ui.native.size.width, 280);
  await ui.advance(300);
  assert.equal(ui.native.size.width, 6);
});

test("leaving during a tray reveal preserves the remaining two-second deadline", async () => {
  const ui = setup();
  await ui.advance(0);
  ui.tray(); await ui.advance(300);
  await ui.document.dispatch("mouseenter"); await ui.advance(100);
  await ui.document.dispatch("mouseleave"); await ui.advance(1400);
  assert.equal(ui.native.size.width, 280);
  await ui.advance(500);
  assert.equal(ui.native.size.width, 6);
});

test("a queued tray click survives startup while the initial session snapshot alone stays collapsed", async () => {
  for (const pendingReveal of [false, true]) {
    const ui = setup({}, { pendingReveal, initialSessions: [{ agent: "codex", session_id: "running", phase: "waiting_input" }] });
    await ui.advance(300);
    assert.equal(ui.native.size.width, pendingReveal ? 280 : 6);
    await ui.advance(2000);
    assert.equal(ui.native.size.width, 6);
  }
});

test("completed, failed and aborted events reveal once; display expiry does not reopen the card", async () => {
  const ui = setup();
  await ui.advance(0);
  for (const type of ["run_completed", "run_failed", "run_aborted"]) {
    const event = { id: type, agent: "codex", session_id: "one", project: null, type };
    ui.event(event); await ui.advance(300);
    assert.equal(ui.native.size.width, 280);
    await ui.advance(5500);
    assert.equal(ui.native.size.width, 6);
    ui.event(event); await ui.advance(1500);
    assert.equal(ui.native.size.width, 6);
  }
});

test("free-floating widgets remain visible through idle time and tray clicks", async () => {
  const ui = setup({ dock_side: null, auto_hide: true }); // Obsolete saved preference must have no effect.
  await ui.advance(10000);
  ui.tray(); await ui.advance(10000);
  assert.equal(ui.native.size.width, 280);
  assert.equal(ui.result.snapshot().hidden, false);
  assert.ok(ui.calls.some(([name]) => name === "show"));
  assert.equal(ui.calls.some(([name]) => name === "hide"), false);
});

test("a short drag from docking stays undocked until the pointer leaves the edge zone", async () => {
  const ui = setup({ dock_side: "left", x: 0 });
  await ui.advance(300);
  await ui.document.dispatch("mouseenter"); await ui.advance(300);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 1, detail: 1 });
  ui.move(2, 120, { x: 3, y: 130 }); await ui.advance(0);
  ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, null);
  assert.equal(ui.native.size.width, 280);
});

test("cancel docking restores the full window and persists a free position", async () => {
  const ui = setup();
  await ui.advance(300);
  const opened = ui.result.openMenu(3, 20); await ui.advance(300); await opened;
  await ui.node("ctx-undock").dispatch("click"); await ui.advance(600);
  assert.equal(ui.result.snapshot().dockSide, null);
  assert.equal(ui.native.size.width, 280);
  assert.equal(ui.native.position.x, 1640);
  assert.deepEqual(ui.saves.at(-1), [1640, 120]);
});

test("docking uses target monitor physical coordinates before native DPI has caught up", async () => {
  const monitor = { name: "display-2", position: { x: -2560, y: -300 }, size: { width: 2560, height: 1440 }, workArea: { position: { x: -2560, y: -300 }, size: { width: 2560, height: 1380 } }, scaleFactor: 1.5 };
  const ui = setup({ dock_side: null, x: 400, y: 400 }, { monitor });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 30, detail: 1 });
  ui.move(-2530, 0, { x: -2558, y: 10 }); await ui.advance(0);
  ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "left");
  assert.equal(ui.native.position.x, -2560);
  assert.equal(ui.native.size.width, 9);
  assert.equal(ui.native.size.height, 66);
  assert.deepEqual(ui.saves.at(-1), [-1707, 0, "left", "display-2"]);
});

test("dispatch acknowledgement cannot finish a drag before release, even when movement pauses", async () => {
  const ui = setup({ dock_side: null, x: 600, y: 300 });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 100, detail: 1 });
  await ui.advance(1500);
  assert.ok(ui.calls.some(([name]) => name === "drag-started"));
  assert.equal(ui.result.snapshot().dragging, true);
  assert.equal(ui.saves.length, 0);
  ui.move(600, 0, { x: 700, y: 22 });
  await ui.advance(1500);
  assert.equal(ui.result.snapshot().dragging, true);
  assert.equal(ui.saves.length, 0);
  ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "top");
  assert.equal(ui.native.size.height, 6);
  assert.deepEqual(ui.saves.at(-1), [600, 0, "top", null]);
});

test("the final native position docks even when WebView missed all moved events", async () => {
  const ui = setup({ dock_side: null, x: 600, y: 300 });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 100, detail: 1 });
  ui.move(1640, 300, { x: 1740, y: 322 }, false);
  ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "right");
  assert.equal(ui.native.size.width, 6);
  assert.deepEqual(ui.saves.at(-1), [1640, 300, "right", null]);
});

test("macOS fallback: docking works on position comparison when moved events and the IPC moved flag are all lost", async () => {
  // macOS 的 start_dragging 即返回（回执 moved 恒 false），模态拖动期间 onMoved 也可能
  // 不派发——两个信号全丢也不能跳过吸附判定：松手位置 ≠ 拖动前位置就是真的拖动了
  const ui = setup({ dock_side: null, x: 600, y: 300 }, { ipcMovedFalse: true });
  await ui.advance(0);
  await ui.node("app").dispatch("mousedown", { button: 0, clientX: 100, detail: 1 });
  ui.move(1640, 300, { x: 1740, y: 322 }, false);
  ui.endDrag(); await ui.advance(300);
  assert.equal(ui.result.snapshot().dockSide, "right");
  assert.equal(ui.native.size.width, 6);
  assert.deepEqual(ui.saves.at(-1), [1640, 300, "right", null]);
});

// ---- 会话行合成（widget.ts 的 currentRows）：占位父行 / 孤儿驻留行 ----

const realApi = loadApi();

/** 列表容器：appEl → cardEl → listEl（见 widget.ts 的 append 顺序） */
function rowElements(ui) {
  return ui.node("app").children[0].children[0].children;
}

test("a vanished parent keeps the name it had instead of its session tail", async () => {
  // 回归：占位行的合成名曾被写进 lastSeenRow，还原时读回来的就是它自己——主会话在子代理
  // 还跑着的那段时间一直显示 `会话 …尾号`。占位父行只有真实 sessionRows 才造得出来。
  const session = (id, extra = {}) => ({
    agent: "dsh", session_id: id, project: "ws", cwd: "D:\\ws", phase: "thinking",
    prompt: null, last_tool: null, turn_count: 1, tool_calls: 1, started_at: 1000, last_activity: 2000,
    ...extra,
  });
  const child = session("C", { parent: "P", started_at: 2000, phase: "tool_running" });
  const ui = setup({}, {
    initialSessions: [session("P"), child],
    apiOverrides: { sessionRows: realApi.sessionRows, isSubagentEvent: realApi.isSubagentEvent },
  });
  await ui.advance(0);
  assert.equal(rowElements(ui).length, 2);
  ui.sessions([child]); // 父行被摘掉，子代理还在跑 → sessionRows 合成占位父行
  await ui.advance(0);
  const [parent, kid] = rowElements(ui);
  assert.ok(parent.innerHTML.includes("dsh"), "占位父行还原成上一帧见到的 agent 名");
  assert.ok(!parent.innerHTML.includes("session …P"), "不能摆一串 id 尾号");
  assert.ok(!parent.className.includes("placeholder"), "有子在跑的占位行按真实行渲染");
  assert.ok(kid.className.includes("sub"), "子代理行仍带子代理标记");
});

test("a subagent completion with no parent row gets one, so it never floats alone", async () => {
  // 回归：父行连列表都不在（daemon 刚重启 / 父回合已摘档）时，驻留行孤零零落在末尾，
  // 卡片上只剩「子代理 · 任务完成」，看着像子代理没有主会话。
  const ui = setup({}, { apiOverrides: { isSubagentEvent: realApi.isSubagentEvent } });
  await ui.advance(0);
  ui.event({
    id: "e1", agent: "dsh", type: "run_completed", session_id: "C",
    project: "ws", is_subagent: true, parent_session_id: "P", timestamp: 1,
  });
  await ui.advance(300);
  const rows = rowElements(ui);
  assert.equal(rows.length, 2, "补出父行 + 驻留行");
  assert.ok(!rows[0].className.includes("sub"), "补出来的父行是顶格主行");
  assert.ok(rows[0].innerHTML.includes("dsh"));
  assert.ok(rows[1].className.includes("sub"), "驻留行挂在父行下面");
  assert.ok(rows[1].innerHTML.includes("run_completed"));
});
