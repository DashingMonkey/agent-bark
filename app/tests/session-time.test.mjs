import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";
import * as vue from "vue";

function compile(source) {
  return ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
}
const timeModule = {};
runInNewContext(compile(readFileSync(new URL("../src/session-time.ts", import.meta.url), "utf8")), { exports: timeModule });

// EventsPage 直载时的 ../i18n 替身：按当前 zh 词表做最简插值（断言里的中文文案
// 直接跟着词表走，词表改动若影响文案会在这里暴露）
const zhMessages = JSON.parse(readFileSync(new URL("../src/locales/zh-CN.json", import.meta.url), "utf8"));
const zhT = (key, params) => {
  const text = key.split(".").reduce((node, part) => (node == null ? undefined : node[part]), zhMessages);
  return typeof text === "string" ? text.replace(/\{(\w+)\}/g, (_, name) => String(params?.[name] ?? "")) : key;
};

test("durationUnits maps locales to unit suffixes and formatDuration accepts them", () => {
  // VM 沙箱里的对象原型来自另一个 realm，先摊平成本 realm 的对象再比较
  const unitsOf = (locale) => ({ ...timeModule.durationUnits(locale) });
  assert.deepEqual(unitsOf("zh"), { hour: "小时", minute: "分", second: "秒" });
  assert.deepEqual(unitsOf("en"), { hour: "h", minute: "m", second: "s" });
  // 未知语言按中文兜底
  assert.deepEqual(unitsOf("fr"), { hour: "小时", minute: "分", second: "秒" });
  assert.equal(timeModule.formatDuration(100000, 100000 + 59999, timeModule.durationUnits("en")), "59s");
  assert.equal(timeModule.formatDuration(100000, 100000 + 60000, timeModule.durationUnits("en")), "1m00s");
  assert.equal(timeModule.formatDuration(100000, 100000 + 3601000, timeModule.durationUnits("en")), "1h00m01s");
});

test("duration keeps seconds across minute/hour boundaries and clamps clock rollback", () => {
  for (const [elapsed, expected] of [[-1000, "0秒"], [59999, "59秒"], [60000, "1分00秒"],
    [3599000, "59分59秒"], [3600000, "1小时00分00秒"], [3601000, "1小时00分01秒"]]) {
    assert.equal(timeModule.formatDuration(100000, 100000 + elapsed), expected);
  }
});

function eventsPage() {
  let now = 100000;
  let nextTimer = 0;
  const timers = new Map();
  const hooks = {};
  const eventTarget = () => ({
    listeners: new Map(),
    addEventListener(name, fn) { this.listeners.set(name, fn); },
    removeEventListener(name) { this.listeners.delete(name); },
    dispatch(name) { this.listeners.get(name)?.(); },
  });
  const window = {
    ...eventTarget(),
    setInterval(fn, ms) { const id = ++nextTimer; timers.set(id, { fn, ms }); return id; },
  };
  const document = eventTarget();
  const imports = {
    vue: { ...vue, ...Object.fromEntries(["onMounted", "onActivated", "onDeactivated", "onUnmounted"]
      .map((name) => [name, (fn) => { hooks[name] = fn; }])) },
    "../api": {
      api: { listEvents: async () => [], listActiveSessions: async () => [] },
      store: vue.reactive({ events: [], sessions: [], sessionsSeq: 0 }),
      sessionRows: (sessions) => sessions,
    },
    "../session-time": timeModule,
    "../i18n": { t: zhT, locale: () => "zh" },
  };
  const source = readFileSync(new URL("../src/pages/EventsPage.vue", import.meta.url), "utf8")
    .match(/<script setup lang="ts">([\s\S]*?)<\/script>/)[1];
  const page = {};
  runInNewContext(compile(`${source}\nexport { durationLabel };`), {
    exports: page, window, document, clearInterval: (id) => timers.delete(id), Date: { now: () => now },
    require: (id) => { assert.ok(id in imports, `Unexpected import: ${id}`); return imports[id]; },
  });
  return { page, hooks, timers, window, document,
    advance(ms, tick = true) { now += ms; if (tick) for (const timer of timers.values()) if (timer.ms === 1000) timer.fn(); },
  };
}

test("event page ticks without backend pushes and catches up immediately after KeepAlive activation", () => {
  const ui = eventsPage();
  const session = { started_at: 95000, phase: "thinking" };
  ui.hooks.onMounted();
  ui.hooks.onActivated();
  assert.equal(ui.timers.size, 2);
  assert.equal(ui.page.durationLabel(session), "已运行 5秒");
  ui.advance(1000);
  assert.equal(ui.page.durationLabel(session), "已运行 6秒");
  ui.hooks.onDeactivated();
  assert.equal(ui.timers.size, 0);
  assert.equal(ui.window.listeners.size, 0);
  assert.equal(ui.document.listeners.size, 0);
  ui.advance(60000);
  ui.hooks.onActivated();
  assert.equal(ui.page.durationLabel(session), "已运行 1分06秒");
  assert.equal(ui.timers.size, 2);
  ui.hooks.onUnmounted();
  assert.equal(ui.timers.size, 0);
});

test("focus/visibility resume wall-clock time and a new turn uses its own start", () => {
  const ui = eventsPage();
  ui.hooks.onMounted();
  const session = { started_at: 95000, phase: "waiting_permission" };
  ui.advance(65000, false);
  ui.window.dispatch("focus");
  assert.equal(ui.page.durationLabel(session), "已运行 1分10秒");
  ui.advance(1000, false);
  ui.document.dispatch("visibilitychange");
  assert.equal(ui.page.durationLabel(session), "已运行 1分11秒");
  session.started_at = 166000;
  session.phase = "thinking";
  assert.equal(ui.page.durationLabel(session), "已运行 0秒");
});
