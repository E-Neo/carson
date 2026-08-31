//! The interface for running non-builtin commands.
//!
//! In the wasm build this is backed by a host `exec` import that runs a uutils
//! coreutils multicall module; in native tests a fake is used instead.
use std::collections::HashMap;

/// Commands the shell may hand to [`Exec`]. Everything else is `command not
/// found`. These run as separate sandboxed programs with their own stdio.
/// Kept in sync with the `dispatch()` arms in `crates/carson-tools/src/bin/
/// coreutils.rs`; the same list seeds the `/bin` directory of every sandbox.
pub const EXTERNAL_COMMANDS: &[&str] = &[
    "base32",
    "base64",
    "basename",
    "cat",
    "cksum",
    "comm",
    "cp",
    "csplit",
    "cut",
    "date",
    "dirname",
    "expand",
    "factor",
    "fmt",
    "fold",
    "head",
    "join",
    "link",
    "ln",
    "ls",
    "md5sum",
    "mkdir",
    "mktemp",
    "mv",
    "nl",
    "numfmt",
    "od",
    "paste",
    "pathchk",
    "pr",
    "printf",
    "ptx",
    "pwd",
    "readlink",
    "realpath",
    "rm",
    "rmdir",
    "seq",
    "sha1sum",
    "sha224sum",
    "sha256sum",
    "sha384sum",
    "sha512sum",
    "shuf",
    "sort",
    "split",
    "sum",
    "tail",
    "tee",
    "touch",
    "tr",
    "truncate",
    "tsort",
    "unexpand",
    "uniq",
    "wc",
    "yes",
];

/// A command runner that executes `prog` with its own captured stdio.
pub trait Exec {
    /// `cwd` is the shell's current directory relative to the sandbox root.
    fn run(
        &mut self,
        prog: &str,
        argv: &[String],
        env: &HashMap<String, String>,
        cwd: &str,
        stdin: &[u8],
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> i32;
}

/// An [`Exec`] that knows no commands at all; useful where only builtins run.
pub struct NoExec;

impl Exec for NoExec {
    fn run(
        &mut self,
        _prog: &str,
        _argv: &[String],
        _env: &HashMap<String, String>,
        _cwd: &str,
        _stdin: &[u8],
        _stdout: &mut Vec<u8>,
        _stderr: &mut Vec<u8>,
    ) -> i32 {
        127
    }
}
