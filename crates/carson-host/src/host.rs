use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::WasiCtxBuilder;

use crate::bindings::AgentWorld;
use crate::db::{Db, PersistedSession, StoredBlock};
use crate::drivers::{LlmDriver, OpenAiCompatDriver, Usage};

/// Host wall-clock time in milliseconds since the epoch.
pub fn ms_since_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
use crate::hub::Hub;
use crate::registry::{AgentDef, AgentInstance, AgentPool, AgentRegistry, ProviderDef, ToolDef};
use crate::state::State;
use crate::tools::{Capabilities, ToolRunner};

/// The single agent module, baked into the binary by `build.rs`.
pub const EMBEDDED_AGENT: &[u8] = include_bytes!(env!("CARSON_AGENT_WASM"));

/// Built-in tool component, baked into the binary by `build.rs`.
pub const EMBEDDED_TIME_TOOL: &[u8] = include_bytes!(env!("CARSON_TOOL_TIME_WASM"));

/// The bash interpreter component and its coreutils runner.
pub const EMBEDDED_BASH_TOOL: &[u8] = include_bytes!(env!("CARSON_TOOL_BASH_WASM"));
pub const EMBEDDED_COREUTILS: &[u8] = include_bytes!(env!("CARSON_TOOL_COREUTILS_WASM"));

/// Returns the embedded bytes for a built-in tool, if any.
pub fn embedded_tool(name: &str) -> Option<&'static [u8]> {
    match name {
        "time" => Some(EMBEDDED_TIME_TOOL),
        "bash" => Some(EMBEDDED_BASH_TOOL),
        _ => None,
    }
}

/// Stable namespace for deterministic built-in tool ids (uuid v5).
pub const TOOL_ID_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x6361_7273_6f6e_5f74_6f6f_6c5f_6964_7301);

/// The deterministic id of a built-in tool by its bare name.
pub fn builtin_id(name: &str) -> String {
    uuid::Uuid::new_v5(&TOOL_ID_NAMESPACE, name.as_bytes()).to_string()
}

/// The bundled tools: code-defined, seeded into the runner at startup,
/// immutable through the API. Names are bare and provider-safe.
pub fn builtin_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            id: builtin_id("time"),
            name: "time".into(),
            description: "Return the current UTC time in ISO 8601 format".into(),
            parameters: serde_json::json!({"type": "object"}),
            env: Default::default(),
        },
        ToolDef {
            id: builtin_id("bash"),
            name: "bash".into(),
            description: concat!(
                "Run bash scripts in a sandboxed virtual filesystem rooted at / with ",
                "/bin (coreutils commands, runnable via /bin/<cmd> or PATH=/bin), ",
                "/tmp and /home/carson (the home, also the default cwd). ",
                "Builtins: cd, pwd, echo, printf, export, unset, set, env, test, true, false, exit. ",
                "Commands: most GNU coreutils (ls, cat, cp, mv, rm, mkdir, touch, date, ",
                "head, tail, sort, wc, ...). Arguments: {command, cwd?, env?}."
            )
            .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "A bash one-liner or multi-line script",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory relative to the sandbox root",
                    },
                    "env": {
                        "type": "object",
                        "description": "Extra environment variables for the script",
                    },
                },
                "required": ["command"],
            }),
            env: Default::default(),
        },
    ]
}

/// Look up a built-in tool by its deterministic id.
pub fn builtin_by_id(id: &str) -> Option<ToolDef> {
    builtin_tools().into_iter().find(|t| t.id == id)
}

/// Shared, immutable state used to build every agent instance. Providers and tools are registered
/// at runtime via the API; the shared maps are the live source of truth.
pub struct HostContext {
    pub engine: Engine,
    pub component: Component,
    pub hub: Arc<Hub>,
    pub drivers: Arc<RwLock<HashMap<String, Arc<dyn LlmDriver>>>>,
    pub tool_runner: Arc<ToolRunner>,
    /// Parent directory of every sandbox: `$CARSON_HOME/sandbox`.
    pub sandbox_base: PathBuf,
    /// Live `session_id -> sandbox_id` links, seeded on create/restore and
    /// updated when a session switches sandbox.
    pub sandbox_links: Arc<RwLock<HashMap<String, String>>>,
}

impl HostContext {
    pub fn new() -> Result<Self> {
        Self::with_sandbox_base(std::env::temp_dir().join("carson-sandbox"))
    }

    pub fn with_sandbox_base(sandbox_base: impl Into<PathBuf>) -> Result<Self> {
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
            sandbox_base: sandbox_base.into(),
            sandbox_links: Arc::new(RwLock::new(HashMap::new())),
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

    /// Register a built-in tool, wiring the interpreter for `bash` together
    /// with its coreutils runner.
    pub fn register_builtin(&self, def: &ToolDef) -> Result<()> {
        if def.name == "bash" {
            self.tool_runner
                .register_shell(def, EMBEDDED_BASH_TOOL, EMBEDDED_COREUTILS)?;
        } else {
            let wasm = embedded_tool(&def.name).expect("embedded bytes for builtin");
            self.tool_runner.register(def, wasm)?;
        }
        Ok(())
    }

    pub fn remove_tool(&self, name: &str) -> bool {
        self.tool_runner.remove(name)
    }
}

/// Build the runtime driver for a persisted provider (always OpenAI-compatible).
pub fn openai_driver(def: &ProviderDef) -> Result<Arc<dyn LlmDriver>> {
    let api_key = def.api_key.clone().unwrap_or_default();
    Ok(Arc::new(OpenAiCompatDriver {
        base_url: def.base_url.clone(),
        api_key,
    }))
}

pub async fn build_registry(ctx: &HostContext, agents: &[AgentDef]) -> Result<AgentRegistry> {
    let mut registry = AgentRegistry::new();
    for def in agents {
        registry.insert(build_pool(ctx, def).await?);
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
pub async fn snapshot_session(db: &Arc<Db>, instance: &AgentInstance, session_id: &str) {
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
    let messages: Vec<StoredBlock> = state.blocks.iter().map(StoredBlock::from).collect();
    let persisted = PersistedSession {
        id: session_id.to_string(),
        agent_name: instance.agent_name.clone(),
        agent_version_id: instance.agent_version.clone(),
        // Name and sandbox are session metadata managed by the API layer via
        // their own update calls; the message snapshot must not clobber them.
        name: None,
        sandbox_id: None,
        updated_at: ms_since_epoch(),
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
    session_id: &str,
    persisted: &PersistedSession,
    config: &crate::bindings::exports::carson::agent::agent::SessionConfig,
) -> Result<()> {
    let mut store = instance.store.lock().await;
    let guest = instance.agent.carson_agent_agent();
    let state = crate::bindings::exports::carson::agent::agent::State {
        blocks: persisted
            .messages
            .iter()
            .map(crate::bindings::exports::carson::agent::agent::Block::from)
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
        sandbox_base: ctx.sandbox_base.clone(),
        sandbox_links: ctx.sandbox_links.clone(),
        caps: Capabilities::from_ids(def.capabilities.clone()),
        stop: Arc::new(AtomicBool::new(false)),
        streams: HashMap::new(),
        next_stream_id: 0,
    };
    let mut store = Store::new(&ctx.engine, state);
    let agent = AgentWorld::instantiate_async(&mut store, &ctx.component, &linker).await?;
    let stop = store.data().stop.clone();
    Ok(AgentInstance {
        agent_name: def.name.clone(),
        agent_version: def.id.clone(),
        store: tokio::sync::Mutex::new(store),
        agent,
        stop,
    })
}
