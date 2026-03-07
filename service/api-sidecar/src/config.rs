use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub service: ServiceConfig,
    #[serde(default)]
    pub target: TargetConfig,
    pub openapi: OpenApiConfig,
    #[serde(default)]
    pub arazzo: Option<ArazzoConfig>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub compile: CompileConfig,
    #[serde(default)]
    pub overrides: OverridesConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServiceConfig {
    #[serde(default = "default_service_name")]
    pub name: String,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TargetConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenApiConfig {
    pub source: DocumentSource,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArazzoConfig {
    #[serde(default)]
    pub enabled: bool,
    pub source: DocumentSource,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DocumentSource {
    File {
        path: PathBuf,
    },
    Url {
        url: String,
    },
    Probe {
        base_url: String,
        #[serde(default = "default_probe_candidates")]
        candidates: Vec<String>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum AuthConfig {
    #[default]
    None,
    Bearer {
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        token_env: Option<String>,
    },
    ApiKeyHeader {
        header: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        value_env: Option<String>,
    },
    ApiKeyQuery {
        name: String,
        #[serde(default)]
        value: Option<String>,
        #[serde(default)]
        value_env: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CompileConfig {
    #[serde(default)]
    pub include_tags: Vec<String>,
    #[serde(default)]
    pub exclude_operations: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverridesConfig {
    #[serde(default)]
    pub operation_ids: HashMap<String, OperationOverride>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperationOverride {
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub response_pointer: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let config = match ext {
            "yaml" | "yml" => serde_yaml::from_str(&content)
                .with_context(|| format!("failed to parse YAML config file {}", path.display()))?,
            "json" => serde_json::from_str(&content)
                .with_context(|| format!("failed to parse JSON config file {}", path.display()))?,
            "toml" => toml::from_str(&content)
                .with_context(|| format!("failed to parse TOML config file {}", path.display()))?,
            _ => bail!("unsupported config extension: {}", path.display()),
        };
        Ok(config)
    }
}

impl AuthConfig {
    pub fn resolved_secret(&self) -> Result<Option<(String, String)>> {
        match self {
            Self::None => Ok(None),
            Self::Bearer { token, token_env } => {
                let value = resolve_secret(token.clone(), token_env.as_deref())?;
                Ok(Some((
                    "authorization".to_string(),
                    format!("Bearer {value}"),
                )))
            }
            Self::ApiKeyHeader {
                header,
                value,
                value_env,
            } => {
                let value = resolve_secret(value.clone(), value_env.as_deref())?;
                Ok(Some((header.clone(), value)))
            }
            Self::ApiKeyQuery { .. } => Ok(None),
        }
    }

    pub fn resolved_query_secret(&self) -> Result<Option<(String, String)>> {
        match self {
            Self::ApiKeyQuery {
                name,
                value,
                value_env,
            } => {
                let value = resolve_secret(value.clone(), value_env.as_deref())?;
                Ok(Some((name.clone(), value)))
            }
            _ => Ok(None),
        }
    }
}

fn resolve_secret(value: Option<String>, env_name: Option<&str>) -> Result<String> {
    if let Some(value) = value.filter(|inner| !inner.trim().is_empty()) {
        return Ok(value);
    }
    if let Some(env_name) = env_name.filter(|inner| !inner.trim().is_empty()) {
        return std::env::var(env_name)
            .with_context(|| format!("missing auth secret in environment variable {env_name}"));
    }
    bail!("missing auth secret")
}

fn default_service_name() -> String {
    "api-sidecar".to_string()
}

fn default_timeout_seconds() -> u64 {
    30
}

fn default_probe_candidates() -> Vec<String> {
    vec![
        "/openapi.json".to_string(),
        "/openapi.yaml".to_string(),
        "/swagger/v1/swagger.json".to_string(),
        "/v3/api-docs".to_string(),
    ]
}
