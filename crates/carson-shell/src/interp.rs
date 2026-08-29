//! The interpreter: expansion, command execution, control flow.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use crate::ast::{Command, RedirectOp, SimpleCommand, Word, WordPart};
use crate::exec::{Exec, EXTERNAL_COMMANDS};
use crate::parser::parse;
use crate::state::{read_in, write_out, In, Io, Out, ShellState, Streams};

/// The outcome of one tool call.
#[derive(Debug)]
pub struct ScriptResult {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

/// Parse and run `script`. Returns an error for syntax errors; runtime
/// problems surface through `ScriptResult` (stderr + non-zero status).
pub fn run_script(
    script: &str,
    env: &HashMap<String, String>,
    root: impl Into<PathBuf>,
    exec: &mut dyn Exec,
) -> Result<ScriptResult, String> {
    run_script_with_cwd(script, env, root, "/", exec)
}

/// Like [`run_script`] but starts in `cwd`, a shell path relative to `root`.
pub fn run_script_with_cwd(
    script: &str,
    env: &HashMap<String, String>,
    root: impl Into<PathBuf>,
    cwd: &str,
    exec: &mut dyn Exec,
) -> Result<ScriptResult, String> {
    let list = parse(script).map_err(|e| format!("bash: syntax error: {}", e.msg))?;
    let mut interp = Interp {
        state: ShellState::new(env.clone(), root),
        streams: Streams::default(),
        exec,
    };
    let cwd_rel = cwd.trim_start_matches('/');
    interp.state.cwd = if cwd_rel.is_empty() {
        PathBuf::new()
    } else {
        PathBuf::from(cwd_rel)
    };
    let io = Io::default();
    interp.run_list(&list, &io);
    if let Some(code) = interp.state.exit_requested {
        interp.state.last_status = code;
    }
    Ok(ScriptResult {
        stdout: String::from_utf8_lossy(&interp.streams.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&interp.streams.stderr).into_owned(),
        status: interp.state.last_status,
    })
}

pub struct Interp<'a> {
    pub state: ShellState,
    pub streams: Streams,
    exec: &'a mut dyn Exec,
}

impl Interp<'_> {
    fn emit(&mut self, out: &Out, data: &[u8]) {
        write_out(out, &mut self.streams, data);
    }

    pub(crate) fn emit_str(&mut self, out: &Out, s: &str) {
        self.emit(out, s.as_bytes());
    }

    pub(crate) fn run_list(&mut self, list: &[Command], io: &Io) -> i32 {
        let mut status = 0;
        for cmd in list {
            if self.state.exit_requested.is_some() {
                break;
            }
            status = self.run_command(cmd, io);
        }
        status
    }

    fn run_command(&mut self, cmd: &Command, io: &Io) -> i32 {
        match cmd {
            Command::Simple(s) => self.run_simple(s, io),
            Command::AndOr { lhs, and, rhs } => {
                let left = self.run_command(lhs, io);
                let take_rhs = if *and { left == 0 } else { left != 0 };
                if take_rhs {
                    self.run_command(rhs, io)
                } else {
                    left
                }
            }
            Command::Pipeline { negated, stages } => {
                let mut status = 0;
                let mut prev: Option<Rc<RefCell<Vec<u8>>>> = None;
                for (i, stage) in stages.iter().enumerate() {
                    let last = i + 1 == stages.len();
                    let stdin = match &prev {
                        Some(b) => In::Buffer(b.clone()),
                        None => io.stdin.clone(),
                    };
                    let (stdout, keep) = if last {
                        (io.stdout.clone(), None)
                    } else {
                        let buf = Rc::new(RefCell::new(Vec::new()));
                        (Out::Buffer(buf.clone()), Some(buf))
                    };
                    let stage_io = Io {
                        stdin,
                        stdout,
                        stderr: io.stderr.clone(),
                    };
                    status = self.run_simple(stage, &stage_io);
                    if let Some(b) = keep {
                        prev = Some(b);
                    }
                    if self.state.exit_requested.is_some() {
                        break;
                    }
                }
                if *negated {
                    if status == 0 {
                        1
                    } else {
                        0
                    }
                } else {
                    status
                }
            }
            Command::If {
                cond,
                then,
                elifs,
                els,
            } => {
                if self.run_list(cond, io) == 0 {
                    self.run_list(then, io)
                } else {
                    for (c, b) in elifs {
                        if self.run_list(c, io) == 0 {
                            return self.run_list(b, io);
                        }
                    }
                    if let Some(e) = els {
                        self.run_list(e, io)
                    } else {
                        0
                    }
                }
            }
            Command::For { var, words, body } => {
                let mut values = Vec::new();
                for w in words {
                    values.extend(self.expand_word(w));
                }
                let mut status = 0;
                for v in values {
                    if self.state.exit_requested.is_some() {
                        break;
                    }
                    self.state.env.insert(var.clone(), v);
                    status = self.run_list(body, io);
                }
                status
            }
            Command::While { cond, body, until } => {
                loop {
                    if self.state.exit_requested.is_some() {
                        break;
                    }
                    let c = self.run_list(cond, io);
                    let go = if *until { c != 0 } else { c == 0 };
                    if !go {
                        break;
                    }
                    self.run_list(body, io);
                }
                0
            }
            Command::Subshell(body) => {
                let saved_env = self.state.env.clone();
                let saved_cwd = self.state.cwd.clone();
                let saved_exit = self.state.exit_requested;
                let status = self.run_list(body, io);
                self.state.env = saved_env;
                self.state.cwd = saved_cwd;
                self.state.exit_requested = saved_exit;
                status
            }
            Command::Brace(body) => self.run_list(body, io),
        }
    }

    fn run_simple(&mut self, s: &SimpleCommand, io: &Io) -> i32 {
        let mut argv = Vec::new();
        for w in &s.words {
            argv.extend(self.expand_word(w));
        }

        let cmd_io = match self.apply_redirects(io, &s.redirects) {
            Ok(i) => i,
            Err(msg) => {
                self.emit_str(&io.stderr, &format!("bash: {msg}\n"));
                return 1;
            }
        };

        let mut local_env = HashMap::new();
        for (name, w) in &s.assignments {
            let val = self.expand_assignment(w);
            if argv.is_empty() {
                self.state.env.insert(name.clone(), val);
            } else {
                local_env.insert(name.clone(), val);
            }
        }

        if argv.is_empty() {
            return 0;
        }

        let status = self.dispatch(&argv, &cmd_io, &local_env);
        self.state.last_status = status;
        status
    }

    pub(crate) fn dispatch(&mut self, argv: &[String], io: &Io, local_env: &HashMap<String, String>) -> i32 {
        let name = argv[0].as_str();
        match name {
            "echo" => self.builtin_echo(argv, io),
            "printf" => self.builtin_printf(argv, io),
            "cd" => self.builtin_cd(argv, io),
            "pwd" => self.builtin_pwd(argv, io),
            "export" => self.builtin_export(argv),
            "unset" => self.builtin_unset(argv),
            "set" => self.builtin_set(argv, io),
            "env" => self.builtin_env(argv, io, local_env),
            "exit" => self.builtin_exit(argv),
            "true" => 0,
            "false" => 1,
            "test" => self.builtin_test(&argv[1..], io),
            "[" => {
                let mut args = argv[1..].to_vec();
                if args.last().map(String::as_str) == Some("]") {
                    args.pop();
                } else {
                    self.emit_str(&io.stderr, "bash: [: missing `]'\n");
                    return 2;
                }
                self.builtin_test(&args, io)
            }
            name if EXTERNAL_COMMANDS.contains(&name) => self.run_exec(argv, io, local_env),
            _ => {
                self.emit_str(&io.stderr, &format!("bash: {name}: command not found\n"));
                127
            }
        }
    }

    fn run_exec(&mut self, argv: &[String], io: &Io, local_env: &HashMap<String, String>) -> i32 {
        let stdin = read_in(&io.stdin);
        let mut env = self.state.env.clone();
        env.extend(local_env.clone());
        let cwd = self.state.cwd.to_string_lossy().into_owned();
        let prog = argv[0].clone();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let status = self
            .exec
            .run(&prog, argv, &env, &cwd, &stdin, &mut out, &mut err);
        self.emit(&io.stdout, &out);
        self.emit(&io.stderr, &err);
        status
    }

    fn apply_redirects(&mut self, io: &Io, redirects: &[crate::ast::Redirect]) -> Result<Io, String> {
        let mut out = io.clone();
        for r in redirects {
            let target = self.expand_word(&r.target).into_iter().next().unwrap_or_default();
            match r.op {
                RedirectOp::In => {
                    if r.fd == 0 {
                        out.stdin = In::File(self.state.resolve(&target));
                    } else {
                        return Err(format!("fd {}: bad input redirect", r.fd));
                    }
                }
                RedirectOp::Out | RedirectOp::Append => {
                    let path = self.state.resolve(&target);
                    let append = r.op == RedirectOp::Append;
                    let o = Out::File { path, append };
                    match r.fd {
                        1 => out.stdout = o,
                        2 => out.stderr = o,
                        f => return Err(format!("fd {f}: unsupported output redirect")),
                    }
                }
                RedirectOp::Dup(to) => {
                    let src = out.out_fd(to).cloned().unwrap_or(Out::Err);
                    match r.fd {
                        1 => out.stdout = src,
                        2 => out.stderr = src,
                        f => return Err(format!("fd {f}: unsupported dup redirect")),
                    }
                }
            }
        }
        Ok(out)
    }

    /// Expand a word into argv fields, applying word splitting to unquoted
    /// parameter and command substitutions.
    pub(crate) fn expand_word(&mut self, w: &Word) -> Vec<String> {
        let mut fields: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut after_expansion = false;
        let mut saw_literal = false;
        for part in &w.parts {
            match part {
                WordPart::Lit(s) | WordPart::Quoted(s) => {
                    if after_expansion && !fields.is_empty() {
                        fields.last_mut().unwrap().push_str(s);
                    } else {
                        cur.push_str(s);
                    }
                    after_expansion = false;
                    saw_literal = true;
                }
                WordPart::Var(name) => {
                    let v = self.var_value(name);
                    let tokens: Vec<&str> = v.split_whitespace().collect();
                    apply_tokens(&mut fields, &mut cur, &tokens, &mut after_expansion);
                }
                WordPart::VarQuoted(name) => {
                    let v = self.var_value(name);
                    if after_expansion && !fields.is_empty() {
                        fields.last_mut().unwrap().push_str(&v);
                    } else {
                        cur.push_str(&v);
                    }
                    after_expansion = false;
                    saw_literal = true;
                }
                WordPart::SubRaw(src) => {
                    let v = self.command_sub(src);
                    let tokens: Vec<&str> = v.split_whitespace().collect();
                    apply_tokens(&mut fields, &mut cur, &tokens, &mut after_expansion);
                }
                WordPart::SubQuotedRaw(src) => {
                    let v = self.command_sub(src);
                    if after_expansion && !fields.is_empty() {
                        fields.last_mut().unwrap().push_str(&v);
                    } else {
                        cur.push_str(&v);
                    }
                    after_expansion = false;
                    saw_literal = true;
                }
            }
        }
        if !cur.is_empty() {
            fields.push(cur);
        } else if fields.is_empty() && saw_literal {
            fields.push(String::new());
        }
        fields
    }

    /// Expand a word for an assignment: concatenation, no splitting.
    fn expand_assignment(&mut self, w: &Word) -> String {
        let mut s = String::new();
        for part in &w.parts {
            match part {
                WordPart::Lit(t) | WordPart::Quoted(t) => s.push_str(t),
                WordPart::Var(n) | WordPart::VarQuoted(n) => s.push_str(&self.var_value(n)),
                WordPart::SubRaw(src) | WordPart::SubQuotedRaw(src) => {
                    s.push_str(&self.command_sub(src))
                }
            }
        }
        s
    }

    fn var_value(&self, name: &str) -> String {
        match name {
            "?" => self.state.last_status.to_string(),
            "#" => "0".to_string(),
            n => self.state.env.get(n).cloned().unwrap_or_default(),
        }
    }    fn command_sub(&mut self, src: &str) -> String {
        let list = match parse(src) {
            Ok(l) => l,
            Err(e) => {
                self.emit_str(&Out::Err, &format!("bash: syntax error in $(): {}\n", e.msg));
                return String::new();
            }
        };
        let buf = Rc::new(RefCell::new(Vec::new()));
        let io = Io {
            stdin: In::None,
            stdout: Out::Buffer(buf.clone()),
            stderr: Out::Err,
        };
        let saved_env = self.state.env.clone();
        let saved_cwd = self.state.cwd.clone();
        let saved_exit = self.state.exit_requested;
        self.run_list(&list, &io);
        self.state.env = saved_env;
        self.state.cwd = saved_cwd;
        self.state.exit_requested = saved_exit;
        let s = String::from_utf8_lossy(&buf.borrow()).into_owned();
        s.trim_end_matches('\n').to_string()
    }
}

/// Merge the tokens of one unquoted expansion into the field stream. The first
/// token joins the current field; further tokens each start a new field.
fn apply_tokens(
    fields: &mut Vec<String>,
    cur: &mut String,
    tokens: &[&str],
    after_expansion: &mut bool,
) {
    match tokens {
        [] => {}
        [single] => cur.push_str(single),
        many => {
            cur.push_str(many[0]);
            fields.push(std::mem::take(cur));
            for t in &many[1..] {
                fields.push(t.to_string());
            }
            *after_expansion = true;
        }
    }
}
