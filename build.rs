use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let cargo_profile = if profile == "debug" { "dev" } else { &profile };

    for path in [
        "Cargo.toml",
        "crates/carson-ui/Cargo.toml",
        "crates/carson-ui/src",
        "crates/carson-ui/index.html",
        "crates/carson-ui/style.css",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            manifest_dir.join(path).display()
        );
    }

    let ui_wasm = build_ui(&manifest_dir, cargo_profile);
    let dist = manifest_dir.join("target").join("carson-ui-dist");
    let pkg = dist.join("pkg");
    std::fs::create_dir_all(&pkg).expect("create dist/pkg");
    run_wasm_bindgen(&ui_wasm, &pkg);
    std::fs::copy(
        manifest_dir.join("crates/carson-ui/index.html"),
        dist.join("index.html"),
    )
    .expect("copy index.html");
    std::fs::copy(
        manifest_dir.join("crates/carson-ui/style.css"),
        dist.join("style.css"),
    )
    .expect("copy style.css");
    println!("cargo:rustc-env=CARSON_UI_DIST={}", dist.display());
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
