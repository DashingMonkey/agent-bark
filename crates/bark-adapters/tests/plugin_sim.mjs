// 生成插件的投递可靠性回归测试（Node 直接跑产物，不经 Rust 断言「代码里有什么字符串」）。
//
// 用法：node plugin_sim.mjs <生成的 plugin.js 路径>
//
// 背景（用户实测 bug）：DSH 回合早已结束，屏幕流光却一直停在思考蓝、会话面板卡在
// 「执行工具」，约几十次里出现一次。根因是插件把终态通知 fire-and-forget 出去、
// 并且**在投递前**就把会话标成「本回合已终态」：daemon 的事件通道打满时返回 503
// （请求成功、事件已丢），于是这条终态永久丢失，随后的 status idle 兜底也被
// 「已终态」挡住，daemon 的「运行中」条目只能等 10 分钟判死巡检收场。
//
// 这里用一个假的 fetch 扮演 daemon（可切换挂掉 / 503 / 正常），断言：
//   1. 首投失败后会自动重投，并且重投真正送达；
//   2. 首投成功时不重复发（终态只报一次）；
//   3. 首投失败时，随后的 status idle 兜底仍会补报（去重不能挡住补报）；
//   4. 心跳（activity）不进终态重投队列，丢了不重试。
//
// 退出码 0 = 全部通过；非 0 = 有断言失败。用 process.exitCode 而不是抛异常，
// 保证并行跑的子进程输出干净可读。

const pluginPath = process.argv[2];
if (!pluginPath) {
  console.error('usage: node plugin_sim.mjs <plugin.js>');
  process.exit(2);
}

// --- 环境桩 -----------------------------------------------------------------

globalThis.crypto ??= { randomUUID: () => `id-${Math.random().toString(16).slice(2)}` };

/** 记录达 daemon 的事件；`behavior` 控制返回：null = 正常 200，'fail' = 503，'throw' = 连接被拒 */
const sent = [];
let behavior = null;

globalThis.fetch = async (_url, options) => {
  const body = JSON.parse(options.body);
  if (behavior === 'throw') throw new Error('ECONNREFUSED');
  sent.push({ ...body, rejected: behavior === 'fail' });
  if (behavior === 'fail') return { status: 503 };
  return { status: 200 };
};

/** 定时器桩：记录延迟并由测试手动推进虚拟时间，避免真的等 1s/2s/4s。 */
const timers = [];
let nextTimerId = 1;
const realSetTimeout = globalThis.setTimeout;
globalThis.setTimeout = (fn, ms) => {
  const id = nextTimerId++;
  timers.push({ id, fn, ms: typeof ms === 'number' ? ms : 0 });
  return id;
};
globalThis.clearTimeout = (id) => {
  const i = timers.findIndex((t) => t.id === id);
  if (i !== -1) timers.splice(i, 1);
};

/**
 * 推进虚拟时间到 `limit` 毫秒（默认 Inf = 把所有退避都烧完）。
 *
 * 必须按延迟顺序逐个触发并尊重上限：心跳失败后有一次 500ms 的**即时补投**，
 * 而终态重投是 1s 起的退避——「推进 400ms」和「推进到重投结束」要能分开断言，
 * 否则两类定时器会互相冒充。
 *
 * 守卫耗尽（64 轮后仍有定时器）时**显式 check 失败留痕**（§4.18）——旧实现
 * 静默 return，被测插件若自续排定时器（bug）会让后续断言在「什么都没推进」的
 * 状态下假绿。
 */
async function advance(limit = Number.POSITIVE_INFINITY) {
  for (let guard = 0; guard < 64; guard += 1) {
    const due = timers.filter((t) => t.ms <= limit);
    if (due.length === 0) return;
    const delay = Math.min(...due.map((t) => t.ms));
    for (const t of due.filter((x) => x.ms === delay)) {
      timers.splice(timers.indexOf(t), 1);
      t.fn();
    }
    await flush();
  }
  check(
    false,
    `advance() 推进 64 轮后仍有 ${timers.length} 个定时器挂着（可能存在自续排的定时器）`,
  );
}

/** 让出事件循环，让在途的 fetch promise 链跑完。 */
async function flush(times = 6) {
  for (let i = 0; i < times; i += 1) await Promise.resolve();
  await new Promise((resolve) => realSetTimeout(resolve, 0));
}

/**
 * 触发**恰好一个**匹配的定时器。
 *
 * 周期保活心跳是「自续排」定时器：`advance()` 的忙等语义会把同一个 20s 延迟连烧
 * 64 轮（守卫直接判失败），反而看不出「回合关了之后有没有停」。这里只推一次，
 * 让插件自己决定要不要再排下一跳。
 */
async function fireTimer(predicate) {
  const due = timers.filter(predicate);
  if (due.length === 0) return false;
  const t = due[0];
  timers.splice(timers.indexOf(t), 1);
  t.fn();
  await flush();
  return true;
}

// --- DSH 侧的最小桩 ---------------------------------------------------------

/** 假 DSH：收集插件注册的监听器，并提供 dispatch 辅助。 */
function makeCtx(options = {}) {
  const listeners = new Map();
  const ctx = {
    /** 与 cordis 的 ctx.on(name, listener) 同形 */
    on(name, listener) {
      if (!listeners.has(name)) listeners.set(name, []);
      listeners.get(name).push(listener);
    },
    async dispatch(name, ...args) {
      // session/event 会被顺手写进桩会话的「日志」（seq + eventAt）：插件现在按官方契约
      // 从会话日志推导回合状态，桩会话必须像真会话一样能回扫到 turn/start、turn/end。
      if (name === 'session/event') {
        const [session, event] = args;
        if (session && Array.isArray(session.__events) && event) session.__events.push(event);
      }
      for (const listener of listeners.get(name) ?? []) await listener(...args);
      await flush();
    },
    /** waterfall 事件（审批/提问）：插件必须调用 next() 把请求交还下去。
     *  async listener 返回的 Promise 也要 await（§4.18——不 await 会把异步
     *  观察者的结果/异常整个漏掉，断言在 Promise 还没落定时就跑完）。 */
    async dispatchWaterfall(name, request) {
      let called = false;
      const next = () => {
        called = true;
      };
      const out = [];
      for (const l of listeners.get(name) ?? []) out.push(await l(request, next));
      return { out, called };
    },
    listenerCount: (name) => (listeners.get(name) ?? []).length,
  };
  // 官方回合边界服务（`ctx.sessionProjections`）：不传 = 老版本 DSH，插件走兜底推理
  if (options.projections) ctx.sessionProjections = options.projections;
  return ctx;
}

/**
 * 假 `sessionProjections`：模拟官方 `turnBoundary` 投影
 * （`turn/start` 置 `openTurnStartSeq`、`turn/end` 置回 null）。
 */
function makeProjections() {
  const state = new Map();
  const read = (session) => {
    let s = state.get(session);
    if (s === undefined) {
      s = { openTurnStartSeq: null, lastStepStartSeq: null, lastStepBoundary: null, lastTurn: 0 };
      state.set(session, s);
    }
    return s;
  };
  return {
    stateOf(session, key) {
      if (key !== 'turnBoundary' || !session) return undefined;
      return read(session);
    },
    open(session, seq = 1) {
      const s = read(session);
      s.openTurnStartSeq = seq;
      s.lastTurn += 1;
    },
    close(session) {
      read(session).openTurnStartSeq = null;
    },
  };
}

/** 假 Agent：session.header 是 cwd / 子代理判定 / 血缘（parentSession）的唯一来源 */
function makeAgent(id, cwd = 'D:\\proj\\demo', headerExtra = {}) {
  // 桩会话带一份「日志」：seq = 已写条数（下一个可写下标），eventAt(i) 按真 DSH 语义取第 i 条。
  // 插件按官方契约从日志推导回合状态（不依赖事件一定送到），所以这里必须真的记下来。
  const events = [];
  const session = {
    id,
    header: { cwd, ...headerExtra },
    __events: events,
    get seq() {
      return events.length;
    },
    eventAt(i) {
      return Number.isInteger(i) && i >= 0 && i < events.length ? events[i] : null;
    },
  };
  return { id, session };
}

// --- 断言 -------------------------------------------------------------------

const failures = [];
function check(ok, label) {
  if (ok) {
    console.log(`  ok   ${label}`);
  } else {
    console.log(`  FAIL ${label}`);
    failures.push(label);
  }
}

const terminals = (type) => sent.filter((e) => e.type === type && !e.rejected);
const activities = () => sent.filter((e) => e.type === 'activity');
/** 终态退避重投定时器（语义判定，不数无关定时器的总数，§4.18）：延迟 ≥ 1s */
const RETRY_BACKOFF_MIN_MS = 1000;
const backoffTimers = () => timers.filter((t) => t.ms >= RETRY_BACKOFF_MIN_MS);
/** 调试开关：SIM_DEBUG=1 时打印每个用例末尾的到达事件，便于定位桩本身的问题 */
const debug = process.env.SIM_DEBUG === '1';
function dump(label) {
  if (!debug) return;
  console.log(`  [debug] ${label}: ${JSON.stringify(sent.map((e) => `${e.type}${e.tool_name ? `(${e.tool_name})` : ''}${e.rejected ? ':rejected' : ''}`))}`);
}

// --- 载入被测插件 -----------------------------------------------------------

// 载入被测插件：Windows 盘符路径要转成 file URL，否则 import 解析成相对路径
import { pathToFileURL } from 'node:url';

const plugin = await import(pathToFileURL(pluginPath).href);
if (typeof plugin.name !== 'string' || typeof plugin.apply !== 'function') {
  console.error('plugin does not export name + apply');
  process.exit(2);
}

// --- 用例 1：首投失败 → 自动重投并送达 -------------------------------------

console.log('case 1: 终态首投失败（503）必须重投并送达');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-1');

  // 一个回合：turn/start 建档 → turn/end 落盘 → 静止边沿收尾
  // （收尾不再挂在 agent/turn-stopping 上：官方说它「can steer to keep it open」）
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  behavior = 'fail'; // daemon 事件通道打满
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  const first = sent.filter((e) => e.type === 'run_completed');
  check(first.length === 1 && first[0].rejected, '首投确实发出且被 503 拒绝（503 不算送达）');
  check(terminals('run_completed').length === 0, '被拒的首投不算送达');

  behavior = null; // daemon 恢复
  await advance();
  check(
    terminals('run_completed').length === 1,
    '重投把 run_completed 真正送达',
  );
  check(
    sent.filter((e) => e.type === 'run_completed').length === 2,
    '重投次数 = 1（不重复轰炸）',
  );
  check(
    sent.filter((e) => e.type === 'run_completed').every((e) => e.session_id === 'sess-1'),
    '重投带同一个 session_id（daemon 才认得出是哪个会话结束了）',
  );
  check(
    new Set(sent.filter((e) => e.type === 'run_completed').map((e) => e.id)).size === 1,
    '重投复用同一事件 id（配合 daemon 按 id 去重，绝不双发通知）',
  );
  dump('case1');
}

// --- 用例 2：首投成功 → 只报一次 -------------------------------------------

console.log('case 2: 首投成功时终态只报一次');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-2');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  // 回合收尾闸门照常触发（现在只作回合号提示，不再当场发终态）
  await ctx.dispatch('agent/turn-stopping', { agent, turn: 1, signal: {} });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();

  check(terminals('run_completed').length === 1, '首投成功后再无重复终态');
  check(
    sent.filter((e) => ['run_completed', 'run_failed', 'run_aborted'].includes(e.type)).length === 1,
    '整个回合只有一条终态事件',
  );
}

// --- 用例 3：首投失败 + status idle 兜底仍能补报 ----------------------------

console.log('case 3: 首投失败时 status idle 兜底不得被「已终态」挡住');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-3');

  // 真实回合：turn/start 先行（过建档门槛），兜底路径才对真实回合生效
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  behavior = 'throw'; // daemon 没起来：连接被拒
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await advance(); // 宽限窗到点 → 首投 + 重投全部被拒 → 放弃（但账要清干净）
  check(terminals('run_completed').length === 0, '连接被拒时终态没有到达 daemon');

  // 兜底信号：终态没送达，随后的 status idle 必须再报一次
  // （旧实现这里被「投递前就置位的已终态」/「没清干净的待投账」挡住 → 这条终态永久丢失）
  behavior = null;
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  check(
    terminals('run_completed').length >= 1,
    'status idle 之后终态最终送达（旧实现会一条都不剩）',
  );
  check(
    sent.filter((e) => e.type === 'run_completed').length <= 2,
    '补报与重投合计不超过 2 次（不重复轰炸）',
  );
  dump('case3');
}

// --- 用例 4：心跳不进终态重投队列 -------------------------------------------

console.log('case 4: 心跳失败不重试（丢了下一跳会补）');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-4');

  // 回合开着才有心跳：turn/start 先建档（这条不带 keepalive，允许建行）
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await flush();
  const before = activities().length;
  check(before === 1, `turn/start 建档一条 activity（实际 ${before} 条）`);

  behavior = 'fail';
  await ctx.dispatch('session/event', agent.session, { type: 'tool/call', data: { name: 'Bash' } });
  await flush();
  check(activities().length === before + 1, `工具心跳首投发出（实际新增 ${activities().length - before} 次）`);
  check(terminals('run_failed').length === 0 && terminals('run_completed').length === 0, '心跳不产生终态事件');

  // 心跳只允许一次即时补投，且**不进终态重投队列**（1s 起的退避一轮都不该有）
  await advance(400);
  check(activities().length === before + 1, `400ms 内不补投（实际 ${activities().length - before} 次）`);
  await advance();
  check(activities().length === before + 2, `心跳最多补一次就放弃（实际 ${activities().length - before} 次）`);
  dump('case4');
}

// --- 用例 5：新回合作废上一回合的待投终态（但不能误伤新回合自己） ------------

console.log('case 5: 新回合开始要取消上一回合还挂着的重投');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-5');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  behavior = 'fail';
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await flush();
  behavior = null;
  // 重投还没跑（1s 退避未到点），用户立刻又发了一轮 → 新回合开始，旧终态作废。
  // 按**语义**断言退避定时器的存在/取消（§4.18：不断言无关定时器的总数——
  // 心跳的 500ms 补投定时器排不排队与本用例无关）
  check(backoffTimers().length >= 1, `待投终态的退避定时器已排上（实际 ${backoffTimers().length} 个）`);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  check(backoffTimers().length === 0, `新回合开始时旧终态的重投被取消（剩余 ${backoffTimers().length} 个退避定时器）`);
  await advance();
  check(
    sent.filter((e) => e.type === 'run_completed' && !e.rejected).length === 0,
    '上一回合的迟到终态不得再投递（否则会把刚开始的新回合从状态表里误删）',
  );

  // 新回合自己必须照常报得出去（取消旧终态不能把新回合的终态一起去重掉）
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 2, reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  check(
    terminals('run_completed').length === 1,
    '新回合自己的终态照常送达（去重只按回合，不误伤下一回合）',
  );
  dump('case5');
  check(
    ctx.listenerCount('agent/status') === 1,
    'status 监听只注册一次（避免同一事件被处理多遍）',
  );
  const waterfall = await ctx.dispatchWaterfall('approval/request', { agent, toolName: 'bash' });
  check(waterfall.called, 'waterfall 观察者把审批请求交还下去');
  dump('case5');
}

// --- 用例 6：heldCompletions——子代理在跑时完成要扣住，fan-out 结束才放行 ----

console.log('case 6: 后代活跃时「完成」必须扣住，全部结束后补投');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const parent = makeAgent('sess-p');
  const child = makeAgent('sess-c', 'D:\\proj\\demo', { origin: 'subagent', parentSession: 'sess-p' });

  await ctx.dispatch('session/event', parent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent: parent, status: 'running' });
  await ctx.dispatch('session/event', child.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent: child, status: 'running' });
  // 父回合先结束，但子代理还在跑：「任务完成」是提前的，必须扣住
  await ctx.dispatch('session/event', parent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent: parent, status: 'idle' });
  await advance();
  check(
    terminals('run_completed').length === 0,
    '子代理还在跑时父完成不发（heldCompletions 扣住）',
  );

  // 子代理结束：先发它自己的终态，随即放行父完成
  await ctx.dispatch('session/event', child.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent: child, status: 'idle' });
  await advance();
  const dones = terminals('run_completed');
  check(dones.length === 2, '子代理终态 + 放行的父终态都送达');
  check(
    dones.length === 2 && dones[0].session_id === 'sess-c' && dones[1].session_id === 'sess-p',
    '顺序正确：子代理终态在前、父完成补投在后',
  );
  check(
    dones.length === 2 && dones[0].is_subagent === true && dones[1].is_subagent === false,
    '子代理终态带 is_subagent（daemon 侧据此不亮绿屏）',
  );
  dump('case6');
}

// --- 用例 7：等待类事件必须真的发出去 + 子代理心跳带血缘 ---------------------

console.log('case 7: 等待授权/等待输入真上报（回归 post() 未定义），子代理心跳带 parent_session_id');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-w');
  await ctx.dispatch('agent/status', { agent, status: 'running' });

  const out = await ctx.dispatchWaterfall('approval/request', { agent, toolName: 'Bash', reason: '删库' });
  await flush();
  check(out.called, '审批请求交还下去');
  const perms = sent.filter((e) => e.type === 'permission_required' && !e.rejected);
  check(perms.length === 1, 'permission_required 真的发出去了（旧 bug：post 未定义被 catch 吞掉）');
  check(perms.length === 1 && perms[0].message.includes('Bash'), '正文带工具名');

  // 子代理的工具心跳：不得再被跳过，且带 is_subagent + 血缘（祖先传播存活用）
  const child = makeAgent('sess-c2', 'D:\\proj\\demo', { origin: 'subagent', parentSession: 'sess-w' });
  await ctx.dispatch('session/event', child.session, { type: 'tool/call', data: { name: 'Read' } });
  await flush();
  const childAct = sent.filter((e) => e.type === 'activity' && e.session_id === 'sess-c2');
  check(childAct.length === 1, '子代理工具心跳也上报（驱动面板/流光）');
  check(childAct.length === 1 && childAct[0].is_subagent === true, '子代理心跳带 is_subagent');
  check(
    childAct.length === 1 && childAct[0].parent_session_id === 'sess-w',
    '子代理心跳带 parent_session_id（daemon 据此向祖先传播存活）',
  );
  dump('case7');
}

// --- 用例 8：其余 session/event 折叠成节流保活，user/* 不算活跃 --------------

console.log('case 8: session/event 其余事件按「还活着」处理（节流），user/* 除外');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-l');

  await ctx.dispatch('session/event', agent.session, { type: 'assistant/message', data: {} });
  await flush();
  check(
    sent.filter((e) => e.type === 'tool_finished').length === 1,
    'assistant/message 折叠成一条保活心跳（tool_finished 语义，只续命）',
  );
  await ctx.dispatch('session/event', agent.session, { type: 'step/start', data: {} });
  await flush();
  check(
    sent.filter((e) => e.type === 'tool_finished').length === 1,
    '节流窗内不重发（一轮几十条事件只发一条）',
  );
  await ctx.dispatch('session/event', agent.session, { type: 'user/message', data: {} });
  await flush();
  check(
    sent.filter((e) => e.type === 'tool_finished').length === 1,
    'user/* 不算 agent 活跃，不发保活',
  );
  await ctx.dispatch('session/event', agent.session, { type: 'approval/decided', data: {} });
  await flush();
  check(
    sent.filter((e) => e.type === 'tool_finished').length === 1,
    'approval/* 审计不发保活（会把等待色打回思考色）',
  );
  dump('case8');
}

// --- 用例 9：预创建的空会话不发心跳（DSH rc.3+ 开屏建档防护）----------------

console.log('case 9: 预创建的空会话不建档——生命周期事件不放行，turn/start 之后照常');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-empty');

  // 开屏：DSH 预创建会话后 agent 照样走 running，且可能发零星生命周期事件
  // （实测 ProNet 2026-09-28：第一版「任何事件都算」的门槛挡不住）
  await ctx.dispatch('session/event', agent.session, { type: 'session/created', data: {} });
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await flush();
  check(activities().length === 0, '空会话的 running 不发心跳（不建档，无「思考中」幻影）');
  check(
    sent.filter((e) => e.type === 'tool_finished').length === 0,
    '生命周期事件不折叠成保活（不算真实活动）',
  );
  check(sent.length === 0, '空会话什么都不上报');

  // 空会话唤醒即眠：兜底终态也要被门槛挡住（不报假完成）
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  check(terminals('run_completed').length === 0, '空会话的 running→idle 不报假完成');

  // 用户真的发了一轮：重新唤醒 → turn/start → 此刻才建档
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  await flush();
  check(
    activities().length === 1,
    `turn/start 才建档（一条 activity，实际 ${activities().length} 条）`,
  );

  // 回合收尾（模拟 turn-stopping 丢失）：idle 兜底对**已过门槛**的会话必须放行
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  check(terminals('run_completed').length === 1, '真实回合的完成照常送达（兜底不误伤真实回合）');
  dump('case9');
}

// --- 用例 10：回合外到达的会话级事件不得再上报（幻影行的来源） ---------------

console.log('case 10: 官方回合投影可用时，回合外的会话级事件一律不上报');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-phantom');

  // 一个真跑过的回合：turn/start → turn/end → 静止
  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  proj.close(agent.session);
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  const settled = sent.length;
  check(terminals('run_completed').length === 1, '这一回合照常收尾一次');

  // 回合外到达的会话级事件（title / end-seed / command / model-selection / compaction…）：
  // 旧实现把「任何事件」当「agent 还活着」，于是在 daemon 里凭空建出一条没有终态的
  // 「思考中」——挂满 10 分钟被判死（红光 + 失败音）。这就是用户看到的那条幻影。
  for (const type of ['session/title', 'session/end-seed', 'command/done', 'model/selection', 'compaction/end', 'assistant/message']) {
    await ctx.dispatch('session/event', agent.session, { type, data: {} });
    await flush();
  }
  check(
    sent.length === settled,
    `回合外的会话级事件不得再上报（多出 ${sent.length - settled} 条 = 幽灵行的来源）`,
  );
  check(terminals('run_completed').length === 1, '也不得再补一条终态');
  dump('case10');
}

// --- 用例 11：回合开着时的静默窗口要定期补保活（keepalive，不建档） ----------

console.log('case 11: 回合开着时的静默窗口定期补保活心跳（keepalive）');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-silent');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await flush();
  const built = activities();
  check(built.length === 1, `turn/start 建档一条 activity（实际 ${built.length} 条）`);
  check(built.length === 1 && built[0].keepalive !== true, '建档心跳不带 keepalive（允许 daemon 新建条目）');

  // 长工具 / 长子代理的静默窗口：一条会话事件都没有，靠周期心跳续命
  const beat = (t) => t.ms >= 20000;
  check(await fireTimer(beat), '回合开着时排上了周期保活定时器');
  let beats = sent.filter((e) => e.type === 'tool_finished');
  check(beats.length === 1 && beats[0].keepalive === true, '静默窗口补的保活带 keepalive（daemon 只续命不建档）');
  check(await fireTimer(beat), '回合还开着就继续补下一跳');
  beats = sent.filter((e) => e.type === 'tool_finished');
  check(beats.length === 2, `周期心跳持续补（实际 ${beats.length} 条）`);

  // 回合收尾：心跳停、静止边沿按 turn/end 的 reason 收尾一次
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  proj.close(agent.session);
  const beatsAtClose = sent.filter((e) => e.type === 'tool_finished').length;
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance(60000);
  check(
    sent.filter((e) => e.type === 'tool_finished').length === beatsAtClose,
    '回合关了就不再补保活（否则会在已收档的会话上续命）',
  );
  check(terminals('run_completed').length === 1, '静止边沿按 turn/end 收尾一次');
  dump('case11');
}

// --- 用例 12：turn-stopping 被 steer 打开时不得谎报完成，也不得吞掉真终态 ----

console.log('case 12: turn-stopping 只是「本可收尾」的闸门，不是终态');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-steer');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  // 官方语义：turn-stopping「runs before an otherwise completed turn closes and
  // can steer to keep it open」——运行还在继续，此刻报完成就是谎报
  await ctx.dispatch('agent/turn-stopping', { agent, turn: 1, signal: {} });
  await ctx.dispatch('session/event', agent.session, { type: 'tool/call', data: { name: 'Bash' } });
  await flush();
  check(terminals('run_completed').length === 0, 'turn-stopping 不得当场报完成（运行还在继续）');

  // 真收尾：turn/end + 静止边沿 → 恰好一条（不能被 turn-stopping 那次去重吞掉）
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  proj.close(agent.session);
  await ctx.dispatch('agent/status', { agent, status: 'idle' });
  await advance();
  check(terminals('run_completed').length === 1, '真终态照常送达（旧实现会在这里被吞掉）');
  dump('case12');
}

// --- 用例 13：会话被关闭要补一条「已中止」，把 daemon 的条目收掉 ------------

console.log('case 13: 会话销毁时补一条 run_aborted（否则条目挂到 10 分钟判死）');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-close');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  await ctx.dispatch('agent/disposed', { agent });
  await advance();
  const aborts = sent.filter((e) => e.type === 'run_aborted' && !e.rejected);
  check(
    aborts.length === 1 && aborts[0].session_id === 'sess-close',
    `会话关闭补一条 run_aborted（实际 ${aborts.length} 条）`,
  );
  check(timers.length === 0, `收尾后不留定时器（实际 ${timers.length} 个）`);
  dump('case13');
}

// --- 用例 14：idle 边沿缺失时，turn/end + 宽限窗仍要收尾 --------------------

console.log('case 14: agent/status idle 边沿缺失时，turn/end + 宽限窗仍要收尾');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-noidle');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: {} });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  // 回合正常收尾，但**故意不发** agent/status idle（实测 2026-09-30：DSH 重启后
  // 连续两个回合都没有 idle 边沿，只靠边沿收尾会让终态一条都报不出去）
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  proj.close(agent.session);
  await advance();
  check(
    terminals('run_completed').length === 1,
    `idle 边沿缺失时仍要收尾（实际 ${terminals('run_completed').length} 条）`,
  );
  await advance();
  check(terminals('run_completed').length === 1, `只报一次（实际 ${terminals('run_completed').length} 条）`);
  dump('case14');
}

// --- 用例 15：被附着但没开回合的子代理不得永久扣住父完成 --------------------

console.log('case 15: 被附着但没开回合的子代理不得永久扣住父完成');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const parent = makeAgent('sess-p2');
  const child = makeAgent('sess-c2', 'D:\\proj\\demo', { origin: 'subagent', parentSession: 'sess-p2' });

  await ctx.dispatch('session/event', parent.session, { type: 'turn/start', data: {} });
  await ctx.dispatch('agent/status', { agent: parent, status: 'running' });
  // 子代理被「附着」（status running）但**没有开回合**：它不是「在干活」，
  // 不能因为它把父会话的完成永久扣住（`running` 会在附着/唤醒时翻起，与回合无关）
  await ctx.dispatch('agent/status', { agent: child, status: 'running' });
  await ctx.dispatch('session/event', parent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await ctx.dispatch('agent/status', { agent: parent, status: 'idle' });
  await advance();
  check(
    terminals('run_completed').length === 1,
    `父完成照常送达（不被空转子代理扣住，实际 ${terminals('run_completed').length} 条）`,
  );
  dump('case15');
}

// --- 用例 16：turn/end 事件没送到插件时，靠日志对账仍要收尾 ------------------

console.log('case 16: turn/end 事件没送到插件时，靠日志对账仍要收尾');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const proj = makeProjections();
  const ctx = makeCtx({ projections: proj });
  plugin.apply(ctx);
  const agent = makeAgent('sess-noev');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: { turn: 1 } });
  proj.open(agent.session, 1);
  await ctx.dispatch('agent/status', { agent, status: 'running' });
  // 回合收尾**只落进了日志**：插件既没收到 turn/end，也没收到 agent/status idle
  // （2026-09-30 实测就是这个状态：卡片挂死思考中、光带一直亮）
  agent.session.__events.push({ type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  proj.close(agent.session);
  await advance();
  check(terminals('run_completed').length === 1, `靠日志对账收尾（实际 ${terminals('run_completed').length} 条）`);
  await advance();
  check(terminals('run_completed').length === 1, `只报一次（实际 ${terminals('run_completed').length} 条）`);
  dump('case16');
}

// --- 用例 17：agent/status 一条都不来时也要能收尾 ---------------------------

console.log('case 17: agent/status 一条都不来时（只有回合级事件）也要能收尾');
{
  sent.length = 0;
  timers.length = 0;
  behavior = null;
  const ctx = makeCtx();
  plugin.apply(ctx);
  const agent = makeAgent('sess-nostatus');

  await ctx.dispatch('session/event', agent.session, { type: 'turn/start', data: { turn: 1 } });
  await ctx.dispatch('session/event', agent.session, { type: 'turn/end', data: { turn: 1, reason: { kind: 'completed' } } });
  await advance();
  check(terminals('run_completed').length === 1, `不靠 status 也能收尾（实际 ${terminals('run_completed').length} 条）`);
  dump('case17');
}

// --- 汇总 -------------------------------------------------------------------

console.log('');
if (failures.length > 0) {
  console.error(`FAILED: ${failures.length} 项断言未通过`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exitCode = 1;
} else {
  console.log('plugin delivery simulation: all checks passed');
}
