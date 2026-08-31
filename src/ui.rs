use axum::Router;
use axum::extract::Request;
use axum::http::header::{CONTENT_TYPE, HeaderValue};
use axum::http::{StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};

static UI: Dir<'_> = include_dir!("$CARSON_UI_DIST");

const UI_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data:";

fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

async fn serve_ui(uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api") {
        return (
            StatusCode::NOT_FOUND,
            [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
            r#"{"error":"not found"}"#,
        )
            .into_response();
    }
    let name = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    let (bytes, ctype) = match UI.get_file(name) {
        Some(file) => (
            file.contents(),
            content_type(file.path().to_str().unwrap_or("")),
        ),
        None => {
            let Some(index) = UI.get_file("index.html") else {
                return (StatusCode::NOT_FOUND, "ui not built".to_string()).into_response();
            };
            (index.contents(), "text/html; charset=utf-8")
        }
    };
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static(ctype)),
            (
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// Security headers for the UI (same-origin assets, wasm instantiation allowed).
async fn ui_security(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(UI_CSP),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    resp
}

pub fn router() -> Router {
    Router::new()
        .fallback(|uri: Uri| async move { serve_ui(uri).await })
        .layer(middleware::from_fn(ui_security))
}
