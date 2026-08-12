use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use serde_json::json;

use carson_host::api::{self, SessionEntry};
use carson_host::bindings::exports::carson::agent::agent::SessionConfig;
use carson_host::config::Config;
use carson_host::db::Db;
use carson_host::host::{self, HostContext};

fn carson_home() -> PathBuf {
    if let Ok(home) = std::env::var("CARSON_HOME") {
        return PathBuf::from(home);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".carson");
    }
    PathBuf::from(".carson")
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let home = carson_home();
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;

    let config_path = args
        .iter()
        .position(|a| a == "--config")
        .map(|i| PathBuf::from(&args[i + 1]))
        .unwrap_or_else(|| home.join("carson.toml"));
    let config = Config::load(&config_path)?;

    let db_path = home.join("carson.db");
    let db = Db::open(&db_path)?;

    let ctx = Arc::new(HostContext::new(&config)?);

    let agents = db.list_agents()?;
    let registry = host::build_registry(&ctx, &agents).await?;
    let app_state = host::build_app_state(ctx, registry, db.clone(), config.clone());

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
            SessionEntry {
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

    let app = api::router(app_state);
    let listener = tokio::net::TcpListener::bind(config.server.bind).await?;
    tracing::info!("carson listening on http://{}", config.server.bind);
    axum::serve(listener, app).await?;
    Ok(())
}
