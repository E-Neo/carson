use js_sys::Promise;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit, Response};

pub fn window() -> web_sys::Window {
    web_sys::window().expect("window should exist")
}

async fn fetch(request: &Request) -> Result<Response, JsValue> {
    let promise: Promise = window().fetch_with_request(request);
    JsFuture::from(promise)
        .await?
        .dyn_into::<Response>()
        .map_err(|_| JsValue::from_str("fetch response was not a Response"))
}

async fn send(method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), JsValue> {
    let init = RequestInit::new();
    init.set_method(method);
    if let Some(body) = body {
        init.set_body(&JsValue::from_str(&body.to_string()));
        let headers = Headers::new()?;
        headers.set("content-type", "application/json")?;
        init.set_headers(&headers);
    }
    let request = Request::new_with_str_and_init(path, &init)?;
    let resp = fetch(&request).await?;
    let status = resp.status();
    let text = JsFuture::from(resp.text()?)
        .await?
        .as_string()
        .unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    Ok((status, value))
}

pub async fn get(path: &str) -> Result<(u16, Value), JsValue> {
    send("GET", path, None).await
}

pub async fn post(path: &str, body: &Value) -> Result<(u16, Value), JsValue> {
    send("POST", path, Some(body)).await
}

pub async fn put(path: &str, body: &Value) -> Result<(u16, Value), JsValue> {
    send("PUT", path, Some(body)).await
}

pub async fn delete(path: &str) -> Result<(u16, Value), JsValue> {
    send("DELETE", path, None).await
}

pub fn err_text(err: JsValue) -> String {
    err.as_string()
        .unwrap_or_else(|| "unknown fetch error".to_string())
}
