use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use carson_api::api::router;
use carson_host::bindings::exports::carson::agent::agent::SessionConfig;
use carson_host::config::Config;
use carson_host::db::Db;
use carson_host::host::{self, HostContext};
use clap::Parser;
use serde_json::json;

use crate::cli::Cli;

mod cli;

fn carson_home(cli_home: Option<PathBuf>) -> PathBuf {
    if let Some(home) = cli_home {
        return home;
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".carson");
    }
    PathBuf::from(".carson")
}

/// Log every incoming request at `info`, http.server style.
async fn trace(req: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.to_string());
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    tracing::info!(
        peer = peer.as_deref().unwrap_or("-"),
        method = %method,
        path = %path,
        status = resp.status().as_u16(),
        elapsed_ms = start.elapsed().as_millis(),
        "request"
    );
    resp
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let home = carson_home(cli.home);
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;

    let config = Config::load(&home.join("config.toml"))?;

    let db_path = home.join("carson.db");
    let db = Db::open(&db_path)?;

    let ctx = Arc::new(HostContext::new(&config)?);

    let agents = db.list_agents()?;
    let registry = host::build_registry(&ctx, &agents).await?;
    let app_state = carson_host::app::build_app_state(ctx, registry, db.clone(), config.clone());

    let sessions = db.load_sessions()?;
    let mut max_session_id = 0u64;
    for persisted in &sessions {
        max_session_id = max_session_id.max(persisted.id);
        let Some(pool) = app_state.registry.lock().await.get(&persisted.kind) else {
            tracing::warn!(
                kind = %persisted.kind,
                id = persisted.id,
                "dropping persisted session for unknown agent kind"
            );
            continue;
        };
        let instance = pool.next();
        let config = SessionConfig {
            system_prompt: pool.system_prompt.clone(),
            model: pool.model.clone(),
            capabilities_json: json!(pool.caps).to_string(),
            max_history: pool.max_history,
            context_window: pool.context_window,
            compaction_ratio: pool.compaction_ratio,
            auto_compact: pool.auto_compact,
        };
        if let Err(err) = host::restore_session(&instance, persisted.id, persisted, &config).await {
            tracing::warn!(id = persisted.id, error = %err, "failed to restore session");
            continue;
        }
        app_state.sessions.lock().await.insert(
            persisted.id,
            carson_host::app::SessionEntry {
                kind: persisted.kind.clone(),
                instance,
            },
        );
    }
    if max_session_id > 0 {
        app_state
            .next_session_id
            .store(max_session_id, Ordering::SeqCst);
    }

    let app = router(app_state).layer(middleware::from_fn(trace));
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("carson listening on http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
