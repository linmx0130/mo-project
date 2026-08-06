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
//! * `MO_CONTEXT_WINDOW` — optional model context window in tokens (unset =
//!   unlimited); embedded in `context_usage` journal events for the status bar.
//! * `MO_SUBAGENT_DEPTH` — the session's own subagent depth (0 for root
//!   sessions, which are never framed as subagents; worker-spawned
//!   subagents inherit parent depth + 1, hard-capped at 1).
//! * `MO_AGENTS_DIR` — global agents dir (default `$HOME/.agents`); holds the
//!   global `AGENTS.md` and global skills (`<dir>/<skill>/SKILL.md` or
//!   `<dir>/skills/<skill>/SKILL.md`). Skill name + description are injected
//!   into the system prompt; the body is loaded on demand via `load_skill`.
//! * `MO_MAX_TOOL_CONCURRENCY` — max number of tool calls from a single
//!   assistant message that execute at once (default 8, clamped to ≥ 1);
//!   the gateway passes the `max_tool_concurrency` value from `mo.toml`.
//! * `MO_CONTEXT_COMPRESSION_THRESHOLD` — the fraction of the model's
//!   context window at which the worker asks the model to generate a
//!   handoff prompt and starts sending only the compressed context
//!   (default 0.75, clamped to `(0, 1]`); the gateway passes the value
//!   from `mo.toml`.

use std::env;
use std::path::PathBuf;

use mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD;

/// The hard cap on subagent nesting. Subagents (sessions with a `parent_id`)
/// can never spawn further subagents — the depth limit is 1: a root session
/// may spawn subagents, and those subagents are leaves. The numeric
/// `subagent_depth` value a worker carries is clamped to this cap so the
/// system-prompt framing never claims a deeper nesting.
pub const MAX_SUBAGENT_DEPTH: u32 = 1;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub session_id: String,
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub model_base_url: String,
    pub model_name: String,
    pub auth_token: Option<String>,
    pub context_window: Option<u64>,
    pub subagent_depth: u32,
    /// Max number of tool calls from a single assistant message that
    /// execute concurrently (clamped to at least 1). The gateway passes the
    /// `max_tool_concurrency` value from `mo.toml`; standalone workers fall
    /// back to the config file, then to the default.
    pub max_tool_concurrency: usize,
    /// Fraction of the model's `context_window` at which the worker asks
    /// the model to generate a handoff prompt and starts sending only the
    /// compressed context (clamped to `(0, 1]`; only applies when a
    /// `context_window` is set). The gateway passes the value from
    /// `mo.toml`; standalone workers fall back to the config file, then to
    /// the default (0.75).
    pub context_compression_threshold: f64,
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
    // The session's own subagent depth. Root sessions are depth 0 by
    // definition; the gateway spawns them with MO_SUBAGENT_DEPTH=0 and
    // worker-spawned subagents inherit parent depth + 1 (clamped to the
    // hard cap). When the env is unset (standalone run) the session is a
    // root session, so the depth is 0 — the config file's `subagent_depth`
    // value is *not* used as the session's own depth: a root session must
    // never be framed as a subagent.
    let subagent_depth = env::var("MO_SUBAGENT_DEPTH")
        .ok()
        .map(|v| v.parse::<u32>().map_err(|_| ConfigError::BadDepth(v)))
        .transpose()?
        .unwrap_or(0);
    // Context window: env first (the gateway passes the per-session model's
    // window), then the default model from the config file. Unset = unlimited.
    let context_window = match env::var("MO_CONTEXT_WINDOW") {
        Ok(v) if !v.is_empty() => v.parse::<u64>().ok(),
        _ => file_cfg
            .as_ref()
            .and_then(|c| c.default_model())
            .and_then(|m| m.context_window),
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
    // Tool-call concurrency: env first (the gateway passes the resolved
    // `max_tool_concurrency` from `mo.toml`), then the config file, then the
    // default. Clamped to at least 1 so a misconfigured 0 can never make
    // the tool pipeline deadlock.
    let max_tool_concurrency = env::var("MO_MAX_TOOL_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .or_else(|| file_cfg.as_ref().map(|c| c.max_tool_concurrency))
        .unwrap_or(mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY)
        .max(1);
    // Context-compression threshold: env first (the gateway passes the
    // resolved value from `mo.toml`), then the config file, then the
    // default. Clamped to `(0, 1]` so a misconfigured 0 or >1 can never
    // disable or over-trigger compression silently.
    let context_compression_threshold = env::var("MO_CONTEXT_COMPRESSION_THRESHOLD")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .or_else(|| file_cfg.as_ref().map(|c| c.context_compression_threshold))
        .unwrap_or(DEFAULT_CONTEXT_COMPRESSION_THRESHOLD)
        .clamp(f64::MIN_POSITIVE, 1.0);

    Ok(WorkerConfig {
        session_id,
        data_dir,
        agents_dir,
        model_base_url,
        model_name,
        auth_token,
        context_window,
        subagent_depth,
        max_tool_concurrency,
        context_compression_threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_cap_is_one() {
        assert_eq!(MAX_SUBAGENT_DEPTH, 1);
    }
}
