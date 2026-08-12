use carson_host::config::Config;
use carson_host::tools::ToolRunner;
use wasmtime::Engine;

fn tools_config() -> Config {
    toml::from_str(
        r#"
[tools.time]
description = "Return the current unix time in milliseconds"

[tools.echo]
description = "Echo back the provided arguments"
"#,
    )
    .unwrap()
}

fn runner() -> ToolRunner {
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    ToolRunner::new(&engine, &tools_config()).unwrap()
}

#[test]
fn sandboxed_time_tool_returns_unix_ms() {
    let runner = runner();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let out = runner.run("time", "{}").unwrap().unwrap();
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let unix_ms = value["unix_ms"].as_u64().unwrap();
    assert!((before..=after).contains(&(unix_ms as u128)));
}

#[test]
fn sandboxed_echo_tool_returns_args() {
    let runner = runner();
    assert_eq!(runner.run("echo", "abc").unwrap().unwrap(), "abc");
}

#[test]
fn unknown_tool_is_none() {
    let runner = runner();
    assert!(runner.run("nope", "{}").is_none());
}

#[test]
fn third_party_tool_from_disk() {
    let bytes = carson_host::host::embedded_tool("echo").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("third-party.wasm");
    std::fs::write(&path, bytes).unwrap();

    let config: Config = toml::from_str(&format!(
        r#"
[tools.custom]
module = "{}"
description = "Third-party tool"
"#,
        path.display()
    ))
    .unwrap();
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    let runner = ToolRunner::new(&engine, &config).unwrap();
    assert_eq!(runner.run("custom", "hello").unwrap().unwrap(), "hello");
}
