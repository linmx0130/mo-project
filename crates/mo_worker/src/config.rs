//! Worker CLI/env configuration.
//!
//! The worker is spawned by the gateway with `--session-id <id>` and reads
//! the rest of its configuration from the environment:
//!
//! * `MO_DATA_DIR` — runtime data dir (default `./data`); holds `mo.db` and `sessions/`.
//! * `MO_MODEL_BASE_URL` — OpenAI-compatible endpoint base URL (no trailing `/`).
//! * `MO_MODEL_NAME` — model name to request.
//! * `MO_AUTH_TOKEN` — optional Bearer token / API key.
//! * `MO_SUBAGENT_DEPTH` — subagent nesting depth (default 0, hard cap 3).

use std::env;
use std::path::PathBuf;

pub const MAX_SUBAGENT_DEPTH: u32 = 3;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub session_id: String,
    pub data_dir: PathBuf,
    pub model_base_url: String,
    pub model_name: String,
    pub auth_token: Option<String>,
    pub subagent_depth: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required argument: --session-id <id>")]
    MissingSessionId,
    #[error("invalid MO_SUBAGENT_DEPTH: {0}")]
    BadDepth(String),
    #[error("missing required env var MO_MODEL_BASE_URL")]
    MissingBaseUrl,
    #[error("missing required env var MO_MODEL_NAME")]
    MissingModelName,
}

pub fn parse_config() -> Result<WorkerConfig, ConfigError> {
    let mut session_id: Option<String> = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--session-id" {
            session_id = args.next();
        }
    }
    let session_id = session_id.ok_or(ConfigError::MissingSessionId)?;

    let data_dir = env::var("MO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let model_base_url = env::var("MO_MODEL_BASE_URL").map_err(|_| ConfigError::MissingBaseUrl)?;
    let model_name = env::var("MO_MODEL_NAME").map_err(|_| ConfigError::MissingModelName)?;
    let auth_token = env::var("MO_AUTH_TOKEN").ok().filter(|v| !v.is_empty());
    let subagent_depth = match env::var("MO_SUBAGENT_DEPTH") {
        Ok(v) => v.parse::<u32>().map_err(|_| ConfigError::BadDepth(v))?,
        Err(_) => 0,
    };

    Ok(WorkerConfig {
        session_id,
        data_dir,
        model_base_url,
        model_name,
        auth_token,
        subagent_depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_cap_is_three() {
        assert_eq!(MAX_SUBAGENT_DEPTH, 3);
    }
}
