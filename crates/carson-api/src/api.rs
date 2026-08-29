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
    /// Link the session to an existing sandbox; omitted creates a fresh one.
    #[serde(default)]
    sandbox_id: Option<String>,
}

/// Rename a session and/or point it at a different sandbox. Omitted fields
/// leave the corresponding setting unchanged.
#[derive(Deserialize, ToSchema)]
pub struct SessionUpdateReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    sandbox_id: Option<String>,
}

/// Create or rename a sandbox by its display alias.
#[derive(Deserialize, ToSchema)]
pub struct SandboxReq {
    name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct MessageReq {
    content: String,
}

/// Request body for registering or updating a custom tool. The wasm is
/// base64-encoded; on update, omitting `wasm_b64` keeps the stored module.
#[derive(Deserialize, ToSchema)]
pub struct ToolReq {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub wasm_b64: Option<String>,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}")]
pub(crate) struct SessionPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/message")]
pub(crate) struct MessagePath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stream")]
pub(crate) struct StreamPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/stop")]
pub(crate) struct StopPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/reset")]
pub(crate) struct ResetPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sessions/{id}/compact")]
pub(crate) struct CompactPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/sandboxes/{id}")]
pub(crate) struct SandboxPath {
    id: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/agents/{name}")]
pub(crate) struct AgentNamePath {
    name: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/agents/{name}/versions")]
pub(crate) struct AgentVersionsPath {
    name: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/providers/{name}")]
pub(crate) struct ProviderNamePath {
    name: String,
}

#[derive(TypedPath, Deserialize)]
#[typed_path("/api/tools/{*id}")]
pub(crate) struct ToolIdPath {
    id: String,
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
        BlockInfo,
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
    "tools": [{"name": "core/time", "description": "Return the current UTC time in ISO 8601 format", "parameters": {}}],
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
#[schema(example = json!({"name": "assistant", "version_id": "<uuid>", "status": "created"}))]
pub struct AgentCommandResponse {
    pub name: String,
    pub version_id: String,
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"name": "assistant", "status": "deleted"}))]
pub struct AgentDeleteResponse {
    pub name: String,
    pub status: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"id": "<uuid>", "agent": "assistant"}))]
pub struct SessionSummary {
    pub id: String,
    pub agent: String,
}

#[derive(ToSchema)]
#[schema(example = json!({"sessions": [], "total": 0}))]
pub struct SessionListResponse {
    pub sessions: Vec<SessionSummary>,
    pub total: usize,
}

#[derive(ToSchema)]
#[schema(example = json!({"session_id": "<uuid>", "agent": "assistant", "agent_version_id": "<uuid>"}))]
pub struct SessionCreateResponse {
    pub session_id: String,
    pub agent: String,
    pub agent_version_id: String,
}

/// One entry of the conversation block log.
#[derive(ToSchema)]
#[schema(example = json!({
    "kind": "text",
    "text": "hello",
    "tool_call_id": null,
    "tool_name": null,
    "arguments": null,
    "is_error": false,
    "input_tokens": 10,
    "cache_read_tokens": 0,
    "cache_creation_tokens": 0,
    "output_tokens": 5,
    "created_at_ms": 1755000000000u64,
    "finished_at_ms": 1755000000500u64
}))]
pub struct BlockInfo {
    pub kind: String,
    pub text: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub arguments: Option<String>,
    pub is_error: bool,
    pub input_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub output_tokens: u32,
    pub created_at_ms: u64,
    pub finished_at_ms: u64,
}

#[derive(ToSchema)]
#[schema(example = json!({
    "session_id": "<uuid>",
    "agent": "assistant",
    "agent_version_id": "<uuid>",
    "model": "groq/llama-3",
    "message_count": 1,
    "messages": [{"kind": "user", "text": "hello"}]
}))]
pub struct SessionResponse {
    pub session_id: String,
    pub agent: String,
    pub agent_version_id: String,
    pub model: Option<String>,
    pub message_count: usize,
    pub messages: Vec<BlockInfo>,
}

#[derive(ToSchema)]
#[schema(example = json!({"status": "deleted", "session_id": "<uuid>"}))]
pub struct SessionCommandResponse {
    pub status: String,
    pub session_id: String,
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
        .route(AgentNamePath::PATH, put(update_agent).delete(delete_agent))
        .route(AgentVersionsPath::PATH, get(list_agent_versions))
        .route("/api/providers", get(list_providers).post(create_provider))
        .route(
            ProviderNamePath::PATH,
            put(update_provider).delete(delete_provider),
        )
        .route("/api/tools", get(list_tools).post(create_tool))
        .route(ToolIdPath::PATH, put(update_tool).delete(delete_tool))
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route(
            SessionPath::PATH,
            get(get_session).put(update_session).delete(destroy_session),
        )
        .route("/api/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route(SandboxPath::PATH, put(rename_sandbox))
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
        "id": def.id,
        "name": def.name,
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

/// List the current version of every named agent.
#[utoipa::path(
    get,
    path = "/api/agents",
    responses(
        (status = 200, description = "Agent list", body = AgentListResponse)
    )
)]
pub(crate) async fn list_agents(State(st): State<AppState>) -> Response {
    let agents = match st.db.list_agents() {
        Ok(agents) => agents,
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("db error: {err}"),
            );
        }
    };
    let agents: Vec<Value> = agents.iter().map(agent_json).collect();
    json_ok(json!({"agents": agents, "total": agents.len()}))
}

/// Every version of one agent, oldest first.
#[utoipa::path(
    get,
    path = "/api/agents/{name}/versions",
    params(
        ("name" = String, Path, description = "Agent name")
    ),
    responses(
        (status = 200, description = "Version history"),
        (status = 404, description = "Unknown agent name", body = ErrorResponse)
    )
)]
pub(crate) async fn list_agent_versions(
    State(st): State<AppState>,
    path: AgentVersionsPath,
) -> Response {
    let versions = match st.db.list_agent_versions(&path.name) {
        Ok(v) => v,
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("db error: {err}"),
            );
        }
    };
    if versions.is_empty() {
        return json_err(StatusCode::NOT_FOUND, "unknown agent name");
    }
    let versions: Vec<Value> = versions.iter().map(agent_json).collect();
    json_ok(json!({"versions": versions, "total": versions.len()}))
}

/// Create a new named agent (its first version).
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
    Json(mut def): Json<AgentDef>,
) -> Response {
    if def.name.is_empty() {
        return json_err(StatusCode::BAD_REQUEST, "agent name is required");
    }
    if st
        .db
        .current_agent(&def.name)
        .map(|c| c.is_some())
        .unwrap_or(false)
    {
        return json_err(StatusCode::CONFLICT, "agent name already exists");
    }
    if let Some(err) = validate_agent_model(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, err);
    }
    if let Some(err) = validate_agent_caps(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, &err);
    }
    def.id = uuid::Uuid::new_v4().to_string();
    let version_id = def.id.clone();
    let name = def.name.clone();
    if let Err(err) = st.db.insert_agent_version(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    if let Err(err) = st.db.set_current_agent(&name, &version_id) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    match carson_host::registry::get_or_build_pool(&st.ctx, &st.registry, &def).await {
        Ok(_) => json_created(json!({"name": name, "version_id": version_id, "status": "created"})),
        Err(err) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Providers only accept function names matching `^[a-zA-Z0-9_-]+$`, so a
/// `custom/…` tool name must sanitize cleanly: the namespace prefix plus a
/// remainder of plain word characters. Anything else would be rejected
/// upstream on every request.
fn invalid_tool_name(name: &str) -> Option<&'static str> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Some("tool names may only contain [a-zA-Z0-9_-]");
    }
    None
}

/// Validate an agent's capability list: every entry must resolve to a known
/// tool id, and no two entries may share the same bare name (a bundled and a
/// custom tool may both be called `time`, but one agent cannot select both —
/// the model would have no way to disambiguate).
fn validate_agent_caps(st: &AppState, def: &AgentDef) -> Option<String> {
    let mut seen: Vec<(String, String)> = Vec::new(); // (bare name, id)
    for id in &def.capabilities {
        let def = if let Some(b) = carson_host::host::builtin_by_id(id) {
            b
        } else {
            match st.db.get_tool(id) {
                Ok(Some(d)) => d,
                _ => return Some(format!("unknown tool id '{id}'")),
            }
        };
        if seen.iter().any(|(name, _)| name == &def.name) {
            return Some(format!(
                "duplicate tool '{}': bundled and custom tools with identical names \
                 cannot both be selected",
                def.name
            ));
        }
        seen.push((def.name, def.id.clone()));
    }
    None
}

/// Validate that the agent's model is `provider/model` and the provider is registered.
fn validate_agent_model(st: &AppState, def: &AgentDef) -> Option<&'static str> {
    let Some((provider, model)) = def.model.split_once('/') else {
        return Some("model must be in 'provider/model' form");
    };
    if provider.is_empty() || model.is_empty() {
        return Some("model must be in 'provider/model' form");
    }
    if !st.ctx.has_driver(provider) {
        return Some("unknown provider in model");
    }
    None
}

/// Update an agent: inserts a new immutable version and repoints the name.
/// Existing sessions stay pinned to the version they were created with.
#[utoipa::path(
    put,
    path = "/api/agents/{name}",
    params(
        ("name" = String, Path, description = "Agent name")
    ),
    request_body = AgentDef,
    responses(
        (status = 200, description = "New version created and pointer moved", body = AgentCommandResponse),
        (status = 400, description = "Name mismatch or invalid model", body = ErrorResponse),
        (status = 404, description = "Unknown agent name", body = ErrorResponse)
    )
)]
pub(crate) async fn update_agent(
    State(st): State<AppState>,
    path: AgentNamePath,
    Json(mut def): Json<AgentDef>,
) -> Response {
    if def.name != path.name {
        return json_err(StatusCode::BAD_REQUEST, "name in body must match path");
    }
    if st
        .db
        .current_agent(&path.name)
        .map(|c| c.is_none())
        .unwrap_or(true)
    {
        return json_err(StatusCode::NOT_FOUND, "unknown agent name");
    }
    if let Some(err) = validate_agent_model(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, err);
    }
    if let Some(err) = validate_agent_caps(&st, &def) {
        return json_err(StatusCode::BAD_REQUEST, &err);
    }
    def.id = uuid::Uuid::new_v4().to_string();
    let version_id = def.id.clone();
    let name = def.name.clone();
    if let Err(err) = st.db.insert_agent_version(&def) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    if let Err(err) = st.db.set_current_agent(&name, &version_id) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    match carson_host::registry::get_or_build_pool(&st.ctx, &st.registry, &def).await {
        Ok(_) => json_ok(json!({"name": name, "version_id": version_id, "status": "updated"})),
        Err(err) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Delete an agent name pointer. Version rows and their pinned sessions are
/// kept; sessions of this name keep working against their pinned version.
#[utoipa::path(
    delete,
    path = "/api/agents/{name}",
    params(
        ("name" = String, Path, description = "Agent name")
    ),
    responses(
        (status = 200, description = "Pointer removed", body = AgentDeleteResponse),
        (status = 404, description = "Unknown agent name", body = ErrorResponse),
        (status = 500, description = "Db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn delete_agent(State(st): State<AppState>, path: AgentNamePath) -> Response {
    match st.db.delete_agent_pointer(&path.name) {
        Ok(()) => json_ok(json!({"name": path.name, "status": "deleted"})),
        Err(err) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        ),
    }
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

/// List tools as two distinct groups: bundled (immutable) and custom
/// (editable). Names are bare; identity is the `id`.
#[utoipa::path(
    get,
    path = "/api/tools",
    responses(
        (status = 200, description = "Tool list", body = ToolListResponse)
    )
)]
pub(crate) async fn list_tools(State(st): State<AppState>) -> Response {
    let builtins: Vec<Value> = carson_host::host::builtin_tools()
        .iter()
        .map(|def| serde_json::to_value(def).unwrap_or(Value::Null))
        .collect();
    let customs = match st.db.list_tools() {
        Ok(t) => t,
        Err(err) => {
            return json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("db error: {err}"),
            );
        }
    };
    let customs: Vec<Value> = customs
        .iter()
        .map(|def| serde_json::to_value(def).unwrap_or(Value::Null))
        .collect();
    json_ok(json!({
        "builtins": builtins,
        "customs": customs,
        "total": builtins.len() + customs.len(),
    }))
}

/// Register a custom tool. The wasm module is base64-encoded in the request.
/// A custom tool may reuse a bundled tool's name; agents disambiguate by id.
#[utoipa::path(
    post,
    path = "/api/tools",
    request_body = ToolReq,
    responses(
        (status = 201, description = "Tool created", body = ToolCommandResponse),
        (status = 400, description = "Invalid tool name or wasm", body = ErrorResponse),
        (status = 409, description = "A custom tool with this name already exists", body = ErrorResponse),
        (status = 500, description = "Tool compile or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn create_tool(State(st): State<AppState>, Json(req): Json<ToolReq>) -> Response {
    if let Some(reason) = invalid_tool_name(&req.name) {
        return json_err(StatusCode::BAD_REQUEST, reason);
    }
    if st
        .db
        .list_tools()
        .map(|t| t.iter().any(|d| d.name == req.name))
        .unwrap_or(false)
    {
        return json_err(
            StatusCode::CONFLICT,
            "a custom tool with this name already exists",
        );
    }
    upsert_tool_common(&st, &req, "created", None)
}

/// Update an existing custom tool by id.
#[utoipa::path(
    put,
    path = "/api/tools/{id}",
    params(
        ("id" = String, Path, description = "Tool id")
    ),
    request_body = ToolReq,
    responses(
        (status = 200, description = "Tool updated", body = ToolCommandResponse),
        (status = 400, description = "Invalid tool name or wasm", body = ErrorResponse),
        (status = 404, description = "Unknown tool id", body = ErrorResponse),
        (status = 500, description = "Tool compile or db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn update_tool(
    State(st): State<AppState>,
    path: ToolIdPath,
    Json(req): Json<ToolReq>,
) -> Response {
    if carson_host::host::builtin_by_id(&path.id).is_some() {
        return json_err(StatusCode::BAD_REQUEST, "bundled tools cannot be modified");
    }
    if st.db.get_tool(&path.id).ok().flatten().is_none() {
        return json_err(StatusCode::NOT_FOUND, "unknown tool id");
    }
    if req.name != item_name_of(&st, &path.id) {
        // Renaming must not collide with another custom tool.
        if st
            .db
            .list_tools()
            .map(|t| t.iter().any(|d| d.name == req.name && d.id != path.id))
            .unwrap_or(false)
        {
            return json_err(
                StatusCode::CONFLICT,
                "a custom tool with this name already exists",
            );
        }
    }
    upsert_tool_common(&st, &req, "updated", Some(path.id.clone()))
}

fn item_name_of(st: &AppState, id: &str) -> String {
    st.db
        .get_tool(id)
        .ok()
        .flatten()
        .map(|d| d.name)
        .unwrap_or_default()
}

/// Shared create/update body: validate, decode, persist under `id` (a fresh
/// uuid when creating), and register into the live runner.
fn upsert_tool_common(st: &AppState, req: &ToolReq, status: &str, id: Option<String>) -> Response {
    if let Some(reason) = invalid_tool_name(&req.name) {
        return json_err(StatusCode::BAD_REQUEST, reason);
    }
    let wasm = match &req.wasm_b64 {
        Some(b64) => {
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
                Ok(bytes) if !bytes.is_empty() => bytes,
                Ok(_) => {
                    return json_err(StatusCode::BAD_REQUEST, "wasm must not be empty");
                }
                Err(err) => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        &format!("invalid base64 wasm: {err}"),
                    );
                }
            }
        }
        None => {
            // Update-only: keep the stored module.
            let Some(id) = &id else {
                return json_err(StatusCode::BAD_REQUEST, "wasm_b64 is required");
            };
            match st.db.get_tool_wasm(id) {
                Ok(Some(bytes)) => bytes,
                _ => return json_err(StatusCode::NOT_FOUND, "unknown tool id"),
            }
        }
    };
    let def = ToolDef {
        id: id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        name: req.name.clone(),
        description: req.description.clone(),
        parameters: req.parameters.clone(),
        env: req.env.clone(),
    };
    let db_result = match &id {
        Some(_) => st.db.update_tool(&def, &wasm).map(|_| ()),
        None => st.db.insert_tool(&def, &wasm),
    };
    if let Err(err) = db_result {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("db error: {err}"),
        );
    }
    if let Err(err) = st.ctx.register_tool(&def, &wasm) {
        return json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("tool compile failed: {err}"),
        );
    }
    let payload = json!({"id": def.id, "name": def.name, "status": status});
    if id.is_none() {
        json_created(payload)
    } else {
        json_ok(payload)
    }
}

/// Delete a custom tool by id. Bundled tools are immutable.
#[utoipa::path(
    delete,
    path = "/api/tools/{id}",
    params(
        ("id" = String, Path, description = "Tool id")
    ),
    responses(
        (status = 200, description = "Tool deleted", body = ToolCommandResponse),
        (status = 400, description = "Bundled tools cannot be deleted", body = ErrorResponse),
        (status = 404, description = "Unknown tool id", body = ErrorResponse),
        (status = 500, description = "Db failure", body = ErrorResponse)
    )
)]
pub(crate) async fn delete_tool(State(st): State<AppState>, path: ToolIdPath) -> Response {
    if carson_host::host::builtin_by_id(&path.id).is_some() {
        return json_err(StatusCode::BAD_REQUEST, "bundled tools cannot be deleted");
    }
    match st.db.delete_tool(&path.id) {
        Ok(0) => json_err(StatusCode::NOT_FOUND, "unknown tool id"),
        Ok(_) => {
            st.ctx.remove_tool(&path.id);
            json_ok(json!({"id": path.id, "status": "deleted"}))
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
        .map(|(id, entry)| {
            json!({
                "id": id,
                "agent": entry.agent_name,
                "name": entry.name,
                "sandbox_id": entry.sandbox_id,
            })
        })
        .collect();
    json_ok(json!({"sessions": sessions, "total": sessions.len()}))
}

/// Create a new session pinned to the current version of an agent.
#[utoipa::path(
    post,
    path = "/api/sessions",
    request_body = CreateSessionReq,
    responses(
        (status = 201, description = "Session created", body = SessionCreateResponse),
        (status = 404, description = "Unknown agent name", body = ErrorResponse)
    )
)]
pub(crate) async fn create_session(
    State(st): State<AppState>,
    Json(req): Json<CreateSessionReq>,
) -> Response {
    let Some(def) = st.db.current_agent(&req.agent).ok().flatten() else {
        return json_err(StatusCode::NOT_FOUND, "unknown agent name");
    };
    let pool = match carson_host::registry::get_or_build_pool(&st.ctx, &st.registry, &def).await {
        Ok(pool) => pool,
        Err(err) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    };
    let instance = pool.next();
    let session_id = uuid::Uuid::new_v4().to_string();
    let config = pool.config();

    // Resolve the session's sandbox: link an existing one or mint a fresh
    // sandbox. The directory itself is created lazily on first tool use.
    let sandbox_id = match &req.sandbox_id {
        Some(id) if st.db.sandbox_name(id).ok().flatten().is_some() => id.clone(),
        Some(_) => return json_err(StatusCode::NOT_FOUND, "sandbox not found"),
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let name = format!("Sandbox {}", &id[..8]);
            if let Err(err) = st.db.insert_sandbox(&id, &name) {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
            }
            id
        }
    };

    instance.stop.store(false, Ordering::SeqCst);
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let result = guest
        .func_create_session()
        .call_async(&mut *store, (&session_id, &config))
        .await;
    drop(store);

    match result {
        Ok((Ok(()),)) => {
            st.ctx
                .sandbox_links
                .write()
                .unwrap()
                .insert(session_id.clone(), sandbox_id.clone());
            st.sessions.lock().await.insert(
                session_id.clone(),
                SessionEntry {
                    agent_name: def.name.clone(),
                    agent_version_id: def.id.clone(),
                    name: None,
                    sandbox_id: sandbox_id.clone(),
                    instance: instance.clone(),
                },
            );
            host::snapshot_session(&st.db, &instance, &session_id).await;
            let _ = st.db.set_session_sandbox(&session_id, &sandbox_id);
            json_created(json!({
                "session_id": session_id,
                "agent": def.name,
                "agent_version_id": def.id,
                "sandbox_id": sandbox_id,
            }))
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

/// Get a session's block log.
#[utoipa::path(
    get,
    path = "/api/sessions/{id}",
    params(
        ("id" = String, Path, description = "Session id")
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
        .call_async(&mut *store, (&id,))
        .await;
    drop(store);
    let blocks = match result {
        Ok((Ok(blocks),)) => blocks,
        _ => return json_err(StatusCode::NOT_FOUND, "session not found"),
    };
    // Model metadata derives from the pinned agent version, never stored per block.
    let model = st
        .db
        .get_agent_version(&entry.agent_version_id)
        .ok()
        .flatten()
        .map(|def| def.model);
    // Tool blocks carry their identity/payload as a JSON envelope inside the
    // content; project it back into flat fields for clients.
    let messages: Vec<Value> = blocks
        .iter()
        .map(|b| {
            let payload: Value =
                serde_json::from_str(b.text.as_deref().unwrap_or("")).unwrap_or(Value::Null);
            let text = match b.kind.as_str() {
                "tool-result" => payload["output"].as_str().map(String::from),
                _ => b.text.clone(),
            };
            json!({
                "kind": b.kind,
                "text": text,
                "tool_call_id": payload.get("id").and_then(|v| v.as_str()),
                "tool_name": payload.get("name").and_then(|v| v.as_str()),
                "arguments": payload.get("arguments").and_then(|v| v.as_str()),
                "is_error": payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                "input_tokens": b.input_tokens,
                "cache_read_tokens": b.cache_read_tokens,
                "cache_creation_tokens": b.cache_creation_tokens,
                "output_tokens": b.output_tokens,
                "created_at_ms": b.created_at_ms,
                "finished_at_ms": b.finished_at_ms,
            })
        })
        .collect();
    json_ok(json!({
        "session_id": id,
        "agent": entry.agent_name,
        "agent_version_id": entry.agent_version_id,
        "name": entry.name,
        "sandbox_id": entry.sandbox_id,
        "model": model,
        "message_count": messages.len(),
        "messages": messages,
    }))
}

/// Destroy a session.
#[utoipa::path(
    delete,
    path = "/api/sessions/{id}",
    params(
        ("id" = String, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session deleted", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse)
    )
)]
pub(crate) async fn update_session(
    State(st): State<AppState>,
    path: SessionPath,
    Json(req): Json<SessionUpdateReq>,
) -> Response {
    let id = path.id;
    let mut entry = {
        let sessions = st.sessions.lock().await;
        match sessions.get(&id) {
            Some(e) => e.clone(),
            None => return json_err(StatusCode::NOT_FOUND, "session not found"),
        }
    };
    if let Some(name) = &req.name {
        let name = if name.trim().is_empty() {
            None
        } else {
            Some(name.trim().to_string())
        };
        if st.db.set_session_name(&id, name.as_deref()).is_err() {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "failed to rename session");
        }
        entry.name = name;
    }
    if let Some(sandbox_id) = &req.sandbox_id {
        if st.db.sandbox_name(sandbox_id).ok().flatten().is_none() {
            return json_err(StatusCode::NOT_FOUND, "sandbox not found");
        }
        if st.db.set_session_sandbox(&id, sandbox_id).is_err() {
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "failed to switch sandbox");
        }
        st.ctx
            .sandbox_links
            .write()
            .unwrap()
            .insert(id.clone(), sandbox_id.clone());
        entry.sandbox_id = sandbox_id.clone();
    }
    st.sessions.lock().await.insert(id.clone(), entry.clone());
    json_ok(json!({
        "session_id": id,
        "name": entry.name,
        "sandbox_id": entry.sandbox_id,
    }))
}

/// List every sandbox in the pool.
#[utoipa::path(
    get,
    path = "/api/sandboxes",
    responses((status = 200, description = "Sandbox list"))
)]
pub(crate) async fn list_sandboxes(State(st): State<AppState>) -> Response {
    match st.db.list_sandboxes() {
        Ok(list) => json_ok(json!({"sandboxes": list, "total": list.len()})),
        Err(err) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Create a new sandbox with the given display name.
#[utoipa::path(
    post,
    path = "/api/sandboxes",
    request_body = SandboxReq,
    responses((status = 201, description = "Sandbox created"))
)]
pub(crate) async fn create_sandbox(
    State(st): State<AppState>,
    Json(req): Json<SandboxReq>,
) -> Response {
    let id = uuid::Uuid::new_v4().to_string();
    match st.db.insert_sandbox(&id, &req.name) {
        Ok(sb) => json_created(json!({"id": sb.id, "name": sb.name})),
        Err(err) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Rename a sandbox's display alias.
#[utoipa::path(
    put,
    path = "/api/sandboxes/{id}",
    request_body = SandboxReq,
    responses((status = 200, description = "Sandbox renamed"))
)]
pub(crate) async fn rename_sandbox(
    State(st): State<AppState>,
    path: SandboxPath,
    Json(req): Json<SandboxReq>,
) -> Response {
    if st.db.sandbox_name(&path.id).ok().flatten().is_none() {
        return json_err(StatusCode::NOT_FOUND, "sandbox not found");
    }
    if st.db.rename_sandbox(&path.id, &req.name).is_err() {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "failed to rename sandbox");
    }
    json_ok(json!({"id": path.id, "name": req.name}))
}

/// Destroy a session.
#[utoipa::path(
    delete,
    path = "/api/sessions/{id}",
    params(
        ("id" = String, Path, description = "Session id")
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
        .call_async(&mut *store, (&id,))
        .await;
    drop(store);
    let _ = st.db.delete_session(&id);
    st.ctx.sandbox_links.write().unwrap().remove(&id);
    json_ok(json!({"status": "deleted", "session_id": id}))
}

/// Reset a session's history.
#[utoipa::path(
    post,
    path = "/api/sessions/{id}/reset",
    params(
        ("id" = String, Path, description = "Session id")
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
        .call_async(&mut *store, (&id,))
        .await;
    drop(store);
    match result {
        Ok((Ok(()),)) => {
            host::snapshot_session(&st.db, &entry.instance, &id).await;
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
        ("id" = String, Path, description = "Session id")
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
        ("id" = String, Path, description = "Session id")
    ),
    responses(
        (status = 200, description = "Session compacted", body = SessionCommandResponse),
        (status = 404, description = "Session not found", body = ErrorResponse),
        (status = 500, description = "Compaction failed", body = ErrorResponse)
    )
)]
pub(crate) async fn compact_session(State(st): State<AppState>, path: CompactPath) -> Response {
    let id = path.id;
    let Some(pinned) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let entry = sync_session_agent(&st, &id, &pinned).await;
    let mut store = entry.instance.store.lock().await;
    let guest = entry.instance.agent.carson_agent_agent();
    let result = guest
        .func_compact_session()
        .call_async(&mut *store, (&id,))
        .await;
    drop(store);
    match result {
        Ok((Ok(()),)) => {
            host::snapshot_session(&st.db, &entry.instance, &id).await;
            json_ok(json!({"status": "compacted", "session_id": id}))
        }
        Ok((Err(carson_host::bindings::exports::carson::agent::agent::Error::NotFound),)) => {
            json_err(StatusCode::NOT_FOUND, "session not found")
        }
        _ => json_err(StatusCode::INTERNAL_SERVER_ERROR, "compaction failed"),
    }
}

/// Bring a session onto the current version of its agent name before a turn.
///
/// The session's `agent_name` resolves through the name pointer on every
/// message; if the pointer moved since the session was created, the block log
/// is snapshotted and restored onto an instance built from the new version's
/// config. When the pointer is gone (agent deleted), the pinned version keeps
/// serving so orphaned sessions still work.
async fn sync_session_agent(st: &AppState, id: &str, entry: &SessionEntry) -> SessionEntry {
    let Some(def) = st.db.current_agent(&entry.agent_name).ok().flatten() else {
        return entry.clone();
    };
    if def.id == entry.agent_version_id {
        return entry.clone();
    }
    let Ok(pool) = carson_host::registry::get_or_build_pool(&st.ctx, &st.registry, &def).await
    else {
        tracing::warn!(
            session = %id,
            version = %def.id,
            "failed to build agent pool; staying on pinned version"
        );
        return entry.clone();
    };

    host::snapshot_session(&st.db, &entry.instance, id).await;
    let persisted = st
        .db
        .load_sessions()
        .ok()
        .and_then(|sessions| sessions.into_iter().find(|p| p.id == id));
    let Some(persisted) = persisted else {
        return entry.clone();
    };

    let instance = pool.next();
    instance.stop.store(false, Ordering::SeqCst);
    if let Err(err) = host::restore_session(&instance, id, &persisted, &pool.config()).await {
        tracing::warn!(session = %id, error = %err, "agent sync failed; staying on pinned version");
        return entry.clone();
    }

    let updated = SessionEntry {
        agent_name: def.name.clone(),
        agent_version_id: def.id.clone(),
        name: entry.name.clone(),
        sandbox_id: entry.sandbox_id.clone(),
        instance,
    };
    st.sessions
        .lock()
        .await
        .insert(id.to_string(), updated.clone());
    tracing::info!(
        session = %id,
        agent = %def.name,
        version = %def.id,
        "session migrated to current agent version"
    );
    updated
}

async fn run_message_blocking(
    instance: Arc<AgentInstance>,
    session_id: String,
    content: String,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build blocking runtime");
        rt.block_on(run_message(&instance, &session_id, &content))
    })
    .await
    .unwrap_or_else(|err| Err(anyhow::anyhow!("agent task join failed: {err}")))
}

async fn run_message(
    instance: &AgentInstance,
    session_id: &str,
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

async fn session_usage(instance: &AgentInstance, session_id: &str) -> Usage {
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
        ("id" = String, Path, description = "Session id")
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
    let Some(pinned) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let entry = sync_session_agent(&st, &id, &pinned).await;
    let (tx, rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(&id, tx.clone());
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();
    let task_id = id.clone();

    tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result =
            run_message_blocking(instance.clone(), task_id.clone(), req.content.clone()).await;
        host::snapshot_session(&db, &instance, &task_id).await;
        let usage = session_usage(&instance, &task_id).await;
        if let Err(err) = result {
            let _ = hub.send(
                &task_id,
                SseItem {
                    event: "error".into(),
                    data: json!({"message": format!("agent run failed: {err:#}")}),
                },
            );
        }
        let _ = hub.send(
            &task_id,
            SseItem {
                event: "done".into(),
                data: json!({"done": true, "usage": usage}),
            },
        );
        hub.unregister(&task_id, &tx);
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
        ("id" = String, Path, description = "Session id")
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
    let Some(pinned) = st.sessions.lock().await.get(&id).cloned() else {
        return json_err(StatusCode::NOT_FOUND, "session not found");
    };
    let entry = sync_session_agent(&st, &id, &pinned).await;
    let (tx, mut rx) = mpsc::unbounded_channel::<SseItem>();
    st.hub.register(&id, tx.clone());
    let hub = st.hub.clone();
    let db = st.db.clone();
    let instance = entry.instance.clone();
    let task_id = id.clone();

    let task = tokio::spawn(async move {
        instance.stop.store(false, Ordering::SeqCst);
        let result =
            run_message_blocking(instance.clone(), task_id.clone(), req.content.clone()).await;
        host::snapshot_session(&db, &instance, &task_id).await;
        let usage = session_usage(&instance, &task_id).await;
        if let Err(err) = result {
            let _ = hub.send(
                &task_id,
                SseItem {
                    event: "error".into(),
                    data: json!({"message": format!("agent run failed: {err:#}")}),
                },
            );
        }
        let _ = hub.send(
            &task_id,
            SseItem {
                event: "done".into(),
                data: json!({"done": true, "usage": usage}),
            },
        );
        hub.unregister(&task_id, &tx);
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
            body.contains("gsk-secret"),
            "api key is echoed back so edits round-trip: {body}"
        );

        // Updating only the base URL keeps the stored key.
        let (status, _, _) = read(
            put(
                &app,
                "/api/providers/groq",
                r#"{"name":"groq","base_url":"https://changed.example","api_key":"gsk-secret"}"#,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, _, body) = read(response(app.clone(), "/api/providers").await).await;
        assert!(body.contains("changed.example"), "{body}");
        assert!(
            body.contains("gsk-secret"),
            "key survives base_url edit: {body}"
        );

        // Explicitly blanking the key clears it.
        let (status, _, _) = read(
            put(
                &app,
                "/api/providers/groq",
                r#"{"name":"groq","base_url":"https://changed.example","api_key":null}"#,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, _, body) = read(response(app.clone(), "/api/providers").await).await;
        assert!(!body.contains("gsk-secret"), "{body}");
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
    async fn custom_tool_name_charset_is_validated() {
        let app = router(app_state().await);
        let engine = base64::engine::general_purpose::STANDARD;
        let wasm =
            base64::Engine::encode(&engine, carson_host::host::embedded_tool("time").unwrap());
        for bad in ["bad.name", "", "a b", "ns/x"] {
            let (status, _, body) = read(post(
                &app,
                "/api/tools",
                &format!(
                    r#"{{"name":"{bad}","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#
                ),
            ).await)
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        }
        // A legal name passes validation (201).
        let (status, _, body) = read(post(
            &app,
            "/api/tools",
            &format!(
                r#"{{"name":"good-name_1","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#
            ),
        ).await)
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    #[tokio::test]
    async fn custom_tool_roundtrip() {
        let app = router(app_state().await);
        let engine = base64::engine::general_purpose::STANDARD;
        let wasm =
            base64::Engine::encode(&engine, carson_host::host::embedded_tool("time").unwrap());
        let (status, _, body) = read(post(
            &app,
            "/api/tools",
            &format!(r#"{{"name":"x","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#),
        ).await)
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let created: Value = serde_json::from_str(&body).unwrap();
        let id = created["id"].as_str().unwrap().to_string();

        let (status, _, body) = read(response(app.clone(), "/api/tools").await).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"x\""), "{body}");

        // Same name again conflicts.
        let (status, _, _) = read(post(
            &app,
            "/api/tools",
            &format!(r#"{{"name":"x","description":"d","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#),
        ).await)
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, _, _) = read(delete(&app, &format!("/api/tools/{id}")).await).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// A custom tool may reuse a bundled tool's bare name; agents are the
    /// disambiguation point.
    #[tokio::test]
    async fn custom_tool_may_reuse_a_bundled_name() {
        let app = router(app_state().await);
        let engine = base64::engine::general_purpose::STANDARD;
        let wasm =
            base64::Engine::encode(&engine, carson_host::host::embedded_tool("time").unwrap());
        let (status, _, body) = read(post(
            &app,
            "/api/tools",
            &format!(r#"{{"name":"time","description":"shadow","parameters":{{}},"env":{{}},"wasm_b64":"{wasm}"}}"#),
        ).await)
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
}
