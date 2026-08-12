//! Unit tests for the `config` module — production code lives in
//! `mo_core/src/config.rs`. Wired from there with `#[cfg(test)] #[path = "tests/config_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

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
        "MO_MAX_TOOL_CONCURRENCY",
        "MO_CONTEXT_COMPRESSION_THRESHOLD",
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
                max_tool_concurrency = 4
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
    assert_eq!(config.max_tool_concurrency, 4);
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
fn max_tool_concurrency_defaults_to_8() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mo.toml");
    std::fs::write(&path, "port = 4040\n").unwrap();
    let config = MoConfig::load(Some(&path)).unwrap();
    assert_eq!(config.max_tool_concurrency, DEFAULT_MAX_TOOL_CONCURRENCY);
}

#[test]
fn context_compression_threshold_defaults_to_0_75() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mo.toml");
    std::fs::write(&path, "port = 4040\n").unwrap();
    let config = MoConfig::load(Some(&path)).unwrap();
    assert_eq!(
        config.context_compression_threshold,
        DEFAULT_CONTEXT_COMPRESSION_THRESHOLD
    );
}

#[test]
fn context_compression_threshold_is_parsed() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mo.toml");
    std::fs::write(&path, "context_compression_threshold = 0.9\n").unwrap();
    let config = MoConfig::load(Some(&path)).unwrap();
    assert_eq!(config.context_compression_threshold, 0.9);
    // 1.0 is allowed (compress only at the very limit).
    std::fs::write(&path, "context_compression_threshold = 1.0\n").unwrap();
    let config = MoConfig::load(Some(&path)).unwrap();
    assert_eq!(config.context_compression_threshold, 1.0);
}

#[test]
fn context_compression_threshold_out_of_range_rejected() {
    let _guard = env_lock();
    let dir = tempfile::tempdir().unwrap();
    for bad in ["0", "-0.1", "1.5", "2"] {
        let path = dir.path().join("mo.toml");
        std::fs::write(&path, format!("context_compression_threshold = {bad}\n")).unwrap();
        assert!(
            matches!(
                MoConfig::load(Some(&path)),
                Err(ConfigError::InvalidThreshold { .. })
            ),
            "threshold {bad} must be rejected"
        );
    }
}

#[test]
fn context_compression_threshold_env_fallback() {
    let _guard = env_lock();
    clear_legacy_env();
    let dir = tempfile::tempdir().unwrap();
    sandboxed_home(&dir);
    let cwd = env::current_dir().unwrap();
    env::set_current_dir(dir.path()).unwrap();

    // Unset -> default.
    let config = MoConfig::load(None).unwrap();
    assert_eq!(
        config.context_compression_threshold,
        DEFAULT_CONTEXT_COMPRESSION_THRESHOLD
    );

    // Set -> parsed.
    unsafe {
        env::set_var("MO_CONTEXT_COMPRESSION_THRESHOLD", "0.5");
    }
    let config = MoConfig::load(None).unwrap();
    assert_eq!(config.context_compression_threshold, 0.5);

    // Unparseable -> default (env fallback is lenient).
    unsafe {
        env::set_var("MO_CONTEXT_COMPRESSION_THRESHOLD", "nope");
    }
    let config = MoConfig::load(None).unwrap();
    assert_eq!(
        config.context_compression_threshold,
        DEFAULT_CONTEXT_COMPRESSION_THRESHOLD
    );

    unsafe {
        env::remove_var("MO_CONTEXT_COMPRESSION_THRESHOLD");
    }
    env::set_current_dir(&cwd).unwrap();
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
        env::set_var("MO_MAX_TOOL_CONCURRENCY", "3");
    }
    let config = MoConfig::load(None).unwrap();
    assert_eq!(config.models.len(), 1);
    assert_eq!(config.models[0].name, "env-model");
    assert_eq!(config.models[0].token.as_deref(), Some("env-tok"));
    assert_eq!(config.port, 9999);
    assert_eq!(config.max_tool_concurrency, 3);
    assert_eq!(config.source, None);

    unsafe {
        env::remove_var("MO_MODEL_BASE_URL");
        env::remove_var("MO_MODEL_NAME");
        env::remove_var("MO_AUTH_TOKEN");
        env::remove_var("MO_PORT");
        env::remove_var("MO_MAX_TOOL_CONCURRENCY");
    }
    env::set_current_dir(&cwd).unwrap();
}
