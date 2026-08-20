use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use axum_extra::routing::TypedPath;
use carson_host::app::{AppState, SessionEntry};
use carson_host::drivers::Usage;
use carson_host::host;
use carson_host::hub::{SseItem, sse_frame};
use carson_host::registry::{AgentDef, AgentInstance, ProviderDef, ToolDef};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

#[derive(Deserialize, ToSchema)]
pub struct CreateSessionReq {
    agent: String,
}

#[derive(Deserialize, ToSchema)]
pub struct MessageReq {
    content: String,
}

/// Request body for registering or updating a `custom/` tool. The wasm is base64-encoded.
#[derive(Deserialize, ToSchema)]
pub struct ToolReq {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub wasm_b64: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}")]
pub(crate) struct SessionPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/message")]
pub(crate) struct MessagePath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stream")]
pub(crate) struct StreamPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stop")]
pub(crate) struct StopPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/reset")]
pub(crate) struct ResetPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/compact")]
pub(crate) struct CompactPath {
    id: u64,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/agents/{kind}")]
pub(crate) struct AgentKindPath {
    kind: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/providers/{name}")]
pub(crate) struct ProviderNamePath {
    name: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/tools/{*name}")]
pub(crate) struct ToolNamePath {
    name: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "carson API",
        version = "0.1.0",
        description = "HTTP API for the carson wasm agent host"
    ),
    paths(
        health,
        status,
        config_info,
        list_agents,
        create_agent,
        update_agent,
        delete_agent,
        list_providers,
        create_provider,
        update_provider,
        delete_provider,
        list_tools,
        create_tool,
        update_tool,
        delete_tool,
        list_sessions,
        create_session,
        get_session,
        destroy_session,
        send_message,
        send_stream,
        stop_session,
        reset_session,
        compact_session,
    ),
    components(schemas(
        AgentDef,
        ProviderDef,
        ToolDef,
        ToolReq,
        CreateSessionReq,
        MessageReq,
        Usage,
        HealthResponse,
        StatusResponse,
        ProviderListResponse,
        ToolListResponse,
        ConfigResponse,
        AgentListResponse,
        AgentCommandResponse,
        AgentDeleteResponse,
        ProviderCommandResponse,
        ToolCommandResponse,
        SessionSummary,
        SessionListResponse,
        SessionCreateResponse,
        ToolCallInfo,
        MessageInfo,
        SessionResponse,
        SessionCommandResponse,
        MessageResponse,
        ErrorResponse,
    ))
)]
pub struct ApiDoc;

#[derive(ToSchema)]
#[schema(example = json!({"status": "ok"}))]
pub struct HealthResponse {
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "status": "running",
    "agent_count": 1,
    "session_count": 2,
    "provider_count": 1,
    "tool_count": 2,
    "bind": "127.0.0.1:8000"
}))]
pub struct StatusResponse {
    pub status: String,
    pub agent_count: usize,
    pub session_count: usize,
    pub provider_count: usize,
    pub tool_count: usize,
    pub bind: String,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "providers": [{"name": "groq", "base_url": "https://api.groq.com/openai/v1"}],
    "total": 1
}))]
pub struct ProviderListResponse {
    pub providers: Vec<ProviderDef>,
    pub total: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "tools": [{"name": "core/time", "description": "Return the current unix time in milliseconds", "parameters": {}}],
    "total": 1
}))]
pub struct ToolListResponse {
    pub tools: Vec<ToolDef>,
    pub total: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({"name": "groq", "status": "created"}))]
pub struct ProviderCommandResponse {
    pub name: String,
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"name": "core/time", "status": "created"}))]
pub struct ToolCommandResponse {
    pub name: String,
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "bind": "127.0.0.1:8000"
}))]
pub struct ConfigResponse {
    pub bind: String,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "agents": [],
    "total": 0
}))]
pub struct AgentListResponse {
    pub agents: Vec<AgentDef>,
    pub total: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({"kind": "assistant", "status": "created"}))]
pub struct AgentCommandResponse {
    pub kind: String,
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"kind": "assistant", "status": "deleted", "sessions_deleted": 0}))]
pub struct AgentDeleteResponse {
    pub kind: String,
    pub status: String,
    pub sessions_deleted: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({"id": 1, "agent": "assistant"}))]
pub struct SessionSummary {
    pub id: u64,
    pub agent: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"sessions": [], "total": 0}))]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
    pub total: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({"session_id": 1, "agent": "assistant"}))]
pub struct SessionCreateResponse {
    pub session_id: u64,
    pub agent: String,
}

#[derive(ToSchema)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "role": "user",
    "content": "hello",
    "tool_calls": null,
    "tool_call_id": null
}))]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCallInfo>>,
    pub tool_call_id: Option<String>,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "session_id": 1,
    "agent": "assistant",
    "message_count": 1,
    "messages": [{"role": "user", "content": "hello", "tool_calls": null, "tool_call_id": null}]
}))]
pub struct SessionResponse {
    pub session_id: u64,
    pub agent: String,
    pub message_count: usize,
    pub messages: Vec<MessageInfo>,
}

#[derive(ToSchema)]
#[schema(example = json!({"status": "deleted", "session_id": 1}))]
pub struct SessionCommandResponse {
    pub status: String,
    pub session_id: u64,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "response": "hello",
    "usage": {
        "input_tokens": 10,
        "cache_read_tokens": 0,
        "cache_creation_tokens": 0,
        "output_tokens": 5
    }
}))]
pub struct MessageResponse {
    pub response: String,
    pub usage: Usage,
}

#[derive(ToSchema)]
#[schema(example = json!({"error": "session not found"}))]
pub struct ErrorResponse {
    pub error: String,
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/config", get(config_info))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(AgentKindPath::PATH, put(update_agent).delete(delete_agent))
        .route("/api/providers", get(list_providers).post(create_provider))
        .route(
            ProviderNamePath::PATH,
            put(update_provider).delete(delete_provider),
        )
        .route("/api/tools", get(list_tools).post(create_tool))
        .route(ToolNamePath::PATH, put(update_tool).delete(delete_tool))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(SessionPath::PATH, get(get_session).delete(destroy_session))
        .route(MessagePath::PATH, post(send_message))
        .route(StreamPath::PATH, post(send_stream))
        .route(StopPath::PATH, post(stop_session))
        .route(ResetPath::PATH, post(reset_session))
        .route(CompactPath::PATH, post(compact_session))
        .layer(middleware::from_fn(security))
        .with_state(state);

    let docs = Router::from(SwaggerUi::new("/api").url("/api/openapi.json", ApiDoc::openapi()));
    api.merge(docs)
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

/// Check service health.
#[utoipa::path(
    get,
    path = "/api/health",
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse)
    )
)]
pub(crate) async fn health() -> Response {
    json_ok(json!({"status": "ok"}))
}

/// Get runtime status.
#[utoipa::path(
    get,
    path = "/api/status",
    responses(
        (status = 200, description = "Server status", body = StatusResponse)
    )
)]
pub(crate) async fn status(State(st): State<AppState>) -> Response {
    let agent_count = st.registry.lock().await.pools().count();
    let session_count = st.sessions.lock().await.len();
    let provider_count = st.ctx.drivers.read().unwrap().len();
    let tool_count = st.ctx.tool_runner.specs().len();
    json_ok(json!({
        "status": "running",
        "agent_count": agent_count,
        "session_count": session_count,
        "provider_count": provider_count,
        "tool_count": tool_count,
        "bind": st.cfg.server.bind().to_string(),
    }))
}

/// Get the effective runtime configuration.
#[utoipa::path(
    get,
    path = "/api/config",
    responses(
        (status = 200, description = "Effective configuration", body = ConfigResponse)
    )
)]
pub(crate) async fn config_info(State(st): State<AppState>) -> Response {
    json_ok(json!({
        "bind": st.cfg.server.bind().to_string(),
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

/// List registered agents.
#[utoipa::path(
    get,
    path = "/api/agents",
    responses(
        (status = 200, description = "Agent list", body = AgentListResponse)
    )
)]
pub(crate) async fn list_agents(State(st): State<AppState>) -> Response {
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

/// Create a new agent from an `AgentDef`.
#[utoipa::path(
    post,
    path = "/api/agents",
    request_body = AgentDef,
    responses(
        (status = 201, description = "Agent created", body = AgentCommandResponse),
        (status = 400, description = "Model is not in 'provider/model' form or the provider is unknown", body = ErrorResponse),
        (status = 500, description = "Agent build or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn create_agent(
    State(st): State<AppState>,
    Json(def): Json<AgentDef>,
) -> Response {
    if let Some(err) = validate_agent_model(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, err);
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
    json_created(json!({"kind": def.kind, "status": "created"}))
}

/// Validate that the agent's model is `provider/model` and the provider is registered.
fn validate_agent_model(st: &AppState, def: &AgentDef) -> Option<&'static str> {
    let (provider, model) = def.model.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return Some("model must be in 'provider/model' form");
    }
    if !st.ctx.has_driver(provider) {
        return Some("unknown provider in model");
    }
    None
}

/// Update an existing agent definition.
#[utoipa::path(
    put,
    path = "/api/agents/{kind}",
    params(
        ("kind" = String, Path, description = "Agent kind")
    ),
    request_body = AgentDef,
    responses(
        (status = 200, description = "Agent updated", body = AgentCommandResponse),
        (status = 400, description = "Kind in body does not match path", body = ErrorResponse)
    )
)]
pub(crate) async fn update_agent(
    State(st): State<AppState>,
    path: AgentKindPath,
    Json(def): Json<AgentDef>,
) -> Response {
    if def.kind != path.kind {
        return json_err(StatusCode::BAD_REQUEST, "kind in body must match path");
    }
    if let Some(err) = validate_agent_model(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, err);
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

/// Delete an agent and its sessions.
#[utoipa::path(
    delete,
    path = "/api/agents/{kind}",
    params(
        ("kind" = String, Path, description = "Agent kind")
    ),
    responses(
        (status = 200, description = "Agent deleted", body = AgentDeleteResponse),
        (status = 500, description = "Db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn delete_agent(State(st): State<AppState>, path: AgentKindPath) -> Response {
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

/// List registered providers.
#[utoipa::path(
    get,
    path = "/api/providers",
    responses(
        (status = 200, description = "Provider list", body = ProviderListResponse)
    )
)]
pub(crate) async fn list_providers(State(st): State<AppState>) -> Response {
    let providers = match st.db.list_providers() {
        Ok(p) => p,
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("db error: {err}"),
            );
        }
    };
    json_ok(json!({"providers": providers, "total": providers.len()}))
}

/// Register a new provider (always OpenAI-compatible).
#[utoipa::path(
    post,
    path = "/api/providers",
    request_body = ProviderDef,
    responses(
        (status = 201, description = "Provider created", body = ProviderCommandResponse),
        (status = 400, description = "Missing base_url", body = ErrorResponse),
        (status = 500, description = "Driver build or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn create_provider(
    State(st): State<AppState>,
    Json(def): Json<ProviderDef>,
) -> Response {
    if def.base_url.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "base_url is required");
    }
    let driver = match host::openai_driver(&def) {
        Ok(d) => d,
        Err(err) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    if let Err(err) = st.db.upsert_provider(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    st.ctx.register_driver(&def.name, driver);
    json_created(json!({"name": def.name, "status": "created"}))
}

/// Update an existing provider.
#[utoipa::path(
    put,
    path = "/api/providers/{name}",
    params(
        ("name" = String, Path, description = "Provider name")
    ),
    request_body = ProviderDef,
    responses(
        (status = 200, description = "Provider updated", body = ProviderCommandResponse),
        (status = 400, description = "Name in body does not match path", body = ErrorResponse),
        (status = 500, description = "Driver build or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn update_provider(
    State(st): State<AppState>,
    path: ProviderNamePath,
    Json(def): Json<ProviderDef>,
) -> Response {
    if def.name != path.name {
        return json_err(StatusCode::BAD_REQUEST, "name in body must match path");
    }
    if def.base_url.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "base_url is required");
    }
    let driver = match host::openai_driver(&def) {
        Ok(d) => d,
        Err(err) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    if let Err(err) = st.db.upsert_provider(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    st.ctx.register_driver(&def.name, driver);
    json_ok(json!({"name": def.name, "status": "updated"}))
}

/// Delete a provider.
#[utoipa::path(
    delete,
    path = "/api/providers/{name}",
    params(
        ("name" = String, Path, description = "Provider name")
    ),
    responses(
        (status = 200, description = "Provider deleted", body = ProviderCommandResponse),
        (status = 500, description = "Db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn delete_provider(
    State(st): State<AppState>,
    path: ProviderNamePath,
) -> Response {
    match st.db.delete_provider(&path.name) {
        Ok(_) => {
            st.ctx.remove_driver(&path.name);
            json_ok(json!({"name": path.name, "status": "deleted"}))
        }
        Err(err) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        ),
    }
}

/// List registered tools (bundled `core/` built-ins plus `custom/` tools from the DB).
#[utoipa::path(
    get,
    path = "/api/tools",
    responses(
        (status = 200, description = "Tool list", body = ToolListResponse)
    )
)]
pub(crate) async fn list_tools(State(st): State<AppState>) -> Response {
    let mut tools: Vec<Value> = st
        .ctx
        .tool_runner
        .specs()
        .into_iter()
        .map(|spec| {
            let name = spec.name.clone();
            if name.starts_with("custom/")
                && let Ok(Some(def)) = st.db.get_tool_def(&name)
            {
                let mut v = serde_json::to_value(&def).unwrap_or(Value::Null);
                if let Ok(Some(wasm)) = st.db.get_tool_wasm(&name)
                    && let Some(obj) = v.as_object_mut()
                {
                    obj.insert(
                        "wasm_b64".into(),
                        Value::String(base64::Engine::encode(
                            &base64::engine::general_purpose::STANDARD,
                            wasm,
                        )),
                    );
                }
                v
            } else {
                serde_json::to_value(spec).unwrap_or(Value::Null)
            }
        })
        .collect();
    tools.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .cmp(&b.get("name").and_then(|n| n.as_str()))
    });
    json_ok(json!({"tools": tools, "total": tools.len()}))
}

/// Register a new `custom/` tool. The wasm module is base64-encoded in the request.
#[utoipa::path(
    post,
    path = "/api/tools",
    request_body = ToolReq,
    responses(
        (status = 201, description = "Tool created", body = ToolCommandResponse),
        (status = 400, description = "Invalid tool name or wasm", body = ErrorResponse),
        (status = 500, description = "Tool compile or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn create_tool(State(st): State<AppState>, Json(req): Json<ToolReq>) -> Response {
    upsert_tool_common(&st, &req, "created", true)
}

fn upsert_tool_common(st: &AppState, req: &ToolReq, status: &str, created: bool) -> Response {
    if !req.name.starts_with("custom/") {
        return json_err(
            StatusCode::BAD_REQUEST,
            "tool name must start with 'custom/'",
        );
    }
    let wasm =
        match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.wasm_b64) {
            Ok(bytes) => bytes,
            Err(err) => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid base64 wasm: {err}"),
                );
            }
        };
    if wasm.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "wasm must not be empty");
    }
    let def = ToolDef {
        name: req.name.clone(),
        description: req.description.clone(),
        parameters: req.parameters.clone(),
        env: req.env.clone(),
    };
    if let Err(err) = st.db.upsert_tool(&def, &wasm) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    match st.ctx.register_tool(&def, &wasm) {
        Ok(()) => {}
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("tool compile failed: {err}"),
            );
        }
    }
    if created {
        json_created(json!({"name": def.name, "status": status}))
    } else {
        json_ok(json!({"name": def.name, "status": status}))
    }
}

/// Update an existing `custom/` tool.
#[utoipa::path(
    put,
    path = "/api/tools/{name}",
    params(
        ("name" = String, Path, description = "Tool name")
    ),
    request_body = ToolReq,
    responses(
        (status = 200, description = "Tool updated", body = ToolCommandResponse),
        (status = 400, description = "Invalid tool name or wasm", body = ErrorResponse),
        (status = 500, description = "Tool compile or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn update_tool(
    State(st): State<AppState>,
    path: ToolNamePath,
    Json(req): Json<ToolReq>,
) -> Response {
    if req.name != path.name {
        return json_err(StatusCode::BAD_REQUEST, "name in body must match path");
    }
    upsert_tool_common(&st, &req, "updated", false)
}

/// Delete a tool.
#[utoipa::path(
    delete,
    path = "/api/tools/{name}",
    params(
        ("name" = String, Path, description = "Tool name")
    ),
    responses(
        (status = 200, description = "Tool deleted", body = ToolCommandResponse),
        (status = 500, description = "Db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn delete_tool(State(st): State<AppState>, path: ToolNamePath) -> Response {
    match st.db.delete_tool(&path.name) {
        Ok(_) => {
            st.ctx.remove_tool(&path.name);
            json_ok(json!({"name": path.name, "status": "deleted"}))
        }
        Err(err) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        ),
    }
}

/// List active sessions.
#[utoipa::path(
    get,
    path = "/api/sessions",
    responses(
        (status = 200, description = "Session list", body = SessionListResponse)
    )
)]
pub(crate) async fn list_sessions(State(st): State<AppState>) -> Response {
    let sessions: Vec<Value> = st
        .sessions
        .lock()
        .await
        .iter()
        .map(|(id, entry)| json!({"id": id, "agent": entry.kind}))
        .collect();
    json_ok(json!({"sessions": sessions, "total": sessions.len()}))
}

/// Create a new session for an agent kind.
#[utoipa::path(
    post,
    path = "/api/sessions",
    request_body = CreateSessionReq,
    responses(
        (status = 201, description = "Session created", body = SessionCreateResponse),
        (status = 404, description = "Unknown agent kind", body = ErrorResponse)
    )
)]
pub(crate) async fn create_session(
    State(st): State<AppState>,
    Json(req): Json<CreateSessionReq>,
) -> Response {
    let Some(pool) = st.registry.lock().await.get(&req.agent) else {
        return json_err(StatusCode::NOT_FOUND, "unknown agent kind");
    };
    let instance = pool.next();
    let session_id = st.next_session_id.fetch_add(1, Ordering::SeqCst) + 1;
    let config = carson_host::bindings::exports::carson::agent::agent::SessionConfig {
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

/// Get a session's message history.
#[utoipa::path(
    get,
    path = "/api/sessions/{id}",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session history", body = SessionResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn get_session(State(st): State<AppState>, path: SessionPath) -> Response {
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

/// Destroy a session.
#[utoipa::path(
    delete,
    path = "/api/sessions/{id}",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session deleted", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn destroy_session(State(st): State<AppState>, path: SessionPath) -> Response {
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

/// Reset a session's history.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/reset",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session reset", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn reset_session(State(st): State<AppState>, path: ResetPath) -> Response {
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

/// Stop a running session.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/stop",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session stopped", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn stop_session(State(st): State<AppState>, path: StopPath) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    entry.instance.stop.store(true, Ordering::SeqCst);
    json_ok(json!({"status": "stopped", "session_id": id}))
}

/// Compact a session's history.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/compact",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session compacted", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 500, description = "Compaction failed", body = ErrorResponse)
    )
)]
pub(crate) async fn compact_session(State(st): State<AppState>, path: CompactPath) -> Response {
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
        Ok((Err(carson_host::bindings::exports::carson::agent::agent::Error::NotFound),)) => {
            json_err(StatusCode::NOT_FOUND, "session not found")
        }
        _ => json_err(StatusCode::INTERNAL_SERVER_ERROR, "compaction failed"),
    }
}

async fn run_message_blocking(
    instance: Arc<AgentInstance>,
    session_id: u64,
    content: String,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build blocking runtime");
        rt.block_on(run_message(&instance, session_id, &content))
    })
    .await
    .unwrap_or_else(|err| Err(anyhow::anyhow!("agent task join failed: {err}")))
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

/// Send a message to a session and stream the response as Server-Sent Events.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/stream",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    request_body = MessageReq,
    responses(
        (status = 200, description = "SSE event stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn send_stream(
    State(st): State<AppState>,
    path: StreamPath,
    Json(req): Json<MessageReq>,
) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let (tx, rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(id, tx.clone());
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();

    tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result = run_message_blocking(instance.clone(), id, req.content.clone()).await;
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
        hub.unregister(id, &tx);
        drop(tx);
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

/// Send a message to a session and await the full response.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/message",
    params(
        ("id" = u64, Path, description = "Session id")
    ),
    request_body = MessageReq,
    responses(
        (status = 200, description = "Full response", body = MessageResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn send_message(
    State(st): State<AppState>,
    path: MessagePath,
    Json(req): Json<MessageReq>,
) -> Response {
    let id = path.id;
    let Some(entry) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(id, tx.clone());
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();

    let task = tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result = run_message_blocking(instance.clone(), id, req.content.clone()).await;
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
        hub.unregister(id, &tx);
        drop(tx);
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
    use axum::http::Request;
    use carson_host::config::Config;
    use carson_host::db::Db;
    use carson_host::hub::Hub;
    use carson_host::registry::AgentRegistry;
    use http_body_util::BodyExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use carson_host::host::HostContext;

    fn config() -> Config {
        toml::from_str("[server]\nip = \"127.0.0.1\"\nport = 8000\n").unwrap()
    }

    async fn app_state() -> AppState {
        let ctx = Arc::new(HostContext::new().unwrap());
        let db = Db::open_in_memory().unwrap();
        AppState {
            ctx,
            registry: Arc::new(Mutex::new(AgentRegistry::new())),
            db,
            hub: Hub::new(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            cfg: Arc::new(config()),
        }
    }

    async fn response(app: Router, uri: &str) -> Response {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
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

    async fn post(app: &Router, uri: &str, body: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn put(app: &Router, uri: &str, body: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn delete(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn health_is_public_and_carries_security_headers() {
        let app = router(app_state().await);
        let (status, headers, body) = read(response(app, "/api/health").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"status\":\"ok\""));
        let header = |name: &str| headers.iter().any(|(k, _)| k == name);
        assert!(header("x-request-id"));
        assert!(header("x-frame-options"));
        assert!(header("content-security-policy"));
        assert!(header("x-content-type-options"));
    }

    #[tokio::test]
    async fn read_endpoints_with_empty_registry() {
        let app = router(app_state().await);
        for path in [
            "/api/agents",
            "/api/sessions",
            "/api/status",
            "/api/config",
            "/api/providers",
            "/api/tools",
        ] {
            let (status, _, body) = read(response(app.clone(), path).await).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
        }
    }

    #[tokio::test]
    async fn unknown_path_is_404() {
        let app = router(app_state().await);
        let (status, _, _) = read(response(app, "/api/nope").await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn docs_page_is_public_and_served() {
        let app = router(app_state().await);
        let (status, headers, _) = read(response(app.clone(), "/api").await).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert!(headers.iter().any(|(k, v)| k == "location" && v == "/api/"));
        let (status, _, body) = read(response(app, "/api/").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("swagger"), "expected swagger page");
    }

    #[tokio::test]
    async fn openapi_spec_is_public_and_complete() {
        let app = router(app_state().await);
        let (status, headers, body) = read(response(app, "/api/openapi.json").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers.iter().any(|(k, _)| k == "content-type"));
        for needle in [
            "\"openapi\"",
            "\"/api/agents\"",
            "\"/api/sessions/{id}\"",
            "\"/api/sessions/{id}/stream\"",
            "\"/api/providers\"",
            "\"/api/tools\"",
        ] {
            assert!(body.contains(needle), "spec missing {needle}");
        }
    }

    #[tokio::test]
    async fn provider_crud_roundtrip() {
        let app = router(app_state().await);
        let (status, _, body) = read(post(
            &app,
            "/api/providers",
            r#"{"name":"groq","base_url":"https://api.groq.com/openai/v1","api_key":"gsk-secret"}"#,
        ).await)
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, _, body) = read(response(app.clone(), "/api/providers").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("groq"), "{body}");
        assert!(
            !body.contains("gsk-secret"),
            "api key must not be exposed: {body}"
        );
        let (status, _, body) = read(
            put(
                &app,
                "/api/providers/groq",
                r#"{"name":"groq","base_url":"https://changed.example","api_key":null}"#,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, _) = read(delete(&app, "/api/providers/groq").await).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn provider_requires_base_url() {
        let app = router(app_state().await);
        let (status, _, body) = read(
            post(
                &app,
                "/api/providers",
                r#"{"name":"groq","base_url":"","api_key":null}"#,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn custom_tool_roundtrip() {
        let app = router(app_state().await);
        let engine = base64::engine::general_purpose::STANDARD;
        let wasm =
            base64::Engine::encode(&engine, carson_host::host::embedded_tool("echo").unwrap());
        let (status, _, body) = read(post(
            &app,
            "/api/tools",
            &format!(r#"{{"name":"custom/x","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#),
        ).await)
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, _, body) = read(response(app.clone(), "/api/tools").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("custom/x"), "{body}");
        let (status, _, _) = read(delete(&app, "/api/tools/custom/x").await).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn bundled_tool_name_is_rejected() {
        let app = router(app_state().await);
        let engine = base64::engine::general_purpose::STANDARD;
        let wasm =
            base64::Engine::encode(&engine, carson_host::host::embedded_tool("echo").unwrap());
        let (status, _, body) = read(post(
            &app,
            "/api/tools",
            &format!(r#"{{"name":"core/time","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#),
        ).await)
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}
