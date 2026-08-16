use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::registry::ToolDef;
use crate::tool_bindings::ToolWorld;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl From<ToolSpec> for ToolDef {
    fn from(spec: ToolSpec) -> Self {
        Self {
            name: spec.name,
            description: spec.description,
            parameters: spec.parameters,
            env: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub tools: Vec<String>,
}

impl Capabilities {
    pub fn from_names(names: Vec<String>) -> Self {
        Self { tools: names }
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t == name)
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
    tools: RwLock<HashMap<String, Arc<ToolSandbox>>>,
    specs: RwLock<Vec<ToolSpec>>,
}

impl ToolRunner {
    pub fn new(engine: &Engine) -> Self {
        Self {
            engine: Arc::new(engine.clone()),
            tools: RwLock::new(HashMap::new()),
            specs: RwLock::new(Vec::new()),
        }
    }

    /// Register a tool component. `core/` names resolve to embedded bytes; `custom/` names use the
    /// provided wasm.
    pub fn register(&self, def: &ToolDef, wasm: &[u8]) -> Result<()> {
        let component = Arc::new(Component::new(&self.engine, wasm)?);
        let sandbox = Arc::new(ToolSandbox {
            component,
            env: def.env.clone(),
        });
        let mut tools = self.tools.write().unwrap();
        let mut specs = self.specs.write().unwrap();
        tools.insert(def.name.clone(), sandbox);
        specs.retain(|spec| spec.name != def.name);
        specs.push(ToolSpec {
            name: def.name.clone(),
            description: def.description.clone(),
            parameters: def.parameters.clone(),
        });
        Ok(())
    }

    pub fn remove(&self, name: &str) -> bool {
        let removed = self.tools.write().unwrap().remove(name).is_some();
        if removed {
            self.specs.write().unwrap().retain(|spec| spec.name != name);
        }
        removed
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs.read().unwrap().clone()
    }

    pub fn run(&self, name: &str, args_json: &str) -> Option<Result<String, String>> {
        let sandbox = self.tools.read().unwrap().get(name).cloned()?;
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

    #[test]
    fn capabilities_filter_tools() {
        let caps = Capabilities::from_names(vec!["time".into()]);
        assert!(caps.allows_tool("time"));
        assert!(!caps.allows_tool("echo"));
    }

    #[test]
    fn empty_capabilities_allow_nothing() {
        let caps = Capabilities::default();
        assert!(!caps.allows_tool("time"));
    }
}
