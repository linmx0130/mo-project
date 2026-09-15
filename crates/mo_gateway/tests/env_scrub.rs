//! Integration test: the gateway must not leak `MO_*` vars from its own
//! environment into a worker whose model does not set them.
//!
//! This lives in its own integration-test binary (its own process) because
//! it mutates the process environment; keeping it separate from `api.rs`
//! avoids racing the other tests that spawn workers.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mo_core::ModelConfig;
use serde_json::json;
use tower::ServiceExt;

use mo_gateway::routes::create_router;
use mo_gateway::state::AppState;

fn write_stub_worker(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("stub_worker.sh");
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A single model with no token, no context window and no reasoning effort.
fn plain_model() -> Vec<ModelConfig> {
    vec![ModelConfig {
        base_url: "http://127.0.0.1:9001".into(),
        name: "plain-model".into(),
        token: None,
        nickname: None,
        context_window: None,
        reasoning_effort: None,
    }]
}

#[tokio::test]
async fn gateway_does_not_leak_inherited_per_model_env_to_workers() {
    // Simulate a gateway launched from a shell/worker that had stale `MO_*`
    // set (the legacy env workflow), while the config file selects a model
    // that sets none of these. The worker must not inherit them.
    unsafe {
        std::env::set_var("MO_AUTH_TOKEN", "stale-token");
        std::env::set_var("MO_CONTEXT_WINDOW", "999999");
        std::env::set_var("MO_REASONING_EFFORT", "stale-effort");
    }

    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let data_dir = dir.path().join("data");
    let env_file = dir.path().join("env.txt");
    let env_tmp = dir.path().join("env.txt.tmp");
    // Dump the worker's MO_* env, then rename it into place atomically (see
    // the note in `api.rs`: a direct `>` can be observed empty mid-write).
    let worker_bin = write_stub_worker(
        dir.path(),
        &format!(
            "#!/bin/sh\nenv | grep MO_ > {}; mv {} {}\n",
            env_tmp.display(),
            env_tmp.display(),
            env_file.display()
        ),
    );
    let conn = mo_core::open_db(&data_dir.join("mo.db")).unwrap();
    let state = Arc::new(AppState {
        data_dir,
        db: Mutex::new(conn),
        worker_bin,
        cwd: std::env::current_dir().unwrap(),
        theme_color: mo_core::config::DEFAULT_THEME_COLOR.to_string(),
        agents_dir: dir.path().join("agents"),
        max_tool_concurrency: mo_core::config::DEFAULT_MAX_TOOL_CONCURRENCY,
        context_compression_threshold: mo_core::config::DEFAULT_CONTEXT_COMPRESSION_THRESHOLD,
        models: plain_model(),
    });
    let app = create_router(state);

    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "workdir": workdir.display().to_string(), "prompt": "env check" }).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let mut written = false;
    for _ in 0..50 {
        if env_file.exists() {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(written, "stub worker never wrote its env");
    let env = std::fs::read_to_string(&env_file).unwrap();

    // The gateway's own config supplies the model...
    assert!(
        env.contains("MO_MODEL_NAME=plain-model"),
        "model env missing: {env}"
    );
    // ...but the stale per-model vars from the gateway's environment must
    // not reach the worker (the model sets none of them).
    for leaked in ["stale-token", "999999", "stale-effort"] {
        assert!(
            !env.contains(leaked),
            "inherited {leaked:?} leaked into the worker env: {env}"
        );
    }
    assert!(
        !env.contains("MO_AUTH_TOKEN="),
        "MO_AUTH_TOKEN must be scrubbed: {env}"
    );
    assert!(
        !env.contains("MO_CONTEXT_WINDOW="),
        "MO_CONTEXT_WINDOW must be scrubbed: {env}"
    );
    assert!(
        !env.contains("MO_REASONING_EFFORT="),
        "MO_REASONING_EFFORT must be scrubbed: {env}"
    );

    unsafe {
        std::env::remove_var("MO_AUTH_TOKEN");
        std::env::remove_var("MO_CONTEXT_WINDOW");
        std::env::remove_var("MO_REASONING_EFFORT");
    }
}
