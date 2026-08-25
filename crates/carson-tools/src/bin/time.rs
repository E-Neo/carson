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
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok(json!({ "time": format_iso8601(ms) }).to_string())
    }
}

/// Format milliseconds since the unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn format_iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

wit::export!(TimeTool with_types_in wit);
