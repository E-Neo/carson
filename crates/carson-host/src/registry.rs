use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use wasmtime::Store;

use crate::bindings::AgentWorld;
use crate::state::State;

/// A persisted agent definition (DB is the single source of truth).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentDef {
    pub kind: String,
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

/// A persisted LLM provider (always OpenAI-compatible).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProviderDef {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// A persisted tool definition. `core/` names map to embedded bytes; `custom/` names carry their
/// wasm in the DB.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ToolDef {
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
    pub kind: String,
    pub store: Mutex<Store<State>>,
    pub agent: AgentWorld,
    pub stop: Arc<AtomicBool>,
}

pub struct AgentPool {
    pub kind: String,
    pub system_prompt: String,
    pub model: String,
    pub max_history: u32,
    pub context_window: u32,
    pub compaction_ratio: f32,
    pub auto_compact: bool,
    pub caps: Vec<String>,
    pub instances: Vec<Arc<AgentInstance>>,
    pub next: AtomicUsize,
}

impl AgentPool {
    pub fn from_def(def: &AgentDef, instances: Vec<Arc<AgentInstance>>) -> Self {
        Self {
            kind: def.kind.clone(),
            system_prompt: def.system_prompt.clone(),
            model: def.model.clone(),
            max_history: def.max_history as u32,
            context_window: def.context_window as u32,
            compaction_ratio: def.compaction_ratio,
            auto_compact: def.auto_compact,
            caps: def.capabilities.clone(),
            instances,
            next: AtomicUsize::new(0),
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

pub struct AgentRegistry {
    pools: HashMap<String, Arc<AgentPool>>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
        }
    }

    pub fn insert(&mut self, kind: String, pool: AgentPool) {
        self.pools.insert(kind, Arc::new(pool));
    }

    pub fn remove(&mut self, kind: &str) -> Option<Arc<AgentPool>> {
        self.pools.remove(kind)
    }

    pub fn get(&self, kind: &str) -> Option<Arc<AgentPool>> {
        self.pools.get(kind).cloned()
    }

    pub fn pools(&self) -> impl Iterator<Item = (&String, &Arc<AgentPool>)> {
        self.pools.iter()
    }
}
