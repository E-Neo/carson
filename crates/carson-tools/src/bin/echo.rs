#![no_main]
#![crate_type = "cdylib"]

mod wit {
    wit_bindgen::generate!({
        path: "wit",
        world: "carson:tool/tool-world",
    });
}

use wit::exports::carson::tool::tool::{Guest, ToolError};

struct EchoTool;

impl Guest for EchoTool {
    fn run(arguments_json: String) -> Result<String, ToolError> {
        Ok(arguments_json.to_string())
    }
}

wit::export!(EchoTool with_types_in wit);
