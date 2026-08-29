//! `carson-shell`: a small, fork-less bash interpreter for the carson agent.
//!
//! The interpreter is pure Rust and runs on any target; the wasm tool embeds
//! it and routes non-builtin commands through a host [`exec`] import.
//!
//! # Compatibility
//!
//! Emulated: pipelines, redirects (`>`, `>>`, `<`, `2>`, `2>&1`), `;`, `&&`,
//! `||`, `!`, `if/elif/else`, `for` (word lists), `while`/`until`, `$(...)`
//! and `( )` (env snapshot), quoting (single/double/backslash), `$VAR`,
//! `$?`, assignments, comments, brace groups `{ ...; }`.
//!
//! Builtins: `echo`, `printf`, `cd`, `pwd`, `export`, `unset`, `set`, `env`,
//! `test`/`[`, `true`, `false`, `exit`.
//!
//! Rejected loudly (exits non-zero): heredocs, `case`, arrays, `[[ ]]`,
//! arithmetic `$(( ))`, functions, globbing, jobs/background, and any command
//! that is neither a builtin nor in [`EXTERNAL_COMMANDS`] (exit 127).
pub mod ast;
pub mod builtins;
pub mod exec;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod state;

pub use exec::{Exec, NoExec, EXTERNAL_COMMANDS};
pub use interp::{Interp, ScriptResult, run_script, run_script_with_cwd};
pub use state::ShellState;
