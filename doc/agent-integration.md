# Agent 接入方法与排查

本文是**每种 agent 的接入细节 + 出问题时的排查手册**：写哪个配置文件、注册哪些事件、需要你在应用侧做什么、以及接不进来该怎么一步步查。

改 adapter 之前请先读这里。**文档与实测冲突时以实测为准**——文中每处结论都标了实测日期/版本，官方文档滞后是常态（已有两例：Qoder CN 的配置路径、Qoder 的授权事件）。

---

## 0. 统一的接入模型

三类接入方式，本质都是「让 agent 在事件发生时调起 `AgentBark.exe hook` 或直接 POST `/event`」：

| 类型 | 做法 | 使用者 |
| --- | --- | --- |
| **Hook 型** | 写 agent 自己的 hooks 配置，事件点上 agent 执行 `"<AgentBark.exe>" hook --agent <id> --event <事件名>`，hook 进程读 stdin JSON 后上报 | Claude Code、TraeCode、CodeBuddy、Qoder（含国内版 Qoder CN）、Codex、ZCode |
| **插件型** | 生成插件文件并登记到宿主的插件配置里，插件内部直接 POST `/event` | OpenCode、DeepSeek Harness |
| **监控型** | 轮询 agent 本地会话库（SQLite，TraeWork 是解密后轮询），靠状态跃迁推断事件 | WorkBuddy、TraeWork |

**统一事件**（所有 agent 归一化到这 9 类；`activity` 与 `tool_finished` 只用于状态推导，不通知、不入历史）：

| 统一事件 | 含义 | 通知 |
| --- | --- | --- |
| `session_start` | 会话开始 | 否（仅统计） |
| `activity` | 心跳：回合开始 / 工具调用 | 否（只用于流光与「运行中会话」） |
| `tool_finished` | **工具收尾**（某次工具调用正常结束，回合仍在跑）；等待状态解除的第一信号——答完提问 / 批完权限后把相位从等待中打回思考中 | 否（只用于流光与「运行中会话」） |
| `tool_failed` | **工具级失败**（某次工具调用出错，agent 会自行重试，回合仍在跑） | 否（事件流里记一条中性色，可排查不吓人） |
| `permission_required` | 等人授权/确认 | 是 |
| `input_required` | 等人输入 | 是 |
| `run_completed` | 回合完成 | 是 |
| `run_failed` | 回合出错 | 是 |
| `run_aborted` | **用户主动中止**（自己按的停止/中断） | 否（流光在没有别的会话在跑时直接收起，见 configuration.md 的状态推导表） |

> **为什么要单独一类 `tool_failed`**：`PostToolUseFailure` 是「这次工具调用没成功」，不是「回合失败」——实测 ZCode 编辑前没先读文件报错，下一秒重试就成功。以前映射成 `run_failed` 时，一次瞬时工具失败会亮终止色全屏特效 + 弹「任务失败」+ 把会话摘出运行中列表，全是噪音。现在它当心跳处理：会话留在表里、相位回落思考中、不通知、不亮终止色；真回合失败仍由回合结束信号 + 心跳判死兜底。

> **为什么要单独一类**：同一个动作（手动点停止）在不同 agent 上落到不同信号——Qoder 是 `SessionEnd`、DSH 是 `turn/end` 的 `aborted`、ZCode 只能读它的日志推断、WorkBuddy 是运行中被归档、TraeWork 是 `chat_turn.turn_status=canceled`。以前有的报「完成」（亮完成色，谎报跑完）有的报「失败」（亮终止色 + 弹「任务失败」，而那是用户自己按的），现在统一成 `run_aborted`：**不亮终止色、不亮完成色、不通知**，没有别的会话在跑时直接收起光效。

**各 agent 的「手动中止」信号现状**（2026-09 核对，详情见各节）：

| Agent | 中止信号 | 中止回合的表现 |
| --- | --- | --- |
| Qoder | `SessionEnd`（实测） | 立即释放（run_aborted） |
| ZCode | 本地日志推断（非官方，~1s） | 立即释放（run_aborted） |
| Claude Code | `SessionEnd`（官方文档，未实测） | 退出会话时释放；中断后继续对话则刷新 |
| CodeBuddy | `SessionEnd`（官方文档；本机可实测） | 同 Claude Code |
| OpenCode | `session.error` 带 `MessageAbortedError`（社区插件实证，未实测） | 立即释放（run_aborted，其后的 idle 被压制） |
| WorkBuddy | 运行中被归档 → run_aborted（实测） | 立即释放 |
| TraeWork | `chat_turn.turn_status=canceled`（实测） | 立即释放（run_aborted） |
| TraeCode / Codex | **无**（官方事件表就没有） | 卡原状态到 10 分钟判死；继续对话则刷新 |

**三条硬约束**（见 `crates/bark-cli/src/lib.rs` 模块注释，改 hook 逻辑必须守住）：
1. 永远退出码 0——绝不能阻塞 agent；
2. 读 stdin 必须带超时；
3. 只读配置，绝不写盘（daemon 离线时改为落盘 `pending.jsonl` 兜底）。

**token 不落到 agent 配置里**：hook 命令只有 `--agent/--event`，端口与 token 由 `bark-cli` 运行时从 `config.json` 读取。

**接入成功 = 三件事都到位**：① UI 上打开开关（写入 agent 配置）→ ② 应用侧确认/信任/重启（插件型必须重启宿主，见 §8；ZCode 需**新建会话**）→ ③ 事件真的到达（事件流页能看到）。

**开关显示的是「实际接上了」**，不只是配置里那个标记（= 配置开着 **且** 校验通过）。因为有的目标应用会重写自己的配置文件、把我们写的 hook 冲掉（实测：Qoder CN 桌面应用启动时整份重写 `~/.qoder-cn/settings.json`，连 CN IDE 自己的插件键都一起冲掉）。这时开关显示为**关**、徽标显示「hook 已丢失」，**点一下开关即重新写入**——不用先关再开。启动时还会**对齐一遍**所有已开启的接入（`state::sync_hook_integrations`）：`hook 失效` / `hook 已丢失` 就地重写（自愈，开关意图不变），内容没变则一个字节都不写（配置与生成的插件文件都是内容比较后才写）；这也让「升级 agent-bark → 重启应用」能把新版生成产物刷到机器上——否则旧版插件会一直留着（实测踩过：应用重编重启后 `plugin.js` 还是旧模板）。只有配置本身无法解析（`配置损坏`，写下去会覆盖掉用户配置）才把开关落成未启用，并把原因显示在 Agents 页的「启动自检发现问题」横幅里；**该横幅按 agent 记录，处理过就消失**——重新开启（或关闭）这个 agent 时后端会清掉它那条告警（`clear_startup_warnings`），不用等下次重启。

**一个 agent 都没接上时，启动会直接引导**：主窗口默认隐藏、例行启动只走托盘（见 README），新用户装完不知道要先开接入——这一页可能永远不会被打开。所以启动时若**一个 agent 都没接上**（判定口径与上面的开关一致：配置开着 **且** 校验通过，见 `commands::needs_agent_setup`），会自动弹出主面板并停在「Agents 接入」页；接上任意一个后条件不再成立，后续启动不再打扰（关掉全部接入则下次启动又会引导）。

**关闭接入会发生什么（回收边界）**：

- **只摘自己写的东西**：hook 型按「可执行文件名恰为 `AgentBark(.exe)`」逐条摘除（不是子串匹配，避免误删用户自己的 hook），组空删组、事件键空删键；OpenCode 摘 `opencode.json` 的 `plugin[]` 条目并删插件文件；DSH 摘 `cordis.patch.yml` 里的 insert 条目并删插件文件（patch 文件本身保留、写成 `[]`，它可能是用户手写的）；监控型只是停掉轮询线程（它从不写盘）。同文件里你自己的条目一律不动；
- **解析不了的文件不碰**：配置/patch 解析失败时跳过该文件（宁可留着自己的条目，也不把你的配置改成空对象）；写入前备份 `<文件名>.agent-bark.bak`（只备份一次，留的是你最初的版本，**关闭时不删除**）；所有写入都是「同目录临时文件 → rename」，不留半写文件与临时文件；
- **残留只有我们自己的痕迹**：上面那个 `.bak`，以及「文件原本不存在、由我们创建」时留下的空壳（如 `{"hooks":{}}`）。重新开启是幂等的，不会重复追加条目；
- **关闭 ≠ 宿主立即停止上报**：插件型（DSH / OpenCode）删的是磁盘文件，宿主进程里已加载的插件仍在内存中继续 POST，必须重启宿主才彻底停止。在宿主重启前，daemon 会按开关忽略该 agent 的事件（只进事件流与历史，不参与状态机、不亮流光、不通知——见 `run_pipeline` 里「已关闭接入的 agent」那道门），所以不会有残留通知；
- **关闭即从「运行中会话」面板消失**：管道那道门要等该 agent 的下一条事件才清表，而 hook 型 agent 关掉后可能再也不上报，所以关闭开关时会立刻摘掉它的会话条目并推一次快照（`drop_agent_sessions_and_sync`，两条路径共用），`list_active_sessions` 也按开关过滤（页面 15s 对账一次）；
- **摘完会话必须把流光重新对齐**：同一个 `drop_agent_sessions_and_sync` 会在表空时收回光效（终态绿/红除外，它们自己淡出）、表非空时按剩余会话重新推导——否则关掉正在跑的最后一个 agent 会留下一盏熄不掉的灯（它的事件从此被门丢弃，判死巡检也因表已空而无变化、不会触发收回），或把僵尸橙色（等待指示）留在屏幕上；
- `pending.jsonl`（离线兜底队列）里可能还有关闭前落盘的事件，会在下次启动补投一次（与开关无关；靠事件 id 去重，不会重复通知）。

---

## 1. Claude Code

| 项 | 值 |
| --- | --- |
| id | `claude-code` |
| 配置文件 | `~/.claude/settings.json` |
| 安装标记 | `~/.claude` 目录存在 |
| 注册事件 | `SessionStart`→session_start、`UserPromptSubmit`/`PreToolUse`→activity、`Notification`→input_required、`PostToolUseFailure`→tool_failed、`Stop`→run_completed、**`StopFailure`→run_failed（回合因 API 错误结束）**、**`SessionEnd`→run_aborted（会话结束释放）** |
| 需要你做 | 无（改完配置即时生效） |

**坑**：`~/.claude/settings.json` 常被手工编辑，若文件不是合法 JSON，我们**拒绝写入**（不会把你的配置覆盖成空对象）。UI 会显示「配置损坏」并给出文件路径。

**中止信号（2026-09 按官方文档补齐，⚠️ 本机未安装 Claude Code，未实测）**：官方 hooks 文档（code.claude.com/docs/en/hooks，33 事件）明确 **Stop 在用户中断（Esc）时不触发**——中断的回合收不到完成信号。归途有二：
- `SessionEnd`（reason: clear / logout / prompt_input_exit / other）：会话级结束。中断回合后**退出会话**就靠它释放「运行中」条目（归一成 run_aborted，不通知不亮终止色）；正常回合 Stop 之后紧跟的 SessionEnd 落在 10s 终态回声宽限窗内被抑制，更晚退出会记一条「已中止」（会话确实结束了，语义成立）。
- **中断后不退出、也不再继续**的会话仍无信号，只能等 10 分钟判死（能力边界，非遗漏）。
另：`StopFailure`（回合因 API 错误结束）是 Claude Code 唯一的回合真失败信号，较新版本才有；若老版本对未知事件键严格校验会丢整份 hooks——真遇到就关掉开关回退，事件流可观测。

---

## 2. TraeCode

| 项 | 值 |
| --- | --- |
| id | `trae-code` |
| 配置文件 | `~/.trae-cn/hooks.json`（国内版）与 `~/.trae/hooks.json`（国际版），**存在哪个写哪个，都存在则都写** |
| 安装标记 | 上述任一目录存在 |
| 注册事件 | `SessionStart`、`UserPromptSubmit`→activity、`PreToolUse`→activity、`Stop`→run_completed、`Notification`→input_required |
| 需要你做 | 首次要在 IDE 的 **设置 > Hooks** 里点「启用」（安全警示面板），否则不生效 |

**坑**：
- TraeCode **没有失败事件**，所以收不到 `run_failed`；
- **手动中止无信号**（官方文档 docs.trae.cn/ide_hook-configuration-reference，2026-09 核对）：官方事件表就 6 个，没有 SessionEnd、没有中断事件；中断的回合只能等继续对话（UserPromptSubmit 会刷新会话）或 10 分钟判死。这是官方事件表的能力边界，后续版本加了 SessionEnd 再补；
- `PreToolUse` 是阻塞型 hook，我们的子命令恒为放行（exit 0、不输出 stdout），不影响工具执行；
- 心跳（activity）是「思考中 / 执行工具」实时状态的来源，缺了它流光只会亮警告 / 完成 / 终止色，不会亮思考色。

---

## 3. CodeBuddy

| 项 | 值 |
| --- | --- |
| id | `codebuddy` |
| 配置文件 | `~/.codebuddy/settings.json` |
| 安装标记 | `~/.codebuddy` 目录存在 |
| 注册事件 | `SessionStart`、`UserPromptSubmit`/`PreToolUse`→activity、`Notification`→input_required、`PostToolUseFailure`→tool_failed、`Stop`→run_completed、**`StopFailure`→run_failed**、**`SessionEnd`→run_aborted（会话结束释放）** |
| 需要你做 | 在会话里运行 `/hooks` 面板，**审核外部修改的 hooks** 后才会生效 |

**坑**：官方 CLI 事件表此前未公开，现在已挂出（codebuddy.ai/docs/zh/cli/hooks，CodeBuddy Code CLI v1.16.0+ Beta，27+ 事件，近逐字镜像 Claude Code）——已按 Claude Code 同款口径补齐 SessionEnd / StopFailure / 心跳（**未实测**，但本机装有 CodeBuddy 可直接验证）。官方同样明确 Stop 在用户中断时不触发，中止释放逻辑与 §1 Claude Code 一致。升级后需**重新开关一次**让新事件写入配置（校验只认 exe 路径，旧配置不会误报失效，但缺新事件）。

---

## 4. Qoder（国际版 + 国内版 Qoder CN）

| 项 | 值 |
| --- | --- |
| id | `qoder` |
| 配置文件 | **`~/.qoder/settings.json`（国际版，桌面应用 / IDE / CLI 三个入口共用）** · **`~/.qoder-cn/settings.json`（国内版 Qoder CN）** |
| 安装标记 | `~/.qoder` 或 `~/.qoder-cn` 目录存在（装任一版本都能检测到） |
| 注册事件 | `UserPromptSubmit`→activity、`PreToolUse`→activity、`Notification`→**见下（授权等待=警告色）**、`PostToolUseFailure`→tool_failed、`Stop`→run_completed、**`SessionEnd`→run_aborted（主动中断清场）** |
| 需要你做 | 改完配置**要完全退出并重启对应应用**（三个入口都只在启动时加载 hooks） |

> 国际版与国内版**合并成一条 adapter**（与 TraeCode 的「国际版 / 国内版」同样式）：存在哪个配置文件就写哪个，两个都在就都写——一般人不会同时装两个版本。原来的独立条目 `qoder-cn` 已下线，老配置里的开关会在加载时并到 `qoder` 上。

**事件表取两个版本都支持的交集**（没注册的两个实测都无功能损失）：`SessionStart` 在本仓库是空操作（不通知、不建档，见 `state.rs` 的 `apply_session_event`）；`PermissionRequest` 国际版实测**从不触发**（授权等待走 `Notification`），CN 文档也没列它。

**2026-09 探针实测**（探针方法见本文最后一节）：

- **国际版**：桌面应用 `Qoder v0.2.3`（`QODER_HOOK_SOURCE=cli`、hook 协议 `1.1.47`）与 IDE（`=ide`、`1.29.0`）**都从 `~/.qoder/settings.json` 读 hooks**；应用重写该文件时会**保留**我们写入的 `hooks` 键；
- **国内版**：CN IDE **实测读 `~/.qoder-cn/settings.json`**（`QODER_HOOK_SOURCE=ide`、协议 `1.29.0`）。⚠️ 官方帮助文档（`help.aliyun.com/zh/lingma/hooks`）至今仍写 `~/.lingma/settings.json`，那是灵码时代「VS Code 插件线」的家——埋在那儿的探针**一次都没被调起**，CN 自带引擎（`resources/app/resources/bin/…/QoderCN.exe`）里也搜不到任何 `.lingma` 文件路径，只有 `.qoder-cn/shared_client`。**别照文档改回去**；
- 「等你授权」两版一致：**`Notification` + `notification_type=permission_prompt`**（`message` 形如 `Tool Bash requires confirmation`）→ 归一到 `InputRequired` 后被 `normalize()` 升级为 `PermissionRequired` → 警告色「需要确认」；
- 「主动中断」两版一致：正常回合是 `… Stop` + `SessionEnd`（相隔约 150ms），**用户主动中断的回合只有 `SessionEnd`、没有 `Stop`**。不注册 `SessionEnd` 的话，被中断的会话会永远留在「运行中」列表、流光一直停在思考色；它归一成 `run_aborted`（中止）——会话立即清出列表，流光在没有别的会话在跑时直接收起（不亮终止色也不亮完成色、不通知，那是你自己按的停止）。正常回合那条 `SessionEnd` 会被状态机的**终态回声**抑制（不重复处理、不压掉完成色）；
- `Stop` 带 `last_assistant_message`（助手回复全文），已优先取作通知正文；桌面应用模式下 `cwd` 是它自己的会话目录（`…\Documents\Qoder\<日期>\<会话id>`），项目名推导会自动跳过会话 id / 日期这类目录段；
- ⚠️ **国内版桌面应用启动时会整份重写 `~/.qoder-cn/settings.json`**（连 CN IDE 自己的插件键都一起冲掉），我们写的 hooks 会被清掉。UI 侧已把「配置开着但 hook 不在」显示成开关关闭，点一下即重新写入（见 §0 与排查清单）。

**实测状态**：

| 入口 | 是否读对应配置 | 状态 |
| --- | --- | --- |
| Qoder 桌面应用（国际版）/ IDE / CLI | ✅ `~/.qoder/settings.json` | 探针实测通过 |
| Qoder CN IDE | ✅ `~/.qoder-cn/settings.json` | **端到端验证通过**（2026-09-10：授权弹窗亮起警告色、主动中断立刻清场不卡在思考色） |
| Qoder CN JetBrains 插件 | 同家目录、同引擎（`shared_client/bin/QoderCN.exe`） | 不适用（见下） |
| Qoder CN CLI（独立发行版） | 官方 CLI 文档写的就是这份 | 待验证 |
| Qoder CN VS Code 插件 | ✖ 不读——`tongyi-lingma 2.6.7` 引擎里没有任何 hook 事件代码 | 该产品线不支持 hooks |

> 插件线**只在你用它的 Agent / 对话功能时才会触发 hook**。只用它做代码自动补全（不走 agent）的话，一条事件都不会产生——这种用法既不需要接入，也不需要验证。

---

## 5. Codex

| 项 | 值 |
| --- | --- |
| id | `codex` |
| 配置文件 | `~/.codex/hooks.json` |
| 安装标记 | `~/.codex` 目录存在 |
| 注册事件 | `SessionStart`、`Stop`→run_completed、`PermissionRequest`→permission_required |
| 需要你做 | 注册后需在会话内运行 `/hooks` 完成**信任确认** |

**手动中止无信号**（2026-09 核对官方与社区资料）：Codex hooks 仍是 beta，事件表只有 `SessionStart` / `Stop` / `agent-turn-complete`，没有 SessionEnd、没有结束原因载荷、没有中断事件；社区还报过 hooks 在交互式会话不触发的问题（openai/codex#17532）。中断的回合同样只能等下一条事件或 10 分钟判死，官方补事件后再接。

---

## 6. ZCode

Z.ai（智谱 GLM）的编程 Agent（Electron 桌面应用 `ZCode.exe`；**实测版本 3.11.2.6792 / Windows**）。桌面应用与内置 CLI 共用同一份用户级配置。

| 项 | 值 |
| --- | --- |
| id | `zcode` |
| 配置文件 | `~/.zcode/cli/config.json`（Windows `%USERPROFILE%\.zcode\cli\config.json`）。模型配置在 `~/.zcode/v2/config.json`，我们**不碰** |
| 安装标记 | `~/.zcode` 目录存在 |
| 注册事件 | `SessionStart`→session_start、`UserPromptSubmit`/`PreToolUse`→activity、`PostToolUse`→**tool_finished**（等待状态解除信号，见下）、`PermissionRequest`→permission_required（`AskUserQuestion` 时→input_required）、`PostToolUseFailure`→tool_failed（带 `is_interrupt` 时→run_aborted，防御式）、`Stop`→run_completed、手动中断→**run_aborted**（非官方，见下） |
| 需要你做 | **新建会话**（配置在会话启动时拍快照）。没有信任/审核环节 |

**写入形态**（与 Claude 系有两处结构性不同：事件多嵌一层 `events`、有文件级总开关）：

```json
{
  "hooks": {
    "enabled": true,
    "events": {
      "Stop": [
        { "hooks": [ { "type": "process", "command": "C:\\...\\AgentBark.exe",
                       "args": ["hook", "--agent", "zcode", "--event", "Stop"],
                       "enabled": true, "timeoutMs": 5000 } ] }
      ]
    }
  }
}
```

条目用 `type: "process"`（`command` 是 argv[0]，其余走 `args`，**不经过 shell**）：ZCode 的 `type: "command"` 会把整串交给系统 shell，而 Windows 上用哪个 shell 官方没写、各版本还不一样，`process` 直接绕开引号与方言问题。

**坑**：

- **事件嵌在 `hooks.events.<Event>` 两级下**。插件的 `hooks/hooks.json` 才是顶层 `hooks.<Event>`，两者混淆会让**整份 config.json 加载失败**（同类实现的原话是 "config.json fails to load"）；
- **文件级总开关 `hooks.enabled` 默认 false**，不置 true 一个 hook 都不会执行；但我们**只在该键缺失时补 true**——你显式写的 `false` 会被保留并**拒绝注册**（界面会提示怎么处理），绝不覆盖你的选择；
- **配置文件必须是「无 BOM」的合法 JSON**。实测：带 UTF-8 BOM 的文件被判 `config_file_invalid`、**整份 hooks 静默失效**——`~/.zcode/cli/log/zcode-<日期>.jsonl` 里能看到 `Config file failed to load`（`Unexpected token '锘?`）。我们写出的文件不带 BOM；手工编辑后失效先查这里；
- **`matcher` 键整个省略**：文档说缺省/空串/`*` 都是「匹配全部」，但有严格解析器会把空串当非法、进而丢弃整份来源；
- **`PermissionRequest` 是阻塞型，hook 可以返回 Allow/Deny**，而且**同事件 hook 串行执行、后写的决定覆盖先写的**（一个放行 hook 能悄悄盖掉别人写的 deny）。我们只通知：**永远不输出决定**（stdout 为空、退出 0），任何异常都 fail-closed 回 ZCode 自己的权限流程；条目 `timeoutMs` 也只给 5000（同类实现给这个事件配到 600000，是因为它真的要替用户做决定）。代价：授权提示会多等我们一下——daemon 在跑是毫秒级，daemon 未运行最坏约 2 秒（bark-cli 读 stdin 的 2s 超时）；
- **只有 7 个事件**，没有 `Notification`、`SessionEnd`、子代理生命周期事件。**实测：手动终止（停止按钮 / Esc）一个 hook 都不发**——两种时机都验过：① 模型思考中中断，探针停在 `PostToolUse`；② **工具正在执行时中断**（让它跑 `sleep 180`，在 130 秒内中断），探针停在 `PreToolUse`。更细的一层：ZCode 内部其实**试图**调 `PostToolUseFailure`，但 1ms 内就把 hook 进程取消了（日志里是 `hook.run.failed`），所以我们什么都收不到；文档里的 `is_interrupt` 字段在当前版本的中断路径上从未出现。
  **对策（非官方）**：daemon 会**每秒增量读一次 ZCode 的本地日志**（`~/.zcode/cli/log/zcode-<日期>.jsonl`），识别中断记录后合成一条 **`run_aborted`（中止）**——效果是**按停止后约 1 秒内**会话清出列表、流光在没有别的会话在跑时直接收起（不亮终止色、不亮完成色、不通知；与 Qoder 的 `SessionEnd`、DSH 的 `aborted` 统一到同一类）。判据见下文「中断信号」；应用升级改了日志格式时会自然退化回判死超时，不会误报。不想让它读日志，关掉 ZCode 接入开关即可（那时它完全不工作）。
  「等你输入」只覆盖走 `PreToolUse`/`PermissionRequest` 的提问（`AskUserQuestion`），计划模式批准（`ExitPlanMode`）按「需要确认」通知——这类交互本身**不能**由 hook 代替你回答；
- **`PostToolUse` 注册为 `tool_finished`（工具收尾）**：它是**等待状态解除的唯一信号**——答完 `AskUserQuestion` / 批完权限后，agent 要先思考一段时间才可能调下一个工具，这期间没有 `PreToolUse`（下一次工具开始才有）、没有 `UserPromptSubmit`（新回合才有）。曾经以「进程开销翻倍」为由不注册它，实测代价算错了方向：每轮多 3 次起进程（毫秒级）几乎无感，而「答完问题后警告色卡几十秒、直到下一个工具才开始」是用户每天可见的（实测复现）。它当心跳处理：相位回落思考中、会话留在表里；不通知、不入历史、不响音效（与 `activity` 同口径的纯状态信号）。注意收尾时点=工具结束：被批准工具**执行期间**相位仍是等待中（那段时间本来就没有事件），收尾才解除；乱序晚到（Stop 之后才到）由状态机的回声防护抑制，不会复活刚结束的会话。
- **子代理不做识别**：实测载荷里没有 `agent_type`/`agent_id`/`is_subagent`（主会话与子代理都只有 `session_id`），所以 ZCode 的子代理事件**会照常通知**。通用的「`agent_type` 非空即判子代理」启发式对 ZCode 会误伤主会话，适配器里已显式覆盖：只认 `is_subagent`/`subagent` 显式布尔值与 `sess_subagent_*` 会话命名；
- **别指望项目级 hooks**：写在 `<工作区>/.zcode/config.json` 或 `zcode.json` 里的 `hooks` 在现行版本被整体忽略（日志记 `config_project_hooks_ignored`），设置页也隐藏了工作区作用域入口；只有用户级这份生效；
- **同一份文件里可能有别的工具的条目**（clawd / OpenViking / Hindsight 等都往这里合并 hook）。我们只在数组末尾追加**自己的独立组**、按条目摘除，既不并入也不重排别人的组；反过来，clawd 见到我们的 `PermissionRequest` 条目会拒绝注册它自己的阻塞 hook（它的自我保护，对我们无影响）；
- **`AskUserQuestion` / `ExitPlanMode` 不走授权决定通道**，对我们只是「通知怎么写」的差别：前者归一为 `input_required`（「等待输入」），其余 `PermissionRequest` 是 `permission_required`（「需要确认」）。

**实测证据**（2026-09-10，ZCode 3.11.2.6792 / Windows）：埋 `type: "process"` 探针（`command` 为含空格的 node 绝对路径）后新建会话，**10 次 hook 被真实调起**，顺序为 `SessionStart → UserPromptSubmit → PreToolUse → PostToolUse → PreToolUse → PostToolUse → PreToolUse → PermissionRequest → PostToolUse → Stop`；stdin 载荷同时保留 camelCase 与 snake_case（`hook_event_name`/`hookEventName`、`tool_name`/`toolName`、`session_id`/`sessionId`、`tool_use_id`/`toolCallId`）；`Stop` 同时带 `last_assistant_message`、`responseText`、`responsePreview`（适配器三个都试，取到即用）；`PermissionRequest` 带 `tool_name`、`riskLevel`、`reason`（如 `High risk tools require explicit approval`）；`hook.run.failed` 全程 0 条。同一轮还验证了「真实 `AgentBark.exe hook --agent zcode` 子命令 → daemon」链路（HTTP 200、退出 0、stdout 为空）。

**另一轮专门验中断**（7 个事件全埋探针）：① 会话跑到一半手动终止 → 探针序列停在 `PostToolUse`，无任何终态事件；② **工具执行中终止**——`PreToolUse` 拿到 `Bash {"command":"sleep 180 && echo WAIT_DONE_180S","timeout":200000}` 后中断，130 秒后直接是**新会话**的 `SessionStart`，中间一个事件都没有。两次中断的真相在 ZCode 自己的日志里：

```
16:28:00.959  v4.stop.foreground_execution_inspected  {"runtimeStopKind":"stopped","activeForegroundExecutionId":"runtime_command_1"}
16:28:00.960  hook.run.failed  {"hookEventName":"PostToolUseFailure","hookIndex":0,"source":"config.PostToolUseFailure.0.0"}   ← 它试了，1ms 后取消
16:28:00.982  turn.failed
16:28:00.984  "v4 background turn failed" {"error":"Turn was cancelled."}
```

**中断信号**（看门狗用的判据，跨 4 天日志实测 12 次中断 1:1 对应）：`v4.stop.foreground_execution_inspected` 且 `runtimeStopKind == "stopped"`，或一条**顶层没有 `event` 字段**的 warn 记录其 `context.error` 含 `cancelled`。**不能**用 `turn.failed` 判：实测 20 条里只有 12 条是中断，其余是真实的模型/工具失败（那种情况 hook 正常上报，再合成一条就重复了）。看门狗还有两道保险：只对「会话表里还在跑」的会话生效（正常回合的 `Stop` 已清表 → 不重复通知），同一会话 30 秒内只合成一次（两条记录相隔 25ms）。

另外两条**通用**的加载/行为规则值得记住：
- **hooks 配置里任何一条不合规，整份配置都会被拒**（`config.file.invalid`，日志给出精确路径如 `hooks.events.PostToolUse.0: Invalid input: expected object, received null`），不是只有那一条失效——BOM 那次是同一个机制；
- ZCode 的 Bash 调用会带**自己声明的超时**（上例 `tool_input.timeout = 200000` 毫秒），理论上可用来「按工具真实预期时长」自适应判断静默是否合理（当前实现没有用，仍按固定时长判死）。

---

## 7. OpenCode（插件型）

| 项 | 值 |
| --- | --- |
| id | `opencode` |
| 生成的插件 | `<配置目录>/agent-bark/opencode-plugin.js`（Windows 为 `%APPDATA%\agent-bark\opencode-plugin.js`） |
| 登记位置 | `~/.config/opencode/opencode.json` 的 `plugin` 数组（绝对路径） |
| 安装标记 | `~/.config/opencode/opencode.json` 存在 |
| 事件映射 | 终态：`session.idle`→run_completed、`session.error`→run_failed（**载荷含 `MessageAbortedError`（Esc 取消）时→run_aborted**，且其后 10s 内的 `session.idle` 被压制）；打断：`permission.updated`→permission_required；会话：`session.created`→session_start；心跳：`session.status`（busy→activity、retry→tool_failed）、`message.part.updated` 的 tool 部件（pending/running→activity 带 tool_name、error→tool_failed） |

**心跳与元数据（事件名以官方 SDK `types.gen.ts` 为准）**：
- 旧版映射里的 `permission.asked` / `question.asked` **在 opencode 里不存在**（权限打断实际是 `permission.updated`，question 事件根本没有），导致权限打断也收不到；已修正。
- 心跳缺失曾是「跑的时候毫无表现（无思考中/执行工具、流光不亮）、完成才弹一条通知」的根因：会话「运行中」表完全靠 Activity 心跳建档。现在回合开始走 `session.status` busy（按会话去重，retry 恢复后的 busy 不重复计回合），工具执行走 `message.part.updated` 的 tool 部件（按部件 id 去重——state 会 pending→running→completed 逐态各推一条，不去重 tool_calls 会双计）。
- 运行中的事件只带 `sessionID`，cwd 唯一来源是 `session.created`/`session.updated` 的 `info.directory`，子代理标记唯一来源是 `info.parentID`——插件内按 sessionID 记忆（有界缓存），终态/等待通知才有项目名，子代理会话的终态不刷通知。
- 工具部件 `state.status === "error"` 与模型调用重试（`session.status` retry）都归一 **tool_failed**（会话保持运行中、不通知、中性色入事件流），不是回合失败。

**中止检测**：官方事件页没有 abort 语义文档；`MessageAbortedError` 判定来自社区插件 opencode-auto-resume 的实证（Esc 取消走 session.error + 同名错误）。插件 JS 里做防御式字符串匹配（错误 name 含 abort / 序列化载荷含 MessageAbortedError），匹配不上回落 run_failed——最坏情况 = 无中止检测的旧行为。升级后需**重新开关一次**刷新插件文件。

**坑**：
- 插件对象由通用 **`event` 回调**接收事件流（`event: async ({ event }) => …`）后按 `event.type` 分发；早期版本把 `"session.idle"` 当 hook 键直接挂，与官方 API 不符、根本不会被调用（已修）；
- 生成的 JS 里**内嵌 daemon token**（在注释里也写了「勿提交到版本库」）——重新登记会刷新该文件。

---

## 8. DeepSeek Harness（插件型）

| 项 | 值 |
| --- | --- |
| id | `dsh` |
| 数据根 | `$DSH_HOME`，未设置时默认 `~/.dsh` |
| 生成的插件 | `$DSH_HOME/agent-bark/plugin.js` |
| 登记位置 | `$DSH_HOME/cordis.patch.yml`（**insert 补丁**形式，按固定 `id` 寻址，不与用户条目混） |
| 安装标记 | `$DSH_HOME` 存在（不存在时**拒绝注册**，绝不凭空创建） |
| 生效方式 | **必须重启 dsh**（实测运行中的 dsh 不会挂载 patch 新增条目） |

**事件映射**（插件内部归一化后直接 POST `/event`，不经过 hook 子命令）：

| DSH 事件 | 模式 | → 统一事件 | 正文 |
| --- | --- | --- | --- |
| `agent/turn-stopping` | serial（无 `next()`） | `run_completed` | 会话日志里最后一条 `assistant/message` 的 text 块；取不到退「回合 N 结束」 |
| `agent/status` running | emit | `activity` | 只发根会话，喂「运行中会话」与流光 |
| `agent/status` running→idle | emit | 兜底终态 | 本回合没被 `turn-stopping` 报过才发；按 `turn/end` 的 reason 分类（`completed` → 完成；`aborted`/`interrupted` → **`run_aborted`**（用户主动中止：不亮终止色、不亮完成色、不通知）；`error` → `run_failed`；`blocked` → `run_failed`「回合被拦截」；`max-tokens` → 完成但正文带「〔达到输出上限〕」前缀；未知类型按完成兜底） |
| `agent/error` | emit | `run_failed` | `payload.error`（DSH 声明为 `unknown`）：字符串直用、对象取 `message`、否则截断 JSON |
| `approval/request` | waterfall | `permission_required` | `toolName：reason`；观察完必须 `next()` |
| `user-questions/request` | waterfall | `input_required` | 首个问题的 `header：question`；同样必须 `next()` |
| `session/event` 的 `tool/call` | emit | `activity` | 带 `tool_name`，面板显示「执行工具」；只发根会话 |

**坑**：
- **cordis.patch.yml 是补丁层，不是条目清单**：顶层 `- id: xxx` 的语义是*覆盖基础树里已有的同名条目*，id 不存在时 DSH 只在 stderr 打一句 `patch: entry "xxx" not found` 然后跳过——插件根本不会挂载，且 GUI 看不到任何报错。新增插件必须写 `- insert: [{id, name}]`（早期版本写顶层 `{id, name}` 导致接入完全无效，已修；旧写法会被识别为漂移并在重新勾选时就地升级）。排障金标准：`dsh web --dump-config`（或 `dsh --profile <名> --dump-config`），条目没出现在输出里就是没挂上；
- `name` 写绝对路径即可，DSH 加载 insert 条目时会经 `pathToFileURL` 转 file:// URL，Windows 盘符路径不需要自己转；
- **改完 patch 必须重启 dsh**（2026-09-10 在 `dsh web` + 0.1.5-rc.1 实测）：往 `$DSH_HOME/cordis.patch.yml` 追加 insert 条目后 20 秒内插件不会被挂载，即使 profile 的 `patchReload` 是 `live`（HMR 只重新加载已挂载的条目）；社区通知插件文档同样写「重启 dsh 生效」。所以 UI 提示写的是「需要重启」，别信「即时生效」；
- 字段实测（0.1.5-rc.1）：sessionId 取 `agent.id`；**cwd 取 `agent.session.header.cwd`**（`session.cwd` 不存在——早期版本取它，导致通知正文没有项目名、只剩一句 `turn N`；`Agent` 上也没有 `cwd` 字段）；最后一条 assistant 文本取 `assistant/message` 的 `data.message.content` 里的 text 块（`Session` 没有 `events` 字段，只有 `eventAt()` / `snapshotEvents()` / `ownEvents()`，`seq` 是「下一个可写偏移」即有效下标 `0..seq-1`）；子代理用 `session.header.origin === 'subagent'` **或** `header.delegationDepth > 0` 判定（DSH 权威口径是 `delegationDepthOf(agent) = max(header.delegationDepth, agent.options.subagentDepth)`——运行时深度「只能加深不能降低」，只读 header 在冷 resume / 策略覆盖时可能偏低）；
- 选型参照社区插件（dsh-plugin-notify / dsh-notify / dsh-notification）：只挂 `agent/turn-stopping` 会漏掉「停下来等你确认/回答」这类会话终态，所以另挂 `agent/status` + `approval/request` + `user-questions/request`；
- 两个 waterfall 事件（`approval/request`、`user-questions/request`）是**观察者**：必须把 `next()` 交还下去，否则会把审批/提问请求吞掉；两者的 `reason` / `header` 都是可选字段（键可能整个不出现），取值前要兜底，别把 `undefined` 拼进正文；
- `agent/turn-stopping` 是 serial 事件且 DSH 会 `await` 监听器返回的 Promise（这样终态不会因进程随即退出而丢），但 POST 必须带超时（插件用 `AbortSignal.timeout(2000)`）：端口被「只接受连接不响应」的进程占用时，否则会把回合收尾一起拖住；
- **终态投递必须看响应状态码且要重投**（2026-09-23 修，用户实测「回合早已结束、流光仍是思考蓝、面板卡在『执行工具』」）：daemon 的 `/event` 在事件通道打满时用 `try_send` 回 **503**（见 `bark-server::handle_event`）——请求「成功」但事件已丢。旧插件对响应不闻不问、并且在投递**之前**就把会话标成「本回合已报过终态」，于是一次瞬时失败就让这条终态永久消失，随后的 `status idle` 兜底也被那句「已终态」挡住，daemon 的「运行中」条目只能等 10 分钟判死巡检收场（几十次里中一次，与工具调用量成正比）。现在的做法：`deliver()` 只认 2xx；终态首投失败进重投队列（1s→2s→4s→8s→16s，共 5 次，每次投递用**新的**事件 id，避免 daemon 按 id 去重把重投本身丢掉）；`markSettled` 只在**确认送达**的路径上调用；新回合开始时取消上一回合还挂着的重投（迟到的终态会把刚开始的新回合从状态表里误删），但**不**顺手置「已终态」——那会把 idle 兜底这条补救路径重新堵死。心跳（`activity`）不进这个队列：丢了下一跳会补，只补一次；
- 上面这条有两条回归测试锁着：`dsh.rs` 的结构断言（生成物必须出现 2xx 判定 / 重投 / `markSettled` 只在送达路径），以及 `crates/bark-adapters/tests/plugin_sim.mjs`（Node 直接跑生成物，用假 `fetch` 把 daemon 换成「503 / 连接被拒 / 正常」三种行为，覆盖首投失败重投、idle 兜底补报、新回合作废迟到终态、心跳不重试；本机没有 node 时该测试自动跳过）；
- 插件型 adapter 只送 cwd，daemon 侧 `handle_event` 会按 hook 链路的同一规则补出 `project`，两条链路的通知正文（项目名 + 摘要）保持一致；
- 排障链路：`dsh web --dump-config` 证明条目在列；DSH 侧有没有真的发出去，看 Windows 通知历史 `%LOCALAPPDATA%\Microsoft\Windows\Notifications\wpndatabase.db` 的 `Notification` 表（按 AUMID `com.agentbark.app` 过滤就是 agent-bark 弹过的 toast）；
- 桥接包需在 profile 里自行安装：`dsh plugin add @deepseek-ai/dsh-hooks-claude-code`；
- `$DSH_HOME` 迁移（或程序被移动）会让 insert 条目的 `name` 指向旧路径，UI 显示 hook 失效——重新勾选即可就地修复。

---

## 9. WorkBuddy（监控型 · 非官方）

| 项 | 值 |
| --- | --- |
| id | `workbuddy` |
| 数据源 | `~/.workbuddy/workbuddy.db`（SQLite，`sessions` 表） |
| 轮询间隔 | 5 秒；运行态（`working` / `planning`）每 12 轮（60 秒）重发一次 activity 心跳 |
| 安装标记 | 上述 db 文件存在 |

**坑**：
- 非官方方案：**应用升级可能失效**（已在 2.137.1 实测校准状态机），UI 上标「监控型」；
- 读库采用「先复制 db 与 `-wal` 到临时目录再打开」，避免与宿主写锁冲突；两次复制无法保证同一时点，极端情况下会读到撕裂快照（单轮失败可接受，下轮自愈）；
- 监控型拿不到每回合的 `UserPromptSubmit`/`PreToolUse`，所以靠周期性心跳（activity）维持「运行中」——否则 daemon 侧 10 分钟无活动会把会话判死，流光永远不亮思考色。**「接入成功但流光不亮」多半就是心跳没发出去**。
- 状态跃迁映射：`working/planning` → `pending` = 等输入（警告色）；→ `completed` = 完成（完成色）；→ `terminated`/`error` = 失败（终止色）；**运行中被 `archived` = `run_aborted`（中止：不亮终止色也不亮完成色，没有别的会话时收起光效）**。`terminated` 分不清「用户终止」还是「进程挂了」，保守按失败处理（终止色）——这是本仓唯一一处中止语义做不到精确的地方。

---

## 10. TraeWork（监控型 · 非官方）

| 项 | 值 |
| --- | --- |
| id | `trae-work` |
| 数据源 | `%APPDATA%\TRAE SOLO CN\ModularData\ai-agent\database.db`（**SQLCipher 4 加密**） |
| 轮询间隔 | 5 秒；主库 + `-wal` 的指纹（大小+修改时间）没变则跳过重新解密，空闲零开销 |
| 安装标记 | 上述 db 文件存在 |
| 需要你做 | 无（不用信任/重启/新建会话） |

**接入路线（2026-09 实测打通，TraeWork CN 0.1.69）**：官方 hook 是 TraeCode 的（TraeWork 仅企业版提供 hook），个人版走监控路线。会话库是 SQLCipher 4（AES-256-CBC + HMAC-SHA512，page=4096，reserve=80 即 16B IV + 64B HMAC），但**密钥是固定的**——与 Trae CN 同源的常量链（`mics_sj10gy` 加密数据 → `rust`/`cpp`/`electron` 三表 XOR 得固定密码 → PBKDF2-HMAC-SHA256 十万次迭代派生密钥）。我们做**页面级解密**成明文 SQLite（**含 `-wal` 帧叠加**，见下「坑」）后照常轮询：密钥派生与页面解密见 `crates/bark-adapters/src/traework_db.rs`（已知答案单测钉死派生链）；社区资料：[Oh-My-Trae/trae-db-decrypt](https://github.com/Oh-My-Trae/trae-db-decrypt)、[DirWang 的逆向分析](https://www.cnblogs.com/DirWang/p/21513074)（注意其 `article_solo.md`「SOLO CN 密钥随机、解不开」的结论**已过时**——本机实测固定密钥 HMAC 校验通过）。

**schema 校准**（2026-09，TraeWork CN 0.1.69 解密库实测）：

- 回合生命周期在 `chat_turn`（每回合一行）：`turn_status` 终态值域 `completed` / `failed` / `canceled`；
- 状态跃迁映射：→ `completed` = `run_completed`；→ `failed` = `run_failed`；→ **`canceled` = `run_aborted`（用户主动停止，信号非常干净）**；其余非终态值一律视为运行中（值域扩展也兼容）；
- 运行中的心跳：`chat_message` / `history_v2` / `chat_turn` 的 max(id) 增长（`history_v2` 回合内持续追加，实测约 13 行/回合）——监控型拿不到 UserPromptSubmit/PreToolUse，靠它点亮流光（机制同 WorkBuddy）；
- 会话标题 `chat_session.session_title`；工作目录 `session_project → project.absolute_path`。

**坑**：

- **非官方机制，TraeWork 升级可能失效**（换密钥常量 / 改 schema）：可用性探针（HMAC 校验 + 解密 + 查询，重试 3 次）失败时 UI 给出具体原因；核对工具 `tools/traework-probe.mjs`（`verify` / `strings` / `decrypt` / `dump` / `sql` 子命令，`strings` 扫 `ai_agent.dll` 判断密钥常量是否变更，用法见文件头注释），以及本机冒烟测试 `cargo test -p bark-adapters --lib -- --ignored`；
- **`-wal` 必须一起看**（2026-09-24 事故教训）：宿主是 SQLite WAL 模式，新提交先落 `database.db-wal`、checkpoint 后才进主库，实测 `-wal` 常驻 4-5MB（上千帧）——旧实现只解主库、轮询指纹也只看主库，导致 checkpoint 之前的新回合**全程不可见**（当天两次提问均无光效，事件一个都没发）。现在 `decrypt_db` 按 SQLite WAL 恢复语义把 `-wal` 帧叠回明文库（checksum 链 + 页级 HMAC + 只到最后提交帧，RESTART checkpoint 残留的陈旧尾帧被链式校验排除），轮询指纹同样盯住 `-wal`；
- 每轮把整库读入内存再解密（75MB 级，AES-NI 下几十毫秒），结果写临时文件后 rusqlite 只读查询；
- 与 WorkBuddy 同款「先快照再推断」：撕裂快照单轮失败可接受、下轮自愈（连续失败每 12 轮 warn 一次）；运行中回合的行从库里消失（会话被删）时主动发 `run_aborted` 闭环，不让 daemon 侧 10 分钟后僵死判死误亮终止色。

**备选路线（未采用，供后续参考）**：TraeWork 个人版支持 **MCP**（设置里手动配置 stdio/HTTP server；或项目级 `.trae/mcp.json`，需在 设置 > MCP 打开「启用项目级 MCP」）与 **Skill**（`SKILL.md` 放 `%userprofile%/.trae-cn/skills/` 全局生效，或项目 `.trae/skills/`；规则可用全局规则或 `AGENTS.md`）。组合方式：做一个 notify 型 MCP 工具 + Skill/规则要求模型在关键节点调用。缺点：**模型可能忘调**（可靠性不如库轮询）、事件粒度糙、拿不到干净的中止信号。适合做补充（如回合内的细节信号），不适合做主路线。

---

## 11. 已下线

| id | 说明 |
| --- | --- |
| `qoder-work` | QoderWork（本地办公 Agent），官方已从产品线除名 |
| `qoder-cn-cli` | Qoder CN CLI，几乎无人使用；其配置路径 `~/.qoder-cn/settings.json` 已由 `qoder-cn` 接管 |

两者不再注册 adapter、设置页里看不到。残留不会出问题：
- `config.json` 的 `agents` 段存的是**字符串 id**，不是枚举，所以老配置能正常解析；
- 这两个软件配置文件里残留的 AgentBark hook 只会空转——`bark-cli` 遇到未知 agent 打印一行到 stderr 并 `exit 0`，不会阻塞 agent；
- 更彻底一点：注册任意 adapter 时会顺手清掉配置里指向这两个 id 的死条目（`RETIRED_AGENT_IDS`，见 `crates/bark-adapters/src/claude_style.rs`）。

---

## 12. 通用排查清单

先看 **Agents 接入**页的徽标，再对症处理：

| 徽标 / 现象 | 含义 | 处理 |
| --- | --- | --- |
| 未安装 | 安装标记目录不存在 | 装到别的路径 / 用了别的用户目录 / 只在另一个系统账户下装过 |
| 已安装（蓝） | 检测到软件，但开关没开 | 打开开关 |
| hook 已丢失 | 配置里开关是开的，但 agent 配置文件里找不到我们的 hook（**界面会把开关显示为关**） | 目标应用把配置重写/清空了（实测 Qoder CN 桌面应用启动时重写 `~/.qoder-cn/settings.json`）→ 点一下开关重新写入，再重启目标应用 |
| 监控未运行 | 监控型开关开着，但轮询没跑起来（**界面同样显示为关**） | 会话库不可用或启动失败，点开关重试；卡片会给出具体原因 |
| 已接入（绿） | 配置文件里能找到我们的条目 | 若仍无通知，看下面「完全不通知」 |
| 监控中 | 监控型已跑起来并在产出事件 | — |
| hook 失效（红） | 条目指向的可执行文件已不存在（程序被移动/重装） | 重新勾选即可**就地修复**，不用手工改 JSON |
| 配置损坏（红） | agent 的配置文件不是合法 JSON | 先修那份文件（我们拒绝写入，避免覆盖你的配置） |

**完全不通知**，按顺序查：
1. **agent-bark 在跑吗**：托盘图标在不在、进程里有没有 `AgentBark.exe`（关了窗口只是隐藏到托盘）；
2. **事件到了吗**：GUI「事件流」页看有没有新事件。有事件但没通知 → 是规则问题（免打扰时段、聚合窗口、子代理事件不通知、渠道开关、静音）；
3. **hook 被调起了吗**：设 `BARK_DEBUG=1` 后重启 agent，hook 会在 stderr 打印上报结果（含失败原因，不含 token 明文）；
4. **端口/token 改过吗**：设置页「服务信息」会提示「需重启生效」；
5. **应用侧要不要信任/重启**：见各 agent 的「需要你做」一列——TraeCode / CodeBuddy / Codex 需要信任、Qoder / Qoder CN 需要重启、**ZCode 需要新建会话**；
6. **ZCode 专项**：① 它读的是**用户级** `~/.zcode/cli/config.json`，写在项目里的 `hooks` 会被整体忽略；② 看看 `~/.zcode/cli/log/zcode-<日期>.jsonl`——`config.file.invalid`/`Config file failed to load` 说明那份 JSON 不被接受（**最常见的原因是文件带了 UTF-8 BOM**），`hook.run.failed` 说明我们的条目被调起但失败了。

**流光不亮但通知正常**：多半是心跳类事件没注册（见 TraeCode / Qoder / CN 的 activity 说明），或者该 agent 是监控型且心跳没发出去。

**回合结束后流光一直停在思考色、「运行中会话」列表里那条不消失**：说明这次回合结束的信号没被我们收到。已知情形：
- Qoder / Qoder CN **用户主动中断**时只有 `SessionEnd`、没有 `Stop`（见 §4，已注册处理 → `run_aborted`）；
- **ZCode 手动终止（停止按钮 / Esc）一个事件都不发**（实测，见 §6）：既没有 `SessionEnd` 也没有 `Stop`，连 `PostToolUseFailure` 都不带 `is_interrupt`。daemon 会读它的本地日志推断中断（非官方机制，约 1 秒）；
- 其它 agent 若进程被直接杀掉，不会有任何终态事件——这种情况靠**心跳静止判死**兜底（流光转终止色、条目清出列表）。
- **判死时长**默认 10 分钟，由后台巡检每 30s 检查一次（不需要等下一条事件到来——早期版本只在有新事件时才算判死，所以会一直卡在思考色，已修）。想让中断后的反馈更快，用环境变量 `BARK_STALE_AFTER_MS`（毫秒，最小 5000）覆盖，例如 `BARK_STALE_AFTER_MS=60000` = 静默 1 分钟即判死；代价是**长时间跑单个工具**时可能被误判成「意外终止」。
- **不想等**：托盘菜单 →「重置流光」立刻驱散当前颜色（下一次事件到来时按正常逻辑重新点亮），会话条目也会在下一次快照时消失。

**手动中止后流光为什么不亮终止色也不亮完成色**：这是刻意的。用户主动中止归一成 `run_aborted`——不是失败（不亮终止色、不弹「任务失败」）也不是完成（不亮完成色、不谎报跑完）；没有别的会话在跑时**直接收起光效**，还有别的会话在跑就照常显示它们的状态。若你看到某个 agent 中止后仍亮完成 / 终止色，说明它的中止信号还没被归到这一类（见 §0 的说明与各 agent 的「注册事件」一行）。

---

## 13. 探针验证法（文档与实测冲突时用）

要确认「某个 agent 到底读哪个配置文件」「哪些事件真的会触发」，唯一可靠的办法是埋一个探针：只记日志、永不阻断。

1. 备份目标配置文件；
2. 在它的 `hooks` 里加一个探针条目（命令指向一个只追加日志的脚本）：

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [ { "type": "command", "command": "\"C:/path/to/probe.cmd\" 标记 事件名" } ] }
    ]
  }
}
```

3. **完全退出并重启目标应用**（hooks 只在启动时加载）；
4. 在应用里发一条 prompt（最好让它读个文件以触发 `PreToolUse`，需要时触发一次授权）；
5. 看日志里哪个探针被调起 → 那就是该应用真正读取的配置。

**注意**：
- 探针脚本在 Windows 上若是 `.cmd`，**必须纯 ASCII**（cmd.exe 按 OEM 代码页读批处理，中文注释会导致解析错乱）；
- 探针所在的事件名要选「目标文档确认支持」的，未知事件名有让整份 hooks 配置被丢弃的风险；
- 测「旧路径是否还在用」时可以新建配置文件，但这会让 agent-bark 的「已安装」检测变成假阳性（它只看目录是否存在），**测完记得清理**；
- **配置文件必须是无 BOM 的 UTF-8**：用 PowerShell `Set-Content -Encoding utf8` 很容易带上 BOM，而不少 agent 的 JSON 解析器**不接受 BOM**（实测 ZCode 直接判 `config_file_invalid`、整份 hooks 静默失效，日志里是 `Unexpected token '锘?`）。写完先看一眼文件头三个字节是不是 `EF BB BF`。

**ZCode 的探针怎么写**（它与 Claude 系形态不同，照抄上面的例子会完全不生效）：

1. 事件写进 **`hooks.events.<事件名>`**（两级），并确保 **`hooks.enabled: true`**——插件的 `hooks/hooks.json` 才是顶层 `hooks.<事件名>`，混淆会让整份配置加载失败；
2. **用 `type: "process"`**（`command` = node 等可执行文件的**绝对路径**，参数放 `args`）：不经 shell，既能绕开 Windows 引号/方言问题，也不需要「.cmd 必须纯 ASCII」这条限制；
3. 改完**新建会话**即可（ZCode 在会话启动时拍快照），不必退出应用；探针条目与 agent-bark 的条目可以并存（各自独立组）；
4. 看两处：探针日志有没有被追加，以及 `~/.zcode/cli/log/zcode-<日期>.jsonl` 里有没有 `hook.run.failed`（有 = 条目被执行但失败了）。

```json
{
  "hooks": {
    "enabled": true,
    "events": {
      "Stop": [ { "hooks": [ { "type": "process", "command": "D:\\Program Files\\nodejs\\node.exe",
                               "args": ["D:/tmp/zprobe.mjs", "Stop"], "enabled": true } ] } ]
    }
  }
}
```

本机开发时曾有一套现成脚本（探针 / 目录快照 / 全盘找字符串）放在仓库外的 `D:\MySpace\qoder-hook-probe`（`probe.mjs` / `probe-hook.mjs` / `snapshot.mjs` / `findneedle.mjs`）；**该目录已不在**，需要时按本节自己写一个十几行的 `probe.mjs` 即可。
