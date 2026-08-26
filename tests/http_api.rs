use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use std::sync::Arc;

use carson_api::api::router;
use carson_host::config::Config;
use carson_host::db::Db;
use carson_host::drivers::EchoDriver;
use carson_host::host::{HostContext, build_registry};
use carson_host::registry::{AgentDef, ToolDef};

fn config() -> Config {
    toml::from_str("[server]\nip = \"127.0.0.1\"\nport = 8000\n").unwrap()
}

fn coder_def() -> AgentDef {
    AgentDef {
        id: uuid::Uuid::new_v4().to_string(),
        name: "coder".into(),
        system_prompt: "You are a coding agent.".into(),
        model: "mock/mock".into(),
        instances: 1,
        max_history: 40,
        context_window: 128_000,
        compaction_ratio: 0.8,
        auto_compact: true,
        capabilities: vec![carson_host::host::builtin_id("time")],
    }
}

fn time_id() -> String {
    carson_host::host::builtin_id("time")
}

fn ctx_with_fake() -> Arc<HostContext> {
    let ctx = Arc::new(HostContext::new().unwrap());
    ctx.register_driver("mock", Arc::new(EchoDriver));
    let wasm = carson_host::host::embedded_tool("time").unwrap();
    ctx.register_tool(
        &ToolDef {
            id: carson_host::host::builtin_id("time"),
            name: "time".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
            env: Default::default(),
        },
        wasm,
    )
    .unwrap();
    ctx
}

async fn app() -> Router {
    app_with_db().await.0
}

async fn app_with_db() -> (Router, Arc<Db>) {
    let config = config();
    let ctx = ctx_with_fake();
    let db = Db::open_in_memory().unwrap();
    let coder = coder_def();
    db.insert_agent_version(&coder).unwrap();
    db.set_current_agent("coder", &coder.id).unwrap();
    let registry = build_registry(&ctx, &[coder]).await.unwrap();
    (
        router(carson_host::app::build_app_state(
            ctx,
            registry,
            std::sync::Arc::clone(&db),
            config,
        )),
        db,
    )
}

async fn post(app: &Router, uri: &str, body: &str) -> (u16, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn get(app: &Router, uri: &str) -> (u16, String) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_raw(app: &Router, uri: &str, body: &str) -> (u16, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn put(app: &Router, uri: &str, body: &str) -> (u16, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn del(app: &Router, uri: &str) -> (u16, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_ids_are_uuids() {
    let app = app().await;
    let (status, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    assert_eq!(status, 201, "{created}");
    let id = created["session_id"].as_str().unwrap();
    assert_eq!(id.len(), 36, "uuid format: {id}");
    assert!(created["agent_version_id"].as_str().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_create_and_stream_roundtrip() {
    let app = app().await;
    let (_, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    let session_id = created["session_id"].as_str().unwrap();

    let (status, body) = post_raw(
        &app,
        &format!("/api/sessions/{session_id}/stream"),
        r#"{"content":"hello"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("event: chunk"), "{body}");
    assert!(body.contains("Echo: "), "{body}");
    assert!(body.contains("\"hello\""), "{body}");
    assert!(body.contains("event: done"), "{body}");
    assert!(body.contains("\"done\":true"), "{body}");
    assert!(body.contains("\"usage\""), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_returns_the_echo_reply() {
    let app = app().await;
    let (_, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    let session_id = created["session_id"].as_str().unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/sessions/{session_id}/message"),
        r#"{"content":"hello"}"#,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["response"], "Echo: hello");
    assert!(body["usage"]["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_agent_is_404() {
    let app = app().await;
    let (status, body) = post(&app, "/api/sessions", r#"{"agent":"nope"}"#).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_lifecycle_endpoints() {
    let app = app().await;
    let (_, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    let session_id = created["session_id"].as_str().unwrap();
    let uri = format!("/api/sessions/{session_id}");

    let (status, body) = get(&app, &uri).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"message_count\":0"));

    let (status, _) = post(&app, &format!("{uri}/reset"), "{}").await;
    assert_eq!(status, 200);

    let (status, _) = post(&app, &format!("{uri}/stop"), "{}").await;
    assert_eq!(status, 200);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let (status, _) = get(&app, &uri).await;
    assert_eq!(status, 404);
}

/// The block log records every message kind in stream order, with metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_session_returns_ordered_blocks_with_metadata() {
    let app = app().await;
    let (_, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    let session_id = created["session_id"].as_str().unwrap();

    let (status, _) = post(
        &app,
        &format!("/api/sessions/{session_id}/message"),
        r#"{"content":"what time is it?"}"#,
    )
    .await;
    assert_eq!(status, 200);

    let (status, body) = get(&app, &format!("/api/sessions/{session_id}")).await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["model"], "mock/mock");
    let kinds: Vec<&str> = v["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["user", "tool-use", "tool-result", "text"]);

    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(
        msgs[2]["tool_call_id"], msgs[1]["tool_call_id"],
        "result links to its call"
    );
    assert_eq!(msgs[1]["tool_name"], "time");
    assert!(msgs[1]["created_at_ms"].as_u64().unwrap() > 0);
    // The final text carries its LLM usage.
    assert!(msgs[3]["output_tokens"].as_u64().unwrap() > 0);

    // Reload via a second GET — thinking/tool data survives because it is persisted.
    let (status, body2) = get(&app, &format!("/api/sessions/{session_id}")).await;
    assert_eq!(status, 200);
    assert_eq!(body, body2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_model_requires_known_provider() {
    let app = app().await;
    let (status, body) = post(
        &app,
        "/api/agents",
        r#"{"name":"writer","model":"ghost/model","system_prompt":"x"}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_endpoint() {
    let app = app().await;
    let (status, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    assert_eq!(status, 201);
    let session_id = created["session_id"].as_str().unwrap();
    let (status, body) = post(&app, &format!("/api/sessions/{session_id}/compact"), "{}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "compacted");

    let (status, body) = post(
        &app,
        "/api/sessions/00000000-0000-0000-0000-000000000000/compact",
        "{}",
    )
    .await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_crud_via_api() {
    let app = app().await;
    let (status, created) = post(
        &app,
        "/api/agents",
        json!({"name":"writer","model":"mock/mock","system_prompt":"write","capabilities":[time_id()]})
            .to_string()
            .as_str(),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let v1 = created["version_id"].as_str().unwrap();

    let (status, body) = get(&app, "/api/agents").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"name\":\"writer\""), "{body}");
    assert!(body.contains("\"name\":\"coder\""), "{body}");

    let (status, _sess) = post(&app, "/api/sessions", r#"{"agent":"writer"}"#).await;
    assert_eq!(status, 201, "{_sess}");

    // Update creates a NEW version and repoints the name.
    let (status, updated) = put(
        &app,
        "/api/agents/writer",
        r#"{"name":"writer","model":"mock/mock","system_prompt":"new prompt","capabilities":[]}"#,
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    let v2 = updated["version_id"].as_str().unwrap();
    assert_ne!(v1, v2);

    let (_, body) = get(&app, "/api/agents").await;
    assert!(body.contains("\"system_prompt\":\"new prompt\""), "{body}");

    // History keeps both versions.
    let (status, body) = get(&app, "/api/agents/writer/versions").await;
    assert_eq!(status, 200, "{body}");
    let versions: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(versions["total"], 2);
}

/// Sessions resolve their agent by name on every turn: after the name is
/// repointed, the next message runs against the new version's config while
/// prior blocks stay in the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_follow_the_current_agent_version() {
    let app = app().await;

    let (status, created) = post(
        &app,
        "/api/agents",
        json!({"name":"writer","model":"mock/mock","system_prompt":"v1","capabilities":[time_id()]})
            .to_string()
            .as_str(),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let v1 = created["version_id"].as_str().unwrap().to_string();

    let (_, sess) = post(&app, "/api/sessions", r#"{"agent":"writer"}"#).await;
    let session_id = sess["session_id"].as_str().unwrap().to_string();
    assert_eq!(sess["agent_version_id"], json!(v1));

    // Repoint: v2 drops every capability.
    let (status, updated) = put(
        &app,
        "/api/agents/writer",
        r#"{"name":"writer","model":"mock/mock","system_prompt":"v2","capabilities":[]}"#,
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    let v2 = updated["version_id"].as_str().unwrap().to_string();
    assert_ne!(v1, v2);

    // New sessions land on v2 immediately.
    let (_, new_sess) = post(&app, "/api/sessions", r#"{"agent":"writer"}"#).await;
    assert_eq!(new_sess["agent_version_id"], json!(v2));

    // The existing session still reports its pinned version until its next
    // turn syncs it.
    let (_, view) = get(&app, &format!("/api/sessions/{session_id}")).await;
    let view: Value = serde_json::from_str(&view).unwrap();
    assert_eq!(view["agent_version_id"], json!(v1));

    // The next message migrates it onto v2 and keeps the block history.
    let (status, body) = post_raw(
        &app,
        &format!("/api/sessions/{session_id}/stream"),
        r#"{"content":"hi"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("Echo: "), "{body}");

    let (_, view) = get(&app, &format!("/api/sessions/{session_id}")).await;
    let view: Value = serde_json::from_str(&view).unwrap();
    assert_eq!(view["agent_version_id"], json!(v2));
    let kinds: Vec<&str> = view["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"user"), "history preserved: {kinds:?}");
}

/// Deleting an agent removes only the pointer; its versions and pinned
/// sessions remain fully functional.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_keeps_history_rows_and_sessions() {
    let app = app().await;
    let (status, _) = post(
        &app,
        "/api/agents",
        r#"{"name":"temp","model":"mock/mock","system_prompt":"temp"}"#,
    )
    .await;
    assert_eq!(status, 201);
    let (status, sess) = post(&app, "/api/sessions", r#"{"agent":"temp"}"#).await;
    assert_eq!(status, 201, "{sess}");
    let session_id = sess["session_id"].as_str().unwrap();

    let (status, deleted) = del(&app, "/api/agents/temp").await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["status"], "deleted");

    // The pointer is gone: no more new sessions on this name…
    let (status, _) = post(&app, "/api/sessions", r#"{"agent":"temp"}"#).await;
    assert_eq!(status, 404);

    // …and it no longer lists among current agents.
    let (_, body) = get(&app, "/api/agents").await;
    assert!(!body.contains("\"name\":\"temp\""), "{body}");

    // But the existing session keeps working against its kept version.
    let (status, body) = get(&app, &format!("/api/sessions/{session_id}")).await;
    assert_eq!(status, 200, "{body}");
    let (status, stream) = post_raw(
        &app,
        &format!("/api/sessions/{session_id}/stream"),
        r#"{"content":"hi"}"#,
    )
    .await;
    assert_eq!(status, 200);
    assert!(stream.contains("Echo: "), "{stream}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_agent_name_mismatch_rejected() {
    let app = app().await;
    let (status, body) = put(
        &app,
        "/api/agents/coder",
        r#"{"name":"other","model":"mock/mock"}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// Tools are listed as two distinct groups; bundled tools carry their
/// deterministic ids and cannot be modified or deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_are_grouped_and_bundled_tools_are_immutable() {
    let app = app().await;

    let (status, body) = get(&app, "/api/tools").await;
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    let builtins = v["builtins"].as_array().unwrap();
    assert_eq!(builtins.len(), 1);
    assert_eq!(builtins[0]["name"], "time");
    let time_tool_id = builtins[0]["id"].as_str().unwrap().to_string();
    let _ = time_tool_id;
    assert_eq!(v["customs"].as_array().unwrap().len(), 0);

    // Bundled tools are immutable (PUT carries a valid body to pass
    // extraction and reach the guard).
    let put_body = {
        use base64::Engine as _;
        json!({
            "name":"time",
            "description":"x",
            "parameters":{},
            "env":{},
            "wasm_b64": base64::engine::general_purpose::STANDARD
                .encode(carson_host::host::embedded_tool("time").unwrap())
        })
        .to_string()
    };
    let (status, body) = put(
        &app,
        &format!("/api/tools/{}", builtins[0]["id"].as_str().unwrap()),
        &put_body,
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = del(
        &app,
        &format!("/api/tools/{}", builtins[0]["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

/// Custom tools may reuse a bundled tool's name (agents disambiguate by id),
/// but duplicate custom names are rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_tools_can_shadow_builtin_names_but_not_each_other() {
    let app = app().await;
    let wasm_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(carson_host::host::embedded_tool("time").unwrap())
    };

    // Reusing the bundled name "time" is fine.
    let (status, created) = post(
        &app,
        "/api/tools",
        json!({"name":"time","description":"custom clock","parameters":{},"env":{},"wasm_b64":wasm_b64})
            .to_string()
            .as_str(),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let custom_time_id = created["id"].as_str().unwrap().to_string();
    assert_ne!(custom_time_id, carson_host::host::builtin_id("time"));

    // A second custom with the same name conflicts.
    let (status, body) = post(
        &app,
        "/api/tools",
        json!({"name":"time","description":"again","parameters":{},"env":{},"wasm_b64":wasm_b64})
            .to_string()
            .as_str(),
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // The customs array now holds it.
    let (_, body) = get(&app, "/api/tools").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["customs"].as_array().unwrap().len(), 1);

    let _ = custom_time_id;
}

/// An agent cannot select a bundled and a custom tool that share a bare name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_rejects_two_tools_with_the_same_name() {
    let app = app().await;
    let wasm_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(carson_host::host::embedded_tool("time").unwrap())
    };
    let (_, created) = post(
        &app,
        "/api/tools",
        json!({"name":"time","description":"custom clock","parameters":{},"env":{},"wasm_b64":wasm_b64})
            .to_string()
            .as_str(),
    )
    .await;
    let custom_id = created["id"].as_str().unwrap();

    let (status, body) = post(
        &app,
        "/api/agents",
        json!({
            "name":"ambiguous",
            "model":"mock/mock",
            "system_prompt":"x",
            "capabilities":[time_id(), custom_id]
        })
        .to_string()
        .as_str(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|e| e.contains("duplicate tool 'time'")),
        "expected duplicate-name error: {body}"
    );
}

/// Per-block provenance survives an agent edit: messages generated before a
/// repoint keep the old version id, only new turns stamp the new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn message_blocks_keep_their_original_agent_version() {
    let (app, db) = app_with_db().await;

    let (status, created) = post(
        &app,
        "/api/agents",
        r#"{"name":"writer","model":"mock/mock","system_prompt":"v1"}"#,
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let v1 = created["version_id"].as_str().unwrap().to_string();

    let (_, sess) = post(&app, "/api/sessions", r#"{"agent":"writer"}"#).await;
    let session_id = sess["session_id"].as_str().unwrap().to_string();

    // Turn 1 under v1.
    let (status, _) = post(
        &app,
        &format!("/api/sessions/{session_id}/message"),
        r#"{"content":"first"}"#,
    )
    .await;
    assert_eq!(status, 200);

    // Repoint to v2.
    let (_, updated) = put(
        &app,
        "/api/agents/writer",
        r#"{"name":"writer","model":"mock/mock","system_prompt":"v2"}"#,
    )
    .await;
    let v2 = updated["version_id"].as_str().unwrap().to_string();

    // Turn 2 runs on v2 and syncs the session.
    let (status, _) = post(
        &app,
        &format!("/api/sessions/{session_id}/message"),
        r#"{"content":"second"}"#,
    )
    .await;
    assert_eq!(status, 200);

    // Inspect persisted blocks through the shared DB handle.
    let sessions = db.load_sessions().unwrap();
    let stored = sessions.iter().find(|s| s.id == session_id).unwrap();
    let versions: Vec<&str> = stored
        .messages
        .iter()
        .map(|b| b.agent_version_id.as_str())
        .collect();
    let v1_count = versions.iter().filter(|v| **v == v1).count();
    let v2_count = versions.iter().filter(|v| **v == v2).count();
    assert!(v1_count >= 2, "turn-1 blocks keep v1: {versions:?}");
    assert!(v2_count >= 1, "turn-2 blocks stamped v2: {versions:?}");
}
