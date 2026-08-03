//! `mo_core` — shared types, journal I/O, TOML config, and the SQLite
//! metadata DB used by both the gateway and the worker processes.

pub mod config;
pub mod db;
pub mod journal;
pub mod types;

pub use config::{
    ConfigError, DEFAULT_PORT, FileConfig, MoConfig, ModelConfig, default_agents_dir,
};
pub use db::{DbError, open as open_db};
pub use journal::{JournalError, JournalWriter, read_events, read_events_after};
pub use types::{
    JournalEvent, JournalEventKind, JournalMessage, Session, SessionStatus, ToolCallInfo,
};
