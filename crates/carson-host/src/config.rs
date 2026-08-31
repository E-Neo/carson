use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Server {
    pub ip: IpAddr,
    pub port: u16,
    /// API bearer token (also the login password for the web UI). When absent,
    /// `main` generates one, writes it back into `config.toml`, and logs it.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    /// Load config, treating a missing file as defaults (the file is created
    /// on first bootstrap when a token is generated).
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(err) => return Err(err).with_context(|| format!("read config {}", path.display())),
        };
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
    fn load_missing_file_returns_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/carson.toml")).unwrap();
        assert_eq!(cfg.server.bind().to_string(), "127.0.0.1:8000");
    }

    #[test]
    fn roundtrip_serializes_token_back() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !text.contains("token"),
            "absent token is not written: {text}"
        );
        let mut cfg = cfg;
        cfg.server.token = Some("abc".into());
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("token = \"abc\""), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.server.token.as_deref(), Some("abc"));
    }
}
