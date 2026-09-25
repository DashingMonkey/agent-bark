use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// 统一事件类型（所有 agent 的事件归一化到这几类）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// 会话开始（不产生通知，用于统计/聚焦）
    SessionStart,
    /// 心跳：回合开始（UserPromptSubmit）或工具调用（PreToolUse）等运行中信号。
    /// 不通知、不入历史，只用于推导会话实时状态（思考中 / 执行工具）。
    Activity,
    /// **工具级失败**：某次工具调用出错（如编辑前没先读文件），agent 会自行重试，
    /// 回合仍在跑——**不是** `RunFailed`。
    ///
    /// 引入它的原因（实测 ZCode）：一次瞬时工具失败曾触发全链路的回合失败表现——
    /// 终止色全屏特效 + 事件流红色「任务失败」+ 系统通知 + 会话被摘出运行中表，
    /// 而下一秒重试就成功了，全是噪音。归一成这一类后：会话当心跳留在表里
    /// （相位回落思考中）、不通知、不触发终止色；事件仍入历史与事件流
    /// （中性色「工具失败」，可排查不吓人）。真回合失败仍由回合结束信号 +
    /// 心跳判死兜底，不漏报。
    ToolFailed,
    /// **工具收尾**：某次工具调用正常结束（PostToolUse），回合仍在跑——
    /// 与 `ToolFailed` 同属「工具级信号」，但方向相反：一个是失败后的重试前夜，
    /// 一个是收尾后的思考期开始。
    ///
    /// 引入它的原因（实测 ZCode）：工具结束是**等待状态解除的唯一信号**——
    /// 答完 `AskUserQuestion` / 批完权限之后，agent 要先思考一段时间才可能调下一个
    /// 工具，这期间没有 `PreToolUse`（下一次工具开始才有）、没有 `UserPromptSubmit`
    /// （新回合才有），不收「工具收尾」的话会话相位会一直卡在等待中、警告色一直亮到
    /// 下一个工具开始。它当心跳处理：相位回落思考中、会话留在表里；
    /// **不通知、不入历史、不响音效**（与 `Activity` 同口径的纯状态信号：
    /// 一轮可达多次，入历史会冲掉环形缓冲，响思考音会把「新回合开始」的语义淹没）。
    ToolFinished,
    /// 打断：等待用户授权/确认权限
    PermissionRequired,
    /// 打断：等待用户输入（提问/空闲提醒）
    InputRequired,
    /// 完成：回合结束
    RunCompleted,
    /// 失败：回合执行出错
    RunFailed,
    /// **中止**：用户主动终止（或外部取消）导致回合结束——既不是完成，也不是失败。
    ///
    /// 引入它的原因：同一个动作（手动点停止）在不同 agent 上落到了不同的信号里——
    /// Qoder 是 `SessionEnd`、ZCode 靠读日志推断、WorkBuddy 是运行中被归档，
    /// 而「Stop 之后紧跟的终态回声」又会把正常完成的完成色压掉。按失败处理会亮终止色 +
    /// 弹「任务失败」（用户自己按的，纯噪音），按完成处理会亮完成色（谎报完成）。
    /// 统一成这一类：**不通知**；流光在没有别的会话在跑时**直接收起**（不亮终止色也不亮完成色），
    /// 还有别的会话在跑就照常显示它们的状态。
    RunAborted,
}

impl EventKind {
    pub fn default_title(&self) -> &'static str {
        match self {
            EventKind::SessionStart => "会话开始",
            EventKind::Activity => "运行中",
            EventKind::ToolFailed => "工具失败",
            EventKind::ToolFinished => "工具完成",
            EventKind::PermissionRequired => "需要确认",
            EventKind::InputRequired => "等待输入",
            EventKind::RunCompleted => "任务完成",
            EventKind::RunFailed => "任务失败",
            EventKind::RunAborted => "已中止",
        }
    }

    /// SessionStart / Activity / ToolFailed / ToolFinished 不触发用户通知
    /// （Activity 一个回合可达上百次，通知管道绝不能见到它；
    /// ToolFailed 是 agent 自己会重试的瞬时失败，弹「任务失败」纯属噪音；
    /// ToolFinished 是每次工具收尾都发的纯状态信号，同理只能进状态机）。
    ///
    /// RunAborted 也不通知：这是**用户自己按的停止**，他就在键盘前，
    /// 再来一条「已中止」只是噪音（事件仍入历史与事件流，会话列表照常清掉）。
    pub fn should_notify(&self) -> bool {
        !matches!(
            self,
            EventKind::SessionStart
                | EventKind::Activity
                | EventKind::ToolFailed
                | EventKind::ToolFinished
                | EventKind::RunAborted
        )
    }
}

/// 归一化后的事件，全链路统一格式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub id: String,
    /// AgentKind::id()
    pub agent: String,
    #[serde(rename = "type")]
    pub kind: EventKind,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
    /// 从 cwd 推导的项目名
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub message: String,
    /// unix 毫秒
    pub timestamp: i64,
    /// 子代理（subagent）事件：默认不通知，避免一个任务被拆成十几条通知
    #[serde(default)]
    pub is_subagent: bool,
    /// 心跳事件携带的工具名（PreToolUse 的 tool_name）。
    /// 仅 Activity 类事件使用：状态机据此区分「思考中」与「执行工具」。
    /// `ToolFinished` 虽也带工具名，但相位由 kind 决定（一律回落思考中），不读它。
    #[serde(default)]
    pub tool_name: Option<String>,
}

impl NormalizedEvent {
    /// 从 agent 原始 hook stdin JSON 提取公共字段并构造事件
    pub fn from_raw(agent: &str, kind: EventKind, raw: &Value) -> Self {
        let session_id = raw
            .get("session_id")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("sessionId").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let cwd = raw
            .get("cwd")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("workspace_roots").and_then(|v| v.as_array()).and_then(|a| a.first()).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let message = truncate_chars(&extract_message(raw), MAX_MESSAGE_CHARS);
        let project = project_name_from_cwd(&cwd);
        NormalizedEvent {
            id: uuid::Uuid::new_v4().to_string(),
            agent: agent.to_string(),
            kind,
            session_id,
            cwd,
            project,
            message,
            timestamp: now_millis(),
            is_subagent: detect_subagent(raw),
            tool_name: extract_tool_name(raw),
        }
    }

    pub fn notify_title(&self) -> String {
        format!("{} · {}", self.kind.default_title(), agent_display(&self.agent))
    }

    pub fn notify_body(&self) -> String {
        let project = self.project.clone().unwrap_or_else(|| self.cwd.clone());
        let mut body = if project.is_empty() { String::new() } else { project };
        if !self.message.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&truncate_chars(&self.message, 200));
        }
        body
    }
}

/// 子代理（subagent）事件判定启发式：
/// 1. 显式布尔字段 `is_subagent` / `subagent` 优先——agent 明确告知时直接尊重；
/// 2. 否则看 `agent_id` / `agent_type`：Claude Code 系 hook 只在「hook 在子代理内部
///    触发」时给出 agent_id，agent_id + agent_type 同现于 SubagentStart/SubagentStop，
///    故二者任一非空即判子代理（依据 https://code.claude.com/docs/en/hooks ）；
/// 3. 误判代价不对称：漏判子代理会刷屏，误判主事件只会少一条推送通知
///    （事件仍进历史与前端事件流），故宁可偏向判 true。
///    若将来实测到某 agent 的主事件也带这些字段，需回看此处。
fn detect_subagent(raw: &Value) -> bool {
    for key in ["is_subagent", "subagent"] {
        if let Some(b) = raw.get(key).and_then(|v| v.as_bool()) {
            return b;
        }
    }
    ["agent_id", "agent_type"].iter().any(|key| {
        raw.get(*key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    })
}

fn extract_tool_name(raw: &Value) -> Option<String> {
    raw.get("tool_name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn extract_message(raw: &Value) -> String {
    // "prompt" 排最后：UserPromptSubmit 的 prompt 是回合任务的展示文案，
    // 但对已有 message 的 agent 事件不能抢优先级
    for key in ["message", "notification", "last_assistant_message", "reason", "error", "prompt"] {
        if let Some(s) = raw.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// 从 cwd 取最后一段作为项目名（通知正文 / 会话列表展示用）。
///
/// 末段是「会话 id / 日期」这类非项目名时逐个向上找：实测 Qoder 桌面应用的会话
/// cwd 是 `<用户目录>\Documents\Qoder\<日期>\<8位会话id>`，直接取末段会让通知正文
/// 顶部出现一串无意义的哈希；IDE / CLI 模式下 cwd 就是真实项目目录，行为不变。
pub fn project_name_from_cwd(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let path = std::path::Path::new(cwd);
    let mut cur = Some(path);
    while let Some(p) = cur {
        match p.file_name().map(|n| n.to_string_lossy().into_owned()) {
            Some(name) if !is_scratch_name(&name) => return Some(name),
            _ => cur = p.parent(),
        }
    }
    // 整条路径都是 id / 日期（异常输入）：保持旧行为，有总比空着好
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// 「不是项目名」的目录名：会话 id、UUID、日期。
///
/// 误判代价不对称——判成 scratch 只是少显示一层目录名（会继续向上找），
/// 所以宁可宽一点：8 位以上含数字的纯十六进制（Qoder 应用用 8 位会话 id 当目录名）、
/// UUID、`YYYY-MM-DD`。
fn is_scratch_name(name: &str) -> bool {
    let hex_id = (8..=40).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_hexdigit())
        && name.chars().any(|c| c.is_ascii_digit());
    let uuid = {
        let parts: Vec<&str> = name.split('-').collect();
        let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
        parts.len() == 5
            && lens == [8, 4, 4, 4, 12]
            && name.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    };
    let bytes = name.as_bytes();
    let date = bytes.len() == 10
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() });
    hex_id || uuid || date
}

fn agent_display(id: &str) -> String {
    crate::agent::AgentKind::from_id(id)
        .map(|k| k.display_name().to_string())
        .unwrap_or_else(|| id.to_string())
}

/// 事件 message 的最大字符数（超出截断，见 [`truncate_chars`]）。
///
/// 为什么需要上限：hook stdin 里的 message 长度完全不受控（agent 可以塞进整份
/// 大文件内容），不截断会随 pending 补投行与 500 条历史无界占用内存/磁盘。
/// 4000 字符对「通知正文 + 事件流摘要」绰绰有余。
pub const MAX_MESSAGE_CHARS: usize = 4000;

/// 按**字符**（不是字节）截断到 `max`，超出部分丢弃并追加 `…` 标记省略；
/// 不足 `max` 时原样返回。按字符截断保证多字节字符（中文/emoji）永不被劈开。
///
/// 用途：事件 message 等**长度不受控**的用户内容在进入状态机/历史前做上界防御
/// （[`MAX_MESSAGE_CHARS`]），通知正文等展示场景也用它做统一截断——
/// app 侧展示层（state.rs 等）复用本函数，不再各自实现一份。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn subagent_flag(raw: Value) -> bool {
        NormalizedEvent::from_raw("test", EventKind::RunCompleted, &raw).is_subagent
    }

    #[test]
    fn agent_scope_fields_mark_subagent() {
        // Claude Code 系 hook 只在「hook 在子代理内部触发」时给出 agent_id；
        // agent_id / agent_type 任一非空即视为子代理作用域事件。
        assert!(subagent_flag(json!({"agent_id": "abc123"})));
        assert!(subagent_flag(json!({"agent_type": "coder"})));
        assert!(subagent_flag(json!({"agent_id": "abc", "agent_type": "coder"})));
        // 空白字符串不算
        assert!(!subagent_flag(json!({"agent_id": "   "})));
        assert!(!subagent_flag(json!({})));
    }

    #[test]
    fn activity_event_extracts_tool_name_and_prompt() {
        // PreToolUse：tool_name 非空 → 状态机判「执行工具」
        let ev = NormalizedEvent::from_raw("trae-code", EventKind::Activity, &json!({
            "session_id": "s", "tool_name": "Bash", "tool_input": {}
        }));
        assert_eq!(ev.tool_name.as_deref(), Some("Bash"));
        // UserPromptSubmit：无 tool_name → 判「思考中」；prompt 进 message 供展示
        let ev = NormalizedEvent::from_raw("trae-code", EventKind::Activity, &json!({
            "session_id": "s", "prompt": "帮我修个 bug"
        }));
        assert_eq!(ev.tool_name, None);
        assert_eq!(ev.message, "帮我修个 bug");
        // 空白 tool_name 不算（防 agent 送空串）
        let ev = NormalizedEvent::from_raw("trae-code", EventKind::Activity, &json!({"tool_name": "  "}));
        assert_eq!(ev.tool_name, None);
    }

    #[test]
    fn explicit_boolean_field_wins() {
        // 显式布尔字段优先，即使与 agent_id/agent_type 矛盾也以它为准
        assert!(subagent_flag(json!({"is_subagent": true})));
        assert!(subagent_flag(json!({"subagent": true})));
        assert!(!subagent_flag(json!({"is_subagent": false, "agent_type": "coder"})));
        assert!(!subagent_flag(json!({"subagent": false, "agent_id": "abc"})));
    }

    #[test]
    fn project_name_skips_session_id_and_date_segments() {
        // 普通项目目录：仍然取末段
        assert_eq!(
            project_name_from_cwd(r"D:\Workspace\中融新大").as_deref(),
            Some("中融新大")
        );
        // Qoder 桌面应用的会话工作区：跳过 <会话id> 与 <日期>，落到 "Qoder"
        assert_eq!(
            project_name_from_cwd(r"C:\Users\x\Documents\Qoder\2026-09-10\4c7d79ff").as_deref(),
            Some("Qoder")
        );
        // UUID 形态的会话目录同理
        assert_eq!(
            project_name_from_cwd("/home/x/proj/a2783b28-fd69-496c-9db2-1d54ca00639a").as_deref(),
            Some("proj")
        );
        // 全是 id 的异常路径：回退旧行为（末段），不返回 None
        assert_eq!(project_name_from_cwd("4c7d79ff").as_deref(), Some("4c7d79ff"));
        // 纯字母的短目录名不是会话 id，照常当项目名
        assert_eq!(project_name_from_cwd("/home/x/cafebabe").as_deref(), Some("cafebabe"));
        assert_eq!(project_name_from_cwd(""), None);
    }

    #[test]
    fn message_is_truncated_to_bounded_length() {
        // message 长度完全不受控（agent 可塞进整份大文件）：进事件前必须截断到
        // MAX_MESSAGE_CHARS，防 pending 行 / 历史环形缓冲无界占内存
        let huge = "巨".repeat(MAX_MESSAGE_CHARS + 500);
        let ev = NormalizedEvent::from_raw("test", EventKind::RunCompleted, &json!({"message": huge}));
        assert!(ev.message.chars().count() <= MAX_MESSAGE_CHARS + 1, "截断后仍带省略号标记");
        assert!(ev.message.ends_with('…'), "被截断要留痕，不能静默砍尾");
        // 短 message 原样保留（绝大多数事件走这条路）
        let ev = NormalizedEvent::from_raw("test", EventKind::RunCompleted, &json!({"message": "hi"}));
        assert_eq!(ev.message, "hi");
    }

    #[test]
    fn truncate_chars_cuts_by_chars_and_marks_omission() {
        // 公开的截断助手契约（app 侧展示层复用）：
        // 不足 max 原样返回；超出按字符（非字节）截断并追加 …，多字节字符永不被劈开
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("中文测试", 2), "中文…");
        assert_eq!(truncate_chars("👍👍👍", 1), "👍…");
    }
}
