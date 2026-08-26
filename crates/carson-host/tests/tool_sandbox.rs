use carson_host::registry::ToolDef;
use carson_host::tools::ToolRunner;
use wasmtime::Engine;

fn runner() -> ToolRunner {
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    let runner = ToolRunner::new(&engine);
    let wasm = carson_host::host::embedded_tool("time").unwrap();
    runner
        .register(
            &ToolDef {
                id: carson_host::host::builtin_id("time"),
                name: "time".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
                env: Default::default(),
            },
            wasm,
        )
        .unwrap();
    runner
}

#[test]
fn sandboxed_time_tool_returns_iso8601() {
    let runner = runner();
    let out = runner
        .run(&carson_host::host::builtin_id("time"), "{}")
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    let time = value["time"].as_str().expect("time field");
    // 2026-08-24T09:01:24.123Z
    assert_eq!(time.len(), 24, "ISO 8601 with millis: {time}");
    assert_eq!(&time[4..5], "-");
    assert_eq!(&time[10..11], "T");
    assert!(time.ends_with('Z'), "{time}");
}

#[test]
fn unknown_tool_is_none() {
    let runner = runner();
    assert!(runner.run("nope", "{}").is_none());
}

#[test]
fn remove_drops_a_tool() {
    let runner = runner();
    // Register a second tool so removing one leaves the other behind.
    let bytes = carson_host::host::embedded_tool("time").unwrap().to_vec();
    runner
        .register(
            &ToolDef {
                id: "custom-extra-id".into(),
                name: "extra".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
                env: Default::default(),
            },
            &bytes,
        )
        .unwrap();
    assert!(runner.remove(&carson_host::host::builtin_id("time")));
    assert!(
        runner
            .run(&carson_host::host::builtin_id("time"), "{}")
            .is_none()
    );
    assert_eq!(
        runner
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["extra"]
    );
}

#[test]
fn third_party_tool_registered_with_bytes() {
    let bytes = carson_host::host::embedded_tool("time").unwrap().to_vec();
    let engine = Engine::new(&wasmtime::Config::new()).unwrap();
    let runner = ToolRunner::new(&engine);
    runner
        .register(
            &ToolDef {
                id: "third-party-id".into(),
                name: "tool".into(),
                description: "Third-party tool".into(),
                parameters: serde_json::json!({}),
                env: Default::default(),
            },
            &bytes,
        )
        .unwrap();
    // The registered module is the time tool's wasm; its output must flow
    // back through the sandbox untouched.
    let out = runner.run("third-party-id", "{}").unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(
        value["time"]
            .as_str()
            .is_some_and(|t| t.len() == 24 && t.ends_with('Z')),
        "expected ISO 8601 time, got {out}"
    );
}
