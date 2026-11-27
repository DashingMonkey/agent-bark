import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import ts from "typescript";

/**
 * 把 `src/api.ts` 转成 CommonJS 后在 VM 里跑，只给 Tauri / vue / i18n / 内联 SVG 塞最小替身。
 *
 * 这样测的是**真实代码**（`sessionRows` / `ghostSession` / `isSubagentEvent`），而不是复制
 * 一遍逻辑。单独成文件是因为两个用例都要它：`session-rows.test.mjs` 直接测这些函数，
 * `widget-edge.test.mjs` 要真实的 `sessionRows` 才造得出占位父行（它自己的桩是拍平的）。
 */
export function loadApi() {
  const source = readFileSync(new URL("../src/api.ts", import.meta.url), "utf8");
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const requireStub = (id) => {
    if (id === "vue") return { reactive: (value) => value };
    // 只用到 `t` 取词（占位名）。文案不参与断言，别把界面文案抄进测试
    if (id === "./i18n") return { t: (key, args) => (key === "events.placeholderSession" ? `…${args?.id ?? ""}` : key) };
    if (id.endsWith("?raw")) return "";
    if (id.startsWith("@tauri-apps")) return { invoke: async () => undefined, listen: async () => () => {} };
    throw new Error(`unexpected require: ${id}`);
  };
  const module = { exports: {} };
  runInNewContext(compiled, { exports: module.exports, require: requireStub, module, console, process, window: {}, document: {} });
  return module.exports;
}
