use std::sync::Arc;

use carson_host::bindings::exports::carson::agent::agent::SessionConfig;
use carson_host::db::Db;
use carson_host::drivers::EchoDriver;
use carson_host::host::{HostContext, build_instance, restore_session, snapshot_session};
use carson_host::hub::{Hub, SseItem};
use carson_host::registry::{AgentDef, AgentInstance, ToolDef};

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

fn def() -> AgentDef {
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

fn session_config(def: &AgentDef) -> SessionConfig {
    SessionConfig {
        system_prompt: def.system_prompt.clone(),
        model: def.model.clone(),
        capabilities_json: "[]".into(),
        max_history: def.max_history as u32,
        context_window: def.context_window as u32,
        compaction_ratio: def.compaction_ratio,
        auto_compact: def.auto_compact,
    }
}

async fn make_instance_with(
    def: &AgentDef,
) -> (Arc<HostContext>, Arc<AgentInstance>, SessionConfig) {
    let ctx = ctx_with_fake();
    let instance = Arc::new(build_instance(&ctx, def).await.unwrap());
    (ctx, instance, session_config(def))
}

async fn make_instance() -> (Arc<HostContext>, Arc<AgentInstance>, SessionConfig) {
    make_instance_with(&def()).await
}

async fn create_session(instance: &AgentInstance, id: u64, config: &SessionConfig) {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_create_session()
        .call_async(&mut *store, (id, config))
        .await
        .unwrap();
    result.unwrap();
}

async fn send_message(instance: &AgentInstance, hub: &Arc<Hub>, id: u64, content: &str) {
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
    let (ctx, instance, cfg) = make_instance().await;
    create_session(&instance, 1, &cfg).await;
    let hub = ctx.hub.clone();
    send_message(&instance, &hub, 1, "hello world").await;
    snapshot_session(&db, &instance, 1).await;

    let loaded = db.load_sessions().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, 1);
    assert_eq!(loaded[0].messages.len(), 2, "user + assistant");
    assert_eq!(
        loaded[0].messages[0].content.as_deref(),
        Some("hello world")
    );
    assert!(loaded[0].usage.output_tokens > 0, "usage persisted");
    assert!(loaded[0].usage.input_tokens > 0, "usage persisted");

    drop(instance);
    let (_ctx2, instance2, cfg2) = make_instance().await;
    restore_session(&instance2, 1, &loaded[0], &cfg2)
        .await
        .unwrap();

    let mut store = instance2.store.lock().await;
    let guest = instance2.agent.carson_agent_agent();
    let (history,) = guest
        .func_session_history()
        .call_async(&mut *store, (1,))
        .await
        .unwrap();
    let (state,) = guest
        .func_session_state()
        .call_async(&mut *store, (1,))
        .await
        .unwrap();
    drop(store);

    let messages = history.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].content.as_deref(), Some("hello world"));
    assert_eq!(messages[1].content.as_deref(), Some("Echo: hello world"));

    let state = state.unwrap();
    assert_eq!(state.usage.input_tokens, loaded[0].usage.input_tokens);
    assert_eq!(state.usage.output_tokens, loaded[0].usage.output_tokens);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_is_restored() {
    let db = Db::open_in_memory().unwrap();
    let mut small = def();
    small.max_history = 4;
    let (ctx, instance, cfg) = make_instance_with(&small).await;
    create_session(&instance, 2, &cfg).await;
    snapshot_session(&db, &instance, 2).await;
    // Force a summary via compaction after a message.
    let hub = ctx.hub.clone();
    let long = "lorem ipsum dolor sit amet ".repeat(20);
    for _ in 0..4 {
        send_message(&instance, &hub, 2, &long).await;
    }
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_compact_session()
        .call_async(&mut *store, (2,))
        .await
        .unwrap();
    result.unwrap();
    drop(store);
    snapshot_session(&db, &instance, 2).await;

    let loaded = db.load_sessions().unwrap();
    let session = loaded.iter().find(|s| s.id == 2).unwrap();
    assert!(session.summary.is_some(), "summary persisted");

    drop(instance);
    let (_ctx2, instance2, cfg2) = make_instance().await;
    restore_session(&instance2, 2, session, &cfg2)
        .await
        .unwrap();
    let mut store = instance2.store.lock().await;
    let guest = instance2.agent.carson_agent_agent();
    let (state,) = guest
        .func_session_state()
        .call_async(&mut *store, (2,))
        .await
        .unwrap();
    drop(store);
    assert!(state.unwrap().summary.is_some(), "summary restored");
}
