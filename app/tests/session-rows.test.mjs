import { test } from "node:test";
import assert from "node:assert/strict";
import { loadApi } from "./api-loader.mjs";

/**
 * api.ts 是会话行口径的唯一来源（悬浮窗与事件流共用）：用 api-loader 跑**真实代码**
 * （sessionRows / ghostSession / isSubagentEvent），而不是复制一遍逻辑。
 */
const api = loadApi();

/** 一条会话快照（后端 SessionStatus 的 snake_case 形态） */
function session(id, extra = {}) {
  return {
    agent: "dsh",
    session_id: id,
    project: "dsh-git-panel",
    cwd: "D:\\MySpace\\gitee\\dsh-git-panel",
    phase: "thinking",
    prompt: null,
    last_tool: null,
    turn_count: 1,
    tool_calls: 1,
    started_at: 1000,
    last_activity: 2000,
    ...extra,
  };
}

test("a subagent whose parent is missing keeps a ghost parent row above it", () => {
  const rows = api.sessionRows([session("child", { parent: "P", started_at: 2000 })]);
  assert.equal(rows.length, 2);
  assert.equal(rows[0].session.session_id, "P");
  assert.equal(rows[0].depth, 0);
  assert.equal(rows[0].placeholder, true, "缺的父行是合成行");
  assert.equal(rows[1].session.session_id, "child");
  assert.equal(rows[1].depth, 1);
  assert.equal(rows[1].placeholder, false);
});

test("the ghost parent inherits the child's workspace", () => {
  // 父会话派生子代理时沿用同一个 cwd（DSH 会话头写死），所以工作区不是猜的：
  // 不继承的话占位行没有行内项目名、悬停也没有工作区，读起来像一条「临时会话」。
  const rows = api.sessionRows([session("child", { parent: "P", started_at: 2000, project: "dsh-git-panel", cwd: "D:\\w\\dsh-git-panel" })]);
  assert.equal(rows[0].session.project, "dsh-git-panel");
  assert.equal(rows[0].session.cwd, "D:\\w\\dsh-git-panel");
});

test("a parent that is still in the snapshot gets no extra ghost row", () => {
  const rows = api.sessionRows([
    session("parent", { started_at: 1000 }),
    session("child", { parent: "parent", started_at: 2000 }),
  ]);
  // Array.from：结果数组来自 VM 沙箱，跨 realm 的 deepStrictEqual 会因为原型不同而失败
  assert.deepEqual(Array.from(rows, (row) => [row.session.session_id, row.depth, row.placeholder]), [
    ["parent", 0, false],
    ["child", 1, false],
  ]);
});

test("siblings with the same missing parent share one ghost row", () => {
  const rows = api.sessionRows([
    session("c1", { parent: "P", started_at: 3000 }),
    session("c2", { parent: "P", started_at: 2000 }),
  ]);
  assert.deepEqual(Array.from(rows, (row) => [row.session.session_id, row.depth]), [
    ["P", 0],
    ["c1", 1],
    ["c2", 1],
  ]);
});

test("an event is only a subagent row when it carries parent lineage", () => {
  const event = (extra) => ({
    id: "e1",
    agent: "trae-code",
    type: "run_completed",
    session_id: "s",
    cwd: "",
    project: null,
    message: "",
    timestamp: 1,
    ...extra,
  });
  assert.equal(api.isSubagentEvent(event({ is_subagent: true, parent_session_id: "P" })), true);
  // TraeCode 实测：hook 载荷恒带 agent_id/agent_type，主会话被通用启发式误判成子代理，
  // 而事件里没有父 id —— 没有父行可挂，必须按主会话渲染（否则卡片上多一行孤儿「子代理」）
  assert.equal(api.isSubagentEvent(event({ is_subagent: true })), false);
  assert.equal(api.isSubagentEvent(event({ is_subagent: true, parent_session_id: "" })), false);
  // hook 型 adapter 的事件恒无血缘（from_raw 置空），真子代理事件也一样 —— 与 daemon 建档
  // 口径一致：那些子代理会话在快照里本来就没有 parent，按主会话行显示
  assert.equal(api.isSubagentEvent(event({ is_subagent: true, parent_session_id: null })), false);
  assert.equal(api.isSubagentEvent(event({ is_subagent: false, parent_session_id: "P" })), false);
  assert.equal(api.isSubagentEvent(event({})), false);
});
