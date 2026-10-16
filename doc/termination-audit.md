# Agent 终止状态审查（2026-10-14）

范围：审查时注册表全部 **21 个接入**（随后按用户要求移除 Windsurf，现支持 20 个）的事件映射、载荷归一化、监控推断，以及统一事件进入会话状态机后的行为。本轮执行的是代码审查、官方协议核对与回归测试，**不是对全部宿主逐一现场点击停止**。表中本机样本包含先前已记录的验证结果。

统一语义：正常完成 → `run_completed`；明确取消 → `run_aborted`（显示「已终止」，不发完成/失败通知）；执行失败 → `run_failed`。`SessionEnd` 只证明会话结束，不能据此承诺用户按停止一定会触发。没有取消证据时也不能把所有完成统一改成终止。

| 接入 | 已检查的结束信号与处理 | 证据与限制 |
| --- | --- | --- |
| Claude Code | Stop 完成、StopFailure 失败、SessionEnd 释放 | 仓库协议记录与归一化测试；Esc 不触发 Stop，缺少即时取消事件，需后续事件/超时收场 |
| TraeCode | Stop 完成；renderer.log DoneHandler canceled → 终止 | 本机取消日志样本和尾读/状态机测试；日志格式变化时降级为超时 |
| CodeBuddy | Stop 完成、StopFailure 失败、SessionEnd 释放 | 代码映射及测试；未验证本版每次按停止都发 SessionEnd |
| Qoder（含 CN） | Stop 完成、SessionEnd → 终止 | 仓库已有本机停止样本及归一化测试；正常 Stop 后的 SessionEnd 由回声规则抑制 |
| Codex | Stop 完成、Interrupt/SessionEnd → 终止；transcript 的 task_complete.error → 失败 | 2026-10-15 补充：本机额度耗尽样本确认没有结束 hook，按回合 ID 补偿失败；过滤内部输入建议回合与迟到工具事件。日志格式变化时降级超时，仍需信任 hooks |
| ZCode | Stop 完成；本地 turn.failed/status=cancelled 等日志 → 终止；failed → 失败 | 已有本机样本、日志解析测试；中断 Hook 进程常被宿主取消，依赖日志补充 |
| OpenCode | MessageAbortedError → 终止；session.idle → 完成 | 本轮修正取消后的 idle 抑制范围：持续到新 busy 回合，覆盖迟到 idle 和快速重开；执行生成 JS 的行为测试通过，未现场操作宿主 |
| DeepseekHarness（DSH） | turn/end 的 aborted/interrupted → 终止；completed → 完成；error/blocked → 失败 | 已有结束原因分支、生成插件行为测试；max-tokens 仍以完成事件携带「达到输出上限」说明，不能视作用户取消 |
| TraeWork | chat_turn canceled → 终止、completed → 完成、failed → 失败 | 本机数据库状态记录与跃迁测试；依赖非官方数据库格式 |
| WorkBuddy | 数据库 completed/failed 候选结合 JSONL 明确 Interrupted by user → 终止 | 本机 SDK/回合样本证实取消也写 completed；上一轮新增严格载荷、时间与最新记录校验；记录缺失时仍按数据库状态判断 |
| Gemini CLI | AfterAgent 完成、SessionEnd 释放 | 本轮核对官方 AfterAgent 为最终响应；未确认单独取消事件，不能保证即时识别停止 |
| Qwen Code | Stop 完成、StopFailure 失败、SessionEnd 释放 | 仓库协议记录及映射测试；按停止是否产生终态未现场验证 |
| Factory Droid | Stop 完成、SessionEnd 释放 | 仓库协议记录及映射测试；无已确认的独立取消信号 |
| Grok CLI | StopCancelled → 终止；Stop 完成、StopFailure 失败 | 显式取消映射和测试已有；本轮未现场操作宿主 |
| Cursor | stop.status 的 completed/aborted/error → 完成/终止/失败；sessionEnd.reason 同样分类，关闭窗口释放 | **本轮修复**原来一律把 stop 当完成的错误；当前官方文档及载荷回归测试，未现场操作宿主 |
| Copilot CLI | agentStop 完成；sessionEnd.reason 的 complete/abort/user_exit/error/timeout 分类 | **本轮修复**原来一律把 sessionEnd 当终止的错误；当前官方文档及载荷回归测试。会话结束不等于每次按停止都会收到 |
| Windsurf（已移除） | 按用户要求移除检测与接入 | 不再属于当前支持范围 |
| Antigravity | Stop → 完成 | 当前适配器没有取消分支，宿主载荷未现场核对；不能承诺手动停止已正确识别 |
| Kimi Code | Interrupt/SessionEnd → 终止；Stop 完成、StopFailure 失败 | 显式取消映射及测试已有；本轮未现场操作宿主 |
| Kimi Work | wire.jsonl turn.cancel → 终止；end_turn → 完成 | 已有本机协议样本与行为测试；日志可读时有明确取消信号 |
| 豆包工作 | SDK 日志确认 `click_break_button` + `success`，按会话/消息 ID 匹配 → 终止；数据库清空等待 6s 后 → 完成 | **已补用户停止信号**：2026-10-14 本机真实取消样本验证。重复/旧消息取消不会影响新一轮；日志缺失、未观测到对应消息 ID、失败原因仍属缺口 |

本轮确认的官方资料：

- Cursor：[Hooks — stop / sessionEnd](https://cursor.com/docs/agent/hooks)。`stop.status = completed | aborted | error`；`sessionEnd.reason` 另含 `window_close | user_close`。
- Copilot CLI：[Hook configuration](https://docs.github.com/en/copilot/reference/hooks-configuration)。`sessionEnd.reason = complete | error | abort | timeout | user_exit`。
- Gemini CLI：[Hooks reference — AfterAgent](https://geminicli.com/docs/hooks/reference/)。回合最终响应后触发；此信息本身不构成取消识别保证。

后续重点是 Antigravity 的真实取消样本，以及豆包工作缺失日志时的结束原因，而不是继续扩大通用字符串匹配。当前结论是「全部接入已审查，发现的明确映射错误已修正」，不能写成「全部 Agent 的手动停止均已实测通过」。
