use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use wasmtime::{Config, Engine};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("..").join("..");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let embed_target = workspace_root.join("target").join("wasm32-wasip2-embed");

    // Embedded wasm is always built with the optimized `wasm` profile so it
    // precompiles fast and loads quickly at startup, regardless of the host
    // build profile (dev/release).
    const WASM_PROFILE: &str = "wasm";

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

    let out_dir = embed_target.join("wasm32-wasip2").join(WASM_PROFILE);
    const ARTIFACTS: [(&str, &str); 4] = [
        ("carson_agent", "carson_agent.wasm"),
        ("time", "time.wasm"),
        ("bash", "bash.wasm"),
        ("coreutils", "coreutils.wasm"),
    ];

    // Reuse existing artifacts unless some watched input is newer than all of
    // them (both the wasm and its precompiled cwasm must be fresh).
    let up_to_date = match newest_mtime(&watch_inputs) {
        Some(newest) => ARTIFACTS.iter().all(|(name, file)| {
            let wasm = out_dir.join(file);
            let cwasm = out_dir.join(format!("{name}.cwasm"));
            file_mtime(&wasm).is_some_and(|m| m >= newest)
                && file_mtime(&cwasm).is_some_and(|m| m >= newest)
        }),
        None => false,
    };

    let artifacts: HashMap<String, PathBuf> = if up_to_date {
        ARTIFACTS
            .iter()
            .map(|(name, file)| ((*name).to_string(), out_dir.join(file)))
            .collect()
    } else {
        let built = build_wasm_packages(&cargo, &workspace_root, &embed_target, WASM_PROFILE);
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
    for (name, wasm_key, cwasm_key) in [
        ("carson_agent", "CARSON_AGENT_WASM", "CARSON_AGENT_CWASM"),
        ("time", "CARSON_TOOL_TIME_WASM", "CARSON_TOOL_TIME_CWASM"),
        ("bash", "CARSON_TOOL_BASH_WASM", "CARSON_TOOL_BASH_CWASM"),
        (
            "coreutils",
            "CARSON_TOOL_COREUTILS_WASM",
            "CARSON_TOOL_COREUTILS_CWASM",
        ),
    ] {
        let wasm = &artifacts[name];
        let cwasm = out_dir.join(format!("{name}.cwasm"));
        precompile_if_stale(wasm, &cwasm);
        println!("cargo:rustc-env={wasm_key}={}", wasm.display());
        println!("cargo:rustc-env={cwasm_key}={}", cwasm.display());
        let wasm_bytes =
            std::fs::read(wasm).unwrap_or_else(|e| panic!("read {}: {e}", wasm.display()));
        let cwasm_bytes =
            std::fs::read(&cwasm).unwrap_or_else(|e| panic!("read {}: {e}", cwasm.display()));
        println!("cargo:rustc-env={wasm_key}_FNV={}", fnv1a(&wasm_bytes));
        println!("cargo:rustc-env={cwasm_key}_FNV={}", fnv1a(&cwasm_bytes));
    }
}

/// Precompile a component to a `.cwasm` artifact when the existing one is
/// missing or older than the wasm source.
fn precompile_if_stale(wasm: &Path, cwasm: &Path) {
    let stale = match file_mtime(cwasm) {
        Some(c) => file_mtime(wasm).is_none_or(|w| w > c),
        None => true,
    };
    if !stale {
        return;
    }
    let bytes = std::fs::read(wasm).unwrap_or_else(|e| panic!("read {}: {e}", wasm.display()));
    let engine = Engine::new(&Config::new())
        .unwrap_or_else(|e| panic!("create wasmtime engine for precompile: {e}"));
    let compiled = engine
        .precompile_component(&bytes)
        .unwrap_or_else(|e| panic!("precompile {}: {e}", wasm.display()));
    // Only rewrite when content actually differs, so the artifact mtime (and
    // therefore the rebuild fingerprint) stays stable when nothing changed.
    let same = std::fs::read(cwasm)
        .map(|existing| existing == compiled.as_slice())
        .unwrap_or(false);
    if !same {
        std::fs::write(cwasm, &compiled)
            .unwrap_or_else(|e| panic!("write {}: {e}", cwasm.display()));
        println!("cargo:rerun-if-changed={}", wasm.display());
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
