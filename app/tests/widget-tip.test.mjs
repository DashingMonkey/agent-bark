import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const source = readFileSync(new URL("../src/widget-tip.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;
const timeModule = {};
runInNewContext(ts.transpileModule(readFileSync(new URL("../src/session-time.ts", import.meta.url), "utf8"), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText, { exports: timeModule });

function setup(place = async () => true) {
  const timers = new Map();
  let timerId = 0;
  let now = 100000;
  let closes = 0;
  function element() {
    const children = [];
    const classes = new Set();
    return {
      children, dataset: {}, isConnected: true, attributes: {}, events: {},
      classList: { add: (name) => classes.add(name), remove: (name) => classes.delete(name), contains: (name) => classes.has(name) },
      append: (...items) => children.push(...items),
      appendChild: (item) => children.push(item),
      setAttribute(name, value) { this.attributes[name] = value; },
      removeAttribute(name) { delete this.attributes[name]; },
      addEventListener(name, fn) { this.events[name] = fn; },
      querySelector(selector) { return children.find((child) => `.${child.className}` === selector); },
      getBoundingClientRect: () => ({ top: 10, bottom: 42 }),
      closest: () => ({ getBoundingClientRect: () => ({ top: 5, bottom: 197 }) }),
    };
  }
  const body = element();
  const document = { body, createElement: element, hidden: false, events: {},
    addEventListener(name, fn) { this.events[name] = fn; },
  };
  const window = {
    setTimeout(fn, ms) { const id = ++timerId; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
    setInterval(fn, ms) { const id = ++timerId; timers.set(id, { fn, at: now + ms, interval: ms }); return id; },
    clearInterval(id) { timers.delete(id); },
  };
  const api = {};
  runInNewContext(compiled, { exports: api, document, window, Date: { now: () => now },
    require: (id) => { assert.equal(id, "./session-time"); return timeModule; }, console: { warn() {} },
  });
  api.initRowTip({ place, close: async () => { closes++; } });
  const row = element();
  row.dataset = { tipHeading: "Codex", tip: "passport-firmware-alpha", startedAt: String(now - 5000) };
  return {
    api, row, box: body.children[0], closes: () => closes,
    now: () => now,
    intervalCount: () => [...timers.values()].filter((timer) => timer.interval).length,
    visibility(hidden) { document.hidden = hidden; document.events.visibilitychange(); },
    async advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) if (timer.at <= now) {
        if (timer.interval) timer.at = now + timer.interval;
        else timers.delete(id);
        timer.fn();
      }
      await new Promise(setImmediate);
    },
  };
}

test("leaving before the delay never requests native popup geometry", async () => {
  let placed = 0;
  const ui = setup(async () => { placed++; return true; });
  ui.api.scheduleRowTip(ui.row);
  ui.api.hideRowTip();
  await ui.advance(300);
  assert.equal(placed, 0);
  assert.equal(ui.box.classList.contains("show"), false);
});

test("late native resize completion cannot resurrect a dismissed tooltip", async () => {
  let finish;
  let isCurrent;
  const ui = setup((_box, _row, current) => {
    isCurrent = current;
    return new Promise((resolve) => { finish = resolve; });
  });
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  assert.equal(isCurrent(), true);
  ui.api.hideRowTip();
  assert.equal(isCurrent(), false);
  finish(true);
  await ui.advance(0);
  assert.equal(ui.box.classList.contains("show"), false);
  assert.equal(ui.row.attributes["aria-describedby"], undefined);
});

test("moving across the gap into the details keeps them visible", async () => {
  const ui = setup();
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  ui.api.scheduleTipHide();
  await ui.advance(100);
  ui.box.events.mouseenter();
  await ui.advance(200);
  assert.equal(ui.box.classList.contains("show"), true);
  ui.box.events.mouseleave();
  await ui.advance(160);
  assert.equal(ui.box.classList.contains("show"), false);
});

test("snapshot refresh updates visible details, removed/scrolled-away rows dismiss them", async () => {
  const ui = setup();
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  ui.row.dataset.tip = "updated-project";
  ui.api.refreshRowTip();
  await ui.advance(0);
  assert.equal(ui.box.querySelector(".tip-body").textContent, "updated-project");
  assert.equal(ui.box.classList.contains("show"), true);
  ui.row.getBoundingClientRect = () => ({ top: -40, bottom: -8 });
  ui.api.refreshRowTip();
  assert.equal(ui.box.classList.contains("show"), false);
});

test("blocked or failed native positioning never displays a clipped tooltip", async () => {
  for (const place of [async () => false, async () => { throw Error("window destroyed"); }]) {
    const ui = setup(place);
    ui.api.scheduleRowTip(ui.row);
    await ui.advance(220);
    assert.equal(ui.box.classList.contains("show"), false);
    assert.ok(ui.closes() >= 1);
  }
});

test("hover duration ticks without snapshots or native resize and catches up on reopening", async () => {
  let placements = 0;
  const ui = setup(async () => { placements++; return true; });
  const duration = ui.box.querySelector(".tip-duration");
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  assert.equal(duration.textContent, "已运行 5秒");
  assert.equal(ui.intervalCount(), 1);
  await ui.advance(1000);
  assert.equal(duration.textContent, "已运行 6秒");
  await ui.advance(64000);
  assert.equal(duration.textContent, "已运行 1分10秒");
  assert.equal(placements, 1);
  ui.api.hideRowTip();
  assert.equal(ui.intervalCount(), 0);
  await ui.advance(90000);
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  assert.equal(duration.textContent, "已运行 2分40秒");
  assert.equal(ui.intervalCount(), 1);
});

test("new turns reset hover duration and terminal rows stop it", async () => {
  const ui = setup();
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  ui.row.dataset.startedAt = String(ui.now());
  ui.api.refreshRowTip();
  await ui.advance(0);
  const duration = ui.box.querySelector(".tip-duration");
  assert.equal(duration.textContent, "已运行 0秒");
  await ui.advance(1000);
  assert.equal(duration.textContent, "已运行 1秒");
  for (const value of ["", "invalid"]) {
    ui.row.dataset.startedAt = value;
    ui.api.refreshRowTip();
    await ui.advance(0);
    assert.equal(duration.hidden, true);
    assert.equal(duration.textContent, "");
    assert.equal(ui.intervalCount(), 0);
  }
});

test("a hidden hover panel suspends its timer and recalculates when visible", async () => {
  const ui = setup();
  ui.api.scheduleRowTip(ui.row);
  await ui.advance(220);
  ui.visibility(true);
  assert.equal(ui.intervalCount(), 0);
  await ui.advance(60000);
  ui.visibility(false);
  assert.equal(ui.box.querySelector(".tip-duration").textContent, "已运行 1分05秒");
  assert.equal(ui.intervalCount(), 1);
});
