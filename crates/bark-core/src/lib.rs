pub mod agent;
pub mod config;
pub mod event;
pub mod i18n;

pub use agent::{AgentKind, AdapterMode, RETIRED_AGENT_IDS};
pub use config::{
    AgentState, BarkConfig, ChannelsConfig, GlowConfig, GlowEffect, MonitorTarget, NotifyConfig,
    RulesConfig, ServerConfig, StateEffects, WebhookChannelConfig, WidgetConfig, WidgetDockSide,
    MAX_AGGREGATE_WINDOW_MS,
};
pub use event::{now_millis, project_name_from_cwd, truncate_chars, EventKind, NormalizedEvent, RunBoundary, RunContext, MAX_MESSAGE_CHARS};
pub use i18n::{Lang, LanguagePref};
