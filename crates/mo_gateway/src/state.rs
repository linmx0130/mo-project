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
    /// Global agents dir passed to every worker (`$HOME/.agents` by
    /// default; see `mo_core::config::default_agents_dir`).
    pub agents_dir: PathBuf,
    /// Subagent nesting depth granted to root sessions.
    pub subagent_depth: u32,
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
