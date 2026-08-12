use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::json;

use crate::wit::carson::agent::events::{self, Part};
use crate::wit::carson::agent::llm::{self, Message, Request, ToolCall, Usage};
use crate::wit::carson::agent::tools;
use crate::wit::exports::carson::agent::agent::{Error, Guest, SessionConfig, State};

mod wit {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "carson:agent/agent-world",
    });
}

const MAX_ITERATIONS: usize = 10;
const MAX_TOOL_REPEATS: usize = 5;
const SUMMARY_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Produce a concise summary of the conversation so far, \
     preserving key facts, decisions, and unresolved tasks. Output only the summary.";
const SUMMARY_MAX_TOKENS: u32 = 512;

#[derive(Default)]
struct TurnUsage {
    input: u64,
    cache_read: u64,
    cache_creation: u64,
    output: u64,
}

struct Session {
    id: u64,
    system_prompt: String,
    model: String,
    messages: Vec<Message>,
    max_history: usize,
    context_window: usize,
    compaction_ratio: f32,
    auto_compact: bool,
    summary: Option<String>,
    turn_usage: TurnUsage,
    total_usage: TurnUsage,
    last_input_tokens: u64,
}

fn sessions() -> &'static Mutex<HashMap<u64, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<u64, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct CarsonAgent;

impl Guest for CarsonAgent {
    fn create_session(session_id: u64, config: SessionConfig) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        if sessions.contains_key(&session_id) {
            return Err(Error::Busy);
        }
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                system_prompt: config.system_prompt,
                model: config.model,
                messages: Vec::new(),
                max_history: config.max_history.max(1) as usize,
                context_window: config.context_window.max(1) as usize,
                compaction_ratio: config.compaction_ratio.clamp(0.0, 1.0),
                auto_compact: config.auto_compact,
                summary: None,
                turn_usage: TurnUsage::default(),
                total_usage: TurnUsage::default(),
                last_input_tokens: 0,
            },
        );
        Ok(())
    }

    fn restore_session(session_id: u64, config: SessionConfig, state: State) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let usage = state.usage;
        sessions.insert(
            session_id,
            Session {
                id: session_id,
                system_prompt: config.system_prompt,
                model: config.model,
                messages: state.messages,
                max_history: config.max_history.max(1) as usize,
                context_window: config.context_window.max(1) as usize,
                compaction_ratio: config.compaction_ratio.clamp(0.0, 1.0),
                auto_compact: config.auto_compact,
                summary: state.summary,
                turn_usage: TurnUsage::default(),
                total_usage: TurnUsage {
                    input: usage.input_tokens as u64,
                    cache_read: usage.cache_read_tokens as u64,
                    cache_creation: usage.cache_creation_tokens as u64,
                    output: usage.output_tokens as u64,
                },
                last_input_tokens: usage.input_tokens as u64,
            },
        );
        Ok(())
    }

    fn session_state(session_id: u64) -> Result<State, Error> {
        let sessions = sessions().lock().unwrap();
        let session = sessions.get(&session_id).ok_or(Error::NotFound)?;
        Ok(State {
            messages: session.messages.clone(),
            summary: session.summary.clone(),
            usage: Usage {
                input_tokens: session.total_usage.input.min(u32::MAX as u64) as u32,
                cache_read_tokens: session.total_usage.cache_read.min(u32::MAX as u64) as u32,
                cache_creation_tokens: session.total_usage.cache_creation.min(u32::MAX as u64)
                    as u32,
                output_tokens: session.total_usage.output.min(u32::MAX as u64) as u32,
            },
        })
    }

    fn session_history(session_id: u64) -> Result<Vec<Message>, Error> {
        sessions()
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|s| s.messages.clone())
            .ok_or(Error::NotFound)
    }

    fn session_usage(session_id: u64) -> Result<Usage, Error> {
        let sessions = sessions().lock().unwrap();
        let session = sessions.get(&session_id).ok_or(Error::NotFound)?;
        Ok(Usage {
            input_tokens: session.turn_usage.input.min(u32::MAX as u64) as u32,
            cache_read_tokens: session.turn_usage.cache_read.min(u32::MAX as u64) as u32,
            cache_creation_tokens: session.turn_usage.cache_creation.min(u32::MAX as u64) as u32,
            output_tokens: session.turn_usage.output.min(u32::MAX as u64) as u32,
        })
    }

    fn compact_session(session_id: u64) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let session = sessions.get_mut(&session_id).ok_or(Error::NotFound)?;
        compact(session)
    }

    fn reset_session(session_id: u64) -> Result<(), Error> {
        sessions()
            .lock()
            .unwrap()
            .get_mut(&session_id)
            .map(|s| s.messages.clear())
            .ok_or(Error::NotFound)
    }

    fn handle_message(session_id: u64, message: String) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let session = sessions.get_mut(&session_id).ok_or(Error::NotFound)?;
        session.turn_usage = TurnUsage::default();
        session.messages.push(Message {
            role: "user".into(),
            content: Some(message),
            tool_calls: None,
            tool_call_id: None,
        });
        let result = run_loop(session);
        session.total_usage.input = session
            .total_usage
            .input
            .saturating_add(session.turn_usage.input);
        session.total_usage.cache_read = session
            .total_usage
            .cache_read
            .saturating_add(session.turn_usage.cache_read);
        session.total_usage.cache_creation = session
            .total_usage
            .cache_creation
            .saturating_add(session.turn_usage.cache_creation);
        session.total_usage.output = session
            .total_usage
            .output
            .saturating_add(session.turn_usage.output);
        result
    }

    fn destroy_session(session_id: u64) -> Result<(), Error> {
        sessions()
            .lock()
            .unwrap()
            .remove(&session_id)
            .map(|_| ())
            .ok_or(Error::NotFound)
    }
}

fn trim_history(session: &mut Session) {
    let overflow = session.messages.len().saturating_sub(session.max_history);
    if overflow > 0 {
        session.messages.drain(..overflow);
    }
}

fn emit(session_id: u64, kind: &str, data: &str) -> Result<(), events::EventError> {
    events::emit_event(
        session_id,
        &Part {
            kind: kind.to_string(),
            data: data.to_string(),
        },
    )
}

fn tool_use_json(tc: &ToolCall) -> String {
    json!({"id": tc.id, "name": tc.name, "arguments": tc.arguments_json}).to_string()
}

fn tool_result_json(tc: &ToolCall, preview: &str, is_error: bool) -> String {
    json!({"id": tc.id, "name": tc.name, "result_preview": preview, "is_error": is_error})
        .to_string()
}

fn record_usage(session: &mut Session, usage: Usage) {
    session.turn_usage.input = session
        .turn_usage
        .input
        .saturating_add(usage.input_tokens as u64);
    session.turn_usage.cache_read = session
        .turn_usage
        .cache_read
        .saturating_add(usage.cache_read_tokens as u64);
    session.turn_usage.cache_creation = session
        .turn_usage
        .cache_creation
        .saturating_add(usage.cache_creation_tokens as u64);
    session.turn_usage.output = session
        .turn_usage
        .output
        .saturating_add(usage.output_tokens as u64);
    session.last_input_tokens = usage.input_tokens as u64;
}

fn should_compact(session: &Session) -> bool {
    session.last_input_tokens as usize
        >= (session.context_window as f64 * session.compaction_ratio as f64) as usize
}

/// Summarize the oldest messages into `session.summary`, keeping the recent window.
fn compact(session: &mut Session) -> Result<(), Error> {
    let keep = session.max_history;
    if session.messages.len() <= keep {
        return Ok(());
    }
    let split = session.messages.len() - keep;
    let old = session.messages.drain(..split).collect::<Vec<_>>();
    match summarize(&old, session) {
        Ok(summary) => {
            let _ = emit(
                session.id,
                "status",
                &format!("compacted: {split} messages summarized, {keep} kept"),
            );
            session.summary = Some(summary);
            Ok(())
        }
        Err(err) => {
            session.messages.splice(0..0, old);
            Err(err)
        }
    }
}

fn summarize(old: &[Message], session: &mut Session) -> Result<String, Error> {
    let request = Request {
        session_id: session.id,
        model: session.model.clone(),
        messages: old.to_vec(),
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(SUMMARY_MAX_TOKENS),
    };
    let handle = llm::stream_start(&request).map_err(|_| Error::Internal)?;
    let mut text = String::new();
    loop {
        match llm::stream_next(handle) {
            Ok(Some(chunk)) => {
                if let Some(t) = chunk.text {
                    text.push_str(&t);
                }
            }
            Ok(None) => break,
            Err(_) => {
                let _ = llm::stream_close(handle);
                return Err(Error::Internal);
            }
        }
    }
    if let Ok(usage) = llm::stream_usage(handle) {
        record_usage(session, usage);
    }
    let _ = llm::stream_close(handle);
    Ok(text)
}

fn run_loop(session: &mut Session) -> Result<(), Error> {
    let mut repeats: HashMap<String, usize> = HashMap::new();

    for _ in 0..MAX_ITERATIONS {
        if events::cancelled(session.id) {
            return Ok(());
        }

        if session.auto_compact && should_compact(session) && compact(session).is_err() {
            trim_history(session);
        }

        let mut messages = session.messages.clone();
        if let Some(summary) = &session.summary {
            messages.insert(
                0,
                Message {
                    role: "system".into(),
                    content: Some(summary.clone()),
                    tool_calls: None,
                    tool_call_id: None,
                },
            );
        }

        let request = Request {
            session_id: session.id,
            model: session.model.clone(),
            messages,
            system_prompt: Some(session.system_prompt.clone()),
            tools: tools::list_tools().to_vec(),
            temperature: None,
            max_tokens: None,
        };

        let handle = match llm::stream_start(&request) {
            Ok(handle) => handle,
            Err(err) => {
                let _ = emit(session.id, "error", &format!("llm error: {err:?}"));
                return Ok(());
            }
        };

        let mut assistant_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut failed = false;
        let mut ended = false;
        loop {
            if events::cancelled(session.id) {
                let _ = llm::stream_close(handle);
                return Ok(());
            }
            match llm::stream_next(handle) {
                Ok(Some(chunk)) => {
                    if let Some(text) = chunk.text {
                        assistant_text.push_str(&text);
                        if emit(session.id, "chunk", &text).is_err() {
                            let _ = llm::stream_close(handle);
                            return Ok(());
                        }
                    }
                    if let Some(thinking) = chunk.thinking {
                        let _ = emit(session.id, "thinking", &thinking);
                    }
                    if let Some(tc) = chunk.tool_call_start {
                        let _ = emit(session.id, "tool_use", &tool_use_json(&tc));
                    }
                    if let Some(tc) = chunk.tool_call_end {
                        tool_calls.push(tc);
                    }
                    let _ = chunk.tool_input_delta;
                }
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(llm::LlmError::Cancelled) => {
                    let _ = llm::stream_close(handle);
                    return Ok(());
                }
                Err(err) => {
                    let _ = emit(session.id, "error", &format!("llm error: {err:?}"));
                    failed = true;
                    break;
                }
            }
        }
        if ended && let Ok(usage) = llm::stream_usage(handle) {
            record_usage(session, usage);
        }
        let _ = llm::stream_close(handle);

        if failed {
            return Ok(());
        }

        if tool_calls.is_empty() {
            if !assistant_text.is_empty() {
                session.messages.push(Message {
                    role: "assistant".into(),
                    content: Some(assistant_text),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            return Ok(());
        }

        session.messages.push(Message {
            role: "assistant".into(),
            content: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });

        for tc in &tool_calls {
            let count = repeats
                .entry(format!("{}|{}", tc.name, tc.arguments_json))
                .or_insert(0);
            *count += 1;
            if *count > MAX_TOOL_REPEATS {
                let _ = emit(session.id, "error", "tool loop guard triggered");
                return Ok(());
            }
            if events::cancelled(session.id) {
                return Ok(());
            }

            let (result, is_error) = match tools::invoke(&tc.name, &tc.arguments_json) {
                Ok(output) => (output, false),
                Err(tools::ToolError::PermissionDenied) => ("permission denied".to_string(), true),
                Err(tools::ToolError::NotFound) => (format!("tool not found: {}", tc.name), true),
                Err(tools::ToolError::Failed) => ("tool failed".to_string(), true),
            };
            let preview = truncate(&result, 500);
            let _ = emit(
                session.id,
                "tool_result",
                &tool_result_json(tc, &preview, is_error),
            );
            session.messages.push(Message {
                role: "tool".into(),
                content: Some(result),
                tool_calls: None,
                tool_call_id: Some(tc.id.clone()),
            });
        }
    }
    Ok(())
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        s.to_string()
    } else {
        format!("{}...(truncated {})", &s[..limit], s.len())
    }
}

crate::wit::export!(CarsonAgent with_types_in wit);
