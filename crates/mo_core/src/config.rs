//! Shared TOML configuration.
//!
//! The gateway loads a `mo.toml` file (explicit `--config` path, else
//! `$PWD/mo.toml`, else `$HOME/.config/mo-agents/mo.toml`) and passes the
//! resolved values down to every spawned worker through the environment.
//! The worker also falls back to this file for standalone runs, so a single
//! `mo.toml` replaces the `MO_*` env-var soup.
//!
//! Config keys:
//!
//! ```toml
//! data_dir      = "./data"            # runtime data dir (default ./data)
//! agents_dir    = "~/.agents"         # global agents dir (default $HOME/.agents)
//! port          = 3031                # gateway HTTP port (default 3031)
//! bind          = "0.0.0.0"           # gateway listen address (default "0.0.0.0");
//!                                     # set to "127.0.0.1" behind a reverse proxy so
//!                                     # the public network can never reach the gateway
//!                                     # directly
//! theme_color   = "#009dc4"           # optional; the web UI's accent color as a
//!                                     # hex value (#RGB or #RRGGBB; default #009dc4,
//!                                     # deep cyan). Light mode uses the color
//!                                     # verbatim (translucent tints are derived
//!                                     # from it); dark mode auto-lightens it for
//!                                     # contrast on the dark background.
//! subagent_depth = 0                  # accepted for backward compatibility (no longer
//!                                     # changes behavior): root sessions are always
//!                                     # depth 0; nesting is hard-capped at 1
//! worker_bin    = "..."               # worker binary (default: sibling of mo_gateway)
//! max_tool_concurrency = 8            # optional: how many tool calls from a single
//!                                     # assistant message may run at once (default 8;
//!                                     # clamped to at least 1). Tool calls in one
//!                                     # message run concurrently up to this bound.
//! context_compression_threshold = 0.75 # optional; fraction of the model's
//!                                     # context_window at which the worker asks the
//!                                     # model to generate a handoff prompt and starts
//!                                     # sending only the compressed context (default
//!                                     # 0.75; 0 < value <= 1; only applies when the
//!                                     # model sets a context_window).
//!
//! [[models]]                          # at least one required; first = default
//! base_url  = "https://api.deepseek.com"
//! name      = "deepseek-v4-flash"
//! token     = "sk-..."                # optional
//! nickname  = "deepseek"              # optional, shown in the UI
//! context_window = 65536              # optional; the model's context window in
//!                                     # tokens (unlimited when absent). The UI
//!                                     # shows the session's context length
//!                                     # against this window in the status bar.
//! ```

use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default gateway HTTP port.
pub const DEFAULT_PORT: u16 = 3031;

/// Default gateway listen address (all interfaces). A reverse proxy in
/// front of the gateway sets `bind = "127.0.0.1"` so it is only reachable
/// through the proxy.
pub const DEFAULT_BIND: &str = "0.0.0.0";

/// Default maximum number of tool calls from a single assistant message
/// that execute concurrently (see `MoConfig::max_tool_concurrency`).
pub const DEFAULT_MAX_TOOL_CONCURRENCY: usize = 8;

/// Default context-compression threshold: the fraction of the model's
/// `context_window` at which the worker asks the model to generate a
/// handoff prompt and starts sending only the compressed context (see
/// `MoConfig::context_compression_threshold`).
pub const DEFAULT_CONTEXT_COMPRESSION_THRESHOLD: f64 = 0.75;

/// Default UI accent color: deep cyan (see `MoConfig::theme_color`). Light
/// mode uses it verbatim; dark mode auto-lightens it for contrast.
pub const DEFAULT_THEME_COLOR: &str = "#009dc4";

/// One configured LLM endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelConfig {
    /// OpenAI-compatible endpoint base URL (no trailing `/`).
    pub base_url: String,
    /// Model name to request.
    pub name: String,
    /// Optional Bearer token / API key.
    #[serde(default)]
    pub token: Option<String>,
    /// Optional human-readable label shown in the "New session" UI.
    #[serde(default)]
    pub nickname: Option<String>,
    /// Optional context window in tokens; `None` means unlimited. The
    /// worker embeds it in each `context_usage` journal event, and the UI
    /// renders the session's context length against it.
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// Raw TOML file shape (all fields optional so a minimal file works).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub agents_dir: Option<PathBuf>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Gateway listen address (default `DEFAULT_BIND`, `"0.0.0.0"`). Set
    /// to `"127.0.0.1"` when the gateway sits behind a reverse proxy so it
    /// is never reachable from the public network directly.
    #[serde(default)]
    pub bind: Option<String>,
    /// The web UI's accent color as a hex value (`#RGB` or `#RRGGBB`;
    /// default `DEFAULT_THEME_COLOR`, deep cyan). Light mode uses the color
    /// verbatim with derived translucent tints; dark mode auto-lightens it
    /// for contrast on the dark background.
    #[serde(default)]
    pub theme_color: Option<String>,
    #[serde(default)]
    pub subagent_depth: Option<u32>,
    #[serde(default)]
    pub worker_bin: Option<PathBuf>,
    #[serde(default)]
    pub max_tool_concurrency: Option<usize>,
    /// Fraction of the model's `context_window` at which the worker asks
    /// the model to generate a handoff prompt and starts sending only the
    /// compressed context (default `DEFAULT_CONTEXT_COMPRESSION_THRESHOLD`;
    /// must be in `(0, 1]`; only applies when the model sets a
    /// `context_window`).
    #[serde(default)]
    pub context_compression_threshold: Option<f64>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// The resolved configuration used by the gateway (and, as a fallback, the
/// worker). Built from a TOML file when one is found, otherwise from `MO_*`
/// env vars so existing setups keep working.
#[derive(Debug, Clone, PartialEq)]
pub struct MoConfig {
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub port: u16,
    /// Gateway listen address (default `DEFAULT_BIND`, `"0.0.0.0"`); set it
    /// to `"127.0.0.1"` when nginx fronts the gateway so it is only
    /// reachable through the proxy.
    pub bind: String,
    /// The web UI's accent color as a hex value (default
    /// `DEFAULT_THEME_COLOR`); served to the frontend via `GET /api/meta`,
    /// which derives the translucent tints and the dark-mode variant from
    /// it.
    pub theme_color: String,
    pub subagent_depth: u32,
    pub worker_bin: Option<PathBuf>,
    /// Maximum number of tool calls from a single assistant message that
    /// execute concurrently. Clamped to at least 1 by consumers; the
    /// default is `DEFAULT_MAX_TOOL_CONCURRENCY`.
    pub max_tool_concurrency: usize,
    /// Fraction of the model's `context_window` at which the worker asks
    /// the model to generate a handoff prompt and starts sending only the
    /// compressed context. Clamped to `(0, 1]`; the default is
    /// `DEFAULT_CONTEXT_COMPRESSION_THRESHOLD`.
    pub context_compression_threshold: f64,
    pub models: Vec<ModelConfig>,
    /// The config file that was loaded; `None` when built from env vars.
    pub source: Option<PathBuf>,
}

impl MoConfig {
    /// Load the config: an explicit `--config` path wins, otherwise the
    /// search path `$PWD/mo.toml` > `$HOME/.config/mo-agents/mo.toml`.
    /// With no config file anywhere, falls back to `MO_*` env vars.
    pub fn load(explicit: Option<&Path>) -> Result<MoConfig, ConfigError> {
        match find_config_path(explicit)? {
            Some(path) => {
                let file = parse_file(&path)?;
                if let Some(t) = file.context_compression_threshold
                    && !(t > 0.0 && t <= 1.0)
                {
                    return Err(ConfigError::InvalidThreshold { path, value: t });
                }
                if let Some(color) = &file.theme_color
                    && !is_valid_hex_color(color)
                {
                    return Err(ConfigError::InvalidThemeColor {
                        path,
                        value: color.clone(),
                    });
                }
                let config = file.into_config();
                Ok(config.with_source(Some(path)))
            }
            None => Ok(MoConfig::from_env()),
        }
    }

    /// The default model: the first one defined in the config file, used to
    /// launch jobs and generate session titles.
    pub fn default_model(&self) -> Option<&ModelConfig> {
        self.models.first()
    }

    /// Find a configured model by its model name (the value the UI sends).
    pub fn find_model(&self, name: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|m| m.name == name)
    }

    /// Build a config from `MO_*` env vars (legacy fallback when no config
    /// file exists). `models` is empty unless `MO_MODEL_BASE_URL` and
    /// `MO_MODEL_NAME` are both set.
    fn from_env() -> MoConfig {
        let models = match (env::var("MO_MODEL_BASE_URL"), env::var("MO_MODEL_NAME")) {
            (Ok(base_url), Ok(name)) if !base_url.is_empty() && !name.is_empty() => {
                vec![ModelConfig {
                    base_url,
                    name,
                    token: env::var("MO_AUTH_TOKEN").ok().filter(|v| !v.is_empty()),
                    nickname: None,
                    context_window: None,
                }]
            }
            _ => Vec::new(),
        };
        MoConfig {
            data_dir: env::var("MO_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./data")),
            agents_dir: env::var("MO_AGENTS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| default_agents_dir()),
            port: env::var("MO_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            bind: env::var("MO_BIND")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_BIND.to_string()),
            theme_color: env::var("MO_THEME_COLOR")
                .ok()
                .filter(|v| is_valid_hex_color(v))
                .unwrap_or_else(|| DEFAULT_THEME_COLOR.to_string()),
            subagent_depth: env::var("MO_SUBAGENT_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            worker_bin: env::var("MO_WORKER_BIN").ok().map(PathBuf::from),
            max_tool_concurrency: env::var("MO_MAX_TOOL_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_MAX_TOOL_CONCURRENCY),
            context_compression_threshold: env::var("MO_CONTEXT_COMPRESSION_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_CONTEXT_COMPRESSION_THRESHOLD),
            models,
            source: None,
        }
    }

    fn with_source(mut self, source: Option<PathBuf>) -> MoConfig {
        self.source = source;
        self
    }
}

impl FileConfig {
    fn into_config(self) -> MoConfig {
        MoConfig {
            data_dir: self.data_dir.unwrap_or_else(|| PathBuf::from("./data")),
            agents_dir: self.agents_dir.unwrap_or_else(default_agents_dir),
            port: self.port.unwrap_or(DEFAULT_PORT),
            bind: self.bind.unwrap_or_else(|| DEFAULT_BIND.to_string()),
            theme_color: self
                .theme_color
                .unwrap_or_else(|| DEFAULT_THEME_COLOR.to_string()),
            subagent_depth: self.subagent_depth.unwrap_or(0),
            worker_bin: self.worker_bin,
            max_tool_concurrency: self
                .max_tool_concurrency
                .unwrap_or(DEFAULT_MAX_TOOL_CONCURRENCY),
            context_compression_threshold: self
                .context_compression_threshold
                .unwrap_or(DEFAULT_CONTEXT_COMPRESSION_THRESHOLD),
            models: self.models,
            source: None,
        }
    }
}

/// Default global agents dir when unset: `$HOME/.agents`. Falls back to
/// `./.agents` when `$HOME` is not set either.
pub fn default_agents_dir() -> PathBuf {
    match env::var("HOME") {
        Ok(home) if !home.is_empty() => PathBuf::from(home).join(".agents"),
        _ => PathBuf::from(".agents"),
    }
}

/// Whether a string is a valid hex color: `#` followed by 3 or 6 hex
/// digits (`#RGB` / `#RRGGBB`, case-insensitive). Used to validate
/// `theme_color` from `mo.toml` (the env fallback is lenient and drops
/// invalid values instead).
fn is_valid_hex_color(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve the config file path: explicit `--config` first, then the search
/// path `$PWD/mo.toml` > `$HOME/.config/mo-agents/mo.toml`.
fn find_config_path(explicit: Option<&Path>) -> Result<Option<PathBuf>, ConfigError> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(Some(path.to_path_buf()));
        }
        return Err(ConfigError::NotFound(path.to_path_buf()));
    }
    let candidates = [
        env::current_dir()
            .map(|dir| dir.join("mo.toml"))
            .unwrap_or_else(|_| PathBuf::from("mo.toml")),
        env::var("HOME")
            .map(|home| PathBuf::from(home).join(".config/mo-agents/mo.toml"))
            .unwrap_or_default(),
    ];
    Ok(candidates
        .into_iter()
        .find(|path| path.is_file())
        .map(|path| path.canonicalize().unwrap_or(path)))
}

fn parse_file(path: &Path) -> Result<FileConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&content).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "invalid context_compression_threshold in {path}: {value} (expected a fraction in (0, 1])"
    )]
    InvalidThreshold { path: PathBuf, value: f64 },
    #[error(
        "invalid theme_color in {path}: {value} (expected a hex color like \"#009dc4\" — #RGB or #RRGGBB)"
    )]
    InvalidThemeColor { path: PathBuf, value: String },
}

// Unit tests live in `mo_core/src/tests/config_tests.rs` (see AGENTS.md).
#[cfg(test)]
#[path = "tests/config_tests.rs"]
mod tests;
