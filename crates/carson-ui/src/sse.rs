use js_sys::Uint8Array;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, ReadableStreamDefaultReader, Request, RequestInit, Response, TextDecoder};

use crate::api::window;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// POST `body` to `path` and read the `text/event-stream` response incrementally,
/// invoking `on_event` for every parsed frame.
pub async fn stream_post(
    path: &str,
    body: &Value,
    mut on_event: impl FnMut(SseEvent),
) -> Result<(), String> {
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body.to_string()));
    let headers = Headers::new().map_err(|_| "failed to create headers")?;
    headers
        .set("content-type", "application/json")
        .map_err(|_| "failed to set content-type")?;
    init.set_headers(&headers);
    let request =
        Request::new_with_str_and_init(path, &init).map_err(|_| "failed to build request")?;
    let resp: Response = JsFuture::from(window().fetch_with_request(&request))
        .await
        .map_err(|_| "stream request failed")?
        .dyn_into()
        .map_err(|_| "stream response was not a Response")?;
    if !resp.ok() {
        return Err(format!("stream request returned status {}", resp.status()));
    }
    let stream = resp.body().ok_or("response has no body")?;
    let reader: ReadableStreamDefaultReader = stream
        .get_reader()
        .dyn_into()
        .map_err(|_| "failed to create stream reader")?;
    let decoder = TextDecoder::new().map_err(|_| "failed to create TextDecoder")?;

    let mut buffer = String::new();
    loop {
        let result = JsFuture::from(reader.read())
            .await
            .map_err(|_| "stream read failed")?;
        if result.is_undefined() || result.is_null() {
            break;
        }
        let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))
            .map(|v| v.is_truthy())
            .unwrap_or(false);
        if done {
            break;
        }
        let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))
            .unwrap_or(JsValue::UNDEFINED);
        if value.is_undefined() || value.is_null() {
            continue;
        }
        let bytes: Uint8Array = value.dyn_into().map_err(|_| "chunk was not a Uint8Array")?;
        let text = decoder
            .decode_with_buffer_source(&bytes)
            .map_err(|_| "failed to decode chunk")?;
        buffer.push_str(&text);
        while let Some(idx) = buffer.find("\n\n") {
            let frame = buffer[..idx].to_string();
            buffer = buffer[idx + 2..].to_string();
            if let Some(event) = parse_frame(&frame) {
                on_event(event);
            }
        }
    }
    if let Some(event) = parse_frame(&buffer) {
        on_event(event);
    }
    Ok(())
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut event = String::new();
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim());
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseEvent { event, data })
}
