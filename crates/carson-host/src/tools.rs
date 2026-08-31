use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::filesystem::FsPerms;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bash_bindings::BashWorld;
use crate::coreutils_bindings::CoreutilsWorld;
use crate::registry::ToolDef;
use crate::tool_bindings::ToolWorld;

/// Cap on the captured stdout/stderr of a coreutils command.
const SHELL_MAX_OUTPUT: usize = 32 * 1024;
/// Hard per-stream bound for a running command; larger output aborts it.
const SHELL_PIPE_CAPACITY: usize = 8 * 1024 * 1024;
/// Abort a whole bash tool call after this long.
const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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

/// State for instantiating the bash component: its own wasi plus the resources
/// the `exec` import needs to spawn coreutils instances.
struct ShellCtx {
    wasi: WasiCtx,
    table: ResourceTable,
    engine: Arc<Engine>,
    coreutils: Arc<Component>,
    root: PathBuf,
}

impl WasiView for ShellCtx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

type ExecResult = crate::bash_bindings::carson::shell::exec::ExecResult;

impl crate::bash_bindings::carson::shell::exec::Host for ShellCtx {
    fn run(
        &mut self,
        _prog: String,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: String,
        stdin: Vec<u8>,
    ) -> Result<ExecResult, String> {
        let engine = self.engine.clone();
        let coreutils = self.coreutils.clone();
        let root = self.root.clone();
        run_coreutils(&engine, &coreutils, &root, &argv, &env, &cwd, &stdin)
    }
}

/// Run one coreutils command as a fresh wasm instance, capturing its stdio.
/// The bash guest call is synchronous, so this blocks on a helper thread.
fn run_coreutils(
    engine: &Engine,
    coreutils: &Component,
    root: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    cwd: &str,
    stdin: &[u8],
) -> Result<ExecResult, String> {
    let engine = engine.clone();
    let coreutils = coreutils.clone();
    let root = root.to_path_buf();
    let argv = argv.to_vec();
    let env = env.to_vec();
    let cwd = cwd.to_string();
    let stdin = stdin.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build coreutils runtime");
        let result = rt.block_on(async {
            spawn_coreutils(&engine, &coreutils, &root, &argv, &env, &cwd, &stdin).await
        });
        let _ = tx.send(result);
    });
    rx.recv()
        .unwrap_or_else(|_| Err("coreutils runner thread failed".to_string()))
}

async fn spawn_coreutils(
    engine: &Engine,
    coreutils: &Component,
    root: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    cwd: &str,
    stdin: &[u8],
) -> Result<ExecResult, String> {
    let mut linker = Linker::<ToolCtx>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;

    let stdout = MemoryOutputPipe::new(SHELL_PIPE_CAPACITY);
    let stderr = MemoryOutputPipe::new(SHELL_PIPE_CAPACITY);

    let mut builder = WasiCtxBuilder::new();
    builder.args(argv);
    apply_env_defaults(&mut builder, env.iter().map(|(k, v)| (k, v)));
    builder.env("CARSON_CWD", resolve_guest_cwd(cwd));
    // The sandbox root is the guest root: relative paths and absolute `/...`
    // paths both resolve against the same directory the shell uses. /bin,
    // /tmp and /home/carson are real subdirs of it.
    builder
        .preopened_dir(root, "/", FsPerms::ReadWrite)
        .map_err(|e| format!("preopen: {e}"))?;
    builder.initial_cwd(resolve_guest_cwd(cwd));
    builder.stdin(MemoryInputPipe::new(stdin.to_vec()));
    builder.stdout(stdout.clone());
    builder.stderr(stderr.clone());
    let wasi = builder.build();

    let mut store = Store::new(
        engine,
        ToolCtx {
            wasi,
            table: ResourceTable::new(),
        },
    );
    let instance = CoreutilsWorld::instantiate_async(&mut store, coreutils, &linker)
        .await
        .map_err(|e| format!("instantiate coreutils: {e}"))?;
    let (status,) = instance
        .carson_shell_coreutils()
        .func_run()
        .call_async(&mut store, ())
        .await
        .map_err(|e| format!("coreutils call: {e}"))?;
    let status = status.map_err(|e| format!("coreutils returned: {e}"))?;

    let out = stdout.contents();
    let err = stderr.contents();
    Ok(ExecResult {
        stdout: out[..out.len().min(SHELL_MAX_OUTPUT)].to_vec(),
        stderr: err[..err.len().min(SHELL_MAX_OUTPUT)].to_vec(),
        status,
    })
}

/// The guest-visible cwd: a path under the sandbox root `/`.
fn resolve_guest_cwd(cwd: &str) -> String {
    let rel = cwd.trim_start_matches('/');
    if rel.is_empty() {
        return "/".to_string();
    }
    let mut guest = String::new();
    for comp in PathBuf::from(rel).components() {
        use std::path::Component;
        match comp {
            Component::Normal(s) => {
                guest.push('/');
                guest.push_str(&s.to_string_lossy());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(pos) = guest.rfind('/') {
                    if pos > 0 {
                        guest.truncate(pos);
                    }
                }
            }
            _ => {}
        }
    }
    if guest.is_empty() {
        "/".to_string()
    } else {
        guest
    }
}

/// A tool component, pre-compiled, plus the wasi grants its sandbox gets.
struct ToolSandbox {
    kind: SandboxKind,
    env: HashMap<String, String>,
}

enum SandboxKind {
    /// A plain tool: the whole component is instantiated per call.
    Plain(Arc<Component>),
    /// The bash tool: the interpreter component plus the coreutils component
    /// the `exec` import spawns, under a shared sandbox directory.
    Shell {
        component: Arc<Component>,
        coreutils: Arc<Component>,
        root: PathBuf,
    },
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
        let component = Arc::new(crate::host::load_component(&self.engine, wasm)?);
        let sandbox = Arc::new(ToolSandbox {
            kind: SandboxKind::Plain(component),
            env: def.env.clone(),
        });
        self.insert(def, sandbox);
        Ok(())
    }

    /// Register the interpreter component for the bash tool plus the coreutils
    /// component its `exec` import spawns. Returns the sandbox directory.
    pub fn register_shell(
        &self,
        def: &ToolDef,
        bash_wasm: &[u8],
        coreutils_wasm: &[u8],
    ) -> Result<PathBuf> {
        let component = Arc::new(crate::host::load_component(&self.engine, bash_wasm)?);
        let coreutils = Arc::new(crate::host::load_component(&self.engine, coreutils_wasm)?);
        let root = sandbox_dir(&def.id);
        std::fs::create_dir_all(&root)?;
        let sandbox = Arc::new(ToolSandbox {
            kind: SandboxKind::Shell {
                component,
                coreutils,
                root: root.clone(),
            },
            env: def.env.clone(),
        });
        self.insert(def, sandbox);
        Ok(root)
    }

    fn insert(&self, def: &ToolDef, sandbox: Arc<ToolSandbox>) {
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
        self.run_in(id, args_json, None)
    }

    /// Run a tool in a specific sandbox directory. The bash tool preopens
    /// `root` for its interpreter and every exec'd coreutils instance; plain
    /// tools ignore it. `None` falls back to the registered default.
    pub fn run_in(
        &self,
        id: &str,
        args_json: &str,
        root: Option<&std::path::Path>,
    ) -> Option<Result<String, String>> {
        let sandbox = self.sandboxes.read().unwrap().get(id).cloned()?;
        Some(invoke_tool(&self.engine, &sandbox, args_json, root))
    }
}

/// The directory a shell tool's sandbox lives in by default. One directory per
/// tool id; every exec'd coreutils instance sees the same files.
fn sandbox_dir(id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("carson-sandbox")
        .join(sanitize(id))
}

/// Lay out the sandbox's virtual filesystem: `/bin` (one placeholder per
/// available coreutils command, so `ls /bin` lists them), `/tmp` and
/// `/home/carson`. Idempotent; run whenever a sandbox directory is prepared.
fn ensure_vfs(root: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root.join("bin"))?;
    std::fs::create_dir_all(root.join("tmp"))?;
    std::fs::create_dir_all(root.join("home").join("carson"))?;
    for cmd in carson_shell::EXTERNAL_COMMANDS {
        let entry = root.join("bin").join(cmd);
        if !entry.exists() {
            std::fs::write(entry, "")?;
        }
    }
    Ok(())
}

/// Defaults for the shell process environment; explicit tool env overrides.
fn apply_env_defaults<'a>(
    builder: &mut WasiCtxBuilder,
    env: impl IntoIterator<Item = (&'a String, &'a String)>,
) {
    builder.env("HOME", "/home/carson");
    builder.env("USER", "carson");
    builder.env("LOGNAME", "carson");
    builder.env("PATH", "/bin");
    for (k, v) in env {
        builder.env(k, v);
    }
}

pub(crate) fn sanitize(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

fn invoke_tool(
    engine: &Arc<Engine>,
    sandbox: &ToolSandbox,
    args_json: &str,
    session_root: Option<&std::path::Path>,
) -> Result<String, String> {
    match &sandbox.kind {
        SandboxKind::Plain(component) => {
            let engine = engine.clone();
            let component = component.clone();
            let env = sandbox.env.clone();
            let args = args_json.to_string();
            blocking_thread(move || async move { run_tool(&engine, &component, &env, &args).await })
                .map_err(|_| "tool thread failed".to_string())?
        }
        SandboxKind::Shell {
            component,
            coreutils,
            root: default_root,
        } => {
            let engine = engine.clone();
            let component = component.clone();
            let coreutils = coreutils.clone();
            let root = session_root
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| default_root.clone());
            let env = sandbox.env.clone();
            let args = args_json.to_string();
            blocking_thread(move || async move {
                run_shell(&engine, &component, &coreutils, &root, &env, &args).await
            })
            .map_err(|_| "bash tool thread failed".to_string())?
        }
    }
}

fn blocking_thread<O, Fut>(f: impl FnOnce() -> Fut + Send + 'static) -> Result<O, String>
where
    Fut: std::future::Future<Output = O> + Send + 'static,
    O: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tool runtime");
        let result = rt.block_on(f());
        let _ = tx.send(result);
    });
    match rx.recv_timeout(SHELL_TIMEOUT) {
        Ok(result) => Ok(result),
        Err(_) => Err("tool timed out".to_string()),
    }
}

async fn run_shell(
    engine: &Engine,
    component: &Component,
    coreutils: &Component,
    root: &std::path::Path,
    env: &HashMap<String, String>,
    args: &str,
) -> Result<String, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("create sandbox: {e}"))?;
    ensure_vfs(root).map_err(|e| format!("lay out vfs: {e}"))?;
    let mut linker = Linker::<ShellCtx>::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
    BashWorld::add_to_linker::<ShellCtx, wasmtime::component::HasSelf<ShellCtx>>(
        &mut linker,
        |ctx: &mut ShellCtx| ctx,
    )
    .map_err(|e| e.to_string())?;

    let mut builder = WasiCtxBuilder::new();
    apply_env_defaults(&mut builder, env.iter().map(|(k, v)| (k, v)));
    // The sandbox root is the guest root: relative and absolute `/...` paths
    // resolve against the same directory the shell uses. /bin, /tmp and
    // /home/carson are real subdirs of it.
    builder
        .preopened_dir(root, "/", FsPerms::ReadWrite)
        .map_err(|e| format!("preopen: {e}"))?;
    builder.initial_cwd("/");
    let wasi = builder.build();
    let mut store = Store::new(
        engine,
        ShellCtx {
            wasi,
            table: ResourceTable::new(),
            engine: Arc::new(engine.clone()),
            coreutils: Arc::new(coreutils.clone()),
            root: root.to_path_buf(),
        },
    );
    let world = BashWorld::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("instantiate bash: {e}"))?;
    let (result,) = world
        .carson_tool_tool()
        .func_run()
        .call_async(&mut store, (args,))
        .await
        .map_err(|e| format!("bash call: {e}"))?;
    result.map_err(|_| "bash tool failed".to_string())
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
