use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
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
        kind: "coder".into(),
        system_prompt: "You are a coding agent.".into(),
        model: "mock/mock".into(),
        instances: 1,
        max_history: 40,
        context_window: 128_000,
        compaction_ratio: 0.8,
        auto_compact: true,
        capabilities: vec!["core/time".into(), "core/echo".into()],
    }
}

fn ctx_with_fake() -> Arc<HostContext> {
    let ctx = Arc::new(HostContext::new().unwrap());
    ctx.register_driver("mock", Arc::new(EchoDriver));
    for name in ["time", "echo"] {
        let wasm = carson_host::host::embedded_tool(name).unwrap();
        ctx.register_tool(
            &ToolDef {
                name: format!("core/{name}"),
                description: String::new(),
                parameters: serde_json::json!({}),
                env: Default::default(),
            },
            wasm,
        )
        .unwrap();
    }
    ctx
}

async fn app() -> Router {
    let config = config();
    let ctx = ctx_with_fake();
    let registry = build_registry(&ctx, &[coder_def()]).await.unwrap();
    let db = Db::open_in_memory().unwrap();
    router(carson_host::app::build_app_state(ctx, registry, db, config))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_create_and_stream_roundtrip() {
    let app = app().await;
    let (status, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    assert_eq!(status, 201, "{created}");
    let session_id = created["session_id"].as_u64().unwrap();

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
    let session_id = created["session_id"].as_u64().unwrap();

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
    let session_id = created["session_id"].as_u64().unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_model_requires_known_provider() {
    let app = app().await;
    let (status, body) = post(
        &app,
        "/api/agents",
        r#"{"kind":"writer","model":"ghost/model","system_prompt":"x"}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_endpoint() {
    let app = app().await;
    let (status, created) = post(&app, "/api/sessions", r#"{"agent":"coder"}"#).await;
    assert_eq!(status, 201);
    let session_id = created["session_id"].as_u64().unwrap();
    let (status, body) = post(&app, &format!("/api/sessions/{session_id}/compact"), "{}").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "compacted");

    let (status, body) = post(&app, "/api/sessions/9999/compact", "{}").await;
    assert_eq!(status, 404, "{body}");
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
async fn agent_crud_via_api() {
    let app = app().await;
    let (status, created) = post(
        &app,
        "/api/agents",
        r#"{"kind":"writer","model":"mock/mock","system_prompt":"write","capabilities":["core/time"]}"#,
    )
    .await;
    assert_eq!(status, 201, "{created}");

    let (status, body) = get(&app, "/api/agents").await;
    assert_eq!(status, 200);
    assert!(body.contains("\"kind\":\"writer\""), "{body}");
    assert!(body.contains("\"kind\":\"coder\""), "{body}");

    let (status, _sess) = post(&app, "/api/sessions", r#"{"agent":"writer"}"#).await;
    assert_eq!(status, 201, "{_sess}");

    let (status, updated) = put(
        &app,
        "/api/agents/writer",
        r#"{"kind":"writer","model":"mock/mock","system_prompt":"new prompt","capabilities":[]}"#,
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["status"], "updated");

    let (_, body) = get(&app, "/api/agents").await;
    assert!(body.contains("\"system_prompt\":\"new prompt\""), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_agent_cascades_sessions() {
    let app = app().await;
    let (status, _) = post(
        &app,
        "/api/agents",
        r#"{"kind":"temp","model":"mock/mock","system_prompt":"temp"}"#,
    )
    .await;
    assert_eq!(status, 201);
    let (status, sess) = post(&app, "/api/sessions", r#"{"agent":"temp"}"#).await;
    assert_eq!(status, 201, "{sess}");
    let session_id = sess["session_id"].as_u64().unwrap();

    let (status, deleted) = del(&app, "/api/agents/temp").await;
    assert_eq!(status, 200, "{deleted}");
    assert_eq!(deleted["sessions_deleted"], 1);

    let (status, _) = get(&app, &format!("/api/sessions/{session_id}")).await;
    assert_eq!(status, 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_agent_kind_mismatch_rejected() {
    let app = app().await;
    let (status, body) = put(
        &app,
        "/api/agents/coder",
        r#"{"kind":"other","model":"mock/mock"}"#,
    )
    .await;
    assert_eq!(status, 400, "{body}");
}
