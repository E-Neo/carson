use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent: String,
    pub name: Option<String>,
    pub sandbox_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxSummary {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}
