use std::sync::Arc;

use carson_host::db::{Db, StoredBlock};
use carson_host::drivers::EchoDriver;
use carson_host::host::{HostContext, build_instance, restore_session, snapshot_session};
use carson_host::hub::{Hub, SseItem};
use carson_host::registry::{AgentDef, AgentInstance, ToolDef};

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

fn def() -> AgentDef {
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

async fn make_instance_with(def: &AgentDef) -> (Arc<HostContext>, Arc<AgentInstance>) {
    let ctx = ctx_with_fake();
    let instance = Arc::new(build_instance(&ctx, def).await.unwrap());
    (ctx, instance)
}

async fn make_instance() -> (Arc<HostContext>, Arc<AgentInstance>) {
    make_instance_with(&def()).await
}

async fn create_session(instance: &AgentInstance, id: &str, def: &AgentDef) {
    let config = crate_config(def);
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_create_session()
        .call_async(&mut *store, (id, &config))
        .await
        .unwrap();
    result.unwrap();
}

fn crate_config(
    def: &AgentDef,
) -> carson_host::bindings::exports::carson::agent::agent::SessionConfig {
    use carson_host::bindings::exports::carson::agent::agent::SessionConfig;
    SessionConfig {
        agent_version_id: def.id.clone(),
        system_prompt: def.system_prompt.clone(),
        model: def.model.clone(),
        capabilities_json: serde_json::json!(def.capabilities).to_string(),
        max_history: def.max_history as u32,
        context_window: def.context_window as u32,
        compaction_ratio: def.compaction_ratio,
        auto_compact: def.auto_compact,
    }
}

async fn send_message(instance: &AgentInstance, hub: &Arc<Hub>, id: &str, content: &str) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<SseItem>();
    hub.register(id, tx.clone());
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_handle_message()
        .call_async(&mut *store, (id, content))
        .await
        .unwrap();
    result.unwrap();
    drop(store);
    hub.unregister(id, &tx);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_persists_and_restores() {
    let db = Db::open_in_memory().unwrap();
    let agent = def();
    let (ctx, instance) = make_instance_with(&agent).await;
    create_session(&instance, "s-1", &agent).await;
    let hub = ctx.hub.clone();
    send_message(&instance, &hub, "s-1", "hello world").await;
    snapshot_session(&db, &instance, "s-1").await;

    let loaded = db.load_sessions().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, "s-1");
    assert_eq!(loaded[0].agent_name, "coder");
    assert_eq!(loaded[0].agent_version_id, agent.id);
    // user block + assistant text block
    assert_eq!(loaded[0].messages.len(), 2);
    assert_eq!(loaded[0].messages[0].kind, "user");
    assert_eq!(loaded[0].messages[0].text.as_deref(), Some("hello world"));
    assert_eq!(loaded[0].messages[1].kind, "text");
    assert_eq!(
        loaded[0].messages[1].text.as_deref(),
        Some("Echo: hello world")
    );
    assert!(loaded[0].usage.output_tokens > 0, "usage persisted");
    assert!(loaded[0].usage.input_tokens > 0, "usage persisted");

    drop(instance);
    let (_ctx2, instance2) = make_instance().await;
    restore_session(&instance2, "s-1", &loaded[0], &crate_config(&agent))
        .await
        .unwrap();

    let mut store = instance2.store.lock().await;
    let guest = instance2.agent.carson_agent_agent();
    let (history,) = guest
        .func_session_history()
        .call_async(&mut *store, ("s-1",))
        .await
        .unwrap();
    let (state,) = guest
        .func_session_state()
        .call_async(&mut *store, ("s-1",))
        .await
        .unwrap();
    drop(store);

    let blocks = history.unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].kind, "user");
    assert_eq!(blocks[0].text.as_deref(), Some("hello world"));
    assert_eq!(blocks[1].text.as_deref(), Some("Echo: hello world"));

    let state = state.unwrap();
    assert_eq!(state.usage.input_tokens, loaded[0].usage.input_tokens);
    assert_eq!(state.usage.output_tokens, loaded[0].usage.output_tokens);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_is_restored() {
    let db = Db::open_in_memory().unwrap();
    let mut small = def();
    small.max_history = 4;
    let (ctx, instance) = make_instance_with(&small).await;
    create_session(&instance, "s-2", &small).await;
    snapshot_session(&db, &instance, "s-2").await;
    // Force a summary via compaction after a message.
    let hub = ctx.hub.clone();
    let long = "lorem ipsum dolor sit amet ".repeat(20);
    for _ in 0..4 {
        send_message(&instance, &hub, "s-2", &long).await;
    }
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_compact_session()
        .call_async(&mut *store, ("s-2",))
        .await
        .unwrap();
    result.unwrap();
    drop(store);
    snapshot_session(&db, &instance, "s-2").await;

    let loaded = db.load_sessions().unwrap();
    let session = loaded.iter().find(|s| s.id == "s-2").unwrap();
    assert!(session.summary.is_some(), "summary persisted");

    drop(instance);
    let (_ctx2, instance2) = make_instance().await;
    restore_session(&instance2, "s-2", session, &crate_config(&small))
        .await
        .unwrap();
    let mut store = instance2.store.lock().await;
    let guest = instance2.agent.carson_agent_agent();
    let (state,) = guest
        .func_session_state()
        .call_async(&mut *store, ("s-2",))
        .await
        .unwrap();
    drop(store);
    assert!(state.unwrap().summary.is_some(), "summary restored");
}

/// A tool-calling turn must persist thinking/text/tool-use/tool-result blocks
/// in stream order with metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_turn_persists_ordered_blocks() {
    let db = Db::open_in_memory().unwrap();
    let agent = def();
    let (ctx, instance) = make_instance_with(&agent).await;
    create_session(&instance, "s-3", &agent).await;
    let hub = ctx.hub.clone();
    send_message(&instance, &hub, "s-3", "what is the time?").await;
    snapshot_session(&db, &instance, "s-3").await;

    let loaded = db.load_sessions().unwrap();
    let session = loaded.iter().find(|s| s.id == "s-3").unwrap();
    let kinds: Vec<&str> = session.messages.iter().map(|b| b.kind.as_str()).collect();
    assert!(
        kinds == ["user", "tool-use", "tool-result", "text"]
            || kinds == ["user", "text", "tool-use", "tool-result"],
        "unexpected order: {kinds:?}"
    );
    let call = session
        .messages
        .iter()
        .find(|b| b.kind == "tool-use")
        .unwrap();
    let call_payload: serde_json::Value =
        serde_json::from_str(call.text.as_deref().unwrap()).unwrap();
    assert_eq!(call_payload["name"], "time");
    assert_eq!(call_payload["arguments"], "{}");
    assert_eq!(call.agent_version_id, agent.id, "provenance stamped");

    let result = session
        .messages
        .iter()
        .find(|b| b.kind == "tool-result")
        .unwrap();
    let result_payload: serde_json::Value =
        serde_json::from_str(result.text.as_deref().unwrap()).unwrap();
    assert_eq!(result_payload["id"], call_payload["id"]);
    assert!(
        result_payload["output"].as_str().unwrap().contains("time"),
        "result output: {}",
        result_payload["output"]
    );
    // Assistant-side blocks carry the LLM usage of their turn.
    assert!(call.input_tokens > 0 || call.output_tokens > 0);
}

/// StoredBlock round-trips through the DB with its metadata intact.
#[test]
fn stored_block_metadata_roundtrip() {
    let block = StoredBlock {
        agent_version_id: "v9".into(),
        kind: "thinking".into(),
        text: Some("hmm".into()),
        input_tokens: 11,
        cache_read_tokens: 3,
        cache_creation_tokens: 1,
        output_tokens: 7,
        created_at_ms: 1234,
        finished_at_ms: 5678,
    };
    let wit: carson_host::bindings::exports::carson::agent::agent::Block =
        carson_host::bindings::exports::carson::agent::agent::Block::from(&block);
    assert_eq!(wit.created_at_ms, 1234);
    assert_eq!(wit.finished_at_ms, 5678);
    let back = StoredBlock::from(&wit);
    assert_eq!(back.input_tokens, 11);
    assert_eq!(back.cache_read_tokens, 3);
    assert_eq!(back.agent_version_id, "v9");
}
