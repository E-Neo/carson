#![no_main]
#![crate_type = "cdylib"]

mod wit {
    wit_bindgen::generate!({
        path: "wit-shell",
        world: "carson:shell/bash-world",
    });
}

use std::collections::HashMap;

use serde_json::json;
use wit::carson::shell::exec::run as host_exec_run;
use wit::exports::carson::tool::tool::{Guest, ToolError};

use carson_shell::Exec;

/// Routes non-builtin commands to the host exec shim, which runs them as a
/// separate sandboxed coreutils instance and returns captured stdio.
struct HostExec;

impl Exec for HostExec {
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
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        match host_exec_run(prog, argv, &env, cwd, stdin) {
            Ok(result) => {
                *stdout = result.stdout;
                *stderr = result.stderr;
                result.status as i32
            }
            Err(e) => {
                stderr
                    .extend_from_slice(format!("bash: {prog}: execution failed: {e}\n").as_bytes());
                126
            }
        }
    }
}

/// `{ command, cwd?, env? }` in -> `{ stdout, stderr, exit_code }` out.
struct BashTool;

impl Guest for BashTool {
    fn run(arguments_json: String) -> Result<String, ToolError> {
        let args: serde_json::Value =
            serde_json::from_str(&arguments_json).map_err(|_| ToolError::Failed)?;
        let command = args["command"]
            .as_str()
            .ok_or(ToolError::Failed)?
            .to_string();
        let cwd = args["cwd"].as_str().unwrap_or("/home/carson");
        let mut env: HashMap<String, String> = std::env::vars().collect();
        if let Some(extra) = args["env"].as_object() {
            for (k, v) in extra {
                env.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
            }
        }
        const ROOT: &str = "/";
        let mut exec = HostExec;
        let result = carson_shell::run_script_with_cwd(&command, &env, ROOT, cwd, &mut exec);
        let (stdout, stderr, exit_code) = match result {
            Ok(r) => (r.stdout, r.stderr, r.status),
            Err(e) => (String::new(), e, 2),
        };
        Ok(json!({ "stdout": stdout, "stderr": stderr, "exit_code": exit_code }).to_string())
    }
}

wit::export!(BashTool with_types_in wit);
