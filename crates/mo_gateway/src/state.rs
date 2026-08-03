//! Gateway shared state.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

pub struct AppState {
    pub data_dir: PathBuf,
    pub db: Mutex<Connection>,
    pub worker_bin: PathBuf,
    /// Working directory the gateway process was started in; the frontend
    /// uses it as the default session workdir.
    pub cwd: PathBuf,
}
