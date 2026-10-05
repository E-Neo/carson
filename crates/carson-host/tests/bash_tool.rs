//! End-to-end tests for the embedded bash tool: interpreter component +
//! coreutils runner wired through the host exec shim.
use std::collections::HashMap;

use carson_host::registry::ToolDef;
use carson_host::tools::ToolRunner;
use serde_json::{Value, json};
use wasmtime::Engine;

fn bash_runner() -> &'static ToolRunner {
    static RUNNER: std::sync::OnceLock<ToolRunner> = std::sync::OnceLock::new();
    RUNNER.get_or_init(|| {
        let engine = Engine::new(&wasmtime::Config::new()).unwrap();
        let runner = ToolRunner::new(&engine);
        let def = ToolDef {
            id: "bash".into(),
            name: "bash".into(),
            description: String::new(),
            parameters: Value::Null,
            env: HashMap::new(),
        };
        let bash = carson_host::host::embedded_tool("bash").expect("bash wasm");
        runner
            .register_shell(&def, bash, carson_host::host::EMBEDDED_COREUTILS)
            .expect("register shell");
        runner
    })
}

fn run_bash(runner: &ToolRunner, script: &str) -> (String, String, i64) {
    let args = json!({ "command": script }).to_string();
    let out = runner
        .run("bash", &args)
        .expect("bash registered")
        .expect("bash invocation");
    let v: Value = serde_json::from_str(&out).expect("bash json result");
    let stdout = v["stdout"].as_str().unwrap_or_default().to_string();
    let stderr = v["stderr"].as_str().unwrap_or_default().to_string();
    let code = v["exit_code"].as_i64().unwrap_or(-1);
    (stdout, stderr, code)
}

#[test]
fn echo_through_wasm() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "echo hello world");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "hello world\n");
    assert_eq!(err, "");
}

#[test]
fn builtins_work() {
    let runner = bash_runner();
    let (out, _, code) = run_bash(&runner, "x=5; echo $x; exit 0");
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");

    let (out, _, _) = run_bash(&runner, "echo hi && echo there");
    assert_eq!(out, "hi\nthere\n");

    let (out, _, _) = run_bash(&runner, "for i in a b; do echo $i; done");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn coreutils_via_exec() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "echo hi | cat");
    assert_eq!(code, 0, "echo|cat failed: out={out:?} err={err:?}");
    assert_eq!(out, "hi\n");

    let (out, err, code) = run_bash(&runner, "touch u1.txt && ls u1.txt");
    assert_eq!(code, 0, "touch&ls failed: out={out:?} err={err:?}");
    assert_eq!(out, "u1.txt\n");

    let (out, err, code) = run_bash(&runner, "mkdir -p a/b && echo made");
    assert_eq!(code, 0, "mkdir failed: out={out:?} err={err:?}");
    assert_eq!(out, "made\n");
}

#[test]
fn date_command_runs() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "date");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!out.is_empty(), "date printed something: {out}");
}

#[test]
fn files_persist_across_calls() {
    let runner = bash_runner();
    assert_eq!(run_bash(&runner, "echo data > notes.txt").0, "");
    let (out, _, code) = run_bash(&runner, "cat notes.txt");
    assert_eq!(code, 0);
    assert_eq!(out, "data\n");
}

#[test]
fn command_not_found_is_127() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "nosuchbinary123");
    assert_eq!(out, "");
    assert!(err.contains("command not found"), "stderr: {err}");
    assert_eq!(code, 127);
}

#[test]
fn env_and_cwd() {
    let runner = bash_runner();
    let args = json!({
        "command": "echo $GREETING; pwd",
        "env": { "GREETING": "hi" },
        "cwd": "/"
    })
    .to_string();
    let out = runner.run("bash", &args).unwrap().unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["stdout"].as_str().unwrap(), "hi\n/\n");
}

#[test]
fn sandbox_sharing_and_isolation() {
    let runner = bash_runner();
    let base = std::env::temp_dir().join(format!("carson-sb-test-{}", std::process::id()));
    let root_a = base.join("a");
    let root_b = base.join("b");
    let run_in = |root: &std::path::Path, script: &str| -> (String, String, i64) {
        let args = json!({ "command": script }).to_string();
        let out = runner
            .run_in("bash", &args, Some(root))
            .expect("bash registered")
            .expect("bash invocation");
        let v: Value = serde_json::from_str(&out).expect("bash json result");
        (
            v["stdout"].as_str().unwrap_or_default().to_string(),
            v["stderr"].as_str().unwrap_or_default().to_string(),
            v["exit_code"].as_i64().unwrap_or(-1),
        )
    };

    // A shared sandbox: a file written in one call is visible in the next.
    assert_eq!(run_in(&root_a, "echo data > f.txt").0, "");
    let (out, _, code) = run_in(&root_a, "cat f.txt");
    assert_eq!(code, 0);
    assert_eq!(out, "data\n");

    // A different sandbox is isolated: the file is not there.
    let (_, err, code) = run_in(&root_b, "cat f.txt");
    assert_eq!(code, 1, "stderr: {err}");
    assert!(err.contains("f.txt"), "stderr: {err}");

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn vfs_layout_bin_tmp_home() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "ls /bin");
    assert_eq!(code, 0, "stderr: {err}");
    for cmd in ["ls", "cat", "sort", "wc", "date"] {
        assert!(
            out.lines().any(|l| l == cmd),
            "expected {cmd} in /bin, got:\n{out}"
        );
    }

    let (out, err, code) = run_bash(&runner, "test -d /tmp && test -d /home/carson && echo ok");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "ok\n");

    // /tmp is an ordinary directory and persists across calls.
    assert_eq!(run_bash(&runner, "echo x > /tmp/t.txt").0, "");
    let (out, _, code) = run_bash(&runner, "cat /tmp/t.txt");
    assert_eq!(code, 0);
    assert_eq!(out, "x\n");
}

#[test]
fn bin_commands_runnable_by_path() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "/bin/ls /bin | head -2");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!out.is_empty(), "expected listing, got {out:?}");

    let (out, err, code) = run_bash(&runner, "echo hi | /bin/cat");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "hi\n");
}

#[test]
fn env_defaults_and_home_cwd() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "echo $HOME $USER $LOGNAME $PATH");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "/home/carson carson carson /bin\n");

    // Default cwd is the carson home.
    let (out, err, code) = run_bash(&runner, "pwd");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "/home/carson\n");
}

#[test]
fn every_coreutils_command_runs_without_trapping() {
    let runner = bash_runner();
    // A trap inside a coreutils instance surfaces as exit 126 from the shell;
    // a normal util error (wrong args, missing file, ...) is a real exit code.
    for cmd in carson_shell::EXTERNAL_COMMANDS {
        let script = format!("{cmd} --version");
        let (out, err, code) = run_bash(&runner, &script);
        assert!(code != 126, "{cmd} trapped: out={out:?} err={err:?}");
    }
}

#[test]
fn dup_redirect_merges_stderr_into_stdout() {
    let runner = bash_runner();
    // Builtin: 2>&1 sends the write to stdout.
    let (out, err, code) = run_bash(&runner, "echo boom 2>&1");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "boom\n");
    assert_eq!(err, "");

    // External command: its stderr lands on the tool's stdout.
    let (out, err, code) = run_bash(&runner, "ls /nope 2>&1");
    assert_ne!(code, 0, "expected ls to fail");
    assert!(out.contains("nope"), "stdout: {out:?}");
    assert_eq!(err, "", "stderr should have been merged: {err:?}");
}

#[test]
fn dup_redirect_flows_through_a_pipeline() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "ls /nope 2>&1 | wc -c");
    assert_eq!(code, 0, "stderr: {err}");
    let count: usize = out.trim().parse().expect("wc printed a count");
    assert!(
        count > 0,
        "pipeline should carry the error text, got {out:?}"
    );
    assert_eq!(err, "");
}

#[test]
fn dup_redirect_ordering_and_files() {
    let runner = bash_runner();
    // `> f 2>&1`: both streams into the file.
    let (out, err, code) = run_bash(&runner, "ls /nope > log.txt 2>&1; cat log.txt");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("nope"), "file should hold the error: {out:?}");
    assert_eq!(err, "");

    // `2>&1 > f`: stderr stays on stdout (tool output), file holds stdout only.
    let (out, err, code) = run_bash(&runner, "ls /nope 2>&1 > log2.txt; cat log2.txt");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("nope"),
        "stderr belongs to the tool stdout: {out:?}"
    );
    assert_eq!(err, "");
}

#[test]
fn dup_redirect_append_and_success_path() {
    let runner = bash_runner();
    // 2>> append is untouched by the new dup lexing.
    let (out, err, code) = run_bash(
        &runner,
        "echo pre > app.txt; ls /nope 2>> app.txt; cat app.txt",
    );
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("pre"), "stdout: {out:?}");
    assert!(out.contains("nope"), "appended error missing: {out:?}");
    assert_eq!(err, "");

    // A success invocation carrying 2>&1 stays clean.
    let (out, err, code) = run_bash(&runner, "touch /tmp/f 2>&1 && echo ok");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "ok\n");
    assert_eq!(err, "");
}

#[test]
fn command_substitution_expands() {
    let runner = bash_runner();
    let (out, err, code) = run_bash(&runner, "x=$(echo hi); echo $x");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "hi\n");

    let (out, _, _) = run_bash(&runner, "echo $(echo a; echo b)");
    assert_eq!(out, "a b\n");

    let (out, _, _) = run_bash(&runner, "echo $(echo $(echo deep))");
    assert_eq!(out, "deep\n");

    let (out, err, code) = run_bash(&runner, "echo $(date)");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!out.trim().is_empty(), "date should print something");
}

#[test]
fn command_substitution_splitting_and_quoting() {
    let runner = bash_runner();
    // Unquoted $() word-splits the result; quoted $() keeps it as one field.
    let (out, _, code) = run_bash(&runner, "printf '%s|' $(echo a b)");
    assert_eq!(code, 0);
    assert_eq!(out, "a|b|");

    let (out, _, _) = run_bash(&runner, "printf '%s|' \"$(echo a b)\"");
    assert_eq!(out, "a b|");

    // Embedded in a larger word.
    let (out, _, _) = run_bash(&runner, "echo pre$(echo mid)post");
    assert_eq!(out, "premidpost\n");
}

#[test]
fn command_substitution_env_and_redirect_target() {
    let runner = bash_runner();
    // The outer environment is visible inside the substitution.
    let (out, err, code) = run_bash(&runner, "v=hello; echo $(echo $v)");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "hello\n");

    // A substitution may name the redirect target.
    let (out, err, code) = run_bash(&runner, "echo x > $(echo out.txt); cat out.txt");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "x\n");
}

#[test]
fn command_substitution_empty_and_errors() {
    let runner = bash_runner();
    // Empty substitution -> empty argument.
    let (out, err, code) = run_bash(&runner, "echo \"$(true)\"");
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out, "\n");

    // Inner failure prints to stderr and yields an empty substitution.
    let (out, err, code) = run_bash(&runner, "echo $(nosuchbinary123)");
    assert_eq!(code, 0, "the outer echo succeeds");
    assert_eq!(out, "\n");
    assert!(err.contains("command not found"), "stderr: {err:?}");
}

#[test]
fn command_substitution_combines_with_dup_redirect() {
    let runner = bash_runner();
    // The originally broken path: $(...) capturing a merged 2>&1 stream.
    let (out, err, code) = run_bash(&runner, "x=$(ls /nope 2>&1); echo $x");
    assert_eq!(code, 0, "stderr: {err}");
    assert!(
        out.contains("nope"),
        "substitution should hold the error: {out:?}"
    );
    assert_eq!(err, "");
}
