//! mo_worker — the agent process.
//!
//! Spawned by the gateway (or by another worker for subagents) with
//! `--session-id <id>`. Loads its session row, updates pid/status/heartbeat,
//! runs the agent loop, and journals everything to the session journal.

mod agent;
mod config;
mod prompt;
mod tools;

use std::path::{Path, PathBuf};
use std::time::Duration;

use mo_core::{JournalEventKind, JournalWriter, SessionStatus};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() {
    // Load MO_* config from a `.env` file when running standalone (the
    // gateway normally passes everything down via inherited env).
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mo_worker=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = match config::parse_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(2);
        }
    };

    let db_path = cfg.data_dir.join("mo.db");
    let conn = match mo_core::open_db(&db_path) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("failed to open DB at {}: {e}", db_path.display());
            std::process::exit(2);
        }
    };
    let session = match mo_core::db::get_session(&conn, &cfg.session_id) {
        Ok(Some(session)) => session,
        Ok(None) => {
            eprintln!("session {} not found", cfg.session_id);
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("failed to load session {}: {e}", cfg.session_id);
            std::process::exit(2);
        }
    };
    let workdir = PathBuf::from(&session.workdir);
    if !workdir.is_dir() {
        eprintln!("workdir {} is not a directory", workdir.display());
        std::process::exit(2);
    }

    // Announce ourselves in the DB before doing any work.
    if let Err(e) = mo_core::db::set_pid(&conn, &cfg.session_id, std::process::id()) {
        eprintln!("failed to set pid: {e}");
        std::process::exit(2);
    }
    if let Err(e) = mo_core::db::update_status(&conn, &cfg.session_id, SessionStatus::Running, None)
    {
        eprintln!("failed to set running status: {e}");
        std::process::exit(2);
    }

    // Heartbeat task (own connection so it never blocks the agent loop).
    {
        let hb_db = cfg.data_dir.join("mo.db");
        let hb_id = cfg.session_id.clone();
        tokio::spawn(async move {
            let Ok(conn) = mo_core::open_db(&hb_db) else {
                return;
            };
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                let _ = mo_core::db::update_heartbeat(&conn, &hb_id);
            }
        });
    }

    let mut journal = match JournalWriter::open(Path::new(&session.journal_path)) {
        Ok(journal) => journal,
        Err(e) => {
            eprintln!("failed to open journal: {e}");
            std::process::exit(2);
        }
    };
    let _ = journal.append(JournalEventKind::StatusChange {
        status: SessionStatus::Running,
        error: None,
    });

    let agent_cfg = agent::AgentConfig {
        session: session.clone(),
        workdir,
        data_dir: cfg.data_dir.clone(),
        model_base_url: cfg.model_base_url,
        model_name: cfg.model_name,
        auth_token: cfg.auth_token,
        subagent_depth: cfg.subagent_depth,
    };

    match agent::run_agent(agent_cfg, &mut journal).await {
        Ok(()) => {
            let _ =
                mo_core::db::update_status(&conn, &cfg.session_id, SessionStatus::Completed, None);
            let _ = journal.append(JournalEventKind::StatusChange {
                status: SessionStatus::Completed,
                error: None,
            });
            tracing::info!("session {} completed", cfg.session_id);
        }
        Err(e) => {
            tracing::error!("session {} failed: {e:#}", cfg.session_id);
            let _ = mo_core::db::update_status(
                &conn,
                &cfg.session_id,
                SessionStatus::Failed,
                Some(format!("{e:#}")),
            );
            let _ = journal.append(JournalEventKind::StatusChange {
                status: SessionStatus::Failed,
                error: Some(format!("{e:#}")),
            });
            std::process::exit(1);
        }
    }
}
