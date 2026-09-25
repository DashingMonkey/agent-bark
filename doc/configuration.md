# 配置与运行参考

开发/排障需要的参考内容：配置字段、流光状态推导、项目结构、开发说明。
**「每种 agent 怎么接入」看 [agent-integration.md](agent-integration.md)。**

---

## 配置文件

位于平台配置目录，一般通过 GUI 修改，也可手工编辑：

| 平台 | 路径 |
| --- | --- |
| Windows | `%APPDATA%\agent-bark\config.json` |
| macOS | `~/Library/Application Support/agent-bark/config.json` |
| Linux | `~/.config/agent-bark/config.json` |

```jsonc
{
  "server": { "port": 36573, "token": "首次启动自动生成" },
  // 状态音效：每个状态可选一个内置音效，effect 为空 = 不响（默认）。
  // 可选：chime 风铃 / confirm 确认 / success 成功 / drop 水滴 /
  //       pluck 弹拨 / glass 玻璃 / error 错误 / deep 低叮
  "notify": {
    "thinking":   { "effect": "", "plays": 1 },   // 思考（新回合开始）
    "warning":    { "effect": "", "plays": 1 },   // 警告（等待确认 / 等待输入）
    "completed":  { "effect": "", "plays": 1 },   // 完成
    "terminated": { "effect": "", "plays": 1 }    // 终止（失败 / 心跳判死）
  },
  "rules": {
    "aggregate_window_ms": 1500,       // 聚合窗口：同 agent 同类事件在窗口内合并为一条
    "quiet_hours": [],                 // 免打扰时段，如 ["23:00-08:00"]（可跨零点）
  },
  "channels": {
    "bark":    { "enabled": false, "url": "https://api.day.app/YourKey" },
    "webhook": {
      "enabled": false,
      "url": "https://open.feishu.cn/...",
      "method": "POST",
      "template": "{\"title\": \"{title}\", \"body\": \"{body}\", \"agent\": \"{agent}\"}"
    }
  },
  "agents": [{ "kind": "claude-code", "enabled": true }],
  "glow": {
    "enabled": true,              // 屏幕光效总开关（启动 / 停止）
    "edge": true,                 // 边缘光效总开关：关掉后只收边缘灯带，全屏特效照常
    "fullscreen": true,           // 全屏特效总开关：每次颜色亮起时整屏补一次特效
    "monitors": "all",            // all=全部显示器 / 数字=第 N 块屏（UI 的「1.显示器1(1920*1080)」）
    // 边缘 / 全屏的类型按状态各选各的（GUI「屏幕光效」两列四行，与「声音」区同款）：
    //   edge_effects  ：none=不亮 / breathing=呼吸 / comet=流光
    //   burst_effects ：none=不放 / fog=雾散 / scan=扫描
    "edge_effects":  { "thinking": "breathing", "warning": "breathing", "completed": "breathing", "terminated": "breathing" },
    "burst_effects": { "thinking": "fog",       "warning": "fog",       "completed": "fog",       "terminated": "fog" }
  },
  "widget": {
    "enabled": true,              // 悬浮窗总开关（关闭后不创建窗口；GUI「悬浮窗」页 / 右键菜单改）
    "x": -1,                      // 卡片左上角（逻辑像素）；-1 = 未放置过，按主屏右上角打开
    "y": -1,
    "pinned": false,              // 固定位置：true 时不响应拖动（悬浮窗右键菜单里切换）
    "auto_hide": true             // 自动隐藏（默认开）：口径见「悬浮窗自动隐藏」
  }
}
```

**约定与注意事项**：

- `agents` 段存的是**字符串 id**（不是枚举），所以老配置里残留已下线 agent 的 id 不会导致解析失败；
- 手改配置漏键不会解析失败，按默认值补齐；文件不是合法 JSON 时**拒绝写入**（避免覆盖你的配置），UI 会提示「配置读取失败」；
- 端口 / 鉴权 token 改过之后，运行中的事件服务器仍用启动时的值——UI 会提示「需重启生效」；
- 配置采用原子写入（临时文件 + fsync + rename）；
- `widget.{x,y,pinned}` 由悬浮窗自己的拖动 / 右键菜单写入，主窗口保存整份配置时会自动保留这三项（不会被旧值覆盖）；
- 旧版的 `glow.effect` / `glow.fullscreen_effect`（全局类型）只作**迁移源**：升级后四态各按它补齐一次，之后再改它们不再生效（运行时只读 `edge_effects` / `burst_effects`）。

### Webhook 模板占位符

`{title}` `{body}` `{agent}` `{event}` `{project}`

### 悬浮窗自动隐藏

`widget.auto_hide`（GUI「悬浮窗」页的「自动隐藏」开关，默认开）：悬浮窗安静时自己收起，有事时自己弹出。

- **安静**（持续 2 秒后淡出隐藏）：没有任何行；或所有行都是思考类（思考中 / 执行工具）；或只剩「已中止」驻留行。
- **有事**（立即淡入弹出，不等 2 秒）：等待确认 / 等待输入（警告）、任务完成、任务失败（含心跳判死）。
- **新会话出现**（新的事件流上卡）：弹出「一下」作回执（照常 2 秒后随安静隐藏）——刚发的任务被接手时能看见卡片亮起。
- **手动中止（`run_aborted`）不弹出、也不阻止隐藏**——与屏幕光效「手动中止不提醒、直接收起」同一口径。
- 任务完成 / 失败的驻留行约 6 秒后过期，之后 2 秒淡出——典型体验是「弹出 → 停留几秒 → 自己消失」。
- 悬停 / 拖动 / 右键菜单打开期间挂起隐藏，离开后重新计 2 秒；隐藏只是展示态，`enabled` 仍为开，出事照样弹出。

---

## 流光状态怎么来的

流光是**双通道**的：边缘灯带按会话表聚合推导（与 GUI「运行中会话」同一份数据），
全屏特效则是独立的**事件通道**——两层互不覆盖。

**边缘（常驻）**多会话并存时以最需要你处理的为准：

| 优先级 | 条件 | 角色 |
| --- | --- | --- |
| 1 | 任一会话 `等待确认 / 等待输入` | 🟠 **警告色**（呼吸） |
| 2 | 任一会话 `思考中 / 执行工具` | 🔵 **思考色**（呼吸） |
| 3 | 无活跃会话，且最近事件为 `run_completed` | 🟢 **完成色**（停留后淡出） |
| 3 | 无活跃会话，且最近事件为 `run_failed`，或运行中会话心跳静止超 10 分钟被判死 | 🔴 **终止色**（心跳双脉冲，停留后淡出） |
| 3 | 无活跃会话，且最近事件为 `run_aborted`（用户主动中止） | ⚫ 收起（不亮） |

**全屏（一次性）**按事件触发：多会话并行时，**任一会话**完成 / 终止（含判死），
哪怕其余会话还在跑，也会立刻以事件角色（完成 / 终止）补放一次全屏特效（类型按该角色
那一行的 `burst_effects`）；边缘继续表达剩余
会话的聚合状态。「有一个完成了」是值得立刻知道的事件，「还有的在跑」是持续状态，两者不互斥。

> 配色（`#rrggbb`）与角色名的对应只写在 `app/src-tauri/src/glow.rs` 的
> `GlowState::color` 一处，并有配套的锁配色测试；改色不需要动本文档。
> 边缘的具体表现按**状态**各选各的（`glow.edge_effects`：「无」/「呼吸」/「流光」），
> 全屏同理（`glow.burst_effects`：「无」/「雾散」/「扫描」），选「无」时对应通道不亮；
> 打开 `glow.fullscreen` 且触发角色那行没选「无」时，每次颜色亮起（状态真的变化、
> 终态重发，或多会话并行时的完成 / 终止事件）会在整屏补放一次，渐显渐隐；
> 同一状态的重复心跳不会重放。
>
> 唯一不重放的例外是**窗口重建**：显示器数量变化（热插拔 / 改「生效显示器」）时覆盖窗会
> 全量重建，此时注入的初始态只是一份「现在长什么样」的快照——颜色照常显示（还在跑的
> 会话必须看得见），但不会借它白闪一次整屏（见 `glow.rs` 的 `create_all` / `replay_burst`）。
>
> 覆盖窗是**置顶 + 点击穿透**的，并且**把整块显示器盖住**（底边只向外多 1 物理像素，见
> `rect_of`）——系统会按「无边框全屏应用」对待它（判据是「窗口是否遮挡整个桌面」，与多出来的
> 那 1px 无关），任务栏自己就退到下面去了。但任务栏刷新自己（被点击、通知闪烁、托盘变化）时
> 会短暂升到最上层，而屏幕**底部**那条光带恰好整条落在它的条带里（边条 + 向内约 60px 辉光，
> Win11 任务栏约 48px），表现就是「只有下方没有光效」。因此灯常驻亮着时（思考 / 等待）每 5s
> 会把覆盖窗重新置顶兜底一次（`glow::raise` / `keep_topmost`）；空闲与终态不抢层级。底部那圈
> 辉光会叠在任务栏上——点击穿透，不影响任务栏操作。
>
> 「屏幕光效」页的预览是**临时接管**：真实事件、保存配置、托盘重置或点「熄灭」都会立刻
> 结束它，几秒后也会自动恢复；连点多次预览只认**第一次预览前**的状态——否则恢复时会把
> 上一次的预览态当成真实状态，灯就再也灭不掉。

> 只有所有会话都结束后，**边缘**才会转完成 / 终止色——A 完成了但 B 还在跑时，边缘报完成
> 是骗人；但全屏特效不等：A 完成的瞬间就会补放一次完成色雾散。「心跳静止超时判死」覆盖的是
> agent 进程被直接杀掉的场景（那种情况下不会有 Stop 事件）；判死由后台每 30s 巡检触发，
> 可以用 `BARK_STALE_AFTER_MS` 调时长（见下）。
>
> **用户主动中止是独立的第三类终态**（`run_aborted`）：不亮终止色（不是失败）也不亮完成色
> （没跑完），没有别的会话在跑时**直接收起光效**；同时**不通知**（自己按的停止，人就在
> 键盘前）。各 agent 的中止信号不一样，已统一到这一类：
> - Qoder / Qoder CN：`SessionEnd`（实测只发它、不发 `Stop`）；
> - DeepSeek Harness：`turn/end` 的 reason 为 `aborted` / `interrupted`；
> - ZCode：它中断时**一个 hook 都不发**，由 daemon 读它的本地日志推断（非官方机制，见 agent-integration.md §6）；
> - WorkBuddy（监控型）：运行中的会话被**归档**。
>
> 正常回合里 `Stop` 之后紧跟的那条中止信号会被状态机当**终态回声**抑制，不会重复处理、也不会把完成色压成终止色。

### 状态音效什么时候响

「通知」页「声音」里给四个状态配的音效，与流光同一套状态语义：

| 状态 | 触发 |
| --- | --- |
| 思考 | 新回合开始（不带工具名的心跳，即 `UserPromptSubmit`）；同回合的工具心跳不重复响 |
| 警告 | `permission_required` / `input_required`（等你确认 / 输入） |
| 完成 | `run_completed`（多会话并行时某一个会话完成也会响） |
| 终止 | `run_failed`，或运行中会话心跳静止超时被判死 |

用户主动中止（`run_aborted`）不响（自己按的停止）；子代理事件不响；托盘「静音」与免打扰时段内不响。默认全部未选 = 完全静音；播放次数范围 1~10。

---

## 项目结构

```
agent-bark/
├── crates/
│   ├── bark-core/       # 领域模型：AgentKind、EventKind、NormalizedEvent、BarkConfig
│   ├── bark-adapters/   # Agent 适配层：Hook 型（写配置）+ 插件型 + Watch 型（轮询本地库）
│   ├── bark-cli/        # hook 子命令：读 stdin → 归一化 → POST daemon，离线落盘兜底
│   ├── bark-server/     # 本地事件接收服务（axum，127.0.0.1 + token 鉴权）
│   └── bark-channels/   # 通知渠道：状态音效、Bark (iOS)、通用 Webhook 模板
├── doc/                 # 文档：agent-integration.md（接入与排查）、configuration.md（本文）
└── app/                 # Tauri 2 桌面应用（Vue 3 + Vite + TypeScript）
    ├── src/             # 前端：事件流 / Agents 接入 / 设置 / 悬浮窗 / 通知（屏幕光效 + 声音）/ 第三方通知 / 关于 页面
    │   ├── glow.ts      # 屏幕光效覆盖层入口（对应 glow.html，Vite 多页构建）
    │   └── widget.ts    # 桌面悬浮窗入口（对应 widget.html）：活动卡片 + 自动隐藏状态机
    └── src-tauri/       # Rust 侧：托盘、推送分发、命令、状态管理
        ├── glow.rs      # 光效覆盖窗口（透明/穿透/置顶）+ 状态机 + 显示器枚举
        └── update.rs    # 关于页检查更新（Gitee 主源 + GitHub 备源并行查最新版本）
```

---

## 开发说明

- Rust workspace 共 6 个成员 crate，核心 crate 均带单元测试：`cargo test --workspace`
  - 本机离线构建：默认 `~/.cargo`（rsproxy sparse）+ `--offline` 即可；仓库里的 `.cargo-home` 是更早准备的本地 registry，**不全**（缺 `serde_yaml` 等），别拿它当 `CARGO_HOME`。
- 前端：`pnpm --dir app build`（含 `vue-tsc --noEmit` 类型检查）。

### ⚠️ 构建桌面应用必须走 Tauri CLI（踩过的坑）

```powershell
cd app
pnpm tauri build              # 出二进制 + NSIS 安装包
pnpm tauri build --no-bundle  # 只要可执行文件（跳过打包，快一些）
pnpm tauri dev                # 开发模式（需要 vite 起在 5173）
```

**不要用裸 `cargo build --release`**。`tauri.conf.json` 里同时配了 `devUrl`（`http://localhost:5173`）和 `frontendDist`（`../dist`），选哪个由 Tauri CLI 注入的环境决定：直接 `cargo build --release` 出来的二进制会去连 `devUrl`，启动后页面显示 **「localhost 拒绝连接」**（此时 vite 并没在跑）。前端资源必须由 CLI 构建才会内嵌。

**怎么判断手上的二进制是哪种**（注意：在 exe 里搜 `localhost:5173` 区分不出来——Tauri 会把整份配置内嵌进二进制，正常的生产构建里也有这个字符串）：

1. 启动后页面报「localhost 拒绝连接」→ dev 模式；
2. 看构建脚本输出：`target/release/build/agent-bark-app-*/output` 里出现 `cargo:rustc-cfg=dev` 就是 dev 模式；
3. 想确认「前端确实内嵌了」：在 exe 里搜 `dist/assets/` 下的资源名（如 `assets/main-BlPIIZQF.js`），搜得到即已内嵌；
4. 最彻底的一招：起一个假 dev server 占住 `127.0.0.1:5173`（只记日志），再启动 exe——如果它来请求这个端口，就说明是 dev 模式构建。

- Hook 侧调试：设 `BARK_DEBUG=1`，hook 会在 stderr 打印上报结果（含失败原因，不含 token 明文）。
- 「关于」页检查更新的联调：设 `BARK_UPDATE_REPO=owner/name` 可把两个平台的查询指到别的仓库（默认 `dashingmonkey/agent-bark`）——指到一个**已发过版本**的仓库就能验证「发现新版本 → 下载新版本」这条路径，不必等本仓库先发版。
- 会话判死时长：默认「心跳静止 10 分钟」判僵死，可用 `BARK_STALE_AFTER_MS`（毫秒，最小 5000，如 `60000`）覆盖。后台每 30s 巡检一次并据结果改流光/清会话——agent 被用户中断或被杀之后**可能再也不发任何事件**（实测 ZCode 手动终止零事件），判死不能只等「下一条事件」，否则流光会一直停在思考色。
- Hook 进程的三条硬约束（见 `crates/bark-cli/src/lib.rs` 模块注释）：永远退出码 0；读 stdin 必须带超时；只读配置、绝不写盘。
- 验证某个 agent 到底读哪份配置 / 哪些事件真的会触发：见 [agent-integration.md 的「探针验证法」](agent-integration.md#13-探针验证法文档与实测冲突时用)。
