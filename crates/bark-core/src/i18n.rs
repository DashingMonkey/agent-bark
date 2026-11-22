//! 界面语言（中/英）。
//!
//! 偏好存 config.json 顶层 `language` 键（三态 `auto` / `zh` / `en`，与主题
//! system / dark / light 同构）；`auto` 表示跟随操作系统语言，每次启动现解析。
//!
//! **解析点全应用只有 Rust 这一处**（[`LanguagePref::resolve`]，用 sys-locale 读
//! 系统语言）：webview 的 `navigator.language` 在 Tauri 下不可靠，且各窗口各自
//! 检测会让悬浮窗与推送通知拿到不同语言。前端通过 `get_locale` 命令拿解析结果，
//! Rust 侧文案（推送标题、命令错误提示）直接用 `AppState` 缓存的 [`Lang`]。
//!
//! 词表不走 rust-i18n 之类的运行时查表：Rust 侧用户可见文案就几十条、集中在
//! event.rs / state.rs / commands.rs / update.rs，直接在函数里按 `Lang` 分支
//! （`default_title(lang)` 等）最直白、可 grep、无构建脚本。

use serde::{Deserialize, Serialize};

/// 用户可选的界面语言（config.json 顶层 `language` 键的取值）。
///
/// 旧配置没有这个键：serde default 补 [`LanguagePref::Auto`]，即按操作系统语言
/// 自动选择（首次启动自动切换的来源）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePref {
    /// 跟随操作系统语言（默认）：zh* → 中文，其余 → 英文
    #[default]
    Auto,
    /// 简体中文
    Zh,
    /// English
    En,
}

impl LanguagePref {
    /// 用操作系统语言解析偏好。读不到系统语言（理论上的异常环境）按英文兜底：
    /// 英文是国际软件的通用兜底，且中文用户大概率会显式选「简体中文」。
    pub fn resolve(self) -> Lang {
        match self {
            Self::Zh => Lang::Zh,
            Self::En => Lang::En,
            Self::Auto => match sys_locale::get_locale() {
                Some(l) if l.to_ascii_lowercase().starts_with("zh") => Lang::Zh,
                _ => Lang::En,
            },
        }
    }
}

/// 解析后的界面语言（运行时真正使用；偏好为 `auto` 时由系统语言决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// 发给前端的语言 id（`get_locale` 命令与 `locale-changed` 事件的载荷），
    /// 与 vue-i18n 实例的 locale 键一致。
    pub fn id(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// 聚合通知的合并附注（state.rs 的 `append_merged_note` 用）
    pub fn merged_note(self, merged: u32) -> String {
        match self {
            Lang::Zh => format!("\n（合并了 {merged} 条同类事件）"),
            Lang::En => format!("\n(merged {merged} similar events)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pref_serializes_as_lowercase_strings() {
        // config.json 里的形态：小写字符串，老配置缺键时 serde default 补 auto
        assert_eq!(serde_json::to_value(LanguagePref::Auto).unwrap(), "auto");
        assert_eq!(serde_json::to_value(LanguagePref::Zh).unwrap(), "zh");
        assert_eq!(serde_json::to_value(LanguagePref::En).unwrap(), "en");
        let pref: LanguagePref = serde_json::from_str("\"zh\"").unwrap();
        assert_eq!(pref, LanguagePref::Zh);
    }

    #[test]
    fn explicit_pref_overrides_system_locale() {
        // 显式选择不受系统语言影响（测试环境无法固定 sys-locale 的返回，
        // 但显式档必须走死分支，这是「用户选了就得算数」的契约）
        assert_eq!(LanguagePref::Zh.resolve(), Lang::Zh);
        assert_eq!(LanguagePref::En.resolve(), Lang::En);
    }

    #[test]
    fn lang_ids_match_frontend_locale_keys() {
        assert_eq!(Lang::Zh.id(), "zh");
        assert_eq!(Lang::En.id(), "en");
    }
}
