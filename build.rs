use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let cargo_profile = if profile == "debug" {
        "dev"
    } else {
        profile.as_str()
    };

    // Watched inputs: everything that can change the generated UI bundle.
    let mut watch_inputs: Vec<PathBuf> = [
        "Cargo.toml",
        "crates/carson-ui/Cargo.toml",
        "crates/carson-ui/src",
        "crates/carson-ui/index.html",
        "crates/carson-ui/index.js",
        "crates/carson-ui/style.css",
    ]
    .iter()
    .map(|path| manifest_dir.join(path))
    .collect();
    watch_inputs.push(manifest_dir.join("Cargo.lock"));
    watch_inputs.push(manifest_dir.join("build.rs"));
    for path in &watch_inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // Per-profile dist: profiles must not overwrite each other's UI bundle.
    let dist = manifest_dir
        .join("target")
        .join("carson-ui-dist")
        .join(&profile);
    let pkg = dist.join("pkg");
    std::fs::create_dir_all(&pkg).expect("create dist/pkg");

    let inputs_newest = newest_mtime(&watch_inputs);
    let up_to_date = match inputs_newest {
        Some(newest) => {
            stamp_of(&pkg).as_deref() == Some(cargo_profile)
                && ["carson_ui.js", "carson_ui_bg.wasm"].iter().all(|file| {
                    let path = pkg.join(file);
                    file_mtime(&path).is_some_and(|mtime| mtime >= newest)
                })
        }
        None => false,
    };

    if !up_to_date {
        let ui_wasm = build_ui(&manifest_dir, cargo_profile);
        run_wasm_bindgen(&ui_wasm, &pkg);
        std::fs::write(pkg.join("profile.txt"), cargo_profile).expect("write pkg profile stamp");
    }

    copy_if_changed(
        &manifest_dir.join("crates/carson-ui/index.html"),
        &dist.join("index.html"),
    );
    copy_if_changed(
        &manifest_dir.join("crates/carson-ui/index.js"),
        &dist.join("index.js"),
    );
    copy_if_changed(
        &manifest_dir.join("crates/carson-ui/style.css"),
        &dist.join("style.css"),
    );

    println!("cargo:rustc-env=CARSON_UI_DIST={}", dist.display());
    println!(
        "cargo:rustc-env=CARSON_UI_DIST_SUM={}",
        dist_sum(&dist, &pkg)
    );
}

/// Hash every embedded file in a fixed order so identical bundles produce
/// identical output and content changes always dirty the binary.
fn dist_sum(dist: &Path, pkg: &Path) -> String {
    let files = [
        dist.join("index.html"),
        dist.join("index.js"),
        dist.join("style.css"),
        pkg.join("carson_ui.js"),
        pkg.join("carson_ui_bg.wasm"),
    ];
    let mut combined = Vec::new();
    for file in &files {
        let bytes = std::fs::read(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        combined.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        combined.extend_from_slice(&bytes);
    }
    fnv1a(&combined)
}

fn stamp_of(pkg: &Path) -> Option<String> {
    std::fs::read_to_string(pkg.join("profile.txt")).ok()
}

/// Copy `src` to `dst` only when the destination bytes differ.
fn copy_if_changed(src: &Path, dst: &Path) {
    if let (Ok(current), Ok(new)) = (std::fs::read(dst), std::fs::read(src))
        && current == new
    {
        return;
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).expect("create dist dir");
    }
    std::fs::copy(src, dst)
        .unwrap_or_else(|e| panic!("copy {} -> {}: {e}", src.display(), dst.display()));
}

fn build_ui(workspace_root: &Path, cargo_profile: &str) -> PathBuf {
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()));
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(workspace_root.join("Cargo.toml"))
        .args(["-p", "carson-ui"])
        .arg("--target")
        .arg("wasm32-unknown-unknown")
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
    cmd.env(
        "CARGO_TARGET_DIR",
        workspace_root
            .join("target")
            .join("wasm32-unknown-unknown-embed"),
    );

    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo for carson-ui: {e}"));
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("building carson-ui for wasm32-unknown-unknown failed:\n{stderr}");
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
            .map(|id| id.contains("carson-ui"))
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
        .remove("carson_ui")
        .unwrap_or_else(|| panic!("cargo did not report the carson-ui cdylib artifact"))
}

fn run_wasm_bindgen(wasm: &Path, out_dir: &Path) {
    let bin = find_tool("wasm-bindgen");
    let status = Command::new(bin)
        .arg("--target")
        .arg("web")
        .arg("--out-dir")
        .arg(out_dir)
        .arg("--no-typescript")
        .arg(wasm)
        .status()
        .unwrap_or_else(|e| panic!("failed to run wasm-bindgen: {e}"));
    assert!(status.success(), "wasm-bindgen failed");
}

fn find_tool(name: &str) -> PathBuf {
    if let Ok(path) = env::var("CARGO_HOME") {
        let candidate = PathBuf::from(path).join("bin").join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    if let Some(candidate) = PathBuf::from(name).parent()
        && !candidate.as_os_str().is_empty()
    {
        return PathBuf::from(name);
    }
    let home = env::var("HOME").map(PathBuf::from).ok();
    if let Some(home) = home {
        let candidate = home.join(".cargo/bin").join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
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
