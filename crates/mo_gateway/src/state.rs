//! Gateway shared state.

use std::path::PathBuf;
use std::sync::Mutex;

use rusqlite::Connection;

pub struct AppState {
    pub data_dir: PathBuf,
    pub db: Mutex<Connection>,
    pub worker_bin: PathBuf,
}
