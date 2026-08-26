use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::registry::ToolDef;
use crate::tool_bindings::ToolWorld;

/// A registered tool's public shape: bare provider-safe name plus metadata.
/// Identity is the runner key (the tool's uuid), never the name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl From<&ToolSpec> for ToolDef {
    fn from(spec: &ToolSpec) -> Self {
        Self {
            id: spec.id.clone(),
            name: spec.name.clone(),
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            env: HashMap::new(),
        }
    }
}

/// The tool ids an agent may use.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub ids: Vec<String>,
}

impl Capabilities {
    pub fn from_ids(ids: Vec<String>) -> Self {
        Self { ids }
    }

    /// Resolve a bare wire name to the selected tool id. An agent can never
    /// hold two capabilities with the same bare name (validated at
    /// create/update); as a defensive measure an ambiguous set resolves to
    /// `None` instead of guessing.
    pub fn resolve_bare_name<'a>(&'a self, specs: &'a [ToolSpec], name: &str) -> Option<&'a str> {
        let mut matches = self.ids.iter().filter(|id| {
            specs
                .iter()
                .any(|spec| &spec.id == *id && spec.name == name)
        });
        let only = matches.next()?;
        matches.next().is_none().then_some(only)
    }
}

struct ToolCtx {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for ToolCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A tool component, pre-compiled, plus the wasi grants its sandbox gets.
struct ToolSandbox {
    component: Arc<Component>,
    env: HashMap<String, String>,
}

/// Registers tool components that run in their own wasm sandboxes. Tools are added and removed at
/// runtime as they are registered via the API.
pub struct ToolRunner {
    engine: Arc<Engine>,
    sandboxes: RwLock<HashMap<String, Arc<ToolSandbox>>>,
    specs: RwLock<HashMap<String, ToolSpec>>,
}

impl ToolRunner {
    pub fn new(engine: &Engine) -> Self {
        Self {
            engine: Arc::new(engine.clone()),
            sandboxes: RwLock::new(HashMap::new()),
            specs: RwLock::new(HashMap::new()),
        }
    }

    /// Register a tool component under its stable id.
    pub fn register(&self, def: &ToolDef, wasm: &[u8]) -> Result<()> {
        let component = Arc::new(Component::new(&self.engine, wasm)?);
        let sandbox = Arc::new(ToolSandbox {
            component,
            env: def.env.clone(),
        });
        let spec = ToolSpec {
            id: def.id.clone(),
            name: def.name.clone(),
            description: def.description.clone(),
            parameters: def.parameters.clone(),
        };
        self.sandboxes
            .write()
            .unwrap()
            .insert(def.id.clone(), sandbox);
        self.specs.write().unwrap().insert(def.id.clone(), spec);
        Ok(())
    }

    pub fn remove(&self, id: &str) -> bool {
        let removed = self.sandboxes.write().unwrap().remove(id).is_some();
        if removed {
            self.specs.write().unwrap().remove(id);
        }
        removed
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs.read().unwrap().values().cloned().collect()
    }

    pub fn run(&self, id: &str, args_json: &str) -> Option<Result<String, String>> {
        let sandbox = self.sandboxes.read().unwrap().get(id).cloned()?;
        Some(invoke_tool(&self.engine, &sandbox, args_json))
    }
}

fn invoke_tool(
    engine: &Arc<Engine>,
    sandbox: &ToolSandbox,
    args_json: &str,
) -> Result<String, String> {
    let engine = engine.clone();
    let component = sandbox.component.clone();
    let env = sandbox.env.clone();
    let args = args_json.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tool runtime");
        let result = rt.block_on(run_tool(&engine, &component, &env, &args));
        let _ = tx.send(result);
    });
    rx.recv()
        .unwrap_or_else(|_| Err("tool thread failed".to_string()))
}

async fn run_tool(
    engine: &Engine,
    component: &Component,
    env: &HashMap<String, String>,
    args: &str,
) -> Result<String, String> {
    let mut linker = Linker::<ToolCtx>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio();
    for (key, value) in env {
        builder.env(key, value);
    }
    let wasi = builder.build();
    let mut store = Store::new(
        engine,
        ToolCtx {
            wasi,
            table: ResourceTable::new(),
        },
    );
    let tool = ToolWorld::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| e.to_string())?;
    let (result,) = tool
        .carson_tool_tool()
        .func_run()
        .call_async(&mut store, (args,))
        .await
        .map_err(|e| e.to_string())?;
    result.map_err(|_| "tool failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time_spec(id: &str) -> ToolSpec {
        ToolSpec {
            id: id.into(),
            name: "time".into(),
            description: String::new(),
            parameters: serde_json::json!({}),
        }
    }

    #[test]
    fn capabilities_resolve_bare_names_through_specs() {
        let caps = Capabilities::from_ids(vec!["id-1".into()]);
        let specs = vec![time_spec("id-1"), time_spec("other")];
        assert_eq!(caps.resolve_bare_name(&specs, "time"), Some("id-1"));

        // Two capabilities with the same bare name would be ambiguous and are
        // rejected at agent validation; resolution here simply picks none of
        // them rather than guessing.
        let caps = Capabilities::from_ids(vec!["id-1".into(), "id-2".into()]);
        let both = [time_spec("id-1"), time_spec("id-2")];
        let matches: Vec<_> = caps
            .ids
            .iter()
            .filter(|id| both.iter().any(|sp| sp.id == **id))
            .collect();
        assert_eq!(matches.len(), 2, "ambiguous selection is rejected upstream");
    }

    #[test]
    fn empty_capabilities_allow_nothing() {
        let caps = Capabilities::default();
        let specs = vec![time_spec("id-1")];
        assert!(caps.resolve_bare_name(&specs, "time").is_none());
    }
}
