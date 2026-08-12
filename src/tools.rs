use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::config::Config;
use crate::tool_bindings::ToolWorld;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
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

/// Registers tool components that run in their own wasm sandboxes.
pub struct ToolRunner {
    engine: Arc<Engine>,
    tools: HashMap<String, Arc<ToolSandbox>>,
    specs: Vec<ToolSpec>,
}

impl ToolRunner {
    pub fn new(engine: &Engine, config: &Config) -> Result<Self> {
        let mut tools = HashMap::new();
        let mut specs = Vec::new();
        for (name, kind) in &config.tools {
            let bytes: &[u8] = match &kind.module {
                Some(path) => {
                    &std::fs::read(path).with_context(|| format!("read tool module {path}"))?
                }
                None => crate::host::embedded_tool(name)
                    .with_context(|| format!("no embedded tool named '{name}'"))?,
            };
            let component = Arc::new(Component::new(engine, bytes)?);
            tools.insert(
                name.clone(),
                Arc::new(ToolSandbox {
                    component,
                    env: kind.env.clone(),
                }),
            );
            specs.push(ToolSpec {
                name: name.clone(),
                description: kind.description.clone(),
                parameters: kind.parameters.clone(),
            });
        }
        Ok(Self {
            engine: Arc::new(engine.clone()),
            tools,
            specs,
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    pub fn run(&self, name: &str, args_json: &str) -> Option<Result<String, String>> {
        let sandbox = self.tools.get(name)?;
        Some(invoke_tool(&self.engine, sandbox, args_json))
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
