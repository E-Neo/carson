use std::sync::mpsc;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;

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
}

#[derive(Debug, PartialEq)]
pub enum DriverEvent {
    Text(String),
    Thinking(String),
    ToolCallStart(DriverToolCall),
    ToolCallEnd(DriverToolCall),
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

    /// Emits a `ToolCallEnd` for every tool call that never completed.
    pub fn finish(&mut self) -> Vec<DriverEvent> {
        std::mem::take(&mut self.tool_state)
            .into_values()
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
                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    entry.id = id.to_string();
                }
                if let Some(name) = call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                {
                    entry.name = name.to_string();
                }
                if !self.announced.contains(&index)
                    && !entry.id.is_empty()
                    && !entry.name.is_empty()
                {
                    self.announced.insert(index);
                    events.push(DriverEvent::ToolCallStart(entry.clone()));
                }
                if let Some(arg) = call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|v| v.as_str())
                    && !arg.is_empty()
                {
                    entry.arguments.push_str(arg);
                }
            }
        }
        Some(SseEventBatch::Events(events))
    }
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
                        json!({"id": tc.id, "type": "function", "function": {"name": tc.name, "arguments": tc.arguments}})
                    })
                    .collect();
                messages
                    .push(json!({"role": "assistant", "content": m.content, "tool_calls": calls}));
            } else if let Some(tool_call_id) = &m.tool_call_id {
                messages.push(
                    json!({"role": "tool", "tool_call_id": tool_call_id, "content": m.content}),
                );
            } else {
                messages.push(json!({"role": m.role, "content": m.content}));
            }
        }
        body.insert("messages".into(), json!(messages));
        body.insert("stream".into(), json!(true));

        if !req.tools.is_empty() {
            let tools: Vec<_> = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}})
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

        let client = reqwest::Client::new();
        let mut builder = client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        let resp = builder
            .send()
            .await
            .map_err(|err| DriverError::Internal(format!("request to provider failed: {err}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(if status == reqwest::StatusCode::UNAUTHORIZED {
                DriverError::Auth
            } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                DriverError::RateLimited
            } else {
                DriverError::Internal(format!("upstream status {status}"))
            });
        }

        let mut chunks = resp.bytes_stream();
        let mut decoder = SseDecoder::new();
        let mut buffer: Vec<u8> = Vec::new();

        while let Some(chunk) = chunks.next().await {
            let chunk =
                chunk.map_err(|err| DriverError::Internal(format!("stream read failed: {err}")))?;
            buffer.extend_from_slice(&chunk);

            while let Some(line) = pop_line(&mut buffer) {
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let events = match decoder.decode(data) {
                    Some(SseEventBatch::Done) => decoder.finish(),
                    Some(SseEventBatch::Events(events)) => events,
                    None => continue,
                };
                for event in events {
                    if tx.send(event).is_err() {
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
            if tx.send(event).is_err() {
                return Err(DriverError::Cancelled);
            }
        }
        Ok(decoder.usage().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn user(content: &str) -> DriverMessage {
        DriverMessage {
            role: "user".into(),
            content: Some(content.into()),
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

    fn openai_request() -> LlmRequest {
        LlmRequest {
            model: "mock".into(),
            messages: vec![user("hi")],
            system_prompt: None,
            tools: vec![],
            temperature: None,
            max_tokens: None,
        }
    }

    /// Serves one canned HTTP response, recording the Authorization header.
    async fn stub_server(
        body: &'static str,
        status_line: &'static str,
        authorization: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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
                *authorization.lock().unwrap() = Some(line.to_string());
            }
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
        let authorization = std::sync::Arc::new(std::sync::Mutex::new(None));
        let base_url = stub_server(body, "HTTP/1.1 200 OK", authorization.clone()).await;

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
            authorization.lock().unwrap().as_deref(),
            Some("authorization: Bearer test-key")
        );
    }

    #[tokio::test]
    async fn openai_maps_http_status_errors() {
        for (status_line, expected) in [
            ("HTTP/1.1 401 Unauthorized", DriverError::Auth),
            ("HTTP/1.1 429 Too Many Requests", DriverError::RateLimited),
            (
                "HTTP/1.1 500 Internal Server Error",
                DriverError::Internal("upstream status 500 Internal Server Error".into()),
            ),
        ] {
            let authorization = std::sync::Arc::new(std::sync::Mutex::new(None));
            let base_url = stub_server("", status_line, authorization).await;
            let driver = OpenAiCompatDriver {
                base_url,
                api_key: String::new(),
            };
            let (tx, _rx) = mpsc::channel();
            let result = driver.stream(openai_request(), tx).await;
            assert_eq!(result, Err(expected));
        }
    }
}
