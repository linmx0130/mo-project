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
//! * `MO_REASONING_EFFORT` — optional reasoning effort, forwarded verbatim as
//!   the chat-completion `reasoning_effort` parameter (unset = the field is
//!   omitted); the gateway passes the per-session model's value from `mo.toml`.
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
//!
//! The per-model fields — `MO_AUTH_TOKEN`, `MO_CONTEXT_WINDOW` and
//! `MO_REASONING_EFFORT` — belong to the model that `MO_MODEL_BASE_URL` /
//! `MO_MODEL_NAME` name. When a model arrives via env (the gateway always
//! sends it), the config file's default model is *not* consulted for them —
//! that would leak the default model's token / window / effort into a
//! session running under a different model. The file's default model
//! supplies them only for a standalone run with no env model (see
//! `resolve_model`).

use std::env;
use std::path::PathBuf;

use mo_core::config::{DEFAULT_CONTEXT_COMPRESSION_THRESHOLD, ModelConfig};

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
    /// Optional reasoning effort, forwarded verbatim as the chat-completion
    /// `reasoning_effort` parameter (`None` = the field is omitted). The
    /// gateway passes the per-session model's value from `mo.toml`; a
    /// standalone worker (no env-supplied model) takes it from the config
    /// file's default model (see `resolve_model`).
    pub reasoning_effort: Option<String>,
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

/// The model and its per-model settings, resolved from a single source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub base_url: String,
    pub name: String,
    pub token: Option<String>,
    pub context_window: Option<u64>,
    pub reasoning_effort: Option<String>,
}

/// Resolve the session's model and its per-model settings: environment
/// first (the gateway passes the per-session model this way), otherwise the
/// config file's default (first) model for standalone runs.
///
/// The per-model fields — the context window and the reasoning effort —
/// resolve from the **same source as the model itself**. When a model was
/// supplied via `MO_MODEL_BASE_URL`/`MO_MODEL_NAME`, the gateway also
/// supplied those fields (or deliberately omitted them because the model
/// sets neither), so the config file's default model must never leak its
/// window / effort into a session running under a different model. The
/// file's default model is consulted only when no model came from env.
pub fn resolve_model(
    file_default: Option<&ModelConfig>,
    env_base_url: Option<String>,
    env_name: Option<String>,
    env_token: Option<String>,
    env_context_window: Option<String>,
    env_reasoning_effort: Option<String>,
) -> Result<ResolvedModel, ConfigError> {
    match (env_base_url, env_name) {
        (Some(base_url), Some(name)) => Ok(ResolvedModel {
            base_url,
            name,
            token: env_token.filter(|v| !v.is_empty()),
            // Blank/unparseable env values mean "unset" — not a cue to fall
            // back to the config file (that would leak the default model).
            context_window: env_context_window
                .filter(|v| !v.is_empty())
                .and_then(|v| v.parse::<u64>().ok()),
            reasoning_effort: mo_core::config::normalize_reasoning_effort(env_reasoning_effort),
        }),
        (env_base_url, _) => match file_default {
            Some(model) => Ok(ResolvedModel {
                base_url: model.base_url.clone(),
                name: model.name.clone(),
                token: model.token.clone(),
                context_window: model.context_window,
                reasoning_effort: model.reasoning_effort.clone(),
            }),
            None => Err(if env_base_url.is_none() {
                ConfigError::MissingBaseUrl
            } else {
                ConfigError::MissingModelName
            }),
        },
    }
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
    // The session's model and its per-model settings (context window,
    // reasoning effort). Env first — the gateway passes the per-session
    // model that way — otherwise the config file's default model, so a
    // standalone run works from `mo.toml` alone. Both sources resolve
    // together, so the default model's settings never leak into a session
    // running under a different (env-supplied) model.
    let resolved = resolve_model(
        file_cfg.as_ref().and_then(|c| c.default_model()),
        env::var("MO_MODEL_BASE_URL").ok(),
        env::var("MO_MODEL_NAME").ok(),
        env::var("MO_AUTH_TOKEN").ok(),
        env::var("MO_CONTEXT_WINDOW").ok(),
        env::var("MO_REASONING_EFFORT").ok(),
    )?;
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
        model_base_url: resolved.base_url,
        model_name: resolved.name,
        auth_token: resolved.token,
        context_window: resolved.context_window,
        reasoning_effort: resolved.reasoning_effort,
        subagent_depth,
        max_tool_concurrency,
        context_compression_threshold,
    })
}

// Unit tests live in `mo_worker/src/tests/config_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/config_tests.rs"]
mod tests;
