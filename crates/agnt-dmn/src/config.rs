//! Daemon configuration.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub store_path: Option<String>,
    #[serde(default)]
    pub exposes: Vec<String>,
}

fn default_host() -> String { "127.0.0.1".into() }
fn default_port() -> u16 { 7770 }
fn default_model() -> String { "gemma3:4b".into() }
fn default_provider() -> String { "ollama".into() }
fn default_base_url() -> String { "http://localhost:11434".into() }

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            model: default_model(),
            provider: default_provider(),
            api_key: None,
            base_url: default_base_url(),
            store_path: None,
            exposes: vec![],
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let config_path = config_path();
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).unwrap_or_default();
            toml::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn store_db_path(&self) -> PathBuf {
        if let Some(ref p) = self.store_path {
            PathBuf::from(p)
        } else {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("dmn")
                .join("dmn.db")
        }
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("dmn")
        .join("dmn.toml")
}
