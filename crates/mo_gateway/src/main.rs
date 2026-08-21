//! mo_gateway — the HTTP service: sessions CRUD, journal history, SSE live
//! updates, and worker process management.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mo_gateway::routes::create_router;
use mo_gateway::state::AppState;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// `mo_gateway [--config <mo.toml>]`
///
/// The gateway reads its configuration from a TOML file (see
/// `mo_core::config`): an explicit `--config` path wins, otherwise
/// `$PWD/mo.toml`, otherwise `$HOME/.config/mo-agents/mo.toml`. With no
/// config file anywhere, legacy `MO_*` env vars are used as a fallback.
/// Spawned workers inherit the resolved settings (model, data dir, agents
/// dir, subagent depth) through the environment.
#[tokio::main]
async fn main() {
    let mut config_arg: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => match args.next() {
                Some(path) => config_arg = Some(PathBuf::from(path)),
                None => {
                    eprintln!("error: --config requires a file path");
                    eprintln!("usage: mo_gateway [--config <mo.toml>]");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("error: unknown argument: {other}");
                eprintln!("usage: mo_gateway [--config <mo.toml>]");
                std::process::exit(2);
            }
        }
    }

    let config = match mo_core::MoConfig::load(config_arg.as_deref()) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };
    if config.models.is_empty() {
        eprintln!(
            "no models configured: create mo.toml (see mo.toml.example) with at least one [[models]] entry"
        );
        std::process::exit(2);
    }

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mo_gateway=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!(source = ?config.source, models = config.models.len(), "loaded configuration");

    std::fs::create_dir_all(&config.data_dir).expect("failed to create data dir");

    let cwd = std::env::current_dir().expect("cannot resolve current directory");

    let conn = mo_core::open_db(&config.data_dir.join("mo.db")).expect("failed to open DB");
    let worker_bin = match config.worker_bin.clone() {
        Some(path) => path,
        None => {
            let exe = std::env::current_exe().expect("cannot resolve own executable");
            exe.parent()
                .expect("executable has no parent dir")
                .join("mo_worker")
        }
    };

    let state = Arc::new(AppState {
        data_dir: config.data_dir,
        db: Mutex::new(conn),
        worker_bin,
        cwd,
        theme_color: config.theme_color,
        agents_dir: config.agents_dir,
        max_tool_concurrency: config.max_tool_concurrency,
        context_compression_threshold: config.context_compression_threshold,
        models: config.models,
    });
    let app = create_router(state);

    let port = config.port;
    let listener = tokio::net::TcpListener::bind(format!("{}:{port}", config.bind))
        .await
        .expect("failed to bind port");
    tracing::info!(bind = %config.bind, "mo_gateway listening on http://{}:{port}", config.bind);
    axum::serve(listener, app).await.expect("server error");
}
