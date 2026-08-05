//! `mo_core` — shared types, journal I/O, TOML config, and the SQLite
//! metadata DB used by both the gateway and the worker processes.

pub mod config;
pub mod db;
pub mod journal;
pub mod modes;
pub mod types;

pub use config::{
    ConfigError, DEFAULT_PORT, FileConfig, MoConfig, ModelConfig, default_agents_dir,
};
pub use db::{DbError, open as open_db};
pub use journal::{JournalError, JournalWriter, read_events, read_events_after};
pub use modes::{
    MODES, ModeInfo, ModeMarker, TOOL_NAMES, last_mode_marker, mode_change_approved_message,
    mode_change_message,
};
pub use types::{
    JournalEvent, JournalEventKind, JournalMessage, Mode, Session, SessionStatus, ToolCallInfo,
};
