//! Unit tests for the `config` module — production code lives in
//! `mo_worker/src/config.rs`. Wired from there with `#[cfg(test)] #[path = "tests/config_tests.rs"] mod tests;` so the tests keep `use super::*` access
//! to the module's items (private ones included).

use super::*;

#[test]
fn depth_cap_is_one() {
    assert_eq!(MAX_SUBAGENT_DEPTH, 1);
}

/// The file's default model, with every per-model field set.
fn file_default_model() -> ModelConfig {
    ModelConfig {
        base_url: "https://default.example.com".into(),
        name: "default-model".into(),
        token: Some("tok-default".into()),
        nickname: Some("default".into()),
        context_window: Some(65536),
        reasoning_effort: Some("high".into()),
    }
}

/// An env-supplied model (the gateway passes the per-session model this way)
/// must not inherit the config file's default model's per-model settings:
/// the gateway already sent those fields for *this* model — or deliberately
/// omitted them — so the default's window / effort must not leak in.
#[test]
fn env_model_does_not_inherit_the_file_defaults_per_model_fields() {
    let file = file_default_model();
    let resolved = resolve_model(
        Some(&file),
        Some("https://second.example.com".into()),
        Some("second-model".into()),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(resolved.base_url, "https://second.example.com");
    assert_eq!(resolved.name, "second-model");
    assert_eq!(resolved.token, None);
    // The crux: no window / effort leaks from `default-model`.
    assert_eq!(resolved.context_window, None);
    assert_eq!(resolved.reasoning_effort, None);
}

/// An env-supplied model uses the per-model fields the gateway sent, with
/// blank/unparseable values treated as unset (never as a cue to fall back to
/// the config file).
#[test]
fn env_model_uses_the_env_per_model_fields() {
    let file = file_default_model();
    let resolved = resolve_model(
        Some(&file),
        Some("https://second.example.com".into()),
        Some("second-model".into()),
        Some("tok-2".into()),
        Some("4096".into()),
        Some("  low  ".into()),
    )
    .unwrap();
    assert_eq!(resolved.token.as_deref(), Some("tok-2"));
    assert_eq!(resolved.context_window, Some(4096));
    // Trimmed.
    assert_eq!(resolved.reasoning_effort.as_deref(), Some("low"));

    // Blank / unparseable env values => unset, not the file default.
    let resolved = resolve_model(
        Some(&file),
        Some("https://second.example.com".into()),
        Some("second-model".into()),
        None,
        Some("".into()),
        Some("   ".into()),
    )
    .unwrap();
    assert_eq!(resolved.context_window, None);
    assert_eq!(resolved.reasoning_effort, None);

    let resolved = resolve_model(
        Some(&file),
        Some("https://second.example.com".into()),
        Some("second-model".into()),
        None,
        Some("not-a-number".into()),
        None,
    )
    .unwrap();
    assert_eq!(resolved.context_window, None);
}

/// A standalone run (no env model) resolves everything from the config
/// file's default model.
#[test]
fn file_default_model_used_when_no_env_model() {
    let file = file_default_model();
    let resolved = resolve_model(Some(&file), None, None, None, None, None).unwrap();
    assert_eq!(resolved.base_url, "https://default.example.com");
    assert_eq!(resolved.name, "default-model");
    assert_eq!(resolved.token.as_deref(), Some("tok-default"));
    assert_eq!(resolved.context_window, Some(65536));
    assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));
}

/// No env model and no configured model => the existing missing-model
/// errors (base URL first, then name).
#[test]
fn missing_model_errors() {
    assert!(matches!(
        resolve_model(None, None, None, None, None, None),
        Err(ConfigError::MissingBaseUrl)
    ));
    assert!(matches!(
        resolve_model(
            None,
            Some("https://x.example.com".into()),
            None,
            None,
            None,
            None
        ),
        Err(ConfigError::MissingModelName)
    ));
}
