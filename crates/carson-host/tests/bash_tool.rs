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
        assert!(out.lines().any(|l| l == cmd), "expected {cmd} in /bin, got:\n{out}");
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
        assert!(
            code != 126,
            "{cmd} trapped: out={out:?} err={err:?}"
        );
    }
}