use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::json;

use crate::wit::carson::agent::events::{self, Part};
use crate::wit::carson::agent::llm::{self, Message, Request, ToolCall, Usage};
use crate::wit::carson::agent::tools;
use crate::wit::exports::carson::agent::agent::{Block, Error, Guest, SessionConfig, State};

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

impl TurnUsage {
    fn add(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input_tokens as u64);
        self.cache_read = self
            .cache_read
            .saturating_add(usage.cache_read_tokens as u64);
        self.cache_creation = self
            .cache_creation
            .saturating_add(usage.cache_creation_tokens as u64);
        self.output = self.output.saturating_add(usage.output_tokens as u64);
    }

    fn to_wit(&self) -> Usage {
        Usage {
            input_tokens: self.input.min(u32::MAX as u64) as u32,
            cache_read_tokens: self.cache_read.min(u32::MAX as u64) as u32,
            cache_creation_tokens: self.cache_creation.min(u32::MAX as u64) as u32,
            output_tokens: self.output.min(u32::MAX as u64) as u32,
        }
    }
}

struct Session {
    id: String,
    agent_version_id: String,
    system_prompt: String,
    model: String,
    blocks: Vec<Block>,
    max_history: usize,
    context_window: usize,
    compaction_ratio: f32,
    auto_compact: bool,
    summary: Option<String>,
    turn_usage: TurnUsage,
    total_usage: TurnUsage,
    last_input_tokens: u64,
}

fn sessions() -> &'static Mutex<HashMap<String, Session>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> u64 {
    events::now_ms()
}

fn user_block(version: &str, model: &str, text: String, attachments: Vec<String>) -> Block {
    let now = now_ms();
    Block {
        agent_version_id: version.to_string(),
        model: model.to_string(),
        kind: "user".into(),
        text: Some(text),
        attachments,
        input_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        output_tokens: 0,
        created_at_ms: now,
        finished_at_ms: now,
    }
}

/// Convert the ordered block log into role-based chat messages for an LLM
/// request. Consecutive assistant-side blocks (thinking/text/tool-use) merge
/// into one assistant message; thinking is excluded from the request. User
/// messages carry their attachment ids; the summary request (compaction)
/// passes `include_attachments = false` so image bytes are not re-uploaded.
fn blocks_to_chat(blocks: &[Block], include_attachments: bool) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut content = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();

    fn flush(out: &mut Vec<Message>, content: &mut String, calls: &mut Vec<ToolCall>) {
        if content.is_empty() && calls.is_empty() {
            return;
        }
        out.push(Message {
            role: "assistant".into(),
            content: if content.is_empty() {
                None
            } else {
                Some(std::mem::take(content))
            },
            attachments: Vec::new(),
            tool_calls: if calls.is_empty() {
                None
            } else {
                Some(std::mem::take(calls))
            },
            tool_call_id: None,
        });
    }

    for b in blocks {
        match b.kind.as_str() {
            "user" | "system" => {
                flush(&mut out, &mut content, &mut calls);
                let attachments = if include_attachments && b.kind == "user" {
                    b.attachments.clone()
                } else {
                    Vec::new()
                };
                out.push(Message {
                    role: b.kind.clone(),
                    content: b.text.clone(),
                    attachments,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            "text" => {
                if let Some(t) = &b.text {
                    content.push_str(t);
                }
            }
            "tool-use" => {
                let Ok(v) =
                    serde_json::from_str::<serde_json::Value>(b.text.as_deref().unwrap_or(""))
                else {
                    continue;
                };
                let name = v["name"].as_str().unwrap_or_default();
                if name.is_empty() {
                    // An interrupted tool call leaves no name behind; such a
                    // call cannot be invoked, so do not replay it to the model.
                    continue;
                }
                calls.push(ToolCall {
                    id: v["id"].as_str().unwrap_or_default().to_string(),
                    name: name.to_string(),
                    arguments_json: v["arguments"].as_str().unwrap_or_default().to_string(),
                });
            }
            "tool-result" => {
                let Ok(v) =
                    serde_json::from_str::<serde_json::Value>(b.text.as_deref().unwrap_or(""))
                else {
                    continue;
                };
                flush(&mut out, &mut content, &mut calls);
                out.push(Message {
                    role: "tool".into(),
                    content: Some(v["output"].as_str().unwrap_or_default().to_string()),
                    attachments: Vec::new(),
                    tool_calls: None,
                    tool_call_id: Some(v["id"].as_str().unwrap_or_default().to_string()),
                });
            }
            _ => {}
        }
    }
    flush(&mut out, &mut content, &mut calls);
    out
}

struct CarsonAgent;

impl Guest for CarsonAgent {
    fn create_session(session_id: String, config: SessionConfig) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        if sessions.contains_key(&session_id) {
            return Err(Error::Busy);
        }
        sessions.insert(
            session_id.clone(),
            Session {
                id: session_id,
                agent_version_id: config.agent_version_id,
                system_prompt: config.system_prompt,
                model: config.model,
                blocks: Vec::new(),
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

    fn restore_session(
        session_id: String,
        config: SessionConfig,
        state: State,
    ) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let usage = state.usage;
        sessions.insert(
            session_id.clone(),
            Session {
                id: session_id,
                agent_version_id: config.agent_version_id,
                system_prompt: config.system_prompt,
                model: config.model,
                blocks: state.blocks,
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

    fn session_state(session_id: String) -> Result<State, Error> {
        let sessions = sessions().lock().unwrap();
        let session = sessions.get(&session_id).ok_or(Error::NotFound)?;
        Ok(State {
            blocks: session.blocks.clone(),
            summary: session.summary.clone(),
            usage: session.total_usage.to_wit(),
        })
    }

    fn session_history(session_id: String) -> Result<Vec<Block>, Error> {
        sessions()
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|s| s.blocks.clone())
            .ok_or(Error::NotFound)
    }

    fn session_usage(session_id: String) -> Result<Usage, Error> {
        let sessions = sessions().lock().unwrap();
        let session = sessions.get(&session_id).ok_or(Error::NotFound)?;
        Ok(session.turn_usage.to_wit())
    }

    fn compact_session(session_id: String) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let session = sessions.get_mut(&session_id).ok_or(Error::NotFound)?;
        compact(session)
    }

    fn reset_session(session_id: String) -> Result<(), Error> {
        sessions()
            .lock()
            .unwrap()
            .get_mut(&session_id)
            .map(|s| s.blocks.clear())
            .ok_or(Error::NotFound)
    }

    fn handle_message(
        session_id: String,
        message: String,
        attachments: Vec<String>,
    ) -> Result<(), Error> {
        let mut sessions = sessions().lock().unwrap();
        let session = sessions.get_mut(&session_id).ok_or(Error::NotFound)?;
        session.turn_usage = TurnUsage::default();
        session.blocks.push(user_block(
            &session.agent_version_id,
            &session.model,
            message,
            attachments,
        ));
        let result = run_loop(session);
        // The user message's duration spans the whole turn: stamp its finish
        // at the turn end.
        let turn_end = now_ms();
        for block in session.blocks.iter_mut().rev() {
            if block.kind == "user" {
                block.finished_at_ms = turn_end;
                break;
            }
        }
        // Fold the finished turn into the lifetime totals, but keep
        // `turn_usage` readable for `session-usage` until the next turn.
        let turn = session.turn_usage.to_wit();
        session.total_usage.add(&turn);
        result
    }

    fn destroy_session(session_id: String) -> Result<(), Error> {
        sessions()
            .lock()
            .unwrap()
            .remove(&session_id)
            .map(|_| ())
            .ok_or(Error::NotFound)
    }

    fn set_model(session_id: String, model: String) -> Result<(), Error> {
        sessions()
            .lock()
            .unwrap()
            .get_mut(&session_id)
            .map(|s| s.model = model)
            .ok_or(Error::NotFound)
    }
}

fn trim_history(session: &mut Session) {
    let overflow = session.blocks.len().saturating_sub(session.max_history);
    if overflow > 0 {
        session.blocks.drain(..overflow);
    }
}

fn emit(session_id: &str, kind: &str, data: &str) -> Result<(), events::EventError> {
    events::emit_event(
        session_id,
        &Part {
            kind: kind.to_string(),
            data: data.to_string(),
        },
    )
}

fn tool_use_json(tc: &ToolCall) -> String {
    // Arguments arrive separately in the `tool_args` event at call end.
    json!({"id": tc.id, "name": tc.name}).to_string()
}

/// Human-readable explanation for an LLM failure.
fn describe_llm_error(err: &llm::LlmError) -> String {
    match err {
        llm::LlmError::Network => "network error reaching the provider".to_string(),
        llm::LlmError::Auth => "authentication failed (check the provider API key)".to_string(),
        llm::LlmError::RateLimited => "rate limited by the provider".to_string(),
        llm::LlmError::Timeout => "request timed out".to_string(),
        llm::LlmError::Cancelled => "cancelled".to_string(),
        llm::LlmError::ContextExceeded => {
            "context length exceeded; compaction may help".to_string()
        }
        llm::LlmError::Internal(msg) => msg.clone(),
    }
}

fn tool_args_json(id: &str, arguments: &str) -> String {
    json!({"id": id, "arguments": arguments}).to_string()
}

fn tool_result_json(
    tc: &ToolCall,
    preview: &str,
    is_error: bool,
    created: u64,
    finished: u64,
) -> String {
    json!({
        "id": tc.id,
        "name": tc.name,
        "result_preview": preview,
        "is_error": is_error,
        "created_at_ms": created,
        "finished_at_ms": finished,
    })
    .to_string()
}

/// One assistant-side segment accumulated during a single LLM stream.
///
/// `created` is when the segment began; `finished` is when the next segment
/// started (0 while still open, closed at the stream end). This lets each
/// block carry its own duration instead of stretching to the turn end.
enum Seg {
    Thinking {
        text: String,
        created: u64,
        finished: u64,
    },
    Text {
        text: String,
        created: u64,
        finished: u64,
    },
    ToolUse {
        call: ToolCall,
        created: u64,
        finished: u64,
    },
}

impl Seg {
    fn close_last(segs: &mut [Seg], at: u64) {
        if let Some(seg) = segs.last_mut() {
            let finished = match seg {
                Seg::Thinking { finished, .. }
                | Seg::Text { finished, .. }
                | Seg::ToolUse { finished, .. } => finished,
            };
            if *finished == 0 {
                *finished = at;
            }
        }
    }

    fn push_text(segs: &mut Vec<Seg>, text: &str, started: u64) {
        match segs.last_mut() {
            Some(Seg::Text { text: buf, .. }) => buf.push_str(text),
            _ => {
                Seg::close_last(segs, started);
                segs.push(Seg::Text {
                    text: text.to_string(),
                    created: started,
                    finished: 0,
                });
            }
        }
    }

    fn push_thinking(segs: &mut Vec<Seg>, text: &str, started: u64) {
        match segs.last_mut() {
            Some(Seg::Thinking { text: buf, .. }) => buf.push_str(text),
            _ => {
                Seg::close_last(segs, started);
                segs.push(Seg::Thinking {
                    text: text.to_string(),
                    created: started,
                    finished: 0,
                });
            }
        }
    }

    fn into_block(self, version: &str, model: &str, stream_end: u64, usage: &Usage) -> Block {
        let (kind, text, created, finished) = match self {
            Seg::Thinking {
                text,
                created,
                finished,
            } => ("thinking", Some(text), created, finished),
            Seg::Text {
                text,
                created,
                finished,
            } => ("text", Some(text), created, finished),
            Seg::ToolUse {
                call,
                created,
                finished,
            } => (
                "tool-use",
                Some(
                    json!({"id": call.id, "name": call.name, "arguments": call.arguments_json})
                        .to_string(),
                ),
                created,
                finished,
            ),
        };
        let finished = if finished == 0 { stream_end } else { finished };
        Block {
            agent_version_id: version.to_string(),
            model: model.to_string(),
            kind: kind.into(),
            text,
            attachments: Vec::new(),
            input_tokens: usage.input_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            output_tokens: usage.output_tokens,
            created_at_ms: created,
            finished_at_ms: finished,
        }
    }
}

fn tool_result_block(
    version: &str,
    model: &str,
    tc: &ToolCall,
    result: String,
    is_error: bool,
    created: u64,
    finished: u64,
) -> Block {
    Block {
        agent_version_id: version.to_string(),
        model: model.to_string(),
        kind: "tool-result".into(),
        text: Some(
            json!({"id": tc.id, "name": tc.name, "output": result, "is_error": is_error})
                .to_string(),
        ),
        attachments: Vec::new(),
        input_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        output_tokens: 0,
        created_at_ms: created,
        finished_at_ms: finished,
    }
}

/// Max compaction retries after a `context-exceeded` error, a safety cap so a
/// large per-session retry budget can't produce an unbounded compact loop.
const MAX_COMPACTION_RETRIES: u32 = 8;

/// Start an LLM stream, retrying after `context-exceeded` by compacting the
/// session. Transient failures are retried by the driver itself (per the
/// session's retry budget); this only handles the overflow case. `None` ends
/// the turn (cancelled, a terminal error, or the budget / cap exhausted).
fn start_stream(session: &mut Session) -> Option<u64> {
    let budget = events::retry_budget_ms(&session.id);
    let mut burst_start = now_ms();
    let mut compactions = 0u32;
    loop {
        if events::cancelled(&session.id) {
            return None;
        }
        let request = Request {
            session_id: session.id.clone(),
            model: session.model.clone(),
            messages: blocks_to_chat(&session.blocks, true),
            system_prompt: Some(session.system_prompt.clone()),
            tools: tools::list_tools().to_vec(),
            temperature: None,
            max_tokens: None,
        };
        match llm::stream_start(&request) {
            Ok(handle) => return Some(handle),
            Err(llm::LlmError::ContextExceeded) => {
                if budget == 0
                    || now_ms().saturating_sub(burst_start) >= budget
                    || compactions >= MAX_COMPACTION_RETRIES
                {
                    let _ = emit(
                        &session.id,
                        "error",
                        "context length exceeded even after compaction",
                    );
                    return None;
                }
                compactions += 1;
                let _ = emit(
                    &session.id,
                    "status",
                    "context window exceeded; compacting…",
                );
                if compact(session).is_err() {
                    trim_history(session);
                }
                // A successful compaction is itself an LLM request, so the
                // burst window restarts per the session's retry budget.
                burst_start = now_ms();
            }
            Err(err) => {
                let _ = emit(&session.id, "error", &describe_llm_error(&err));
                return None;
            }
        }
    }
}

fn run_loop(session: &mut Session) -> Result<(), Error> {
    let mut repeats: HashMap<String, usize> = HashMap::new();

    for _ in 0..MAX_ITERATIONS {
        if events::cancelled(&session.id) {
            return Ok(());
        }

        if session.auto_compact && should_compact(session) && compact(session).is_err() {
            trim_history(session);
        }

        let handle = match start_stream(session) {
            Some(handle) => handle,
            None => return Ok(()),
        };

        let mut segs: Vec<Seg> = Vec::new();
        let mut failed = false;
        let mut ended = false;
        let mut tool_start: Option<u64> = None;
        loop {
            if events::cancelled(&session.id) {
                let _ = llm::stream_close(handle);
                return Ok(());
            }
            match llm::stream_next(handle) {
                Ok(Some(chunk)) => {
                    if let Some(text) = chunk.text {
                        Seg::push_text(&mut segs, &text, now_ms());
                        if emit(&session.id, "chunk", &text).is_err() {
                            let _ = llm::stream_close(handle);
                            return Ok(());
                        }
                    }
                    if let Some(thinking) = chunk.thinking {
                        Seg::push_thinking(&mut segs, &thinking, now_ms());
                        let _ = emit(&session.id, "thinking", &thinking);
                    }
                    if let Some(tc) = chunk.tool_call_start {
                        // A tool call begins: close the previous segment and
                        // start timing the yield.
                        let started = now_ms();
                        Seg::close_last(&mut segs, started);
                        tool_start = Some(started);
                        let _ = emit(&session.id, "tool_use", &tool_use_json(&tc));
                    }
                    if let Some(tc) = chunk.tool_call_end {
                        if tc.name.is_empty() {
                            // Never-announced partial call (interrupted before
                            // the name arrived); skip it — nothing to invoke.
                            continue;
                        }
                        // The call is fully formed; the arguments streamed for
                        // `started -> now` (the time to yield the tool call).
                        let yielded = now_ms();
                        let _ = emit(
                            &session.id,
                            "tool_args",
                            &tool_args_json(&tc.id, &tc.arguments_json),
                        );
                        segs.push(Seg::ToolUse {
                            call: tc,
                            created: tool_start.unwrap_or(yielded),
                            finished: yielded,
                        });
                    }
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
                    let _ = emit(&session.id, "error", &describe_llm_error(&err));
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

        // Commit this iteration's assistant-side segments to the log in the
        // order they arrived, stamped with the LLM call's usage.
        let turn_usage = session.turn_usage.to_wit();
        let finished = now_ms();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for seg in std::mem::take(&mut segs) {
            if let Seg::ToolUse { call, .. } = &seg {
                tool_calls.push(call.clone());
            }
            session.blocks.push(seg.into_block(
                &session.agent_version_id,
                &session.model,
                finished,
                &turn_usage,
            ));
        }

        if tool_calls.is_empty() {
            return Ok(());
        }

        for tc in &tool_calls {
            let count = repeats
                .entry(format!("{}|{}", tc.name, tc.arguments_json))
                .or_insert(0);
            *count += 1;
            if *count > MAX_TOOL_REPEATS {
                let _ = emit(&session.id, "error", "tool loop guard triggered");
                return Ok(());
            }
            if events::cancelled(&session.id) {
                return Ok(());
            }

            let invoke_started = now_ms();
            let (result, is_error) = match tools::invoke(&tc.name, &tc.arguments_json, &session.id)
            {
                Ok(output) => (output, false),
                Err(tools::ToolError::PermissionDenied) => ("permission denied".to_string(), true),
                Err(tools::ToolError::NotFound) => (format!("tool not found: {}", tc.name), true),
                Err(tools::ToolError::Failed) => ("tool failed".to_string(), true),
            };
            let invoke_finished = now_ms();
            let preview = truncate(&result, 500);
            let _ = emit(
                &session.id,
                "tool_result",
                &tool_result_json(tc, &preview, is_error, invoke_started, invoke_finished),
            );
            session.blocks.push(tool_result_block(
                &session.agent_version_id,
                &session.model,
                tc,
                result,
                is_error,
                invoke_started,
                invoke_finished,
            ));
        }
    }
    Ok(())
}

fn truncate(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n…", &s[..cut])
}
fn record_usage(session: &mut Session, usage: Usage) {
    session.turn_usage.add(&usage);
    session.last_input_tokens = usage.input_tokens as u64;
}

fn should_compact(session: &Session) -> bool {
    session.last_input_tokens as usize
        >= (session.context_window as f64 * session.compaction_ratio as f64) as usize
}

/// Summarize the oldest blocks into `session.summary`, keeping the recent window.
fn compact(session: &mut Session) -> Result<(), Error> {
    let keep = session.max_history;
    if session.blocks.len() <= keep {
        return Ok(());
    }
    let split = session.blocks.len() - keep;
    let old = session.blocks.drain(..split).collect::<Vec<_>>();
    match summarize(&old, session) {
        Ok(summary) => {
            let _ = emit(
                &session.id,
                "status",
                &format!("compacted: {split} messages summarized, {keep} kept"),
            );
            session.summary = Some(summary);
            Ok(())
        }
        Err(err) => {
            session.blocks.splice(0..0, old);
            Err(err)
        }
    }
}

fn summarize(old: &[Block], session: &mut Session) -> Result<String, Error> {
    let request = Request {
        session_id: session.id.clone(),
        model: session.model.clone(),
        messages: blocks_to_chat(old, false),
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

crate::wit::export!(CarsonAgent with_types_in wit);
