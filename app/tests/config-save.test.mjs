import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";
import * as vue from "vue";

// Execute the actual page scripts with IPC/lifecycle mocks, keeping Vue reactivity.
function page(name, api, exports) {
  const source = readFileSync(new URL(`../src/pages/${name}.vue`, import.meta.url), "utf8");
  const script = source.match(/<script setup lang="ts">([\s\S]*?)<\/script>/)[1];
  const compiled = ts.transpileModule(`${script}\nexport { ${exports.join(", ")} };`, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const result = {};
  const noop = () => {};
  const imports = {
    vue: { ...vue, onMounted: noop, onUnmounted: noop, onActivated: noop, onDeactivated: noop },
    "../api": { api, SOUND_EFFECTS: [], EDGE_EFFECTS: [], BURST_EFFECTS: [] },
    "../toast": { toast: noop },
    "@tauri-apps/api/event": { listen: async () => noop },
    "@tauri-apps/plugin-autostart": { isEnabled: async () => false },
  };
  runInNewContext(compiled, { exports: result, require: (id) => {
    assert.ok(id in imports, `Unexpected import: ${id}`);
    return imports[id];
  } });
  return result;
}

test("cached settings pages only submit their own sections", async () => {
  let saved = {
    server: { port: 36573, token: "unchanged" }, agents: [{ kind: "kimi-code", enabled: true }],
    channels: { bark: { enabled: true, url: "old" }, webhook: null },
    rules: { aggregate_window_ms: 1500, quiet_hours: [] },
    widget: { enabled: true, auto_hide: false, x: 10, y: 20, pinned: true },
    notify: { enabled: true, ...Object.fromEntries(["thinking", "waiting", "completed", "failed"].map((k) => [k, { effect: "", plays: 1 }])) },
    glow: { enabled: true, monitors: "all" },
  };
  const submitted = [];
  const api = {
    getConfig: async () => structuredClone(saved), diagnostics: async () => ({}), monitors: async () => [],
    saveConfig: async (value) => {
      const patch = JSON.parse(JSON.stringify(value));
      submitted.push(patch);
      saved = { ...saved, ...patch, widget: { ...saved.widget, ...patch.widget } };
    },
  };
  const channels = page("ChannelsPage", api, ["load", "save", "config"]);
  const widget = page("WidgetPage", api, ["load", "toggle", "config"]);
  const settings = page("SettingsPage", api, ["load", "save", "config"]);
  const notify = page("NotifyPage", api, ["load", "persist", "config"]);
  for (const view of [channels, widget, settings, notify]) await view.load();
  await widget.toggle("enabled", false);
  channels.config.value.channels.bark.url = "new";
  await channels.save();
  settings.config.value.rules.aggregate_window_ms = 2000;
  await settings.save();
  notify.config.value.notify.enabled = false;
  await notify.persist();
  assert.deepEqual(submitted.map((patch) => Object.keys(patch).sort()), [["widget"], ["channels"], ["rules"], ["glow", "notify"]]);
  assert.equal(saved.widget.enabled, false);
  assert.equal(saved.channels.bark.url, "new");
  assert.equal(saved.rules.aggregate_window_ms, 2000);
  assert.equal(saved.notify.enabled, false);
  assert.equal(saved.server.token, "unchanged");
  assert.equal(saved.agents[0].enabled, true);
  // Even if a window closes before its event reaches this cached page, another
  // widget setting must not restore its stale enabled flag.
  widget.config.value.widget.enabled = true;
  await widget.toggle("auto_hide", true);
  assert.deepEqual(submitted.at(-1), { widget: { auto_hide: true } });
  assert.equal(saved.widget.enabled, false);
});
