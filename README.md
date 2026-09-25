# agent-bark

**让 AI 编程助手主动「叫」你回来** —— agent-bark 是一个运行在本机的守护程序，接入各类 AI 编程 Agent（Claude Code、Codex、OpenCode、Qoder 等），在 Agent **需要确认、等待输入、任务完成、任务失败** 时通过屏幕光效、状态音效、Bark 推送或 Webhook 第一时间通知你，让你可以放心切走窗口去干别的。

## 支持的 Agent

| Agent | 接入方式 |
| --- | --- |
| Claude Code | [Hook](doc/agent-integration.md#1-claude-code) |
| TraeCode | [Hook](doc/agent-integration.md#2-traecode) |
| CodeBuddy | [Hook](doc/agent-integration.md#3-codebuddy) |
| Qoder（含国内版 Qoder CN） | [Hook](doc/agent-integration.md#4-qoder国际版--国内版-qoder-cn) |
| Codex | [Hook](doc/agent-integration.md#5-codex) |
| ZCode | [Hook](doc/agent-integration.md#6-zcode) |
| OpenCode | [插件](doc/agent-integration.md#7-opencode插件型) |
| DeepSeek Harness | [插件](doc/agent-integration.md#8-deepseek-harness插件型) |
| WorkBuddy | [Watch](doc/agent-integration.md#9-workbuddy监控型--非官方) |
| TraeWork | [Watch](doc/agent-integration.md#10-traework监控型--非官方) |

> **每种 agent 的配置路径、注册事件、需要你在应用侧做的动作（信任/重启）以及排障步骤，见 [doc/agent-integration.md](doc/agent-integration.md)。**

## 功能特性

- **屏幕光效**：agent 运行时在屏幕最外圈亮起光带，切走窗口也能从余光看到状态——**思考色**=运行中、**警告色**=等你确认或输入、**完成色**·停留后淡出=完成、**终止色**=意外终止（失败或心跳超时判死）；**手动中止**（你自己按的停止）不亮终止色也不亮完成色——没有别的 agent 在跑时直接收起光效。透明 + 点击穿透 + 不抢焦点；「通知」页的屏幕光效可选**生效显示器**（逐块屏或全部，自动列出分辨率）、**边缘光效 / 全屏特效按状态各选类型**（思考 / 警告 / 完成 / 终止 四行各选「无 / 呼吸 / 流光」与「无 / 雾散 / 扫描」，全屏在每次颜色亮起时整屏补放、多会话并行时按事件角色补放），并可就地预览调参（思路参考 [EdgeGlow](https://github.com/vector4wang/EdgeGlow)）
- **状态音效**：思考 / 警告 / 完成 / 终止四个状态各自可选一个内置音效与播放次数（默认全部不响；8 种 CC0 素材随应用分发、响度已统一，不依赖系统提示音），「通知」页里一键试听；托盘「静音」与免打扰时段内同样不响
- **桌面悬浮窗**：常驻小卡片显示当前活动会话（四态状态与事件流、光效同口径），按住拖动、右键固定位置 / 关闭、双击唤出主面板；**自动隐藏**（默认开启）——安静（无会话或全在思考）时 2 秒后淡出收起，新会话出现时亮一下提示已接手，出现等待确认 / 等待输入、任务完成、任务失败时自动弹出（手动中止不弹）
- **第三方通知**：Bark（iOS）、通用 Webhook（飞书 / 企微 / 钉钉 / ntfy / Server酱 等，模板可自定义）
- **通知规则引擎**：聚合窗口（同 Agent 同类事件合并）、免打扰时段（如 `23:00-08:00`，可跨零点）
- **离线兜底**：daemon 未运行时，hook 把事件落盘到 `pending.jsonl`；daemon 启动后自动补投（限 1 小时内、500 条）
- **安全设计**：事件服务仅监听 `127.0.0.1`，随机 token 常量时间比较鉴权；hook 进程永远退出码 0、读 stdin 带超时，绝不阻塞 Agent
- **托盘常驻**：启动不弹面板，只有双击悬浮窗、双击托盘图标或托盘右键菜单能唤出主面板（**唯一例外**：本机一个 agent 都没接上时，启动会直接弹出面板并停在「Agents 接入」页引导接入）；托盘菜单为自绘样式（与悬浮窗右键菜单同一套），含显示主窗口、重置流光、静音（静音时事件仍入历史）、退出；关闭窗口隐藏到托盘、开机自启、单实例，GUI 内可查看事件流

## 安装与构建

环境要求：[Node.js](https://nodejs.org/) + [pnpm](https://pnpm.io/)、[Rust](https://rustup.rs/)（stable）；Windows 构建需 Visual Studio C++ 构建工具。

```bat
build.bat
```

产物为 NSIS 安装器，位于 `target\release\bundle\nsis\`。

手动构建 / 开发模式：

```powershell
cd app
pnpm install
pnpm tauri build   # 打包
pnpm tauri dev     # 开发模式
```

## 配置

配置文件在平台配置目录（Windows `%APPDATA%\agent-bark\config.json`），一般通过 GUI 修改，也可手工编辑：端口与鉴权 token、状态音效、聚合窗口、免打扰时段、子代理过滤、Bark / Webhook 渠道、各 agent 开关、流光参数、悬浮窗（含自动隐藏）。

完整字段说明、Webhook 模板占位符、流光状态推导规则见 **[doc/configuration.md](doc/configuration.md)**。

## 文档

| 文档 | 内容 |
| --- | --- |
| [doc/agent-integration.md](doc/agent-integration.md) | 每种 agent 的接入方法、注册事件、应用侧动作、通用排查清单、探针验证法 |
| [doc/configuration.md](doc/configuration.md) | 配置字段、流光状态推导、项目结构、开发说明 |

## License

MIT
