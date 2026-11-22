import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const source = readFileSync(new URL("../src/widget-layout.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;
const layout = {};
runInNewContext(compiled, { exports: layout });
const screen = { x: 0, y: 0, width: 1920, height: 1040 };
const overlaps = (a, b) => a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;

test("details prefer the right and keep the hovered row visible", () => {
  const card = { x: 100, y: 120, width: 280, height: 140 };
  const result = layout.placeTip(card, screen, 37, 70);
  assert.equal(result.side, "right");
  assert.equal(result.x, card.x + card.width + 8);
  assert.equal(result.y, card.y + 37);
  assert.equal(result.width, 220);
  assert.equal(result.height, 70);
  assert.equal(overlaps(result, card), false);
});

test("bottom fallback matches both card edges with no extra inset", () => {
  for (const x of [1500, 1640]) {
    const card = { x, y: 100, width: 280, height: 140 };
    const result = layout.placeTip(card, screen, 37, 70);
    assert.equal(result.side, "bottom");
    assert.equal(result.x, x);
    assert.equal(result.width, card.width);
    assert.equal(result.y, card.y + card.height + 8);
    assert.equal(overlaps(result, card), false);
  }
});

test("bottom-right corner flips to the left instead of covering the card", () => {
  const card = { x: 1640, y: 996, width: 280, height: 44 };
  const result = layout.placeTip(card, screen, 6, 112);
  assert.equal(result.side, "left");
  assert.ok(result.x + result.width <= card.x, "详情右缘不越过卡片左缘");
  assert.ok(result.y + result.height <= screen.height, "详情不超出屏幕底");
  assert.equal(overlaps(result, card), false);
});

test("small gap above the Dock flips left at full height instead of truncating", () => {
  // 右吸附、离工作区底 88px（< 详情自然高 112）：不该把详情压成 88px 塞进缝里
  const card = { x: 1640, y: 900, width: 280, height: 44 };
  const result = layout.placeTip(card, screen, 6, 112);
  assert.equal(result.side, "left");
  assert.equal(result.height, 112, "详情保持自然高度，不被截断");
  assert.equal(overlaps(result, card), false);
});

test("roomy gap below shows the full detail beneath the card", () => {
  const card = { x: 1640, y: 800, width: 280, height: 44 };
  const result = layout.placeTip(card, screen, 6, 112);
  assert.equal(result.side, "bottom");
  assert.equal(result.height, 112);
  assert.equal(overlaps(result, card), false);
});

test("negative monitor coordinates and scaled logical work areas stay in bounds", () => {
  const monitors = [
    { x: -1920, y: 0, width: 1920, height: 1040 },
    { x: 0, y: -960, width: 1706, height: 920 },
  ];
  for (const area of monitors) {
    const card = { x: area.x + area.width - 280, y: area.y + area.height - 235, width: 280, height: 235 };
    const result = layout.placeTip(card, area, 180, 112);
    assert.ok(result.x >= area.x);
    assert.ok(result.y >= area.y);
    assert.ok(result.x + result.width <= area.x + area.width);
    assert.ok(result.y + result.height <= area.y + area.height);
    assert.equal(overlaps(result, card), false);
  }
});

test("right-side minimum width is respected and tall details are clamped", () => {
  const card = { x: 0, y: 0, width: 280, height: 44 };
  assert.equal(layout.tipWidth(card, { ...screen, width: 468 }).side, "right");
  assert.equal(layout.tipWidth(card, { ...screen, width: 467 }).side, "bottom");
  const tiny = { x: 0, y: 0, width: 280, height: 160 };
  const result = layout.placeTip(card, tiny, 0, 500);
  assert.equal(result.side, "bottom");
  assert.equal(result.height, 108);
  assert.equal(overlaps(result, card), false);
});

test("project suffixes remain distinct and never consume the whole label", () => {
  const a = layout.splitName("passport-firmware-alpha");
  const b = layout.splitName("passport-firmware-beta");
  assert.equal(a[1], "-alpha");
  assert.equal(b[1], "-beta");
  for (const name of ["Codex", "a".repeat(80), "workspace-" + "b".repeat(80)]) {
    const parts = layout.splitName(name);
    assert.equal(parts.join(""), name);
    assert.ok(parts[1].length <= 12);
  }
});

test("subagent numbers remain stable when siblings finish or new siblings appear", () => {
  const labels = new layout.SubagentLabels();
  const first = labels.get("dsh|one");
  const second = labels.get("dsh|two");
  labels.retain(new Set(["dsh|two"]));
  const third = labels.get("codex|two");
  assert.equal(labels.get("dsh|two"), second);
  assert.notEqual(third, second);
  assert.notEqual(third, first);
});
