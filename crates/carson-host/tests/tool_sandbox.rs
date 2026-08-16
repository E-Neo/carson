use carson_host::registry::ToolDef;
use carson_host::tools::ToolRunner;
use wasmtime::Engine;

fn runner() -> ToolRunner {
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    let runner = ToolRunner::new(&engine);
    for name in ["time", "echo"] {
        let wasm = carson_host::host::embedded_tool(name).unwrap();
        runner
            .register(
                &ToolDef {
                    name: format!("core/{name}"),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                    env: Default::default(),
                },
                wasm,
            )
            .unwrap();
    }
    runner
}

#[test]
fn sandboxed_time_tool_returns_unix_ms() {
    let runner = runner();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let out = runner.run("core/time", "{}").unwrap().unwrap();
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
    assert_eq!(runner.run("core/echo", "abc").unwrap().unwrap(), "abc");
}

#[test]
fn unknown_tool_is_none() {
    let runner = runner();
    assert!(runner.run("nope", "{}").is_none());
}

#[test]
fn remove_drops_a_tool() {
    let runner = runner();
    assert!(runner.remove("core/echo"));
    assert!(runner.run("core/echo", "x").is_none());
    assert_eq!(
        runner
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["core/time"]
    );
}

#[test]
fn third_party_tool_registered_with_bytes() {
    let bytes = carson_host::host::embedded_tool("echo").unwrap().to_vec();
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    let runner = ToolRunner::new(&engine);
    runner
        .register(
            &ToolDef {
                name: "custom/tool".into(),
                description: "Third-party tool".into(),
                parameters: serde_json::json!({}),
                env: Default::default(),
            },
            &bytes,
        )
        .unwrap();
    assert_eq!(
        runner.run("custom/tool", "hello").unwrap().unwrap(),
        "hello"
    );
}
