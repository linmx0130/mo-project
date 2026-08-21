//! Gateway shared state.

use std::path::PathBuf;
use std::sync::Mutex;

use mo_core::ModelConfig;
use rusqlite::Connection;

pub struct AppState {
    pub data_dir: PathBuf,
    pub db: Mutex<Connection>,
    pub worker_bin: PathBuf,
    /// Working directory the gateway process was started in; the frontend
    /// uses it as the default session workdir.
    pub cwd: PathBuf,
    /// The web UI's accent color as a hex value (from `mo.toml`, default
    /// `mo_core::config::DEFAULT_THEME_COLOR`); served to the frontend via
    /// `GET /api/meta`, which derives the translucent tints and the
    /// dark-mode variant from it.
    pub theme_color: String,
    /// Global agents dir passed to every worker (`$HOME/.agents` by
    /// default; see `mo_core::config::default_agents_dir`).
    pub agents_dir: PathBuf,
    /// Maximum number of tool calls from a single assistant message that
    /// execute concurrently; passed to every worker as
    /// `MO_MAX_TOOL_CONCURRENCY` (from `mo.toml`).
    pub max_tool_concurrency: usize,
    /// Fraction of the model's `context_window` at which the worker asks
    /// the model to generate a handoff prompt and starts sending only the
    /// compressed context; passed to every worker as
    /// `MO_CONTEXT_COMPRESSION_THRESHOLD` (from `mo.toml`, default 0.75).
    pub context_compression_threshold: f64,
    /// All configured models (from `mo.toml`); the first is the default.
    pub models: Vec<ModelConfig>,
}

impl AppState {
    /// The default model: the first one in the config, used to launch jobs
    /// and generate session titles.
    pub fn default_model(&self) -> Option<&ModelConfig> {
        self.models.first()
    }

    /// Find a configured model by its model name.
    pub fn find_model(&self, name: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.name == name)
    }
}
