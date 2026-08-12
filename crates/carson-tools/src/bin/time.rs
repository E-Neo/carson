#![no_main]
#![crate_type = "cdylib"]

mod wit {
    wit_bindgen::generate!({
        path: "wit",
        world: "carson:tool/tool-world",
    });
}

use serde_json::json;
use wit::exports::carson::tool::tool::{Guest, ToolError};

struct TimeTool;

impl Guest for TimeTool {
    fn run(_arguments_json: String) -> Result<String, ToolError> {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Ok(json!({"unix_ms": ms}).to_string())
    }
}

wit::export!(TimeTool with_types_in wit);
