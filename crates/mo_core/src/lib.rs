//! `mo_core` — shared types, journal I/O, TOML config, and the SQLite
//! metadata DB used by both the gateway and the worker processes.

pub mod config;
pub mod db;
pub mod journal;
pub mod modes;
pub mod skills;
pub mod tools;
pub mod types;

pub use config::{
    ConfigError, DEFAULT_PORT, FileConfig, MoConfig, ModelConfig, default_agents_dir,
};
pub use db::{DbError, open as open_db};
pub use journal::{JournalError, JournalWriter, read_events, read_events_after, read_events_tail};
pub use modes::{
    MODES, ModeInfo, ModeMarker, last_mode_marker, mode_change_approved_message,
    mode_change_message,
};
pub use skills::{
    SKILL_LOAD_USER_PREFIX, Skill, discover_skills, find_skill, parse_skill_md, skill_load_message,
};
pub use tools::{
    FIXED_TOOLS, TOGGLEABLE_TOOLS, TOOL_NAMES, TOOLS, ToolInfo, is_enabled, is_fixed,
    is_toggleable, resolve_enabled_tools,
};
pub use types::{
    AskUserMarker, AskUserOption, AskUserQuestion, JournalEvent, JournalEventKind, JournalMessage,
    Mode, PermissionDecision, PermissionMarker, PermissionRequestItem, Session, SessionStatus,
    ToolCallInfo, last_ask_user_marker, last_model_marker, last_permission_marker,
};
