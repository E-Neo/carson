use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub default_model: Option<DefaultModel>,
    pub providers: HashMap<String, Provider>,
    pub tools: HashMap<String, ToolKind>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolKind {
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_parameters")]
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_parameters() -> serde_json::Value {
    serde_json::json!({})
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Server {
    pub bind: SocketAddr,
    pub api_key: String,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8000".parse().expect("valid default address"),
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, Deserialize, utoipa::ToSchema)]
pub struct DefaultModel {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub driver: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }

    pub fn api_key(&self) -> Option<&str> {
        let key = self.server.api_key.trim();
        if key.is_empty() { None } else { Some(key) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let text = r#"
[server]
bind = "127.0.0.1:8000"
api_key = "k"

[default_model]
provider = "mock"
model = "mock"

[providers.mock]
driver = "echo"
model = "mock"

[tools.time]
description = "Return the current unix time in milliseconds"
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.server.bind.to_string(), "127.0.0.1:8000");
        assert_eq!(cfg.api_key(), Some("k"));
        assert!(cfg.tools.contains_key("time"));
        assert!(cfg.providers.contains_key("mock"));
    }

    #[test]
    fn empty_api_key_is_unset() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.api_key().is_none());
        assert!(cfg.default_model.is_none());
        assert_eq!(cfg.server.bind.to_string(), "127.0.0.1:8000");
    }

    #[test]
    fn load_parses_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "[server]\nbind = \"127.0.0.1:9000\"\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.bind.to_string(), "127.0.0.1:9000");
    }

    #[test]
    fn load_reports_missing_file() {
        let err = Config::load(Path::new("/nonexistent/carson.toml")).unwrap_err();
        assert!(err.to_string().contains("read config"));
    }
}
