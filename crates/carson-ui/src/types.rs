use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub id: u64,
    pub agent: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallInfo {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "arguments")]
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageInfo {
    #[serde(rename = "role")]
    pub role: String,
    #[serde(rename = "content")]
    pub content: Option<String>,
    #[serde(rename = "tool_calls")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
    #[serde(rename = "tool_call_id")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}
