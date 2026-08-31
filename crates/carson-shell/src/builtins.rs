//! Builtin commands. They run in-process so they can mutate shell state and
//! write to the active output sink.
use std::collections::HashMap;

use crate::Interp;
use crate::state::{Io, ShellState};

/// Validate a shell variable name.
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Interp<'_> {
    pub(crate) fn builtin_echo(&mut self, argv: &[String], io: &Io) -> i32 {
        let mut args = &argv[1..];
        let mut no_newline = false;
        let mut escapes = false;
        match args.first().map(String::as_str) {
            Some("-n") => {
                no_newline = true;
                args = &args[1..];
            }
            Some("-e") => {
                escapes = true;
                args = &args[1..];
            }
            _ => {}
        }
        let joined = args.join(" ");
        let mut out = String::new();
        if escapes {
            push_escapes(&mut out, &joined);
        } else {
            out.push_str(&joined);
        }
        if !no_newline {
            out.push('\n');
        }
        self.emit_str(&io.stdout, &out);
        0
    }

    pub(crate) fn builtin_printf(&mut self, argv: &[String], io: &Io) -> i32 {
        let Some(fmt) = argv.get(1) else {
            return 0;
        };
        let args = &argv[2..];
        let mut out = String::new();
        let mut used = 0;
        loop {
            let before = used;
            format_printf(fmt, args, &mut used, &mut out);
            if used == before || used >= args.len() {
                break;
            }
        }
        self.emit_str(&io.stdout, &out);
        0
    }

    pub(crate) fn builtin_cd(&mut self, argv: &[String], io: &Io) -> i32 {
        let dir = match argv.get(1) {
            Some(d) => d.clone(),
            None => self
                .state
                .env
                .get("HOME")
                .cloned()
                .unwrap_or_else(|| "/".to_string()),
        };
        let path = self.state.resolve(&dir);
        if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
            self.state.cwd = self.state.rel_to_root(&path);
            0
        } else {
            self.emit_str(
                &io.stderr,
                &format!("bash: cd: {dir}: No such file or directory\n"),
            );
            1
        }
    }

    pub(crate) fn builtin_pwd(&mut self, argv: &[String], io: &Io) -> i32 {
        let _ = argv;
        self.emit_str(&io.stdout, &format!("{}\n", self.state.cwd_display()));
        0
    }

    pub(crate) fn builtin_export(&mut self, argv: &[String]) -> i32 {
        let mut status = 0;
        for a in &argv[1..] {
            if let Some(eq) = a.find('=') {
                let (name, val) = (&a[..eq], &a[eq + 1..]);
                if valid_name(name) {
                    self.state.env.insert(name.to_string(), val.to_string());
                } else {
                    status = 1;
                }
            } else if !valid_name(a) {
                status = 1;
            }
        }
        status
    }

    pub(crate) fn builtin_unset(&mut self, argv: &[String]) -> i32 {
        for a in &argv[1..] {
            self.state.env.remove(a);
        }
        0
    }

    pub(crate) fn builtin_set(&mut self, argv: &[String], io: &Io) -> i32 {
        if argv.len() == 1 {
            let mut pairs: Vec<(String, String)> = self
                .state
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            pairs.sort();
            for (k, v) in pairs {
                self.emit_str(&io.stdout, &format!("{k}={v}\n"));
            }
            0
        } else {
            self.emit_str(&io.stderr, "bash: set: only bare `set` is supported\n");
            2
        }
    }

    /// `env` prints the environment, optionally after applying `NAME=value`
    /// prefixes; with a command it runs that command under the overridden
    /// environment.
    pub(crate) fn builtin_env(
        &mut self,
        argv: &[String],
        io: &Io,
        local_env: &std::collections::HashMap<String, String>,
    ) -> i32 {
        let mut overrides: HashMap<String, String> = local_env.clone();
        let mut idx = 1;
        while let Some(arg) = argv.get(idx) {
            if let Some(eq) = arg.find('=')
                && valid_name(&arg[..eq])
            {
                overrides.insert(arg[..eq].to_string(), arg[eq + 1..].to_string());
                idx += 1;
            } else {
                break;
            }
        }
        if idx >= argv.len() {
            let mut merged: HashMap<String, String> = self.state.env.clone();
            merged.extend(overrides);
            let mut pairs: Vec<_> = merged.into_iter().collect();
            pairs.sort();
            for (k, v) in pairs {
                self.emit_str(&io.stdout, &format!("{k}={v}\n"));
            }
            0
        } else {
            let saved: HashMap<String, Option<String>> = overrides
                .keys()
                .map(|k| (k.clone(), self.state.env.get(k).cloned()))
                .collect();
            self.state.env.extend(overrides);
            let status = self.dispatch(&argv[idx..], io, local_env);
            for (k, old) in saved {
                match old {
                    Some(v) => {
                        self.state.env.insert(k, v);
                    }
                    None => {
                        self.state.env.remove(&k);
                    }
                }
            }
            status
        }
    }

    pub(crate) fn builtin_exit(&mut self, argv: &[String]) -> i32 {
        let n = match argv.get(1) {
            Some(s) => s.parse::<i32>().unwrap_or(self.state.last_status),
            None => self.state.last_status,
        };
        self.state.exit_requested = Some(n);
        n
    }

    pub(crate) fn builtin_test(&mut self, args: &[String], io: &Io) -> i32 {
        let strs: Vec<&str> = args.iter().map(String::as_str).collect();
        if eval_test(&self.state, &strs) {
            0
        } else {
            let _ = io;
            1
        }
    }
}

fn push_escapes(out: &mut String, s: &str) {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
}

/// Render a printf format string, pulling arguments from `args[used..]`.
fn format_printf(fmt: &str, args: &[String], used: &mut usize, out: &mut String) {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' if i + 1 < chars.len() => {
                match chars[i + 1] {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '\\' => out.push('\\'),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
                i += 2;
            }
            '%' if i + 1 < chars.len() => {
                let spec = chars[i + 1];
                if spec == '%' {
                    out.push('%');
                    i += 2;
                    continue;
                }
                let arg = if *used < args.len() {
                    let a = args[*used].clone();
                    *used += 1;
                    a
                } else {
                    String::new()
                };
                match spec {
                    's' => out.push_str(&arg),
                    'd' | 'i' => {
                        let n: i64 = arg.parse().unwrap_or(0);
                        out.push_str(&n.to_string());
                    }
                    'f' => {
                        let f: f64 = arg.parse().unwrap_or(0.0);
                        out.push_str(&format!("{f}"));
                    }
                    _ => out.push('%'),
                }
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// Evaluate a `test`/`[` expression. Unsupported forms evaluate false.
fn eval_test(state: &ShellState, args: &[&str]) -> bool {
    match args {
        [] => false,
        [a] => !a.is_empty(),
        ["!", a] => a.is_empty(),
        [a, "=", b] | [a, "==", b] => a == b,
        [a, "!=", b] => a != b,
        ["!", a, "=", b] => a != b,
        ["!", a, "!=", b] => a == b,
        [a, "-eq", b] => num(a) == num(b),
        [a, "-ne", b] => num(a) != num(b),
        [a, "-lt", b] => num(a) < num(b),
        [a, "-le", b] => num(a) <= num(b),
        [a, "-gt", b] => num(a) > num(b),
        [a, "-ge", b] => num(a) >= num(b),
        [a, "-nt", b] => mtime(state, a) > mtime(state, b),
        [a, "-ot", b] => mtime(state, a) < mtime(state, b),
        ["-e", p] => state.resolve(p).exists(),
        ["-f", p] => std::fs::metadata(state.resolve(p)).is_ok_and(|m| m.is_file()),
        ["-d", p] => std::fs::metadata(state.resolve(p)).is_ok_and(|m| m.is_dir()),
        ["-s", p] => std::fs::metadata(state.resolve(p)).is_ok_and(|m| m.is_file() && m.len() > 0),
        ["-n", s] => !s.is_empty(),
        ["-z", s] => s.is_empty(),
        _ => false,
    }
}

fn num(s: &str) -> i64 {
    s.parse().unwrap_or(0)
}

fn mtime(state: &ShellState, p: &str) -> std::time::SystemTime {
    std::fs::metadata(state.resolve(p))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}
