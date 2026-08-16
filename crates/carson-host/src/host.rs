use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

use crate::bindings::AgentWorld;
use crate::db::{Db, PersistedSession, StoredMessage};
use crate::drivers::{LlmDriver, OpenAiCompatDriver, Usage};
use crate::hub::Hub;
use crate::registry::{AgentDef, AgentInstance, AgentPool, AgentRegistry, ProviderDef, ToolDef};
use crate::state::State;
use crate::tools::{Capabilities, ToolRunner};

/// The single agent module, baked into the binary by `build.rs`.
pub const EMBEDDED_AGENT: &[u8] = include_bytes!(env!("CARSON_AGENT_WASM"));

/// Built-in tool components, baked into the binary by `build.rs`.
pub const EMBEDDED_TIME_TOOL: &[u8] = include_bytes!(env!("CARSON_TOOL_TIME_WASM"));
pub const EMBEDDED_ECHO_TOOL: &[u8] = include_bytes!(env!("CARSON_TOOL_ECHO_WASM"));

/// Returns the embedded bytes for a built-in tool, if any.
pub fn embedded_tool(name: &str) -> Option<&'static [u8]> {
    match name {
        "time" => Some(EMBEDDED_TIME_TOOL),
        "echo" => Some(EMBEDDED_ECHO_TOOL),
        _ => None,
    }
}

/// Shared, immutable state used to build every agent instance. Providers and tools are registered
/// at runtime via the API; the shared maps are the live source of truth.
pub struct HostContext {
    pub engine: Engine,
    pub component: Component,
    pub hub: Arc<Hub>,
    pub drivers: Arc<RwLock<HashMap<String, Arc<dyn LlmDriver>>>>,
    pub tool_runner: Arc<ToolRunner>,
}

impl HostContext {
    pub fn new() -> Result<Self> {
        let engine = Engine::new(&wasmtime::Config::new())?;
        let component = Component::new(&engine, EMBEDDED_AGENT)?;
        let hub = Hub::new();
        let tool_runner = Arc::new(ToolRunner::new(&engine));
        Ok(Self {
            engine,
            component,
            hub,
            drivers: Arc::new(RwLock::new(HashMap::new())),
            tool_runner,
        })
    }

    pub fn register_driver(&self, name: &str, driver: Arc<dyn LlmDriver>) {
        self.drivers
            .write()
            .unwrap()
            .insert(name.to_string(), driver);
    }

    pub fn remove_driver(&self, name: &str) -> bool {
        self.drivers.write().unwrap().remove(name).is_some()
    }

    pub fn has_driver(&self, name: &str) -> bool {
        self.drivers.read().unwrap().contains_key(name)
    }

    pub fn register_tool(&self, def: &ToolDef, wasm: &[u8]) -> Result<()> {
        self.tool_runner.register(def, wasm)
    }

    pub fn remove_tool(&self, name: &str) -> bool {
        self.tool_runner.remove(name)
    }
}

/// Build the runtime driver for a persisted provider (always OpenAI-compatible).
pub fn openai_driver(def: &ProviderDef) -> Result<Arc<dyn LlmDriver>> {
    let api_key = def
        .api_key_env
        .as_ref()
        .and_then(|env| std::env::var(env).ok())
        .unwrap_or_default();
    Ok(Arc::new(OpenAiCompatDriver {
        base_url: def.base_url.clone(),
        api_key,
    }))
}

pub async fn build_registry(ctx: &HostContext, agents: &[AgentDef]) -> Result<AgentRegistry> {
    let mut registry = AgentRegistry::new();
    for def in agents {
        registry.insert(def.kind.clone(), build_pool(ctx, def).await?);
    }
    Ok(registry)
}

pub async fn build_pool(ctx: &HostContext, def: &AgentDef) -> Result<AgentPool> {
    let count = def.instances.max(1);
    let mut instances = Vec::with_capacity(count);
    for _ in 0..count {
        instances.push(Arc::new(build_instance(ctx, def).await?));
    }
    Ok(AgentPool::from_def(def, instances))
}

/// Snapshot a session from the guest and persist it to the database.
pub async fn snapshot_session(db: &Arc<Db>, instance: &AgentInstance, session_id: u64) {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let result = guest
        .func_session_state()
        .call_async(&mut *store, (session_id,))
        .await;
    drop(store);
    let Ok((Ok(state),)) = result else {
        return;
    };
    let messages: Vec<StoredMessage> = state.messages.iter().map(StoredMessage::from).collect();
    let persisted = PersistedSession {
        id: session_id,
        kind: instance.kind.clone(),
        summary: state.summary,
        usage: Usage {
            input_tokens: state.usage.input_tokens,
            cache_read_tokens: state.usage.cache_read_tokens,
            cache_creation_tokens: state.usage.cache_creation_tokens,
            output_tokens: state.usage.output_tokens,
        },
        messages,
    };
    let _ = db.upsert_session(&persisted);
}

/// Restore a persisted session into an agent instance.
pub async fn restore_session(
    instance: &AgentInstance,
    session_id: u64,
    persisted: &PersistedSession,
    config: &crate::bindings::exports::carson::agent::agent::SessionConfig,
) -> Result<()> {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let state = crate::bindings::exports::carson::agent::agent::State {
        messages: persisted
            .messages
            .iter()
            .cloned()
            .map(crate::bindings::carson::agent::llm::Message::from)
            .collect(),
        summary: persisted.summary.clone(),
        usage: crate::bindings::carson::agent::llm::Usage {
            input_tokens: persisted.usage.input_tokens,
            cache_read_tokens: persisted.usage.cache_read_tokens,
            cache_creation_tokens: persisted.usage.cache_creation_tokens,
            output_tokens: persisted.usage.output_tokens,
        },
    };
    let (result,) = guest
        .func_restore_session()
        .call_async(&mut *store, (session_id, config, &state))
        .await?;
    result.map_err(|err| anyhow::anyhow!("agent error: {err:?}"))
}

pub async fn build_instance(ctx: &HostContext, def: &AgentDef) -> Result<AgentInstance> {
    let mut linker: Linker<State> = Linker::new(&ctx.engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    AgentWorld::add_to_linker::<State, wasmtime::component::HasSelf<State>>(
        &mut linker,
        |s: &mut State| s,
    )?;

    let wasi = WasiCtxBuilder::new().inherit_stdio().build();
    let state = State {
        wasi,
        table: ResourceTable::new(),
        hub: ctx.hub.clone(),
        drivers: ctx.drivers.clone(),
        tool_runner: ctx.tool_runner.clone(),
        caps: Capabilities::from_names(def.capabilities.clone()),
        stop: Arc::new(AtomicBool::new(false)),
        streams: HashMap::new(),
        next_stream_id: 0,
    };
    let mut store = Store::new(&ctx.engine, state);
    let agent = AgentWorld::instantiate_async(&mut store, &ctx.component, &linker).await?;
    let stop = store.data().stop.clone();
    Ok(AgentInstance {
        kind: def.kind.clone(),
        store: tokio::sync::Mutex::new(store),
        agent,
        stop,
    })
}
