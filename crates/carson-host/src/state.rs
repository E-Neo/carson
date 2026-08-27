use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::carson::agent::events::{EventError, Part};
use crate::bindings::carson::agent::llm::{Chunk, LlmError, Request, ToolCall, ToolDefinition};
use crate::bindings::carson::agent::tools::ToolError;
use crate::drivers::{
    DriverError, DriverEvent, DriverMessage, DriverToolCall, DriverToolDef, LlmDriver, LlmRequest,
    Usage,
};
use crate::hub::Hub;
use crate::tools::{Capabilities, ToolRunner};

pub struct StreamHandle {
    rx: mpsc::Receiver<DriverEvent>,
    usage: Arc<Mutex<Option<Usage>>>,
}

pub struct State {
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    pub hub: Arc<Hub>,
    pub drivers: Arc<RwLock<HashMap<String, Arc<dyn LlmDriver>>>>,
    pub tool_runner: Arc<ToolRunner>,
    pub caps: Capabilities,
    pub stop: Arc<AtomicBool>,
    pub streams: HashMap<u64, StreamHandle>,
    pub next_stream_id: u64,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn resolve_model(model: &str) -> Option<(String, String)> {
    match model.split_once('/') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            Some((provider.to_string(), model.to_string()))
        }
        _ => None,
    }
}

fn to_wit_tool_call(tc: DriverToolCall) -> ToolCall {
    ToolCall {
        id: tc.id,
        name: tc.name,
        arguments_json: tc.arguments,
    }
}

/// Preserve the driver's explanation instead of collapsing every failure
/// into a bare `Internal`.
fn to_llm_error(err: DriverError) -> LlmError {
    match err {
        DriverError::Network => LlmError::Network,
        DriverError::Auth => LlmError::Auth,
        DriverError::RateLimited => LlmError::RateLimited,
        DriverError::Timeout => LlmError::Timeout,
        DriverError::Cancelled => LlmError::Cancelled,
        DriverError::Internal(msg) => LlmError::Internal(msg),
    }
}

fn chunk() -> Chunk {
    Chunk {
        text: None,
        thinking: None,
        tool_call_start: None,
        tool_call_end: None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl crate::bindings::carson::agent::events::Host for State {
    fn emit_event(&mut self, session_id: String, part: Part) -> Result<(), EventError> {
        let item = crate::hub::SseItem {
            event: part.kind,
            data: serde_json::Value::String(part.data),
        };
        if self.hub.send(&session_id, item) {
            Ok(())
        } else {
            Err(EventError::Closed)
        }
    }

    fn cancelled(&mut self, session_id: String) -> bool {
        self.stop.load(Ordering::SeqCst) || !self.hub.alive(&session_id)
    }

    fn now_ms(&mut self) -> u64 {
        now_ms()
    }
}

impl crate::bindings::carson::agent::llm::Host for State {
    fn stream_start(&mut self, request: Request) -> Result<u64, LlmError> {
        let (provider, model) = resolve_model(&request.model).ok_or_else(|| {
            LlmError::Internal(format!(
                "model '{}' must be in 'provider/model' form",
                request.model
            ))
        })?;
        let driver = self
            .drivers
            .read()
            .unwrap()
            .get(&provider)
            .cloned()
            .ok_or_else(|| {
                LlmError::Internal(format!("provider '{provider}' is not configured"))
            })?;

        let handle = self.next_stream_id;
        self.next_stream_id += 1;

        let (tx, rx) = mpsc::channel();
        let messages = request
            .messages
            .iter()
            .map(|m| DriverMessage {
                role: m.role.clone(),
                content: m.content.clone(),
                tool_calls: m
                    .tool_calls
                    .as_deref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tc| DriverToolCall {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                arguments: tc.arguments_json.clone(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();
        let tools = request
            .tools
            .iter()
            .map(|t| DriverToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: serde_json::from_str(&t.parameters_json)
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect();
        let llm_request = LlmRequest {
            model,
            messages,
            system_prompt: request.system_prompt,
            tools,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
        };

        let usage_slot = Arc::new(Mutex::new(None::<Usage>));
        let task_usage = usage_slot.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build driver runtime");
            let result = rt.block_on(driver.stream(llm_request, tx.clone()));
            match result {
                Ok(usage) => {
                    *task_usage.lock().unwrap() = Some(usage);
                }
                Err(err) => {
                    let _ = tx.send(DriverEvent::Failed(err));
                }
            }
        });

        self.streams.insert(
            handle,
            StreamHandle {
                rx,
                usage: usage_slot,
            },
        );
        Ok(handle)
    }

    fn stream_next(&mut self, handle: u64) -> Result<Option<Chunk>, LlmError> {
        let Some(stream) = self.streams.get_mut(&handle) else {
            return Err(LlmError::Internal("unknown stream handle".into()));
        };
        // Skip empty text/thinking deltas so the guest never sees a phantom
        // empty chunk (which would otherwise surface as a blank block and
        // persist as an empty message).
        loop {
            match stream.rx.recv() {
                Ok(DriverEvent::Text(text)) if text.is_empty() => continue,
                Ok(DriverEvent::Thinking(text)) if text.is_empty() => continue,
                Ok(DriverEvent::Text(text)) => {
                    return Ok(Some(Chunk {
                        text: Some(text),
                        ..chunk()
                    }));
                }
                Ok(DriverEvent::Thinking(text)) => {
                    return Ok(Some(Chunk {
                        thinking: Some(text),
                        ..chunk()
                    }));
                }
                Ok(DriverEvent::ToolCallStart(tc)) => {
                    return Ok(Some(Chunk {
                        tool_call_start: Some(to_wit_tool_call(tc)),
                        ..chunk()
                    }));
                }
                Ok(DriverEvent::ToolCallEnd(tc)) => {
                    return Ok(Some(Chunk {
                        tool_call_end: Some(to_wit_tool_call(tc)),
                        ..chunk()
                    }));
                }
                Ok(DriverEvent::Failed(err)) => return Err(to_llm_error(err)),
                Err(_) => return Ok(None),
            }
        }
    }

    fn stream_usage(
        &mut self,
        handle: u64,
    ) -> Result<crate::bindings::carson::agent::llm::Usage, LlmError> {
        let Some(stream) = self.streams.get(&handle) else {
            return Err(LlmError::Internal("unknown stream handle".into()));
        };
        let usage = stream.usage.lock().unwrap().clone().unwrap_or_default();
        Ok(crate::bindings::carson::agent::llm::Usage {
            input_tokens: usage.input_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            output_tokens: usage.output_tokens,
        })
    }

    fn stream_close(&mut self, handle: u64) -> Result<(), LlmError> {
        self.streams.remove(&handle);
        Ok(())
    }
}

impl crate::bindings::carson::agent::tools::Host for State {
    /// Advertise the agent's selected tools by their bare provider-safe
    /// names. Identity stays internal (tool uuids); the wire never sees ids.
    fn list_tools(&mut self) -> Vec<ToolDefinition> {
        let specs = self.tool_runner.specs();
        self.caps
            .ids
            .iter()
            .filter_map(|id| specs.iter().find(|spec| &spec.id == id))
            .map(|spec| ToolDefinition {
                name: spec.name.clone(),
                description: spec.description.clone(),
                parameters_json: spec.parameters.to_string(),
            })
            .collect()
    }

    /// The model invokes by bare name; resolve it through this instance's
    /// capabilities to exactly one tool id, then run that sandbox.
    fn invoke(&mut self, name: String, arguments_json: String) -> Result<String, ToolError> {
        let specs = self.tool_runner.specs();
        let Some(id) = self
            .caps
            .resolve_bare_name(&specs, &name)
            .map(str::to_owned)
        else {
            return Err(ToolError::PermissionDenied);
        };
        match self.tool_runner.run(&id, &arguments_json) {
            Some(Ok(output)) => Ok(output),
            Some(Err(_)) => Err(ToolError::Failed),
            None => Err(ToolError::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::carson::agent::events::Host as _;
    use crate::bindings::carson::agent::llm::Host as _;
    use crate::bindings::carson::agent::tools::Host as _;
    use crate::hub::SseItem;
    use wasmtime::Engine;
    use wasmtime_wasi::WasiCtxBuilder;

    #[test]
    fn resolve_model_with_provider() {
        assert_eq!(
            resolve_model("groq/llama-3"),
            Some(("groq".to_string(), "llama-3".to_string()))
        );
    }

    #[test]
    fn resolve_model_requires_provider() {
        assert_eq!(resolve_model("mock"), None);
        assert_eq!(resolve_model("/model"), None);
        assert_eq!(resolve_model("provider/"), None);
    }

    fn test_state(tool_ids: &[&str]) -> State {
        let engine = Engine::new(&wasmtime::Config::new()).unwrap();
        let tool_runner = Arc::new(ToolRunner::new(&engine));
        let time_id = crate::host::builtin_id("time");
        let wasm = crate::host::embedded_tool("time").unwrap();
        tool_runner
            .register(
                &crate::registry::ToolDef {
                    id: time_id.clone(),
                    name: "time".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                    env: HashMap::new(),
                },
                wasm,
            )
            .unwrap();
        let mut drivers: HashMap<String, Arc<dyn LlmDriver>> = HashMap::new();
        drivers.insert("mock".into(), Arc::new(crate::drivers::EchoDriver));
        State {
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            hub: Hub::new(),
            drivers: Arc::new(RwLock::new(drivers)),
            tool_runner,
            caps: Capabilities::from_ids(tool_ids.iter().map(|s| s.to_string()).collect()),
            stop: Arc::new(AtomicBool::new(false)),
            streams: HashMap::new(),
            next_stream_id: 0,
        }
    }

    #[test]
    fn emit_event_forwards_to_hub() {
        let time_id = crate::host::builtin_id("time");
        let mut state = test_state(&[&time_id]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        state.hub.register("s1", tx.clone());

        let result = state.emit_event(
            "s1".into(),
            Part {
                kind: "chunk".into(),
                data: "hello".into(),
            },
        );
        assert_eq!(result, Ok(()));
        let item: SseItem = rx.try_recv().unwrap();
        assert_eq!(item.event, "chunk");
        assert_eq!(item.data, serde_json::Value::String("hello".into()));
        state.hub.unregister("s1", &tx);
    }

    #[test]
    fn emit_event_closed_without_client() {
        let mut state = test_state(&["time"]);
        let result = state.emit_event(
            "missing".into(),
            Part {
                kind: "chunk".into(),
                data: "x".into(),
            },
        );
        assert_eq!(result, Err(EventError::Closed));
    }

    #[test]
    fn now_ms_is_close_to_host_clock() {
        let mut state = test_state(&[]);
        let before = now_ms();
        let stamp = state.now_ms();
        let after = now_ms();
        assert!((before..=after).contains(&stamp));
    }

    #[test]
    fn cancelled_tracks_stop_flag_and_hub() {
        let mut state = test_state(&["time"]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.hub.register("s3", tx.clone());
        assert!(!state.cancelled("s3".into()));

        state.stop.store(true, Ordering::SeqCst);
        assert!(state.cancelled("s3".into()));
        state.stop.store(false, Ordering::SeqCst);
        assert!(!state.cancelled("s3".into()));

        state.hub.unregister("s3", &tx);
        assert!(state.cancelled("s3".into()));
    }

    #[test]
    fn list_tools_advertises_bare_names() {
        let time_id = crate::host::builtin_id("time");
        let mut state = test_state(&[&time_id]);
        let tools = state.list_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "time");
    }

    #[test]
    fn invoke_resolves_bare_names_through_capabilities() {
        use crate::bindings::carson::agent::tools::ToolError;

        let time_id = crate::host::builtin_id("time");

        // No capabilities: bare name is not permitted at all.
        let mut state = test_state(&[]);
        assert_eq!(
            state.invoke("time".into(), "{}".into()),
            Err(ToolError::PermissionDenied)
        );

        // A capability id that resolves to nothing: still denied, never a
        // name-based fallback.
        let mut state = test_state(&["nope"]);
        assert_eq!(
            state.invoke("time".into(), "{}".into()),
            Err(ToolError::PermissionDenied)
        );

        // Selected capability resolves the wire name to its sandbox.
        let mut state = test_state(&[&time_id]);
        assert!(
            state
                .invoke("time".into(), "{}".into())
                .map(|out| out.contains("\"time\""))
                .unwrap_or(false)
        );
    }

    fn request() -> Request {
        Request {
            session_id: "s1".into(),
            model: "mock/mock".into(),
            messages: Vec::new(),
            system_prompt: None,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
        }
    }

    #[test]
    fn stream_lifecycle_with_echo_driver() {
        let mut state = test_state(&[]);
        let handle = state.stream_start(request()).unwrap();
        let mut saw_text = false;
        while let Some(chunk) = state.stream_next(handle).unwrap() {
            if chunk.text.is_some() {
                saw_text = true;
            }
        }
        assert!(saw_text);
        let usage = state.stream_usage(handle).unwrap();
        assert!(usage.output_tokens > 0);
        state.stream_close(handle).unwrap();
        assert!(!state.streams.contains_key(&handle));
    }

    #[test]
    fn stream_next_unknown_handle_errors() {
        let mut state = test_state(&[]);
        assert!(matches!(state.stream_next(999), Err(LlmError::Internal(_))));
    }

    #[test]
    fn stream_next_skips_empty_text_and_thinking() {
        let mut state = test_state(&[]);
        let (tx, rx) = std::sync::mpsc::channel();
        state.streams.insert(
            777,
            StreamHandle {
                rx,
                usage: Arc::new(Mutex::new(None)),
            },
        );
        // Empty text/thinking deltas are dropped; the next non-empty chunk is
        // the one the guest sees.
        tx.send(DriverEvent::Text("".into())).unwrap();
        tx.send(DriverEvent::Thinking("".into())).unwrap();
        tx.send(DriverEvent::Text("hi".into())).unwrap();
        let chunk = state.stream_next(777).unwrap().unwrap();
        assert_eq!(chunk.text.as_deref(), Some("hi"));
        // Stream closed afterwards.
        drop(tx);
        assert!(state.stream_next(777).unwrap().is_none());
    }
}
