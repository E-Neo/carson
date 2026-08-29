//! Runtime state and stream plumbing for the carson shell.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Mutable state of one shell run. `cwd` is relative to the sandbox `root`.
#[derive(Debug, Clone)]
pub struct ShellState {
    pub env: HashMap<String, String>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub last_status: i32,
    pub exit_requested: Option<i32>,
}

impl ShellState {
    pub fn new(env: HashMap<String, String>, root: impl Into<PathBuf>) -> Self {
        Self {
            env,
            root: root.into(),
            cwd: PathBuf::new(),
            last_status: 0,
            exit_requested: None,
        }
    }

    /// Resolve a shell path (relative to cwd or absolute) to a filesystem path
    /// under the sandbox root.
    pub fn resolve(&self, p: &str) -> PathBuf {
        let joined = if let Some(rel) = p.strip_prefix('/') {
            self.root.join(rel)
        } else {
            self.root.join(&self.cwd).join(p)
        };
        normalize(&joined)
    }

    /// Convert a resolved path back to a shell-relative cwd (`""` = root).
    pub fn rel_to_root(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.root) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_path_buf(),
            _ => PathBuf::new(),
        }
    }

    /// The cwd as the shell prints it: absolute under the sandbox root.
    pub fn cwd_display(&self) -> String {
        if self.cwd.as_os_str().is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.cwd.display())
        }
    }
}

/// Collapse `.` and `..` components lexically without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// The collected output of a whole tool call.
#[derive(Debug, Default)]
pub struct Streams {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Where a command reads input from.
#[derive(Debug, Clone)]
pub enum In {
    None,
    Buffer(Rc<RefCell<Vec<u8>>>),
    File(PathBuf),
}

impl Default for In {
    fn default() -> Self {
        In::None
    }
}

/// Where a command writes output to.
#[derive(Debug, Clone)]
pub enum Out {
    Main,
    Err,
    Buffer(Rc<RefCell<Vec<u8>>>),
    File { path: PathBuf, append: bool },
}

impl Default for Out {
    fn default() -> Self {
        Out::Main
    }
}

/// The three standard fds for the command currently running.
#[derive(Debug, Clone)]
pub struct Io {
    pub stdin: In,
    pub stdout: Out,
    pub stderr: Out,
}

impl Default for Io {
    fn default() -> Self {
        Io {
            stdin: In::None,
            stdout: Out::Main,
            stderr: Out::Err,
        }
    }
}

impl Io {
    pub fn out_fd(&self, fd: u32) -> Option<&Out> {
        match fd {
            1 => Some(&self.stdout),
            2 => Some(&self.stderr),
            _ => None,
        }
    }
}

/// Append `data` to the given output sink, routing to the final streams.
pub fn write_out(out: &Out, streams: &mut Streams, data: &[u8]) {
    match out {
        Out::Main => streams.stdout.extend_from_slice(data),
        Out::Err => streams.stderr.extend_from_slice(data),
        Out::Buffer(b) => b.borrow_mut().extend_from_slice(data),
        Out::File { path, append } => {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.create(true).write(true).append(*append);
            if !append {
                opts.truncate(true);
            }
            if let Ok(mut f) = opts.open(path) {
                let _ = f.write_all(data);
            }
        }
    }
}

/// Read all of `in_` as bytes.
pub fn read_in(in_: &In) -> Vec<u8> {
    match in_ {
        In::None => Vec::new(),
        In::Buffer(b) => b.borrow().clone(),
        In::File(path) => std::fs::read(path).unwrap_or_default(),
    }
}
