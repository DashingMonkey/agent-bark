import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

let now = 100000;
Date.now = () => now;
const sent = [];
globalThis.fetch = async (_url, options) => {
  sent.push(JSON.parse(options.body));
  return { status: 200 };
};
const source = readFileSync(process.argv[2], "utf8");
const { Plugin } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);
const plugin = await Plugin();
async function event(type, properties, advance = 300) {
  now += advance;
  await plugin.event({ event: { type, properties: { sessionID: "s1", ...properties } } });
}
await event("session.status", { status: { type: "busy" } });
await event("session.error", { error: { name: "MessageAbortedError" } });
await event("session.idle", {});
await event("session.idle", {}, 11000);
assert.deepEqual(sent.map(e => e.type), ["activity", "run_aborted"]);

// 取消后的快速重开不能被旧的 10 秒冷却吞掉正常完成。
await event("session.status", { status: { type: "busy" } });
await event("session.idle", {});
assert.deepEqual(sent.slice(-2).map(e => e.type), ["activity", "run_completed"]);

// 另一会话不受取消标记影响；同一会话连续两轮取消仍能上报。
await event("session.status", { status: { type: "busy" } });
await event("session.error", { error: { name: "MessageAbortedError" } });
await event("session.idle", { sessionID: "s2" });
assert.deepEqual(sent.slice(-3).map(e => e.type), ["activity", "run_aborted", "run_completed"]);
assert.equal(sent.at(-1).session_id, "s2");
await event("session.status", { status: { type: "busy" } });
await event("session.idle", {});
assert.deepEqual(sent.slice(-2).map(e => e.type), ["activity", "run_completed"]);
console.log("OpenCode cancellation lifecycle passed");
