use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Server {
    pub ip: IpAddr,
    pub port: u16,
    /// API bearer token. When `None`, `main` generates one, persists it, and
    /// logs it; `CARSON_API_TOKEN` overrides the config value.
    pub token: Option<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            ip: "127.0.0.1".parse().expect("valid default ip"),
            port: 8000,
            token: None,
        }
    }
}

impl Server {
    pub fn bind(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let text = r#"
[server]
ip = "127.0.0.1"
port = 9000
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.server.bind().to_string(), "127.0.0.1:9000");
    }

    #[test]
    fn empty_config_uses_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.server.bind().to_string(), "127.0.0.1:8000");
    }

    #[test]
    fn load_parses_a_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "[server]\nip = \"0.0.0.0\"\nport = 8080\n").unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.server.bind().to_string(), "0.0.0.0:8080");
    }

    #[test]
    fn load_reports_missing_file() {
        let err = Config::load(Path::new("/nonexistent/carson.toml")).unwrap_err();
        assert!(err.to_string().contains("read config"));
    }
}
