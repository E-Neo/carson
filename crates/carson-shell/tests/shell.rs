//! End-to-end tests for the carson shell interpreter.
use std::collections::HashMap;
use std::path::PathBuf;

use carson_shell::{Exec, NoExec, run_script};

/// A tiny fake command runner for tests: implements a few coreutils over a
/// sandbox root so pipelines and redirections can be exercised natively.
struct FakeExec {
    root: PathBuf,
}

impl Exec for FakeExec {
    fn run(
        &mut self,
        prog: &str,
        argv: &[String],
        env: &HashMap<String, String>,
        cwd: &str,
        stdin: &[u8],
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> i32 {
        let base = self.root.join(cwd.trim_start_matches('/'));
        match prog {
            "cat" => {
                if argv.len() > 1 {
                    for f in &argv[1..] {
                        match std::fs::read(base.join(f)) {
                            Ok(bytes) => stdout.extend_from_slice(&bytes),
                            Err(e) => {
                                let _ = e;
                                stderr.extend_from_slice(format!("cat: {f}: no such file\n").as_bytes());
                                return 1;
                            }
                        }
                    }
                } else {
                    stdout.extend_from_slice(stdin);
                }
                0
            }
            "env" => {
                let mut pairs: Vec<_> = env.iter().collect();
                pairs.sort();
                for (k, v) in pairs {
                    stdout.extend_from_slice(format!("{k}={v}\n").as_bytes());
                }
                0
            }
            "date" => {
                stdout.extend_from_slice(b"2026-08-29\n");
                0
            }
            "mkdir" => {
                if let Some(dir) = argv.get(1) {
                    if std::fs::create_dir_all(base.join(dir)).is_err() {
                        stderr.extend_from_slice(format!("mkdir: {dir}: failed\n").as_bytes());
                        return 1;
                    }
                }
                0
            }
            "touch" => {
                for f in &argv[1..] {
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(base.join(f))
                        .ok();
                }
                0
            }
            "ls" => {
                let dir = argv.get(1).map(String::as_str).unwrap_or(".");
                let read = std::fs::read_dir(base.join(dir)).ok();
                match read {
                    Some(entries) => {
                        let mut names: Vec<String> = entries
                            .flatten()
                            .filter_map(|e| e.file_name().into_string().ok())
                            .collect();
                        names.sort();
                        for n in names {
                            stdout.extend_from_slice(format!("{n}\n").as_bytes());
                        }
                        0
                    }
                    None => {
                        stderr.extend_from_slice(format!("ls: {dir}: no such file\n").as_bytes());
                        2
                    }
                }
            }
            "cp" => {
                let (Some(src), Some(dst)) = (argv.get(1), argv.get(2)) else {
                    return 2;
                };
                match std::fs::copy(base.join(src), base.join(dst)) {
                    Ok(_) => 0,
                    Err(_) => 1,
                }
            }
            "mv" => {
                let (Some(src), Some(dst)) = (argv.get(1), argv.get(2)) else {
                    return 2;
                };
                match std::fs::rename(base.join(src), base.join(dst)) {
                    Ok(_) => 0,
                    Err(_) => 1,
                }
            }
            "rm" => {
                for f in &argv[1..] {
                    let p = base.join(f);
                    if p.is_dir() {
                        std::fs::remove_dir_all(p).ok();
                    } else {
                        std::fs::remove_file(p).ok();
                    }
                }
                0
            }
            _ => {
                stderr.extend_from_slice(format!("{prog}: not found\n").as_bytes());
                127
            }
        }
    }
}

struct Harness {
    root: PathBuf,
    env: HashMap<String, String>,
}

impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("carson-shell-test-{}", uuid()));
        std::fs::create_dir_all(&root).unwrap();
        Harness {
            root,
            env: HashMap::new(),
        }
    }

    fn with_env(mut self, key: &str, val: &str) -> Self {
        self.env.insert(key.to_string(), val.to_string());
        self
    }

    fn write(&self, path: &str, content: &str) -> &Self {
        let p = self.root.join(path.trim_start_matches('/'));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
        self
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path.trim_start_matches('/'))).unwrap()
    }

    fn run(&self, script: &str) -> Out {
        let mut exec = FakeExec {
            root: self.root.clone(),
        };
        let res = run_script(script, &self.env, self.root.clone(), &mut exec).unwrap();
        Out {
            stdout: res.stdout,
            stderr: res.stderr,
            status: res.status,
        }
    }
}

struct Out {
    stdout: String,
    stderr: String,
    status: i32,
}

impl Out {
    fn out(&self) -> &str {
        self.stdout.trim_end()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .to_string()
}

#[test]
fn echo_and_printf() {
    let h = Harness::new();
    assert_eq!(h.run("echo hello world").out(), "hello world");
    assert_eq!(h.run("echo -n no-newline").out(), "no-newline");
    assert_eq!(h.run(r"echo -e 'a\nb'").out(), "a\nb");
    assert_eq!(h.run(r"printf '%s-%d\n' foo 42").out(), "foo-42");
}

#[test]
fn variables_and_expansion() {
    let h = Harness::new();
    assert_eq!(h.run("x=hello; echo $x").out(), "hello");
    assert_eq!(h.run("echo ${x}").out(), "");
    assert_eq!(h.run("x=hello; echo ${x}world").out(), "helloworld");
    assert_eq!(h.run("echo \"$x\"").out(), "");
    assert_eq!(h.run("x='a b'; echo $x").out(), "a b");
    assert_eq!(h.run("x='a b'; echo \"$x\"").out(), "a b");
    assert_eq!(h.run("x=a; y=b; echo $x$y").out(), "ab");
}

#[test]
fn word_splitting_and_quotes() {
    let h = Harness::new();
    assert_eq!(h.run("x='1 2 3'; printf '<%s> ' $x").out(), "<1> <2> <3>");
    assert_eq!(h.run("x='1 2 3'; printf '<%s> ' \"$x\"").out(), "<1 2 3>");
    assert_eq!(h.run("printf '<%s> ' a\"b\"c").out(), "<abc>");
}

#[test]
fn exit_status_dollar_question() {
    let h = Harness::new();
    let r = h.run("false; echo $?");
    assert_eq!(r.out(), "1");
    let r = h.run("true; echo $?");
    assert_eq!(r.out(), "0");
    let r = h.run("nosuchcmd; echo $?");
    assert_eq!(r.out(), "127");
    assert!(r.stderr.contains("command not found"));
    assert_eq!(h.run("exit 3").status, 3);
}

#[test]
fn and_or_short_circuit() {
    let h = Harness::new();
    assert_eq!(h.run("true && echo yes").out(), "yes");
    assert_eq!(h.run("false && echo no").out(), "");
    assert_eq!(h.run("false || echo fallback").out(), "fallback");
    assert_eq!(h.run("true || echo no").out(), "");
    assert_eq!(h.run("! false && echo negated").out(), "negated");
}

#[test]
fn if_elif_else() {
    let h = Harness::new();
    assert_eq!(h.run("if true; then echo a; fi").out(), "a");
    assert_eq!(h.run("if false; then echo a; else echo b; fi").out(), "b");
    assert_eq!(
        h.run("if false; then echo a; elif true; then echo b; else echo c; fi").out(),
        "b"
    );
    assert_eq!(
        h.run("if false; then echo a; elif false; then echo b; else echo c; fi").out(),
        "c"
    );
}

#[test]
fn for_loop() {
    let h = Harness::new();
    assert_eq!(h.run("for i in a b c; do echo $i; done").out(), "a\nb\nc");
    assert_eq!(h.run("for i in 1 2; do echo -n $i; done").out(), "12");
}

#[test]
fn while_loop() {
    let h = Harness::new();
    assert_eq!(h.run("n=2; while [ $n -gt 0 ]; do echo $n; n=0; done").out(), "2");
    assert_eq!(h.run("while false; do echo x; done").out(), "");
}

#[test]
fn pipeline_with_fake_exec() {
    let h = Harness::new();
    assert_eq!(h.run("echo hi | cat").out(), "hi");
    assert_eq!(h.run("echo -e 'a\nb\nc' | cat").out(), "a\nb\nc");
    // two-stage pipeline
    assert_eq!(h.run("echo hi | cat | cat").out(), "hi");
}

#[test]
fn command_substitution() {
    let h = Harness::new();
    assert_eq!(h.run("x=$(echo hi); echo $x").out(), "hi");
    assert_eq!(h.run("echo $(echo a)$(echo b)").out(), "ab");
    assert_eq!(h.run("echo \"$(echo a b)\"").out(), "a b");
    assert_eq!(h.run("echo $(echo -e 'x\ny')").out(), "x y");
}

#[test]
fn subshell_isolation() {
    let h = Harness::new();
    let r = h.run("x=outer; (x=inner; echo $x); echo $x");
    assert_eq!(r.out(), "inner\nouter");
    // status propagates
    let r = h.run("(exit 5); echo $?");
    assert_eq!(r.out(), "5");
}

#[test]
fn redirections() {
    let h = Harness::new();
    h.run("echo hi > f.txt");
    assert_eq!(h.read("f.txt"), "hi\n");
    h.run("echo more >> f.txt");
    assert_eq!(h.read("f.txt"), "hi\nmore\n");
    h.run("echo err >&2");
    let r = h.run("true");
    assert!(r.stderr.is_empty(), "no stderr from previous");
    // 2> redirects stderr to a file
    let r = h.run("nosuchcmd 2> err.txt");
    assert_eq!(r.stdout, "");
    assert_eq!(h.read("err.txt"), "bash: nosuchcmd: command not found\n");
}

#[test]
fn heredocs_and_here_strings() {
    let h = Harness::new();

    // Basic heredoc feeds the body to stdin.
    let r = h.run("cat <<EOF\nhello\nEOF\n");
    assert_eq!(r.out(), "hello");

    // Multi-line body, both expansion and quoted-delimiter literal forms.
    let r = h.run("x=world; cat <<EOF\na\nhi $x\nEOF\n");
    assert_eq!(r.out(), "a\nhi world");
    let r = h.run("x=world; cat <<'EOF'\nhi $x\nEOF\n");
    assert_eq!(r.out(), "hi $x");

    // <<- strips leading tabs from body and delimiter.
    let r = h.run("cat <<-EOF\n\thello\n\tEOF\n");
    assert_eq!(r.out(), "hello");

    // Command substitution and $? expand inside an unquoted heredoc.
    let r = h.run("cat <<EOF\n$(echo sub) $?\nEOF\n");
    assert_eq!(r.out(), "sub 0");

    // Heredoc feeding a pipeline.
    let r = h.run("cat <<EOF | cat\na\nb\nEOF\n");
    assert_eq!(r.out(), "a\nb");

    // Heredoc inside command substitution.
    let r = h.run("x=$(cat <<EOF\nhi\nEOF\n); echo $x");
    assert_eq!(r.out(), "hi");

    // Here-strings append a newline and expand the word.
    let r = h.run("cat <<< hello");
    assert_eq!(r.out(), "hello");
    let r = h.run("x=abc; cat <<< \"$x\"");
    assert_eq!(r.out(), "abc");
}

#[test]
fn test_builtin() {
    let h = Harness::new();
    h.write("a.txt", "x");
    assert_eq!(h.run("test -f a.txt && echo yes").out(), "yes");
    assert_eq!(h.run("[ -f a.txt ] && echo yes").out(), "yes");
    assert_eq!(h.run("test -d / && echo yes").out(), "yes");
    assert_eq!(h.run("test -e nope || echo no").out(), "no");
    assert_eq!(h.run("test -z '' && echo z").out(), "z");
    assert_eq!(h.run("test -n x && echo n").out(), "n");
    assert_eq!(h.run("test 3 -lt 5 && echo lt").out(), "lt");
    assert_eq!(h.run("test a = a && echo eq").out(), "eq");
    assert_eq!(h.run("test a != b && echo ne").out(), "ne");
}

#[test]
fn cd_and_pwd() {
    let h = Harness::new();
    h.run("mkdir sub");
    assert_eq!(h.run("cd sub; pwd").out(), "/sub");
    assert_eq!(h.run("cd /; pwd").out(), "/");
    assert_eq!(h.run("cd sub; cd ..; pwd").out(), "/");
    let r = h.run("cd nope; echo $?");
    assert_eq!(r.out(), "1");
}

#[test]
fn export_unset_env() {
    let h = Harness::new().with_env("START", "1");
    assert_eq!(h.run("export FOO=bar; env").out(), "FOO=bar\nSTART=1");
    assert_eq!(h.run("export FOO=bar; unset FOO; env").out(), "START=1");
    assert_eq!(h.run("unset START; echo $START").out(), "");
}

#[test]
fn env_override_prints_or_runs() {
    let h = Harness::new().with_env("BASE", "1");
    assert_eq!(h.run("env").out(), "BASE=1");
    assert_eq!(h.run("A=x env | cat").out(), "A=x\nBASE=1");
    // override does not leak into the parent shell
    assert_eq!(h.run("A=x env > /dev/null; echo $A").out(), "");
}

#[test]
fn assignments_and_commands() {
    let h = Harness::new();
    // bare assignment persists
    assert_eq!(h.run("A=1; echo $A").out(), "1");
    // assignment before a command does not persist
    assert_eq!(h.run("A=2 env; echo $A").out(), "A=2");
}

#[test]
fn brace_group() {
    let h = Harness::new();
    assert_eq!(h.run("{ echo a; echo b; }").out(), "a\nb");
    // braces share state
    assert_eq!(h.run("{ x=1; }; echo $x").out(), "1");
}

#[test]
fn comments_and_blank_lines() {
    let h = Harness::new();
    assert_eq!(h.run("echo a # comment\n# full line\n echo b").out(), "a\nb");
    assert_eq!(h.run("echo a\n\n\necho b").out(), "a\nb");
}

#[test]
fn parse_errors_are_loud() {
    let h = Harness::new();
    assert!(h.run_script_err("if true; then echo").is_some());
    assert!(h.run_script_err("for x in; do").is_some());
    assert!(h.run_script_err("echo \"unterminated").is_some());
}

impl Harness {
    fn run_script_err(&self, script: &str) -> Option<String> {
        let mut exec = NoExec;
        run_script(script, &self.env, self.root.clone(), &mut exec).err()
    }
}

#[test]
fn background_and_separators() {
    let h = Harness::new();
    assert_eq!(h.run("echo a; echo b").out(), "a\nb");
    assert_eq!(h.run("echo a && echo b & echo c").out(), "a\nb\nc");
    assert_eq!(h.run("echo a\necho b").out(), "a\nb");
}
