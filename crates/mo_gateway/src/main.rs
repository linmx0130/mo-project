//! mo_gateway — the HTTP service: sessions CRUD, journal history, SSE live
//! updates, and worker process management.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use mo_gateway::routes::create_router;
use mo_gateway::state::AppState;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    // Load MO_* config from a `.env` file in the project folder (secrets
    // stay out of the shell); spawned workers inherit this process env.
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mo_gateway=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let data_dir = std::env::var("MO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./data"));
    std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

    let conn = mo_core::open_db(&data_dir.join("mo.db")).expect("failed to open DB");
    let worker_bin = match std::env::var("MO_WORKER_BIN") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            let exe = std::env::current_exe().expect("cannot resolve own executable");
            exe.parent()
                .expect("executable has no parent dir")
                .join("mo_worker")
        }
    };

    let state = Arc::new(AppState {
        data_dir,
        db: Mutex::new(conn),
        worker_bin,
    });
    let app = create_router(state);

    let port = std::env::var("MO_PORT").unwrap_or_else(|_| "3000".to_string());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("failed to bind port");
    tracing::info!("mo_gateway listening on http://0.0.0.0:{port}");
    axum::serve(listener, app).await.expect("server error");
}
