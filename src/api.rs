use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, Bytes};
use axum::extract::{FromRequestParts, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use axum_extra::extract::TypedHeader;
use axum_extra::routing::TypedPath;
use headers::{Authorization, authorization::Bearer};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use crate::db::Db;
use crate::drivers::Usage;
use crate::host::{self, HostContext};
use crate::hub::{Hub, SseItem, sse_frame};
use crate::registry::{AgentDef, AgentInstance, AgentRegistry};

#[derive(Clone)]
pub struct AppState {
    pub ctx: Arc<HostContext>,
    pub registry: Arc<Mutex<AgentRegistry>>,
    pub db: Arc<Db>,
    pub hub: Arc<Hub>,
    pub sessions: Arc<Mutex<HashMap<u64, SessionEntry>>>,
    pub next_session_id: Arc<AtomicU64>,
    pub cfg: Arc<Config>,
}

#[derive(Clone)]
pub struct SessionEntry {
    pub kind: String,
    pub instance: Arc<AgentInstance>,
}

#[derive(Deserialize)]
struct CreateSessionReq {
    agent: String,
}

#[derive(Deserialize)]
struct MessageReq {
    content: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}")]
struct SessionPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/message")]
struct MessagePath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stream")]
struct StreamPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stop")]
struct StopPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/reset")]
struct ResetPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/compact")]
struct CompactPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/agents/{kind}")]
struct AgentKindPath {
    kind: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/config", get(config_info))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(AgentKindPath::PATH, put(update_agent).delete(delete_agent))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(SessionPath::PATH, get(get_session).delete(destroy_session))
        .route(MessagePath::PATH, post(send_message))
        .route(StreamPath::PATH, post(send_stream))
        .route(StopPath::PATH, post(stop_session))
        .route(ResetPath::PATH, post(reset_session))
        .route(CompactPath::PATH, post(compact_session))
        .layer(middleware::from_fn(security))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn json_ok(body: Value) -> Response {
    json_response(StatusCode::OK, body)
}

fn json_created(body: Value) -> Response {
    json_response(StatusCode::CREATED, body)
}

fn json_err(status: StatusCode, message: &str) -> Response {
    json_response(status, json!({"error": message}))
}

async fn security(req: Request<Body>, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert("x-request-id", HeaderValue::from_str(&request_id).unwrap());
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'self'"),
    );
    resp
}

async fn auth(State(st): State<AppState>, mut req: Request<Body>, next: Next) -> Response {
    if let Some(key) = st.cfg.api_key() {
        let public = req.uri().path() == "/api/health";
        let (mut parts, body) = req.into_parts();
        let authorized = TypedHeader::<Authorization<Bearer>>::from_request_parts(&mut parts, &st)
            .await
            .map(|TypedHeader(auth)| auth.0.token() == key)
            .unwrap_or(false);
        req = Request::from_parts(parts, body);
        if !authorized && !public {
            return json_err(StatusCode::UNAUTHORIZED, "unauthorized");
        }
    }
    next.run(req).await
}

async fn health() -> Response {
    json_ok(json!({"status": "ok"}))
}

async fn status(State(st): State<AppState>) -> Response {
    let agent_count = st.registry.lock().await.pools().count();
    let session_count = st.sessions.lock().await.len();
    let default_model = st
        .cfg
        .default_model
        .as_ref()
        .map(|m| format!("{}/{}", m.provider, m.model))
        .unwrap_or_default();
    json_ok(json!({
        "status": "running",
        "agent_count": agent_count,
        "session_count": session_count,
        "default_model": default_model,
        "bind": st.cfg.server.bind.to_string(),
    }))
}

async fn config_info(State(st): State<AppState>) -> Response {
    let providers: Vec<Value> = st
        .cfg
        .providers
        .iter()
        .map(|(name, p)| json!({"name": name, "driver": p.driver, "model": p.model}))
        .collect();
    let tools: Vec<Value> = st
        .cfg
        .tools
        .iter()
        .map(|(name, t)| json!({"name": name, "description": t.description}))
        .collect();
    json_ok(json!({
        "bind": st.cfg.server.bind.to_string(),
        "api_key_set": st.cfg.api_key().is_some(),
        "default_model": st.cfg.default_model,
        "providers": providers,
        "tools": tools,
    }))
}

fn agent_json(def: &AgentDef) -> Value {
    json!({
        "kind": def.kind,
        "system_prompt": def.system_prompt,
        "model": def.model,
        "instances": def.instances,
        "max_history": def.max_history,
        "context_window": def.context_window,
        "compaction_ratio": def.compaction_ratio,
        "auto_compact": def.auto_compact,
        "capabilities": def.capabilities,
    })
}

async fn list_agents(State(st): State<AppState>) -> Response {
    let registry = st.registry.lock().await;
    let agents: Vec<Value> = registry
        .pools()
        .map(|(kind, pool)| {
            agent_json(&AgentDef {
                kind: kind.clone(),
                system_prompt: pool.system_prompt.clone(),
                model: pool.model.clone(),
                instances: pool.instances().len(),
                max_history: pool.max_history as usize,
                context_window: pool.context_window as usize,
                compaction_ratio: pool.compaction_ratio,
                auto_compact: pool.auto_compact,
                capabilities: pool.caps.clone(),
            })
        })
        .collect();
    json_ok(json!({"agents": agents, "total": agents.len()}))
}

async fn create_agent(State(st): State<AppState>, Json(def): Json<AgentDef>) -> Response {
    if let Err(err) = st.db.insert_agent(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    let pool = match host::build_pool(&st.ctx, &def).await {
        Ok(pool) => pool,
        Err(err) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    st.registry.lock().await.insert(def.kind.clone(), pool);
    json_created(json!({"kind": def.kind, "status": "created"}))
}

async fn update_agent(
    State(st): State<AppState>,
    path: AgentKindPath,
    Json(def): Json<AgentDef>,
) -> Response {
    if def.kind != path.kind {
        return json_err(StatusCode::BAD_REQUEST, "kind in body must match path");
    }
    if let Err(err) = st.db.insert_agent(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    let pool = match host::build_pool(&st.ctx, &def).await {
        Ok(pool) => pool,
        Err(err) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    st.registry.lock().await.insert(def.kind.clone(), pool);
    json_ok(json!({"kind": def.kind, "status": "updated"}))
}

async fn delete_agent(State(st): State<AppState>, path: AgentKindPath) -> Response {
    let sessions_deleted = match st.db.delete_agent(&path.kind) {
        Ok(n) => n,
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("db error: {err}"),
            );
        }
    };
    st.registry.lock().await.remove(&path.kind);
    st.sessions
        .lock()
        .await
        .retain(|_, entry| entry.kind != path.kind);
    json_ok(json!({"kind": path.kind, "status": "deleted", "sessions_deleted": sessions_deleted}))
}

async fn list_sessions(State(st): State<AppState>) -> Response {
    let sessions: Vec<Value> = st
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, entry)| json!({"id": id, "agent": entry.kind}))
        .collect();
    json_ok(json!({"sessions": sessions, "total": sessions.len()}))
}

async fn create_session(State(st): State<AppState>, Json(req): Json<CreateSessionReq>) -> Response {
    let Some(pool) = st.registry.lock().await.get(&req.agent) else {
        return json_err(StatusCode::NOT_FOUND, "unknown agent kind");
    };
    let instance = pool.next();
    let session_id = st.next_session_id.fetch_add(1, Ordering::SeqCst) + 1;
    let config = crate::bindings::exports::carson::agent::agent::SessionConfig {
        system_prompt: pool.system_prompt.clone(),
        model: pool.model.clone(),
        capabilities_json: json!(pool.caps).to_string(),
        max_history: pool.max_history,
        context_window: pool.context_window,
        compaction_ratio: pool.compaction_ratio,
        auto_compact: pool.auto_compact,
    };

    instance.stop.store(false, Ordering::SeqCst);
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let result = guest
        .func_create_session()
        .call_async(&mut *store, (session_id, &config))
        .await;
    drop(store);

    match result {
        Ok((Ok(()),)) => {
            st.sessions.lock().await.insert(
                session_id,
                SessionEntry {
                    kind: pool.kind.clone(),
                    instance: instance.clone(),
                },
            );
            host::snapshot_session(&st.db, &instance, session_id).await;
            json_created(json!({"session_id": session_id, "agent": pool.kind}))
        }
        Ok((Err(err),)) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("agent error: {err:?}"),
        ),
        Err(err) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("wasm error: {err}"),
        ),
    }
}

async fn get_session(State(st): State<AppState>, path: SessionPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let mut store = entry.instance.store.lock().await;
    let guest = entry.instance.agent.carson_agent_agent();
    let result = guest
        .func_session_history()
        .call_async(&mut *store, (id,))
        .await;
    drop(store);
    let messages = match result {
        Ok((Ok(messages),)) => messages,
        _ => return json_err(StatusCode::NOT_FOUND, "session not found"),
    };
    let messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            let tool_calls = m.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|tc| json!({"id": tc.id, "name": tc.name, "arguments": tc.arguments_json}))
                    .collect::<Vec<_>>()
            });
            json!({
                "role": m.role,
                "content": m.content,
                "tool_calls": tool_calls,
                "tool_call_id": m.tool_call_id,
            })
        })
        .collect();
    json_ok(
        json!({"session_id": id, "agent": entry.kind, "message_count": messages.len(), "messages": messages}),
    )
}

async fn destroy_session(State(st): State<AppState>, path: SessionPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.remove(&id) else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let mut store = entry.instance.store.lock().await;
    let guest = entry.instance.agent.carson_agent_agent();
    let _ = guest
        .func_destroy_session()
        .call_async(&mut *store, (id,))
        .await;
    drop(store);
    let _ = st.db.delete_session(id);
    json_ok(json!({"status": "deleted", "session_id": id}))
}

async fn reset_session(State(st): State<AppState>, path: ResetPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let mut store = entry.instance.store.lock().await;
    let guest = entry.instance.agent.carson_agent_agent();
    let result = guest
        .func_reset_session()
        .call_async(&mut *store, (id,))
        .await;
    drop(store);
    match result {
        Ok((Ok(()),)) => {
            host::snapshot_session(&st.db, &entry.instance, id).await;
            json_ok(json!({"status": "reset", "session_id": id}))
        }
        _ => json_err(StatusCode::NOT_FOUND, "session not found"),
    }
}

async fn stop_session(State(st): State<AppState>, path: StopPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    entry.instance.stop.store(true, Ordering::SeqCst);
    json_ok(json!({"status": "stopped", "session_id": id}))
}

async fn compact_session(State(st): State<AppState>, path: CompactPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let mut store = entry.instance.store.lock().await;
    let guest = entry.instance.agent.carson_agent_agent();
    let result = guest
        .func_compact_session()
        .call_async(&mut *store, (id,))
        .await;
    drop(store);
    match result {
        Ok((Ok(()),)) => {
            host::snapshot_session(&st.db, &entry.instance, id).await;
            json_ok(json!({"status": "compacted", "session_id": id}))
        }
        Ok((Err(crate::bindings::exports::carson::agent::agent::Error::NotFound),)) => {
            json_err(StatusCode::NOT_FOUND, "session not found")
        }
        _ => json_err(StatusCode::INTERNAL_SERVER_ERROR, "compaction failed"),
    }
}

async fn run_message(
    instance: &AgentInstance,
    session_id: u64,
    content: &str,
) -> anyhow::Result<()> {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_handle_message()
        .call_async(&mut *store, (session_id, content))
        .await?;
    result.map_err(|err| anyhow::anyhow!("agent error: {err:?}"))
}

async fn session_usage(instance: &AgentInstance, session_id: u64) -> Usage {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    match guest
        .func_session_usage()
        .call_async(&mut *store, (session_id,))
        .await
    {
        Ok((Ok(usage),)) => Usage {
            input_tokens: usage.input_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            output_tokens: usage.output_tokens,
        },
        _ => Usage::default(),
    }
}

async fn send_stream(
    State(st): State<AppState>,
    path: StreamPath,
    Json(req): Json<MessageReq>,
) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let (tx, rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(id, tx);
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();

    tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result = run_message(&instance, id, &req.content).await;
        host::snapshot_session(&db, &instance, id).await;
        let usage = session_usage(&instance, id).await;
        if result.is_err() {
            let _ = hub.send(
                id,
                SseItem {
                    event: "error".into(),
                    data: json!({"message": "agent run failed"}),
                },
            );
        }
        let _ = hub.send(
            id,
            SseItem {
                event: "done".into(),
                data: json!({"done": true, "usage": usage}),
            },
        );
        hub.unregister(id);
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|item| (Ok::<Bytes, Infallible>(Bytes::from(sse_frame(&item))), rx))
    });
    let mut resp = Response::new(Body::from_stream(stream));
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

async fn send_message(
    State(st): State<AppState>,
    path: MessagePath,
    Json(req): Json<MessageReq>,
) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(id, tx);
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();

    let task = tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result = run_message(&instance, id, &req.content).await;
        host::snapshot_session(&db, &instance, id).await;
        let usage = session_usage(&instance, id).await;
        if result.is_err() {
            let _ = hub.send(
                id,
                SseItem {
                    event: "error".into(),
                    data: json!({"message": "agent run failed"}),
                },
            );
        }
        let _ = hub.send(
            id,
            SseItem {
                event: "done".into(),
                data: json!({"done": true, "usage": usage}),
            },
        );
        hub.unregister(id);
        usage
    });

    let mut response = String::new();
    let mut usage = Usage::default();
    while let Some(item) = rx.recv().await {
        match item.event.as_str() {
            "chunk" => {
                if let Some(text) = item.data.as_str() {
                    response.push_str(text);
                }
            }
            "done" => {
                if let Some(u) = item.data.get("usage") {
                    usage = serde_json::from_value(u.clone()).unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    if let Ok(task_usage) = task.await
        && usage.input_tokens == 0
        && usage.output_tokens == 0
    {
        usage = task_usage;
    }
    json_ok(json!({"response": response, "usage": usage}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use axum::http::header::AUTHORIZATION;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn config(api_key: &str) -> Config {
        toml::from_str(&format!(
            "[server]\nbind = \"127.0.0.1:8000\"\napi_key = \"{api_key}\"\n"
        ))
        .unwrap()
    }

    async fn app_state(config: Config) -> AppState {
        let ctx = Arc::new(HostContext::new(&config).unwrap());
        let db = Db::open_in_memory().unwrap();
        AppState {
            ctx,
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            db,
            hub: Hub::new(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            cfg: Arc::new(config),
        }
    }

    async fn response(app: Router, uri: &str, bearer: Option<&str>) -> Response {
        let mut req = Request::builder().uri(uri);
        if let Some(token) = bearer {
            req = req.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
    }

    async fn read(resp: Response) -> (StatusCode, Vec<(String, String)>, String) {
        let status = resp.status();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn health_is_public_and_carries_security_headers() {
        let app = router(app_state(config("sekret")).await);
        let (status, headers, body) = read(response(app, "/api/health", None).await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"status\":\"ok\""));
        let header = |name: &str| headers.iter().any(|(k, _)| k == name);
        assert!(header("x-request-id"));
        assert!(header("x-frame-options"));
        assert!(header("content-security-policy"));
        assert!(header("x-content-type-options"));
    }

    #[tokio::test]
    async fn auth_requires_valid_bearer_token() {
        let app = router(app_state(config("sekret")).await);
        let (status, _, _) = read(response(app.clone(), "/api/agents", None).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = read(response(app.clone(), "/api/agents", Some("wrong")).await).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = read(response(app, "/api/agents", Some("sekret")).await).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn no_auth_required_when_api_key_unset() {
        let app = router(app_state(config("")).await);
        let (status, _, _) = read(response(app, "/api/agents", None).await).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn read_endpoints_with_empty_registry() {
        let app = router(app_state(config("")).await);
        for path in ["/api/agents", "/api/sessions", "/api/status", "/api/config"] {
            let (status, _, body) = read(response(app.clone(), path, None).await).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
        }
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let app = router(app_state(config("")).await);
        let (status, _, _) = read(response(app, "/api/nope", None).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
