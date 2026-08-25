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
fn sandboxed_time_tool_returns_iso8601() {
    let runner = runner();
    let out = runner.run("core/time", "{}").unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let time = value["time"].as_str().expect("time field");
    // 2026-08-24T09:01:24.123Z
    assert_eq!(time.len(), 24, "ISO 8601 with millis: {time}");
    assert_eq!(&time[4..5], "-");
    assert_eq!(&time[10..11], "T");
    assert!(time.ends_with('Z'), "{time}");
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
