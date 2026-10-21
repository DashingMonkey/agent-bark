import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const source = readFileSync(new URL("../src/widget-tip.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;

function setup(place = async () => true) {
  const timers = new Map();
  let timerId = 0;
  let now = 0;
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
  const document = { body, createElement: element };
  const window = {
    setTimeout(fn, ms) { const id = ++timerId; timers.set(id, { fn, at: now + ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  const api = {};
  runInNewContext(compiled, { exports: api, document, window, console: { warn() {} } });
  api.initRowTip({ place, close: async () => { closes++; } });
  const row = element();
  row.dataset = { tipHeading: "Codex", tip: "passport-firmware-alpha" };
  return {
    api, row, box: body.children[0], closes: () => closes,
    async advance(ms) {
      now += ms;
      for (const [id, timer] of [...timers]) if (timer.at <= now) { timers.delete(id); timer.fn(); }
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
