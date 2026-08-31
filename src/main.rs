use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use carson_api::api::router;
use carson_host::config::Config;
use carson_host::db::Db;
use carson_host::host::{self, HostContext};
use clap::Parser;
use tower::ServiceExt;

use crate::cli::Cli;

mod cli;
mod ui;

/// The full app: JSON/SSE API (with strict CSP) merged with the embedded web UI.
pub fn build_app(app_state: carson_host::app::AppState) -> Router {
    let token = app_state.cfg.server.token.clone();
    router(app_state).merge(ui::router(token))
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

/// Resolve the API bearer token: `CARSON_API_TOKEN` env wins, then the
/// configured `[server] token`, then a freshly generated token persisted to
/// `$CARSON_HOME/api-token`. The token is always Some afterwards.
fn resolve_api_token(home: &std::path::Path, mut config: Config) -> Result<Config> {
    let token = env::var("CARSON_API_TOKEN").ok().filter(|t| !t.is_empty()).or_else(|| {
        config.server.token.clone().filter(|t| !t.is_empty())
    });
    let token = match token {
        Some(t) => t,
        None => {
            let file = home.join("api-token");
            let existing = std::fs::read_to_string(&file)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            match existing {
                Some(t) => t,
                None => {
                    let generated = generate_token();
                    std::fs::write(&file, format!("{generated}\n"))
                        .with_context(|| format!("persist api token to {}", file.display()))?;
                    generated
                }
            }
        }
    };
    config.server.token = Some(token.clone());
    tracing::info!(api_token = %token, "carson api bearer token");
    Ok(config)
}

fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // 32 hex chars from a uuid v4 + a wall-clock mix (best effort local token).
    format!("{}{:016x}", uuid::Uuid::new_v4().simple(), now)
        .chars()
        .take(48)
        .collect()
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
    let config = resolve_api_token(&home, config)?;

    let db_path = home.join("carson.db");
    let db = Db::open(&db_path)?;

    let ctx = Arc::new(HostContext::with_sandbox_base(home.join("sandbox"))?);

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
    // Built-in tools ship with the binary and are seeded into the runner
    // under deterministic ids; custom tools load from the DB.
    for def in host::builtin_tools() {
        match ctx.register_builtin(&def) {
            Ok(()) => tracing::info!(tool = %def.name, id = %def.id, "loaded builtin tool"),
            Err(err) => {
                tracing::warn!(tool = %def.name, error = %err, "failed to compile builtin tool")
            }
        }
    }

    let agents = db.list_agents()?;
    let registry = host::build_registry(&ctx, &agents).await?;
    let app_state = carson_host::app::build_app_state(ctx, registry, db.clone(), config.clone());

    // Restore persisted sessions. Each session is pinned to the agent version
    // that created it; pools for non-current versions are built on demand.
    for persisted in db.load_sessions()? {
        let def = match app_state
            .db
            .get_agent_version(&persisted.agent_version_id)?
        {
            Some(def) => def,
            None => {
                tracing::warn!(
                    id = %persisted.id,
                    version = %persisted.agent_version_id,
                    "dropping persisted session for missing agent version"
                );
                continue;
            }
        };
        let pool = match carson_host::registry::get_or_build_pool(
            &app_state.ctx,
            &app_state.registry,
            &def,
        )
        .await
        {
            Ok(pool) => pool,
            Err(err) => {
                tracing::warn!(id = %persisted.id, error = %err, "failed to build agent pool");
                continue;
            }
        };
        let instance = pool.next();
        if let Err(err) =
            host::restore_session(&instance, &persisted.id, &persisted, &pool.config()).await
        {
            tracing::warn!(id = %persisted.id, error = %err, "failed to restore session");
            continue;
        }
        // Sessions created before sandboxes existed have no sandbox; backfill
        // one (private by default, keyed by the session id).
        let sandbox_id = persisted
            .sandbox_id
            .clone()
            .unwrap_or_else(|| persisted.id.clone());
        if app_state
            .db
            .sandbox_name(&sandbox_id)
            .ok()
            .flatten()
            .is_none()
        {
            let _ = app_state
                .db
                .insert_sandbox(&sandbox_id, &format!("Sandbox {}", &sandbox_id[..8]));
        }
        if persisted.sandbox_id.is_none() {
            let _ = app_state.db.set_session_sandbox(&persisted.id, &sandbox_id);
        }
        app_state
            .ctx
            .sandbox_links
            .write()
            .unwrap()
            .insert(persisted.id.clone(), sandbox_id.clone());
        app_state.sessions.lock().await.insert(
            persisted.id.clone(),
            carson_host::app::SessionEntry {
                agent_name: persisted.agent_name.clone(),
                agent_version_id: persisted.agent_version_id.clone(),
                name: persisted.name.clone(),
                sandbox_id,
                updated_at: persisted.updated_at,
                instance,
            },
        );
        tracing::info!(session = %persisted.id, agent = %persisted.agent_name, "restored session");
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
        assert!(body.contains("Carson"), "{body}");
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
