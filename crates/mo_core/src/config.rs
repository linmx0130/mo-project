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
//! subagent_depth = 0                  # worker subagent depth (default 0, hard cap 1:
//!                                     # subagents can never spawn further subagents)
//! worker_bin    = "..."               # worker binary (default: sibling of mo_gateway)
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
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub agents_dir: Option<PathBuf>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub subagent_depth: Option<u32>,
    #[serde(default)]
    pub worker_bin: Option<PathBuf>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// The resolved configuration used by the gateway (and, as a fallback, the
/// worker). Built from a TOML file when one is found, otherwise from `MO_*`
/// env vars so existing setups keep working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoConfig {
    pub data_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub port: u16,
    pub subagent_depth: u32,
    pub worker_bin: Option<PathBuf>,
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
            subagent_depth: env::var("MO_SUBAGENT_DEPTH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            worker_bin: env::var("MO_WORKER_BIN").ok().map(PathBuf::from),
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
            subagent_depth: self.subagent_depth.unwrap_or(0),
            worker_bin: self.worker_bin,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The tests mutate process-global state (HOME, cwd, MO_* vars); a
    /// static lock serializes them against each other so they never race.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The developer shell may export MO_* vars (the legacy env-var
    /// workflow); clear the ones the fallback reads so the test is
    /// deterministic.
    fn clear_legacy_env() {
        for key in [
            "MO_MODEL_BASE_URL",
            "MO_MODEL_NAME",
            "MO_AUTH_TOKEN",
            "MO_PORT",
            "MO_DATA_DIR",
        ] {
            unsafe {
                env::remove_var(key);
            }
        }
    }

    fn sandboxed_home(dir: &tempfile::TempDir) {
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            env::set_var("HOME", &home);
        }
    }

    #[test]
    fn parses_models_and_defaults() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mo.toml");
        std::fs::write(
            &path,
            r#"
                port = 4040
                subagent_depth = 2
                [[models]]
                base_url = "https://a.example.com"
                name = "model-a"
                token = "tok-a"
                nickname = "alpha"
                context_window = 65536

                [[models]]
                base_url = "https://b.example.com"
                name = "model-b"
            "#,
        )
        .unwrap();
        let config = MoConfig::load(Some(&path)).unwrap();
        assert_eq!(config.port, 4040);
        assert_eq!(config.subagent_depth, 2);
        assert_eq!(config.models.len(), 2);
        assert_eq!(config.default_model().unwrap().name, "model-a");
        assert_eq!(
            config.default_model().unwrap().nickname.as_deref(),
            Some("alpha")
        );
        assert_eq!(
            config.default_model().unwrap().token.as_deref(),
            Some("tok-a")
        );
        assert_eq!(config.default_model().unwrap().context_window, Some(65536));
        assert_eq!(config.find_model("model-b").unwrap().token, None);
        // Unset context_window means unlimited.
        assert_eq!(config.find_model("model-b").unwrap().context_window, None);
        assert!(config.find_model("nope").is_none());
        assert_eq!(config.source.as_deref(), Some(path.as_path()));
        // Unset keys fall back to defaults.
        assert_eq!(config.data_dir, PathBuf::from("./data"));
        assert_eq!(config.worker_bin, None);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mo.toml");
        std::fs::write(&path, "porrt = 1\n").unwrap();
        assert!(matches!(
            MoConfig::load(Some(&path)),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn explicit_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(matches!(
            MoConfig::load(Some(&missing)),
            Err(ConfigError::NotFound(p)) if p == missing
        ));
    }

    #[test]
    fn search_path_prefers_pwd_then_home() {
        let _guard = env_lock();
        let dir = tempfile::tempdir().unwrap();
        sandboxed_home(&dir);

        // PWD config wins over HOME config.
        let pwd = dir.path().join("pwd");
        std::fs::create_dir_all(&pwd).unwrap();
        std::fs::write(pwd.join("mo.toml"), "port = 1111\n").unwrap();
        let home_config = dir.path().join("home/.config/mo-agents");
        std::fs::create_dir_all(&home_config).unwrap();
        std::fs::write(home_config.join("mo.toml"), "port = 2222\n").unwrap();

        let cwd = env::current_dir().unwrap();
        env::set_current_dir(&pwd).unwrap();
        let config = MoConfig::load(None).unwrap();
        env::set_current_dir(&cwd).unwrap();
        assert_eq!(config.port, 1111);
        // `source` is canonicalized (macOS /var -> /private/var), so compare
        // canonical paths.
        assert_eq!(
            config.source.as_deref(),
            Some(pwd.join("mo.toml").canonicalize().unwrap().as_path())
        );

        // Without a PWD config, the HOME config is used.
        std::fs::remove_file(pwd.join("mo.toml")).unwrap();
        env::set_current_dir(&pwd).unwrap();
        let config = MoConfig::load(None).unwrap();
        env::set_current_dir(&cwd).unwrap();
        assert_eq!(config.port, 2222);
        assert_eq!(
            config.source.as_deref(),
            Some(
                home_config
                    .join("mo.toml")
                    .canonicalize()
                    .unwrap()
                    .as_path()
            )
        );
    }

    #[test]
    fn env_fallback_builds_one_model() {
        let _guard = env_lock();
        clear_legacy_env();
        let dir = tempfile::tempdir().unwrap();
        sandboxed_home(&dir);
        let cwd = env::current_dir().unwrap();
        env::set_current_dir(dir.path()).unwrap();

        // No config file and no env vars -> no models, default port.
        let config = MoConfig::load(None).unwrap();
        assert!(config.models.is_empty());
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.source, None);

        // Legacy MO_* vars -> env fallback builds one model.
        unsafe {
            env::set_var("MO_MODEL_BASE_URL", "https://env.example.com");
            env::set_var("MO_MODEL_NAME", "env-model");
            env::set_var("MO_AUTH_TOKEN", "env-tok");
            env::set_var("MO_PORT", "9999");
        }
        let config = MoConfig::load(None).unwrap();
        assert_eq!(config.models.len(), 1);
        assert_eq!(config.models[0].name, "env-model");
        assert_eq!(config.models[0].token.as_deref(), Some("env-tok"));
        assert_eq!(config.port, 9999);
        assert_eq!(config.source, None);

        unsafe {
            env::remove_var("MO_MODEL_BASE_URL");
            env::remove_var("MO_MODEL_NAME");
            env::remove_var("MO_AUTH_TOKEN");
            env::remove_var("MO_PORT");
        }
        env::set_current_dir(&cwd).unwrap();
    }
}
