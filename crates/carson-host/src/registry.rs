use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use wasmtime::Store;

use crate::bindings::AgentWorld;
use crate::state::State;

/// One immutable version of an agent definition.
///
/// Every edit creates a new version identified by `id` (a uuid); the human
/// name is just a pointer to the current version (`agent_names` in the DB).
/// Sessions pin the version they were created with, so old rows must never
/// be mutated or deleted while sessions reference them.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentDef {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub system_prompt: String,
    pub model: String,
    #[serde(default = "default_instances")]
    pub instances: usize,
    #[serde(default = "default_history")]
    pub max_history: usize,
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    #[serde(default = "default_compaction_ratio")]
    pub compaction_ratio: f32,
    #[serde(default = "default_true")]
    pub auto_compact: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

fn default_instances() -> usize {
    1
}

fn default_history() -> usize {
    40
}

fn default_context_window() -> usize {
    128_000
}

fn default_compaction_ratio() -> f32 {
    0.8
}

fn default_true() -> bool {
    true
}

/// A persisted LLM provider (always OpenAI-compatible). The API key is stored
/// in the DB and echoed back to the admin UI so edits can round-trip it
/// without retyping; this host is local-only with no auth by design.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProviderDef {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// A tool definition. `id` is the stable identity agents reference; `name`
/// is the bare, provider-safe display/wire name. Built-ins carry
/// deterministic ids (uuid v5); uploaded tools get random v4 ids.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ToolDef {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_parameters")]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_parameters() -> serde_json::Value {
    serde_json::json!({})
}

pub struct AgentInstance {
    pub agent_name: String,
    pub agent_version: String,
    pub store: Mutex<Store<State>>,
    pub agent: AgentWorld,
    pub stop: Arc<AtomicBool>,
}

pub struct AgentPool {
    pub version_id: String,
    pub agent_name: String,
    pub def: AgentDef,
    pub instances: Vec<Arc<AgentInstance>>,
    pub next: AtomicUsize,
}

impl AgentPool {
    pub fn from_def(def: &AgentDef, instances: Vec<Arc<AgentInstance>>) -> Self {
        Self {
            version_id: def.id.clone(),
            agent_name: def.name.clone(),
            def: def.clone(),
            instances,
            next: AtomicUsize::new(0),
        }
    }

    /// The guest session config derived from this pool's agent definition.
    pub fn config(&self) -> crate::bindings::exports::carson::agent::agent::SessionConfig {
        crate::bindings::exports::carson::agent::agent::SessionConfig {
            system_prompt: self.def.system_prompt.clone(),
            model: self.def.model.clone(),
            capabilities_json: serde_json::json!(self.def.capabilities).to_string(),
            max_history: self.def.max_history as u32,
            context_window: self.def.context_window as u32,
            compaction_ratio: self.def.compaction_ratio,
            auto_compact: self.def.auto_compact,
        }
    }

    pub fn next(&self) -> Arc<AgentInstance> {
        let len = self.instances.len().max(1);
        let index = self.next.fetch_add(1, Ordering::SeqCst) % len;
        self.instances[index].clone()
    }

    pub fn instances(&self) -> &[Arc<AgentInstance>] {
        &self.instances
    }
}

/// Pools keyed by agent version id. Versions pinned by live sessions stay
/// loaded even after the name pointer moves elsewhere.
#[derive(Default)]
pub struct AgentRegistry {
    pools: HashMap<String, Arc<AgentPool>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
        }
    }

    pub fn insert(&mut self, pool: AgentPool) {
        self.pools.insert(pool.version_id.clone(), Arc::new(pool));
    }

    pub fn insert_arc(&mut self, pool: Arc<AgentPool>) {
        self.pools.insert(pool.version_id.clone(), pool);
    }

    pub fn get(&self, version_id: &str) -> Option<Arc<AgentPool>> {
        self.pools.get(version_id).cloned()
    }

    pub fn pools(&self) -> impl Iterator<Item = (&String, &Arc<AgentPool>)> {
        self.pools.iter()
    }
}

/// Locking helper: returns the cached pool for `def`, building and caching it
/// on first use so callers never instantiate wasm twice for one version.
pub async fn get_or_build_pool(
    ctx: &crate::host::HostContext,
    registry: &Mutex<AgentRegistry>,
    def: &AgentDef,
) -> anyhow::Result<Arc<AgentPool>> {
    if let Some(pool) = registry.lock().await.get(&def.id) {
        return Ok(pool);
    }
    let pool = Arc::new(crate::host::build_pool(ctx, def).await?);
    registry.lock().await.insert_arc(pool.clone());
    Ok(pool)
}
