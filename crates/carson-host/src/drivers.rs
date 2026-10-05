use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub struct DriverToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct DriverMessage {
    pub role: String,
    pub content: Option<String>,
    /// Image data URLs (resolved from attachments by the host).
    pub images: Vec<String>,
    pub tool_calls: Vec<DriverToolCall>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DriverToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub messages: Vec<DriverMessage>,
    pub system_prompt: Option<String>,
    pub tools: Vec<DriverToolDef>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Per-session retry budget for transient request-phase failures.
    pub retry: RetryConfig,
    /// Flipped by the host when the turn is stopped; aborts retries.
    pub cancel: Arc<std::sync::atomic::AtomicBool>,
}

/// Per-session retry tuning. `max_ms` is the budget for a burst of
/// consecutive failures (reset per turn and after each successful request).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct RetryConfig {
    pub base_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub max_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            base_backoff_ms: 1000,
            max_backoff_ms: 60_000,
            max_ms: 1_800_000,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum DriverEvent {
    Text(String),
    Thinking(String),
    ToolCallStart(DriverToolCall),
    ToolCallEnd(DriverToolCall),
    /// Non-stream progress, e.g. a retry backoff — relayed to the session's
    /// SSE stream so the user knows the request is being retried.
    Status(String),
    Failed(DriverError),
}

#[derive(
    Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
pub struct Usage {
    pub input_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DriverError {
    Network,
    Auth,
    RateLimited,
    Timeout,
    Cancelled,
    ContextExceeded,
    Internal(String),
}

#[async_trait]
pub trait LlmDriver: Send + Sync {
    async fn stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<DriverEvent>,
    ) -> Result<Usage, DriverError>;
}

/// Incremental results decoded from one `data:` payload of an OpenAI SSE stream.
#[derive(Debug, PartialEq)]
pub enum SseEventBatch {
    Done,
    Events(Vec<DriverEvent>),
}

/// Decodes an OpenAI-compatible streaming `data:` payload into driver events.
///
/// Tool-call state and usage are accumulated across payloads, so one decoder
/// instance must process a single response stream in order. A `ToolCallStart`
/// is only announced once the call's id and name are both known, so consumers
/// never see an identity-less tool call.
#[derive(Debug, Default)]
pub struct SseDecoder {
    tool_state: std::collections::BTreeMap<usize, DriverToolCall>,
    announced: std::collections::HashSet<usize>,
    usage: Usage,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self {
            tool_state: std::collections::BTreeMap::new(),
            announced: std::collections::HashSet::new(),
            usage: Usage::default(),
        }
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Emits a `ToolCallEnd` for every announced tool call that never
    /// completed. Never-announced stubs (an `index`/`id` with no function
    /// name) are dropped so an interrupted call can't leave an empty-named
    /// tool call behind.
    pub fn finish(&mut self) -> Vec<DriverEvent> {
        std::mem::take(&mut self.tool_state)
            .into_values()
            .filter(|tc| !tc.name.is_empty())
            .map(DriverEvent::ToolCallEnd)
            .collect()
    }

    /// Decode one `data:` payload. Returns `None` for lines that carry no
    /// event (e.g. keep-alives or malformed JSON).
    pub fn decode(&mut self, data: &str) -> Option<SseEventBatch> {
        let data = data.trim();
        if data == "[DONE]" {
            return Some(SseEventBatch::Done);
        }
        let value: serde_json::Value = serde_json::from_str(data).ok()?;
        if let Some(usage) = value.get("usage") {
            self.usage.input_tokens = usage["prompt_tokens"]
                .as_u64()
                .or_else(|| usage["input_tokens"].as_u64())
                .unwrap_or(0) as u32;
            self.usage.output_tokens = usage["completion_tokens"]
                .as_u64()
                .or_else(|| usage["output_tokens"].as_u64())
                .unwrap_or(0) as u32;
            self.usage.cache_read_tokens = usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| usage["cache_read_input_tokens"].as_u64())
                .unwrap_or(0) as u32;
            self.usage.cache_creation_tokens =
                usage["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32;
        }
        let delta = value
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))?;

        let mut events = Vec::new();
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            events.push(DriverEvent::Text(content.to_string()));
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            events.push(DriverEvent::Thinking(reasoning.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
            for call in calls {
                let index = call["index"].as_u64().unwrap_or(0) as usize;
                let entry = self
                    .tool_state
                    .entry(index)
                    .or_insert_with(|| DriverToolCall {
                        id: call["id"].as_str().unwrap_or("").to_string(),
                        name: call["function"]["name"].as_str().unwrap_or("").to_string(),
                        arguments: String::new(),
                    });
                if let Some(id) = call
                    .get("id")
                    .and_then(|v| v.as_str())
                    // Some providers repeat `id`/`name` as empty strings on
                    // later argument deltas; never let empty clobber a known.
                    .filter(|s| !s.is_empty())
                {
                    entry.id = id.to_string();
                }
                if let Some(name) = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    entry.name = name.to_string();
                }
                // Announce once the name is known. If the provider never sends
                // an id, mint one — it only correlates this turn's call+result.
                if !self.announced.contains(&index) && !entry.name.is_empty() {
                    if entry.id.is_empty() {
                        entry.id = format!("call_{}", self.announced.len());
                    }
                    self.announced.insert(index);
                    events.push(DriverEvent::ToolCallStart(entry.clone()));
                }
                if let Some(arg) = call.get("function").and_then(|f| f.get("arguments")) {
                    // Arguments are usually a streamed string; some providers
                    // send the JSON object/number directly — accept either.
                    let arg = match arg {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    if !arg.is_empty() {
                        entry.arguments.push_str(&arg);
                    }
                }
            }
        }
        Some(SseEventBatch::Events(events))
    }
}

/// Providers require every function's parameters to be a JSON Schema object
/// of `type: "object"`. Normalize lenient inputs: objects without a type get
/// one injected; anything that is not an object becomes a permissive
/// empty-object schema.
fn normalize_parameters(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut map = map.clone();
            map.entry("type".to_string())
                .or_insert_with(|| json!("object"));
            serde_json::Value::Object(map)
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// Providers constrain function names to `^[a-zA-Z0-9_-]+$`, so canonical
/// names like `core/time` travel the wire as `core_time`. Canonical names
/// carry exactly one slash, which makes this mapping injective.
fn wire_tool_name(canonical: &str) -> String {
    canonical.replacen('/', "_", 1)
}

/// Char-boundary-safe excerpt of a response body for logs and errors.
fn excerpt(body: &str, max_chars: usize) -> String {
    if body.chars().count() <= max_chars {
        return body.to_string();
    }
    let cut: String = body.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Removes one complete line (including its trailing `\n`) from `buffer`.
fn pop_line(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let pos = buffer.iter().position(|&b| b == b'\n')?;
    Some(buffer.drain(..=pos).collect())
}

pub struct EchoDriver;

#[async_trait]
impl LlmDriver for EchoDriver {
    async fn stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<DriverEvent>,
    ) -> Result<Usage, DriverError> {
        let last_user = req.messages.iter().rev().find(|m| m.role == "user");
        let has_tool_result = req.messages.iter().any(|m| m.role == "tool");
        let wants_time = last_user
            .and_then(|m| m.content.as_ref())
            .map(|c| c.to_lowercase().contains("time"))
            .unwrap_or(false);
        let input_tokens = (req.system_prompt.as_deref().map(str::len).unwrap_or(0)
            + req
                .messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .map(str::len)
                .sum::<usize>())
            / 4;

        if wants_time && !has_tool_result && !req.tools.is_empty() {
            let tool = req
                .tools
                .iter()
                .find(|t| t.name.ends_with("/time") || t.name == "time")
                .unwrap_or(&req.tools[0]);
            let tc = DriverToolCall {
                id: "call_time".into(),
                name: tool.name.clone(),
                arguments: "{}".into(),
            };
            let _ = tx.send(DriverEvent::ToolCallStart(tc.clone()));
            let _ = tx.send(DriverEvent::ToolCallEnd(tc));
            return Ok(Usage {
                input_tokens: input_tokens as u32,
                ..Usage::default()
            });
        }

        let text = match last_user.and_then(|m| m.content.clone()) {
            Some(content) => format!("Echo: {content}"),
            None => "Echo: (no message)".to_string(),
        };
        for word in text.split_inclusive(char::is_whitespace) {
            if tx.send(DriverEvent::Text(word.to_string())).is_err() {
                return Err(DriverError::Cancelled);
            }
        }
        Ok(Usage {
            input_tokens: input_tokens as u32,
            output_tokens: text.len() as u32,
            ..Usage::default()
        })
    }
}

/// How long the provider may stream nothing before the request is aborted as
/// a timeout (so a stalled SSE stream can't hang the turn forever).
const LLM_READ_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Classify a failed request phase into a [`DriverError`] and whether it is
/// worth a backoff retry. `send_err` is non-None when the request never
/// produced a response; otherwise `status`/`detail` describe the rejection.
fn classify_upstream(
    status: Option<reqwest::StatusCode>,
    send_err: Option<&reqwest::Error>,
    detail: &str,
) -> (DriverError, bool) {
    if let Some(status) = status {
        if is_context_exceeded(detail) {
            return (DriverError::ContextExceeded, false);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return (DriverError::Auth, false);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return (DriverError::RateLimited, true);
        }
        let shown = excerpt(detail, 512);
        if status.as_u16() == 408 || status.as_u16() == 425 || status.is_server_error() {
            return (
                DriverError::Internal(format!("upstream status {status}: {shown}")),
                true,
            );
        }
        return (
            DriverError::Internal(format!("upstream status {status}: {shown}")),
            false,
        );
    }
    if let Some(err) = send_err {
        if err.is_timeout() {
            return (DriverError::Timeout, true);
        }
        if err.is_connect() {
            return (DriverError::Network, true);
        }
        return (
            DriverError::Internal(format!("request to provider failed: {err}")),
            false,
        );
    }
    (
        DriverError::Internal("no response from provider".into()),
        false,
    )
}

/// True when a rejection body indicates the request exceeded the model's
/// context window (a permanent error — never retry; the agent compacts).
fn is_context_exceeded(detail: &str) -> bool {
    let d = detail.to_lowercase();
    [
        "context length",
        "maximum context",
        "too many tokens",
        "token limit",
        "context window",
        "reduce the length",
    ]
    .iter()
    .any(|marker| d.contains(marker))
}

pub struct OpenAiCompatDriver {
    pub base_url: String,
    pub api_key: String,
}

#[async_trait]
impl LlmDriver for OpenAiCompatDriver {
    async fn stream(
        &self,
        req: LlmRequest,
        tx: mpsc::Sender<DriverEvent>,
    ) -> Result<Usage, DriverError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(req.model));

        // Wire -> canonical lookup for names echoed back by the provider.
        let canonical_of_wire: std::collections::HashMap<String, String> = req
            .tools
            .iter()
            .map(|t| (wire_tool_name(&t.name), t.name.clone()))
            .collect();
        let translate = move |event: DriverEvent| match event {
            DriverEvent::ToolCallStart(mut tc) => {
                if let Some(name) = canonical_of_wire.get(&tc.name) {
                    tc.name = name.clone();
                }
                DriverEvent::ToolCallStart(tc)
            }
            DriverEvent::ToolCallEnd(mut tc) => {
                if let Some(name) = canonical_of_wire.get(&tc.name) {
                    tc.name = name.clone();
                }
                DriverEvent::ToolCallEnd(tc)
            }
            other => other,
        };

        let mut messages: Vec<serde_json::Value> = Vec::new();
        if let Some(system_prompt) = &req.system_prompt {
            messages.push(json!({"role": "system", "content": system_prompt}));
        }
        for m in &req.messages {
            if !m.tool_calls.is_empty() {
                let calls: Vec<_> = m
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        json!({"id": tc.id, "type": "function", "function": {"name": wire_tool_name(&tc.name), "arguments": tc.arguments}})
                    })
                    .collect();
                messages
                    .push(json!({"role": "assistant", "content": m.content, "tool_calls": calls}));
            } else if let Some(tool_call_id) = &m.tool_call_id {
                messages.push(
                    json!({"role": "tool", "tool_call_id": tool_call_id, "content": m.content}),
                );
            } else {
                // Multimodal: a message with images becomes a content array of
                // text + image_url parts; otherwise it stays a plain string.
                let content = if m.images.is_empty() {
                    m.content
                        .clone()
                        .map(serde_json::Value::String)
                        .unwrap_or(Value::Null)
                } else {
                    let mut parts: Vec<Value> = Vec::new();
                    if let Some(text) = &m.content {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    for url in &m.images {
                        parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                    }
                    json!(parts)
                };
                messages.push(json!({"role": m.role, "content": content}));
            }
        }
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));

        if !req.tools.is_empty() {
            let tools: Vec<_> = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {"name": wire_tool_name(&t.name), "description": t.description, "parameters": normalize_parameters(&t.parameters)}})
                })
                .collect();
            body.insert("tools".into(), json!(tools));
            body.insert("tool_choice".into(), json!("auto"));
        }
        if let Some(temperature) = req.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(max_tokens) = req.max_tokens {
            body.insert("max_tokens".into(), json!(max_tokens));
        }

        // The serialized payload is reused for the actual request so the
        // logged body is byte-identical to what was sent.
        let payload =
            serde_json::to_string(&body).map_err(|err| DriverError::Internal(err.to_string()))?;
        tracing::info!(
            url = %url,
            model = %req.model,
            messages = req.messages.len(),
            tools = req.tools.len(),
            bytes = payload.len(),
            "llm request"
        );
        tracing::debug!(body = %payload, "llm request body");

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|err| DriverError::Internal(format!("build http client: {err}")))?;
        let retry = req.retry;
        let burst_start = std::time::Instant::now();
        let mut attempt = 0u32;
        let mut backoff = retry.base_backoff_ms.clamp(1, retry.max_backoff_ms.max(1));
        let resp = loop {
            attempt += 1;
            if req.cancel.load(Ordering::SeqCst) {
                return Err(DriverError::Cancelled);
            }
            let builder = client
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(payload.clone());
            let builder = if self.api_key.is_empty() {
                builder
            } else {
                builder.bearer_auth(&self.api_key)
            };
            let send = builder.send().await;
            let (status, send_err, retry_after_ms, detail) = match send {
                Ok(resp) if resp.status().is_success() => break resp,
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after_ms = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|secs| secs.saturating_mul(1000));
                    let detail = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        status = %status,
                        url = %url,
                        body = %detail,
                        "upstream rejected llm request"
                    );
                    (Some(status), None, retry_after_ms, detail)
                }
                Err(err) => {
                    tracing::warn!(url = %url, error = %err, "llm request failed");
                    (None, Some(err), None, String::new())
                }
            };
            let (err, retryable) = classify_upstream(status, send_err.as_ref(), &detail);
            if !retryable || retry.max_ms == 0 {
                return Err(err);
            }
            let jitter = 75 + (attempt.wrapping_mul(37) % 51) as u64;
            let sleep = backoff.saturating_mul(jitter) / 100;
            backoff = (backoff.saturating_mul(2)).min(retry.max_backoff_ms.max(1));
            let sleep = sleep.max(retry_after_ms.unwrap_or(0));
            if burst_start.elapsed().as_millis() as u64 + sleep > retry.max_ms {
                return Err(err);
            }
            tracing::warn!(url = %url, sleep_ms = sleep, attempt, "llm request retry");
            let _ = tx.send(DriverEvent::Status(
                json!({"retry_in_ms": sleep, "attempt": attempt}).to_string(),
            ));
            tokio::time::sleep(std::time::Duration::from_millis(sleep)).await;
        };

        let mut chunks = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut buffer: Vec<u8> = Vec::new();

        loop {
            // A stalled provider stream (no [DONE], no EOF) must not hang the
            // turn forever: treat an idle read timeout as a retryable Timeout.
            let chunk = match tokio::time::timeout(LLM_READ_IDLE_TIMEOUT, chunks.next()).await {
                Err(_) => return Err(DriverError::Timeout),
                Ok(None) => break,
                Ok(Some(chunk)) => match chunk {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        // Close out any announced-but-unfinished tool call so
                        // the agent can act on it instead of a dangling block.
                        for event in decoder.finish() {
                            if tx.send(translate(event)).is_err() {
                                return Err(DriverError::Cancelled);
                            }
                        }
                        return Err(DriverError::Internal(format!("stream read failed: {err}")));
                    }
                },
            };
            buffer.extend_from_slice(&chunk);

            while let Some(line) = pop_line(&mut buffer) {
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                tracing::debug!(url = %url, line = %excerpt(data, 256), "llm sse data");
                let events = match decoder.decode(data) {
                    Some(SseEventBatch::Done) => decoder.finish(),
                    Some(SseEventBatch::Events(events)) => events,
                    None => continue,
                };
                for event in events {
                    if tx.send(translate(event)).is_err() {
                        return Err(DriverError::Cancelled);
                    }
                }
                if data.trim() == "[DONE]" {
                    return Ok(decoder.usage().clone());
                }
            }
        }

        let events = decoder.finish();
        for event in events {
            if tx.send(translate(event)).is_err() {
                return Err(DriverError::Cancelled);
            }
        }
        Ok(decoder.usage().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> DriverMessage {
        DriverMessage {
            role: "user".into(),
            content: Some(content.into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    fn request(messages: Vec<DriverMessage>, tools: Vec<DriverToolDef>) -> LlmRequest {
        LlmRequest {
            model: "mock".into(),
            messages,
            system_prompt: None,
            tools,
            temperature: None,
            max_tokens: None,
            retry: RetryConfig::default(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn echo_streams_text() {
        let (tx, rx) = mpsc::channel();
        let usage = EchoDriver
            .stream(request(vec![user("hi")], vec![]), tx)
            .await
            .unwrap();
        assert!(usage.output_tokens > 0);
        assert!(matches!(rx.recv().unwrap(), DriverEvent::Text(_)));
    }

    #[tokio::test]
    async fn echo_triggers_tool_when_requested_and_available() {
        let (tx, rx) = mpsc::channel();
        let tools = vec![DriverToolDef {
            name: "time".into(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }];
        EchoDriver
            .stream(request(vec![user("what time is it")], tools), tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), DriverEvent::ToolCallStart(_)));
    }

    #[tokio::test]
    async fn echo_skips_tool_when_not_available() {
        let (tx, rx) = mpsc::channel();
        EchoDriver
            .stream(request(vec![user("what time is it")], vec![]), tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().unwrap(), DriverEvent::Text(_)));
    }

    #[tokio::test]
    async fn echo_skips_tool_when_result_already_present() {
        let (tx, rx) = mpsc::channel();
        let tool = DriverToolDef {
            name: "time".into(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        };
        let tool_message = DriverMessage {
            role: "tool".into(),
            content: Some("{\"time\":\"2026-01-01T00:00:00.000Z\"}".into()),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: Some("call_time".into()),
        };
        let req = request(vec![user("what time is it"), tool_message], vec![tool]);
        EchoDriver.stream(req, tx).await.unwrap();
        assert!(matches!(rx.recv().unwrap(), DriverEvent::Text(_)));
    }

    #[tokio::test]
    async fn echo_cancels_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let result = EchoDriver
            .stream(request(vec![user("hi")], vec![]), tx)
            .await;
        assert_eq!(result, Err(DriverError::Cancelled));
    }

    fn data(payload: &str) -> String {
        payload.to_string()
    }

    #[test]
    fn decoder_extracts_text_and_usage() {
        let mut decoder = SseDecoder::new();
        let batch = decoder
            .decode(&data(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#))
            .unwrap();
        assert_eq!(
            batch,
            SseEventBatch::Events(vec![DriverEvent::Text("Hel".into())])
        );
        let batch = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"content":"lo"}}],"usage":{"prompt_tokens":7,"completion_tokens":9}}"#,
            ))
            .unwrap();
        assert_eq!(
            batch,
            SseEventBatch::Events(vec![DriverEvent::Text("lo".into())])
        );
        assert_eq!(
            decoder.usage(),
            &Usage {
                input_tokens: 7,
                output_tokens: 9,
                ..Default::default()
            }
        );
        assert_eq!(
            decoder.decode(&data("[DONE]")).unwrap(),
            SseEventBatch::Done
        );
    }

    #[test]
    fn decoder_parses_cache_tokens() {
        let mut decoder = SseDecoder::new();
        let _ = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":100,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":60}}}"#,
            ))
            .unwrap();
        assert_eq!(
            decoder.usage(),
            &Usage {
                input_tokens: 100,
                cache_read_tokens: 60,
                cache_creation_tokens: 0,
                output_tokens: 5,
            }
        );

        let mut decoder = SseDecoder::new();
        let _ = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{}}],"usage":{"input_tokens":100,"output_tokens":5,"cache_read_input_tokens":70,"cache_creation_input_tokens":20}}"#,
            ))
            .unwrap();
        assert_eq!(
            decoder.usage(),
            &Usage {
                input_tokens: 100,
                cache_read_tokens: 70,
                cache_creation_tokens: 20,
                output_tokens: 5,
            }
        );
    }

    #[test]
    fn decoder_accumulates_tool_call_arguments() {
        let mut decoder = SseDecoder::new();
        let start = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"time","arguments":""}}]}}]}"#,
            ))
            .unwrap();
        assert_eq!(
            start,
            SseEventBatch::Events(vec![DriverEvent::ToolCallStart(DriverToolCall {
                id: "c1".into(),
                name: "time".into(),
                arguments: String::new(),
            }),])
        );
        let delta = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}"#,
            ))
            .unwrap();
        assert_eq!(delta, SseEventBatch::Events(vec![]));
        let _ = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
            ))
            .unwrap();
        let end = decoder.finish();
        assert_eq!(
            end,
            vec![DriverEvent::ToolCallEnd(DriverToolCall {
                id: "c1".into(),
                name: "time".into(),
                arguments: "{\"x\":1}".into(),
            })]
        );
    }

    #[test]
    fn decoder_handles_reasoning_and_multiple_tools() {
        let mut decoder = SseDecoder::new();
        let batch = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#,
            ))
            .unwrap();
        assert_eq!(
            batch,
            SseEventBatch::Events(vec![DriverEvent::Thinking("hmm".into())])
        );
        let batch = decoder
            .decode(&data(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"a","function":{"name":"f1","arguments":"{}"}},
                    {"index":1,"id":"b","function":{"name":"f2","arguments":"{}"}}
                ]}}]}"#,
            ))
            .unwrap();
        let SseEventBatch::Events(events) = batch else {
            panic!("expected events");
        };
        assert_eq!(
            events,
            vec![
                DriverEvent::ToolCallStart(DriverToolCall {
                    id: "a".into(),
                    name: "f1".into(),
                    arguments: String::new(),
                }),
                DriverEvent::ToolCallStart(DriverToolCall {
                    id: "b".into(),
                    name: "f2".into(),
                    arguments: String::new(),
                }),
            ]
        );
    }

    #[test]
    fn decoder_ignores_non_data_and_invalid_json() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.decode("not json").is_none());
        assert!(decoder.decode(r#"{"choices":[]}"#).is_none());
    }

    /// A non-success response's body — where providers put the real reason —
    /// must be surfaced in the error.
    #[tokio::test]
    async fn openai_surfaces_upstream_error_body() {
        let body =
            r#"{"error":{"message":"model xyz does not exist","type":"invalid_request_error"}}"#;
        let base_url = stub_server(
            body,
            "HTTP/1.1 400 Bad Request",
            std::sync::Arc::new(std::sync::Mutex::new(Recorded::default())),
        )
        .await;

        let driver = OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        };
        let (tx, _rx) = mpsc::channel();
        let err = driver.stream(openai_request(), tx).await.unwrap_err();
        match err {
            DriverError::Internal(msg) => {
                assert!(msg.contains("400"), "{msg}");
                assert!(msg.contains("model xyz does not exist"), "{msg}");
            }
            other => panic!("expected Internal error, got {other:?}"),
        }
    }

    /// Canonical tool names (`core/time`) travel the wire sanitized and come
    /// back translated to their canonical form.
    #[tokio::test]
    async fn openai_sanitizes_tool_names_on_the_wire() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"core_time\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Recorded::default()));
        let base_url = stub_server(body, "HTTP/1.1 200 OK", std::sync::Arc::clone(&recorded)).await;

        let mut req = openai_request();
        req.tools = vec![DriverToolDef {
            name: "core/time".into(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }];
        // A prior turn's assistant message also carries the canonical name.
        req.messages.push(DriverMessage {
            role: "assistant".into(),
            content: None,
            images: vec![],
            tool_calls: vec![DriverToolCall {
                id: "c0".into(),
                name: "core/time".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: None,
        });

        let (tx, rx) = mpsc::channel();
        OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        }
        .stream(req, tx)
        .await
        .unwrap();

        // Outgoing: sanitized in both tools[] and message history.
        let sent = recorded.lock().unwrap().body.clone();
        assert!(sent.contains("\"name\":\"core_time\""), "{sent}");
        assert!(!sent.contains("core/time"), "canonical name leaked: {sent}");

        // Incoming: translated back to canonical for the guest/UI.
        match rx.recv().unwrap() {
            DriverEvent::ToolCallStart(tc) => assert_eq!(tc.name, "core/time"),
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
        match rx.recv().unwrap() {
            DriverEvent::ToolCallEnd(tc) => assert_eq!(tc.name, "core/time"),
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
        assert!(rx.recv().is_err(), "no events after [DONE]");
    }

    /// Lenient parameter schemas are normalized to what providers accept:
    /// objects gain `type: "object"`, non-objects become permissive schemas.
    #[tokio::test]
    async fn openai_normalizes_parameter_schemas() {
        let body = "data: [DONE]\n\n";
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Recorded::default()));
        let base_url = stub_server(body, "HTTP/1.1 200 OK", std::sync::Arc::clone(&recorded)).await;

        let mut req = openai_request();
        req.tools = vec![
            DriverToolDef {
                name: "time".into(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
            DriverToolDef {
                name: "typed".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
            DriverToolDef {
                name: "rich".into(),
                description: String::new(),
                parameters: serde_json::json!({"type":"object","properties":{"x":{"type":"number"}}}),
            },
        ];
        let (tx, _rx) = mpsc::channel();
        OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        }
        .stream(req, tx)
        .await
        .unwrap();

        let sent = recorded.lock().unwrap().body.clone();
        let payload: serde_json::Value = serde_json::from_str(&sent).unwrap();
        let fns: Vec<&serde_json::Value> = payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| &t["function"])
            .collect();
        // Null schema -> permissive object.
        assert_eq!(
            fns[0]["parameters"],
            serde_json::json!({"type":"object","properties":{}})
        );
        // Empty object gains the type key.
        assert_eq!(fns[1]["parameters"], serde_json::json!({"type":"object"}));
        // A well-formed schema passes through untouched.
        assert_eq!(
            fns[2]["parameters"],
            serde_json::json!({"type":"object","properties":{"x":{"type":"number"}}})
        );
    }

    /// Images serialize as a content array of text + image_url parts.
    #[tokio::test]
    async fn openai_serializes_images_as_content_parts() {
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Recorded::default()));
        let base_url = stub_server(
            "data: [DONE]\n\n",
            "HTTP/1.1 200 OK",
            std::sync::Arc::clone(&recorded),
        )
        .await;

        let mut req = openai_request();
        let mut image_msg = user("look at this");
        image_msg.images = vec!["data:image/png;base64,AAAA".into()];
        req.messages = vec![image_msg];
        let (tx, _rx) = mpsc::channel();
        OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        }
        .stream(req, tx)
        .await
        .unwrap();

        let sent = recorded.lock().unwrap().body.clone();
        let payload: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert_eq!(
            payload["messages"][0]["content"],
            serde_json::json!([
                {"type": "text", "text": "look at this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ])
        );
    }

    fn openai_request() -> LlmRequest {
        LlmRequest {
            model: "mock".into(),
            messages: vec![user("hi")],
            system_prompt: None,
            tools: vec![],
            temperature: None,
            max_tokens: None,
            retry: RetryConfig::default(),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// What the stub server saw from the client.
    #[derive(Default)]
    struct Recorded {
        authorization: Option<String>,
        body: String,
    }

    /// Serves one canned HTTP response, recording the Authorization header and
    /// the JSON request body the driver sent.
    async fn stub_server(
        body: &'static str,
        status_line: &'static str,
        recorded: std::sync::Arc<std::sync::Mutex<Recorded>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request_text = String::from_utf8_lossy(&request).to_string();
            if let Some(line) = request_text
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization"))
            {
                recorded.lock().unwrap().authorization = Some(line.to_string());
            }
            recorded.lock().unwrap().body = request_text
                .split_once("\r\n\r\n")
                .map(|(_, payload)| payload.to_string())
                .unwrap_or_default();
            let response = format!(
                "{status_line}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn openai_streams_text_and_tool_calls() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"time\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n",
        );
        let recorded = std::sync::Arc::new(std::sync::Mutex::new(Recorded::default()));
        let base_url = stub_server(body, "HTTP/1.1 200 OK", std::sync::Arc::clone(&recorded)).await;

        let driver = OpenAiCompatDriver {
            base_url,
            api_key: "test-key".into(),
        };
        let (tx, rx) = mpsc::channel();
        let usage = driver.stream(openai_request(), tx).await.unwrap();

        assert_eq!(
            usage,
            Usage {
                input_tokens: 7,
                cache_read_tokens: 4,
                cache_creation_tokens: 0,
                output_tokens: 9,
            }
        );
        assert_eq!(rx.recv().unwrap(), DriverEvent::Text("Hel".into()));
        assert_eq!(rx.recv().unwrap(), DriverEvent::Text("lo".into()));
        assert_eq!(
            rx.recv().unwrap(),
            DriverEvent::ToolCallStart(DriverToolCall {
                id: "c1".into(),
                name: "time".into(),
                arguments: String::new(),
            })
        );
        assert_eq!(
            rx.recv().unwrap(),
            DriverEvent::ToolCallEnd(DriverToolCall {
                id: "c1".into(),
                name: "time".into(),
                arguments: "{}".into(),
            })
        );
        assert!(rx.try_recv().is_err(), "no events after [DONE]");
        assert_eq!(
            recorded.lock().unwrap().authorization.as_deref(),
            Some("authorization: Bearer test-key")
        );
    }

    #[tokio::test]
    async fn openai_maps_http_status_errors() {
        for (status_line, expected, retryable) in [
            ("HTTP/1.1 401 Unauthorized", DriverError::Auth, false),
            (
                "HTTP/1.1 429 Too Many Requests",
                DriverError::RateLimited,
                true,
            ),
            (
                "HTTP/1.1 500 Internal Server Error",
                // Empty body: the excerpt suffix is empty.
                DriverError::Internal("upstream status 500 Internal Server Error: ".into()),
                true,
            ),
        ] {
            // A repeated stub so retries keep seeing the same status until the
            // tiny budget gives up (otherwise the 5xx/429 retry loops forever).
            let responses = vec![format!("{status_line}\r\ncontent-length: 0\r\n\r\n"); 20];
            let (base_url, _hits) = stub_sequence(responses).await;
            let mut req = openai_request();
            if retryable {
                req.retry = RetryConfig {
                    base_backoff_ms: 1,
                    max_backoff_ms: 1,
                    max_ms: 5,
                };
            }
            let driver = OpenAiCompatDriver {
                base_url,
                api_key: String::new(),
            };
            let (tx, _rx) = mpsc::channel();
            assert_eq!(driver.stream(req, tx).await, Err(expected));
        }
    }
    /// A stub that answers each accepted connection with the next response in
    /// `responses`, counting how many requests it served. Each response is a
    /// full HTTP response string.
    async fn stub_sequence(
        responses: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn openai_retries_transient_then_succeeds() {
        let ok_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let responses = vec![
            "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".to_string(),
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{}",
                ok_body.len(),
                ok_body
            ),
        ];
        let (base_url, hits) = stub_sequence(responses).await;
        let mut req = openai_request();
        req.retry = RetryConfig {
            base_backoff_ms: 10,
            max_backoff_ms: 10,
            max_ms: 1000,
        };
        let driver = OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        };
        let (tx, rx) = mpsc::channel();
        assert!(driver.stream(req, tx).await.is_ok());
        assert_eq!(hits.load(Ordering::SeqCst), 2, "503 retried once");
        // The client is told about the retry before any content streams; the
        // payload carries the backoff duration and attempt for a countdown.
        let status = match rx.recv().unwrap() {
            DriverEvent::Status(s) => serde_json::from_str::<serde_json::Value>(&s).unwrap(),
            other => panic!("expected a status event, got {other:?}"),
        };
        assert!(status["retry_in_ms"].as_u64().unwrap_or(0) > 0);
        assert_eq!(status["attempt"], serde_json::json!(1));
        assert_eq!(rx.recv().unwrap(), DriverEvent::Text("hi".into()));
    }

    #[tokio::test]
    async fn openai_gives_up_on_rate_limit_within_budget() {
        let responses = (0..10)
            .map(|_| "HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\n\r\n".to_string())
            .collect();
        let (base_url, _hits) = stub_sequence(responses).await;
        let mut req = openai_request();
        req.retry = RetryConfig {
            base_backoff_ms: 1,
            max_backoff_ms: 1,
            max_ms: 3,
        };
        let driver = OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        };
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            driver.stream(req, tx).await.unwrap_err(),
            DriverError::RateLimited,
            "gives up with RateLimited once the retry budget is spent"
        );
    }

    #[tokio::test]
    async fn openai_detects_context_exceeded_without_retry() {
        let body = r#"{"error":{"message":"This model's maximum context length is 4000 tokens"}}"#;
        let responses = vec![format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )];
        let (base_url, hits) = stub_sequence(responses).await;
        let driver = OpenAiCompatDriver {
            base_url,
            api_key: String::new(),
        };
        let (tx, _rx) = mpsc::channel();
        assert_eq!(
            driver.stream(openai_request(), tx).await.unwrap_err(),
            DriverError::ContextExceeded
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "context-exceeded errors are not retried"
        );
    }

    /// An interrupted tool call (index + id but no function name) must not be
    /// promoted to a ToolCallEnd on stream flush — it has nothing to invoke.
    #[test]
    fn decoder_drops_incomplete_tool_call_without_a_name() {
        let mut decoder = SseDecoder::new();
        let events = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"c1","type":"function"}
                ]}}]}"#,
            )
            .unwrap();
        let SseEventBatch::Events(events) = events else {
            panic!("expected an events batch, got a done marker");
        };
        assert!(events.is_empty(), "no events until the name arrives");
        assert!(
            decoder.finish().is_empty(),
            "an unannounced partial call is dropped, not replayed"
        );
    }

    /// Some providers send `function.arguments` as a JSON object rather than a
    /// streamed string; the decoder must accept both.
    #[test]
    fn decoder_accepts_non_string_arguments() {
        let mut decoder = SseDecoder::new();
        let batch = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"c1","function":{"name":"bash","arguments":{"command":"ls"}}}
                ]}}]}"#,
            )
            .unwrap();
        let SseEventBatch::Events(events) = batch else {
            panic!("expected an events batch, got a done marker");
        };
        let DriverEvent::ToolCallStart(tc) = &events[0] else {
            panic!("expected a tool-call start");
        };
        assert_eq!(tc.name, "bash");
        assert!(tc.arguments.is_empty(), "start carries no args yet");
        let DriverEvent::ToolCallEnd(tc) = &decoder.finish()[0] else {
            panic!("expected a tool-call end");
        };
        assert_eq!(tc.arguments, r#"{"command":"ls"}"#);
    }

    /// Some gateways repeat `id`/`name` as empty strings on later argument
    /// deltas; those must not clobber the real values from the first delta.
    #[test]
    fn decoder_ignores_empty_id_name_on_later_deltas() {
        let mut decoder = SseDecoder::new();
        let batch = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call_x","type":"function","function":{"name":"bash","arguments":""}}
                ]}}]}"#,
            )
            .unwrap();
        let SseEventBatch::Events(events) = batch else {
            panic!("expected an events batch, got a done marker");
        };
        assert!(matches!(
            &events[0],
            DriverEvent::ToolCallStart(tc) if tc.name == "bash" && tc.id == "call_x"
        ));
        // Argument deltas repeat the id/name as empty strings.
        decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"","type":"","function":{"name":"","arguments":"{\"command\":\"l"}}
                ]}}]}"#,
            )
            .unwrap();
        decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"","type":"","function":{"name":"","arguments":"s\"}"}}
                ]}}]}"#,
            )
            .unwrap();
        let DriverEvent::ToolCallEnd(tc) = &decoder.finish()[0] else {
            panic!("expected a tool-call end");
        };
        assert_eq!(tc.name, "bash", "empty deltas must not clobber the name");
        assert_eq!(tc.id, "call_x", "empty deltas must not clobber the id");
        assert_eq!(tc.arguments, r#"{"command":"ls"}"#);
    }

    /// A provider that never sends an id: the call is still announced with a
    /// synthesized id.
    #[test]
    fn decoder_synthesizes_id_when_provider_omits_it() {
        let mut decoder = SseDecoder::new();
        let batch = decoder
            .decode(
                r#"{"choices":[{"delta":{"tool_calls":[
                    {"index":0,"function":{"name":"bash","arguments":"{\"command\":\"ls\"}"}}
                ]}}]}"#,
            )
            .unwrap();
        let SseEventBatch::Events(events) = batch else {
            panic!("expected an events batch, got a done marker");
        };
        let DriverEvent::ToolCallStart(tc) = &events[0] else {
            panic!("expected a tool-call start");
        };
        assert_eq!(tc.name, "bash");
        assert!(!tc.id.is_empty(), "a synthetic id is assigned");
    }
}
