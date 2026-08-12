use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let workspace_root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let cargo_profile = if profile == "debug" { "dev" } else { &profile };
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let embed_target = workspace_root.join("target").join("wasm32-wasip2-embed");

    for dir in [
        "crates/carson-agent",
        "crates/carson-tools",
        "wit",
        "crates/carson-tools/wit",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(dir).display()
        );
    }

    let artifacts = build_wasm_packages(&cargo, &workspace_root, &embed_target, cargo_profile);
    for path in artifacts.values() {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let agent = artifacts
        .get("carson_agent")
        .expect("cargo did not report the carson-agent cdylib artifact");
    println!("cargo:rustc-env=CARSON_AGENT_WASM={}", agent.display());

    for tool in ["time", "echo"] {
        let path = artifacts
            .get(tool)
            .unwrap_or_else(|| panic!("cargo did not report the carson-tools {tool} artifact"));
        println!(
            "cargo:rustc-env=CARSON_TOOL_{}_WASM={}",
            tool.to_uppercase(),
            path.display()
        );
    }
}

fn build_wasm_packages(
    cargo: &str,
    workspace_root: &Path,
    embed_target: &Path,
    cargo_profile: &str,
) -> HashMap<String, PathBuf> {
    let mut cmd = Command::new(cargo);
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(workspace_root.join("Cargo.toml"))
        .args(["-p", "carson-agent", "-p", "carson-tools"])
        .arg("--target")
        .arg("wasm32-wasip2")
        .arg("--profile")
        .arg(cargo_profile)
        .args(["--locked", "--message-format=json-render-diagnostics"]);

    let stale_vars = [
        "OUT_DIR",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "TARGET",
        "HOST",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTDOC",
        "NUM_JOBS",
        "CARGO",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_PATH",
        "CARGO_MANIFEST_LINKS",
    ];
    for key in stale_vars {
        cmd.env_remove(key);
    }
    for (key, _) in env::vars() {
        if key.starts_with("CARGO_PKG_")
            || key.starts_with("CARGO_CRATE_")
            || key.starts_with("CARGO_FEATURE_")
            || key.starts_with("CARGO_CFG_")
            || key == "CARGO_MANIFEST_LINKS"
        {
            cmd.env_remove(key);
        }
    }
    cmd.env("CARGO_TARGET_DIR", embed_target);

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo for wasm packages: {e}"));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("building wasm packages for wasm32-wasip2 failed:\n{stderr}");
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut artifacts: HashMap<String, PathBuf> = HashMap::new();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"].as_str() != Some("compiler-artifact") {
            continue;
        }
        let in_package = message["package_id"]
            .as_str()
            .map(|id| id.contains("carson-agent") || id.contains("carson-tools"))
            .unwrap_or(false);
        if !in_package {
            continue;
        }
        let Some(name) = message["target"]["name"].as_str() else {
            continue;
        };
        let Some(wasm) = message["filenames"].as_array().and_then(|files| {
            files
                .iter()
                .filter_map(|f| f.as_str())
                .find(|f| f.ends_with(".wasm") && !f.contains("/deps/"))
                .map(PathBuf::from)
        }) else {
            continue;
        };
        artifacts.insert(name.to_string(), wasm);
    }
    artifacts
}
