use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::Router;
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
use tower::ServiceExt;

use crate::cli::Cli;

mod cli;
mod ui;

/// The full app: JSON/SSE API (with strict CSP) merged with the embedded web UI.
pub fn build_app(app_state: carson_host::app::AppState) -> Router {
    router(app_state).merge(ui::router())
}

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

    let ctx = Arc::new(HostContext::new()?);

    for provider in db.list_providers()? {
        match host::openai_driver(&provider) {
            Ok(driver) => {
                ctx.register_driver(&provider.name, driver);
                tracing::info!(provider = %provider.name, "loaded provider");
            }
            Err(err) => {
                tracing::warn!(provider = %provider.name, error = %err, "failed to build provider driver")
            }
        }
    }
    for tool in db.list_tools()? {
        match db.get_tool_wasm(&tool.name)? {
            Some(wasm) => match ctx.register_tool(&tool, &wasm) {
                Ok(()) => tracing::info!(tool = %tool.name, "loaded tool"),
                Err(err) => {
                    tracing::warn!(tool = %tool.name, error = %err, "failed to compile tool")
                }
            },
            None => tracing::warn!(tool = %tool.name, "tool row has no wasm"),
        }
    }

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

    let app = build_app(app_state).layer(middleware::from_fn(trace));
    let listener = tokio::net::TcpListener::bind(config.server.bind()).await?;
    let addr = listener.local_addr()?;
    tracing::info!("carson listening on http://{addr}");
    serve_nodelay(listener, app).await;
    Ok(())
}

/// Serve HTTP/1.1 with TCP_NODELAY on every accepted socket so SSE frames flush
/// immediately instead of being coalesced into bursts by Nagle's algorithm.
async fn serve_nodelay(listener: tokio::net::TcpListener, app: axum::Router) {
    loop {
        let Ok((socket, peer)) = listener.accept().await else {
            continue;
        };
        let _ = socket.set_nodelay(true);
        let io = hyper_util::rt::TokioIo::new(socket);
        let app = app.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let app = app.clone();
                async move {
                    let mut req = req.map(axum::body::Body::new);
                    req.extensions_mut().insert(ConnectInfo(peer));
                    app.oneshot(req).await
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(io, svc)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use carson_host::app::AppState;
    use carson_host::config::Config;
    use carson_host::db::Db;
    use carson_host::host::HostContext;
    use carson_host::hub::Hub;
    use carson_host::registry::AgentRegistry;
    use http_body_util::BodyExt;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use super::*;

    fn config() -> Config {
        toml::from_str("[server]\nip = \"127.0.0.1\"\nport = 8000\n").unwrap()
    }

    fn app() -> Router {
        let ctx = Arc::new(HostContext::new().unwrap());
        let db = Db::open_in_memory().unwrap();
        let state = AppState {
            ctx,
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            db,
            hub: Hub::new(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            cfg: Arc::new(config()),
        };
        build_app(state)
    }

    async fn read(resp: Response) -> (StatusCode, String) {
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn ui_serves_shell_at_root() {
        let (status, body) = read(
            app()
                .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("carson"), "{body}");
        assert!(body.contains("/index.js"), "{body}");
    }

    #[tokio::test]
    async fn ui_serves_wasm_loader_glue() {
        let (status, body) = read(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/pkg/carson_ui.js")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("carson_ui_bg.wasm"), "{body}");
    }

    #[tokio::test]
    async fn ui_serves_the_wasm_module() {
        let (status, body) = read(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/pkg/carson_ui_bg.wasm")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("\0asm"), "wasm magic bytes");
    }

    #[tokio::test]
    async fn spa_fallback_serves_shell_for_client_routes() {
        let (status, body) = read(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/chat/3")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("/index.js"), "{body}");
    }

    #[tokio::test]
    async fn unknown_api_path_stays_404() {
        let (status, _) = read(
            app()
                .oneshot(
                    Request::builder()
                        .uri("/api/nope")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
