pub mod agent;
pub mod config;
pub mod event;

pub use agent::{AgentKind, AdapterMode, RETIRED_AGENT_IDS};
pub use config::{
    AgentState, BarkConfig, ChannelsConfig, GlowConfig, GlowEffect, MonitorTarget, NotifyConfig,
    RulesConfig, ServerConfig, StateEffects, WebhookChannelConfig, WidgetConfig,
    MAX_AGGREGATE_WINDOW_MS,
};
pub use event::{now_millis, project_name_from_cwd, truncate_chars, EventKind, NormalizedEvent, MAX_MESSAGE_CHARS};
