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
| Gemini CLI | [Hook](doc/agent-integration.md#11-gemini-cli) |
| Qwen Code | [Hook](doc/agent-integration.md#12-qwen-code) |
| Factory Droid | [Hook](doc/agent-integration.md#13-factory-droid) |
| Grok CLI（Grok Build） | [Hook](doc/agent-integration.md#14-grok-cligrok-build) |
| Cursor | [Hook](doc/agent-integration.md#15-cursor) |
| Copilot CLI | [Hook](doc/agent-integration.md#16-copilot-cli) |
| Antigravity | [Hook](doc/agent-integration.md#18-antigravity) |
| Kimi Code | [Hook](doc/agent-integration.md#19-kimi-code) |
| Kimi Work | [Watch](doc/agent-integration.md#20-kimi-work监控型--非官方) |
| 豆包工作（DoubaoWork） | [Watch](doc/agent-integration.md#21-豆包工作监控型--非官方) |

> **每种 agent 的配置路径、注册事件、需要你在应用侧做的动作（信任/重启）以及排障步骤，见 [doc/agent-integration.md](doc/agent-integration.md)。**

## 功能特性

- **屏幕光效**：agent 运行时在屏幕最外圈亮起光带，切走窗口也能从余光看到状态——**思考色**=运行中、**等待色**=等你确认或输入、**完成色**·停留后淡出=完成、**失败色**=有明确失败信号的意外终止；**手动中止**（你自己按的停止）不亮失败色也不亮完成色——没有别的 agent 在跑时直接收起光效。透明 + 点击穿透 + 不抢焦点；「通知」页的屏幕光效可选**生效显示器**（逐块屏或全部，自动列出分辨率）、**边缘光效 / 全屏特效按状态各选类型**（思考 / 等待 / 完成 / 失败 四行各选「无 / 呼吸 / 流光」与「无 / 雾散 / 扫描」，全屏在每次颜色亮起时整屏补放、多会话并行时按事件角色补放），并可就地预览调参（思路参考 [EdgeGlow](https://github.com/vector4wang/EdgeGlow)）
- **状态音效**：思考 / 等待 / 完成 / 失败四个状态各自可选一个内置音效与播放次数（默认全部不响；8 种 CC0 素材随应用分发、响度已统一，不依赖系统提示音），「通知」页里一键试听；托盘「静音」与免打扰时段内同样不响
- **桌面悬浮窗**：固定 280px 横向卡片显示当前活动会话，多会话内部滚动；等待 / 失败使用状态底色与描边，手动终止为中性色。悬停详情优先右侧，放不下时在下方对齐展示。拖到顶部或左右屏幕外缘后滑动收起，保留同色窄边并记住停靠位置；移入展开、移出收回。贴边时，会话新增、显示状态类别变化及完成 / 失败 / 中止会自动展开 5 秒，鼠标停留期间持续展开；思考与执行工具之间切换、重复心跳均不触发或续时。单击托盘图标可找回已启用的悬浮窗并展开 2 秒；未贴边时保持常驻。右键可打开主面板 / 固定位置 / 取消靠边隐藏 / 关闭，双击唤出主面板。
- **第三方通知**：Bark（iOS）、通用 Webhook（飞书 / 企微 / 钉钉 / ntfy / Server酱 等，模板可自定义）
- **通知规则引擎**：聚合窗口（同 Agent 同类事件合并）、免打扰时段（如 `23:00-08:00`，可跨零点）
- **离线兜底**：daemon 未运行时，hook 把事件落盘到 `pending.jsonl`；daemon 启动后自动补投（限 1 小时内、500 条）
- **运行状态核验**：每 5 分钟主动核验，托盘右键「检测运行状态」可立即触发。Codex 只读 SQLite 索引与当前回合 JSONL，数据库型 Agent 唤醒各自监控；日志观察器持续读取。只有明确结束信号才收档，静默、读取失败和不支持核验均保留最后已知状态，不再按超时终止长任务。Codex 选择框显示等待确认，后台工具活动与失败仍正常记录。事件流可「移除」停滞会话（只影响本地显示，不停止 Agent、不发终态通知；缓存心跳不会复活，真实新活动会恢复）。Kimi Work 与豆包工作降级分支在真实日志静默 5 分钟后停发合成心跳，保留最后状态。事件正文最多显示 8 行、超出内部滚动，支持键盘聚焦。
- **安全设计**：事件服务仅监听 `127.0.0.1`，随机 token 常量时间比较鉴权；hook 进程永远退出码 0、读 stdin 带超时，绝不阻塞 Agent
- **托盘常驻**：左键单击找回已启用的悬浮窗并临时展开 2 秒；启动不弹面板，只有双击悬浮窗、双击托盘图标或托盘右键菜单能唤出主面板（**唯一例外**：本机一个 agent 都没接上时，启动会直接弹出面板并停在「Agents 接入」页引导接入）；托盘菜单为自绘样式（与悬浮窗右键菜单同一套），含显示主窗口、检测运行状态、静音（静音时事件仍入历史）、退出；关闭窗口隐藏到托盘、开机自启、单实例，GUI 内可查看事件流

## 安装与构建

环境要求：[Node.js](https://nodejs.org/) + [pnpm](https://pnpm.io/)、[Rust](https://rustup.rs/)（stable）；Windows 构建需 Visual Studio C++ 构建工具。

```bat
build.bat
```

产物为 NSIS 安装器，位于 `target\release\bundle\nsis\`。

构建脚本会先运行前端类型检查、页面保存回归测试、解密探针测试和 Rust 全量测试（含生成插件的模拟测试），任一步失败即停止打包。首次运行会额外编译测试目标，后续可复用编译缓存。

单独运行检查（从仓库根目录）：

```powershell
cargo test --workspace --all-targets
pnpm --dir app typecheck
pnpm --dir app test
pnpm --dir app test:tools
```

升级后如果 Agents 页提示 DeepSeek Harness 插件已更新，请重启正在运行的 dsh。磁盘插件更新不会替换其内存中的旧代码；旧插件与新 daemon 混用时，回答提问后可能仍显示等待，直到后续真实活动或回合结束。

手动构建 / 开发模式：

```powershell
cd app
pnpm install
pnpm tauri build   # 打包
pnpm tauri dev     # 开发模式
```

## 配置

配置文件在平台配置目录（Windows `%APPDATA%\agent-bark\config.json`），一般通过 GUI 修改，也可手工编辑：端口与鉴权 token、状态音效、聚合窗口、免打扰时段、子代理过滤、Bark / Webhook 渠道、各 agent 开关、流光参数、悬浮窗启用与停靠位置。

完整字段说明、Webhook 模板占位符、流光状态推导规则见 **[doc/configuration.md](doc/configuration.md)**。

## 文档

| 文档 | 内容 |
| --- | --- |
| [doc/agent-integration.md](doc/agent-integration.md) | 每种 agent 的接入方法、注册事件、应用侧动作、通用排查清单、探针验证法 |
| [doc/configuration.md](doc/configuration.md) | 配置字段、流光状态推导、项目结构、开发说明 |

## License

MIT
