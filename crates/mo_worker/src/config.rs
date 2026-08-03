//! Worker CLI/env configuration.
//!
//! The worker is spawned by the gateway with `--session-id <id>` and reads
//! the rest of its configuration from the environment (the gateway passes
//! down the values resolved from `mo.toml`). For standalone runs it falls
//! back to the shared config file (`mo.toml`, see `mo_core::config`) for
//! anything the environment does not provide:
//!
//! * `MO_DATA_DIR` — runtime data dir (default `./data`); holds `mo.db` and `sessions/`.
//! * `MO_MODEL_BASE_URL` — OpenAI-compatible endpoint base URL (no trailing `/`).
//! * `MO_MODEL_NAME` — model name to request.
//! * `MO_AUTH_TOKEN` — optional Bearer token / API key.
//! * `MO_SUBAGENT_DEPTH` — subagent nesting depth (default 0, hard cap 3).
//! * `MO_AGENTS_DIR` — global agents dir (default `$HOME/.agents`); holds the
//!   global `AGENTS.md` and global skills (`<dir>/<skill>/SKILL.md` or
//!   `<dir>/skills/<skill>/SKILL.md`). Skill name + description are injected
//!   into the system prompt; the body is loaded on demand via `load_skill`.

use std::env;
use std::path::PathBuf;

pub const MAX_SUBAGENT_DEPTH: u32 = 3;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub session_id: String,
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
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
    #[error("missing model base URL: set MO_MODEL_BASE_URL or configure [[models]] in mo.toml")]
    MissingBaseUrl,
    #[error("missing model name: set MO_MODEL_NAME or configure [[models]] in mo.toml")]
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

    // The shared config file is a fallback for standalone runs; the gateway
    // normally passes everything down via env, which always wins.
    let file_cfg = mo_core::MoConfig::load(None).ok();

    let data_dir = env::var("MO_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|_| file_cfg.as_ref().map(|c| c.data_dir.clone()).ok_or(()))
        .unwrap_or_else(|_| PathBuf::from("./data"));
    let agents_dir = env::var("MO_AGENTS_DIR")
        .map(PathBuf::from)
        .or_else(|_| file_cfg.as_ref().map(|c| c.agents_dir.clone()).ok_or(()))
        .unwrap_or_else(|_| mo_core::config::default_agents_dir());
    let subagent_depth = match env::var("MO_SUBAGENT_DEPTH") {
        Ok(v) => v.parse::<u32>().map_err(|_| ConfigError::BadDepth(v))?,
        Err(_) => file_cfg.as_ref().map(|c| c.subagent_depth).unwrap_or(0),
    };
    // Model: env first (the gateway passes the per-session model), then the
    // default model from the config file.
    let (model_base_url, model_name, auth_token) =
        match (env::var("MO_MODEL_BASE_URL"), env::var("MO_MODEL_NAME")) {
            (Ok(base_url), Ok(model_name)) => (
                base_url,
                model_name,
                env::var("MO_AUTH_TOKEN").ok().filter(|v| !v.is_empty()),
            ),
            _ => match file_cfg.as_ref().and_then(|c| c.models.first()) {
                Some(model) => (
                    model.base_url.clone(),
                    model.name.clone(),
                    model.token.clone(),
                ),
                None => {
                    if env::var("MO_MODEL_BASE_URL").is_err() {
                        return Err(ConfigError::MissingBaseUrl);
                    }
                    return Err(ConfigError::MissingModelName);
                }
            },
        };

    Ok(WorkerConfig {
        session_id,
        data_dir,
        agents_dir,
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
