use std::sync::Arc;

use carson_host::bindings::exports::carson::agent::agent::{Error, SessionConfig};
use carson_host::drivers::EchoDriver;
use carson_host::host::{HostContext, build_registry};
use carson_host::hub::{Hub, SseItem};
use carson_host::registry::{AgentDef, AgentInstance, AgentRegistry, ToolDef};

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

async fn setup() -> (Arc<Hub>, Arc<AgentRegistry>, Arc<AgentInstance>) {
    setup_with(&[coder_def()]).await
}

async fn setup_with(agents: &[AgentDef]) -> (Arc<Hub>, Arc<AgentRegistry>, Arc<AgentInstance>) {
    let coder = &agents[0];
    let ctx = ctx_with_fake();
    let registry = build_registry(&ctx, agents).await.unwrap();
    let pool = registry.get(&coder.id).expect("coder pool");
    let instance = pool.next();
    (ctx.hub.clone(), Arc::new(registry), instance)
}

async fn create_session_for(instance: &AgentInstance, session_id: &str, agent: &AgentDef) {
    let config = SessionConfig {
        agent_version_id: agent.id.clone(),
        system_prompt: agent.system_prompt.clone(),
        model: agent.model.clone(),
        capabilities_json: "[]".into(),
        max_history: agent.max_history as u32,
        context_window: agent.context_window as u32,
        compaction_ratio: agent.compaction_ratio,
        auto_compact: agent.auto_compact,
    };
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_create_session()
        .call_async(&mut *store, (session_id, &config))
        .await
        .unwrap();
    result.unwrap();
}

async fn create_session(instance: &AgentInstance, session_id: &str) {
    create_session_for(instance, session_id, &coder_def()).await;
}

async fn send_message(
    instance: &AgentInstance,
    hub: &Arc<Hub>,
    session_id: &str,
    content: &str,
) -> Vec<SseItem> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SseItem>();
    hub.register(session_id, tx.clone());
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_handle_message()
        .call_async(&mut *store, (session_id, content))
        .await
        .unwrap();
    result.unwrap();
    drop(store);
    hub.unregister(session_id, &tx);

    let mut items = Vec::new();
    while let Ok(item) = rx.try_recv() {
        items.push(item);
    }
    items
}

fn chunk_text(items: &[SseItem]) -> String {
    items
        .iter()
        .filter(|item| item.event == "chunk")
        .filter_map(|item| item.data.as_str())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_streams_the_echo_reply() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "1").await;
    let items = send_message(&instance, &_hub, "1", "hello").await;
    assert_eq!(chunk_text(&items), "Echo: hello");
    assert!(!items.iter().any(|i| i.event == "thinking"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_invokes_time_and_continues() {
    let (hub, _registry, instance) = setup().await;
    create_session(&instance, "2").await;
    let items = send_message(&instance, &hub, "2", "what time is it?").await;

    let tool_use = items
        .iter()
        .filter(|i| i.event == "tool_use")
        .filter_map(|i| i.data.as_str())
        .collect::<Vec<_>>();
    let tool_result = items
        .iter()
        .filter(|i| i.event == "tool_result")
        .filter_map(|i| i.data.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_use.len(), 1);
    assert!(tool_use[0].contains("\"name\":\"time\""), "{}", tool_use[0]);
    assert_eq!(tool_result.len(), 1);
    let payload: serde_json::Value =
        serde_json::from_str::<serde_json::Value>(tool_result[0]).expect("tool_result json");
    let preview = payload["result_preview"].as_str().expect("preview");
    let parsed: serde_json::Value =
        serde_json::from_str(preview).expect("preview carries the tool's JSON output");
    assert!(
        parsed["time"]
            .as_str()
            .is_some_and(|t| t.len() == 24 && t.ends_with('Z')),
        "expected ISO 8601 time in {preview}"
    );
    assert_eq!(chunk_text(&items), "Echo: what time is it?");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_records_the_conversation() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "3").await;
    let _ = send_message(&instance, &_hub, "3", "hi").await;

    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_session_history()
        .call_async(&mut *store, ("3",))
        .await
        .unwrap();
    drop(store);
    let blocks = result.unwrap();
    let kinds: Vec<_> = blocks.iter().map(|b| b.kind.as_str()).collect();
    assert_eq!(kinds, ["user", "text"]);
    assert_eq!(blocks[1].text.as_deref(), Some("Echo: hi"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_clears_history() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "4").await;
    let _ = send_message(&instance, &_hub, "4", "hi").await;

    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_reset_session()
        .call_async(&mut *store, ("4",))
        .await
        .unwrap();
    let (history,) = guest
        .func_session_history()
        .call_async(&mut *store, ("4",))
        .await
        .unwrap();
    drop(store);
    result.unwrap();
    assert!(history.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destroy_removes_the_session() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "5").await;

    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (destroy,) = guest
        .func_destroy_session()
        .call_async(&mut *store, ("5",))
        .await
        .unwrap();
    destroy.unwrap();
    let (message,) = guest
        .func_handle_message()
        .call_async(&mut *store, ("5", "hi"))
        .await
        .unwrap();
    drop(store);
    assert_eq!(message, Err(Error::NotFound));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_flag_aborts_a_turn() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "6").await;
    let mut store = instance.store.lock().await;
    store
        .data()
        .stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_handle_message()
        .call_async(&mut *store, ("6", "hi"))
        .await
        .unwrap();
    drop(store);
    assert_eq!(result, Ok(()));
}

fn compaction_def(max_history: usize, context_window: usize, auto_compact: bool) -> AgentDef {
    AgentDef {
        id: uuid::Uuid::new_v4().to_string(),
        name: "coder".into(),
        system_prompt: "You are a coding agent.".into(),
        model: "mock/mock".into(),
        instances: 1,
        max_history,
        context_window,
        compaction_ratio: 0.8,
        auto_compact,
        capabilities: vec![carson_host::host::builtin_id("time")],
    }
}

async fn history_len(instance: &AgentInstance, session_id: &str) -> usize {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_session_history()
        .call_async(&mut *store, (session_id,))
        .await
        .unwrap();
    drop(store);
    result.unwrap().len()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compaction_trims_history() {
    let def = compaction_def(4, 100, true);
    let (hub, _registry, instance) = setup_with(std::slice::from_ref(&def)).await;
    create_session_for(&instance, "10", &def).await;
    let long = "lorem ipsum dolor sit amet ".repeat(20);
    let mut saw_compacted = false;
    for _ in 0..6 {
        let items = send_message(&instance, &hub, "10", &long).await;
        if items.iter().any(|i| {
            i.event == "status" && i.data.as_str().is_some_and(|s| s.contains("compacted"))
        }) {
            saw_compacted = true;
        }
    }
    assert!(saw_compacted, "expected a compacted status event");
    assert!(
        history_len(&instance, "10").await <= 8,
        "history was not bounded by compaction"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compaction_trims_history() {
    let def = compaction_def(4, 128_000, false);
    let (hub, _registry, instance) = setup_with(std::slice::from_ref(&def)).await;
    create_session_for(&instance, "11", &def).await;
    for _ in 0..6 {
        let _ = send_message(&instance, &hub, "11", "short message").await;
    }
    let before = history_len(&instance, "11").await;
    assert!(before > 4, "manual mode should accumulate history");

    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (result,) = guest
        .func_compact_session()
        .call_async(&mut *store, ("11",))
        .await
        .unwrap();
    drop(store);
    result.unwrap();
    assert_eq!(history_len(&instance, "11").await, 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_usage_reports_real_tokens() {
    let (_hub, _registry, instance) = setup().await;
    create_session(&instance, "12").await;
    let _ = send_message(&instance, &_hub, "12", "hello world").await;
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let (usage,) = guest
        .func_session_usage()
        .call_async(&mut *store, ("12",))
        .await
        .unwrap();
    drop(store);
    let usage = usage.unwrap();
    assert!(usage.input_tokens > 0);
    assert!(usage.output_tokens > 0);
}
