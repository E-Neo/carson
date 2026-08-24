use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub agent: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
}
