use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..").join("..");
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let cargo_profile = if profile == "debug" {
        "dev"
    } else {
        profile.as_str()
    };
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let embed_target = workspace_root.join("target").join("wasm32-wasip2-embed");

    // Watched inputs: everything that can change the generated wasm packages.
    let mut watch_inputs: Vec<PathBuf> = [
        "crates/carson-agent",
        "crates/carson-tools",
        "crates/carson-shell",
        "wit",
        "crates/carson-tools/wit",
    ]
    .iter()
    .map(|dir| workspace_root.join(dir))
    .collect();
    watch_inputs.push(workspace_root.join("Cargo.toml"));
    watch_inputs.push(workspace_root.join("Cargo.lock"));
    watch_inputs.push(manifest_dir.join("build.rs"));
    for path in &watch_inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let out_dir = embed_target.join("wasm32-wasip2").join(&profile);
    const ARTIFACTS: [(&str, &str); 4] = [
        ("carson_agent", "carson_agent.wasm"),
        ("time", "time.wasm"),
        ("bash", "bash.wasm"),
        ("coreutils", "coreutils.wasm"),
    ];

    // Reuse existing artifacts unless some watched input is newer than all of them.
    let up_to_date = match newest_mtime(&watch_inputs) {
        Some(newest) => ARTIFACTS.iter().all(|(_, file)| {
            let path = out_dir.join(file);
            file_mtime(&path).is_some_and(|mtime| mtime >= newest)
        }),
        None => false,
    };

    let artifacts: HashMap<String, PathBuf> = if up_to_date {
        ARTIFACTS
            .iter()
            .map(|(name, file)| ((*name).to_string(), out_dir.join(file)))
            .collect()
    } else {
        let built = build_wasm_packages(&cargo, &workspace_root, &embed_target, cargo_profile);
        for (name, _) in ARTIFACTS {
            assert!(
                built.contains_key(name),
                "cargo did not report the {name} wasm artifact"
            );
        }
        // A fresh nested build does not restore user-deleted final artifacts;
        // relink them from the cached deps/ copies.
        for (_, file) in ARTIFACTS {
            let path = out_dir.join(file);
            if !path.exists()
                && let Some(parent) = path.parent()
            {
                let cached = parent.join("deps").join(file);
                if cached.exists() {
                    std::fs::copy(&cached, &path)
                        .unwrap_or_else(|e| panic!("copy {}: {e}", cached.display()));
                }
            }
        }
        built
    };

    // Deterministic emission order: identical content must produce identical output text.
    for (name, env_key) in [
        ("carson_agent", "CARSON_AGENT_WASM"),
        ("time", "CARSON_TOOL_TIME_WASM"),
        ("bash", "CARSON_TOOL_BASH_WASM"),
        ("coreutils", "CARSON_TOOL_COREUTILS_WASM"),
    ] {
        let path = &artifacts[name];
        println!("cargo:rustc-env={env_key}={}", path.display());
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        println!("cargo:rustc-env={env_key}_FNV={}", fnv1a(&bytes));
    }
}

/// Build the wasm packages with a nested cargo invocation.
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

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Newest mtime across `paths`, recursing into directories. Returns `None` when
/// any watched path cannot be stat'ed so callers take the rebuild path.
fn newest_mtime(paths: &[PathBuf]) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    fn walk(path: &Path, newest: &mut Option<SystemTime>) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if let Ok(mtime) = meta.modified()
            && newest.is_none_or(|current| mtime > current)
        {
            *newest = Some(mtime);
        }
        if !meta.is_dir() {
            return true;
        }
        let Ok(entries) = std::fs::read_dir(path) else {
            return false;
        };
        for entry in entries.flatten() {
            if !walk(&entry.path(), newest) {
                return false;
            }
        }
        true
    }
    for path in paths {
        if !walk(path, &mut newest) {
            return None;
        }
    }
    newest
}

fn fnv1a(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}
