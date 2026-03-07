use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::ValueEnum;
use tokio::sync::{Mutex, RwLock};

use crate::mcp_client::{McpClient, SpawnConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TenantMode {
    Shared,
    Principal,
    Session,
}

#[derive(Debug, Clone)]
pub struct IdentityContext {
    pub principal: String,
    pub session: String,
    pub smith_user_id: Option<String>,
}

#[derive(Debug)]
pub enum CallClientError {
    MissingIdentity(TenantMode),
    MissingSession,
    Capacity { limit: usize, mode: TenantMode },
    Spawn(anyhow::Error),
}

impl fmt::Display for CallClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingIdentity(mode) => {
                write!(
                    f,
                    "verified identity is required for {}-scoped tenancy",
                    mode.as_str()
                )
            }
            Self::MissingSession => write!(f, "verified identity is missing a session claim"),
            Self::Capacity { limit, mode } => write!(
                f,
                "tenant client limit reached ({}); refusing to spawn new {}-scoped MCP instance",
                limit,
                mode.as_str()
            ),
            Self::Spawn(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CallClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

pub struct ClientPool {
    discovery: RwLock<Arc<McpClient>>,
    tenants: Mutex<HashMap<String, Arc<McpClient>>>,
    config: SpawnConfig,
    mode: TenantMode,
    max_tenant_clients: usize,
}

impl ClientPool {
    pub fn new(
        discovery: Arc<McpClient>,
        config: SpawnConfig,
        mode: TenantMode,
        max_tenant_clients: usize,
    ) -> Self {
        Self {
            discovery: RwLock::new(discovery),
            tenants: Mutex::new(HashMap::new()),
            config,
            mode,
            max_tenant_clients: max_tenant_clients.max(1),
        }
    }

    pub fn mode(&self) -> TenantMode {
        self.mode
    }

    pub async fn discovery_client(&self) -> Arc<McpClient> {
        self.discovery.read().await.clone()
    }

    pub async fn active_tenant_clients(&self) -> usize {
        self.tenants.lock().await.len()
    }

    pub async fn call_client(
        &self,
        identity: Option<&IdentityContext>,
    ) -> std::result::Result<Arc<McpClient>, CallClientError> {
        let Some(key) = self.tenant_key(identity)? else {
            return Ok(self.discovery_client().await);
        };

        let mut tenants = self.tenants.lock().await;
        if let Some(client) = tenants.get(&key) {
            return Ok(client.clone());
        }

        if tenants.len() >= self.max_tenant_clients {
            return Err(CallClientError::Capacity {
                limit: self.max_tenant_clients,
                mode: self.mode,
            });
        }

        tracing::info!(
            tenant_mode = self.mode.as_str(),
            tenant_scope = %redact_tenant_key(&key),
            "spawning isolated MCP instance"
        );
        let client = McpClient::spawn_from_config(&self.config)
            .await
            .with_context(|| format!("failed to spawn {}-scoped MCP instance", self.mode.as_str()))
            .map_err(CallClientError::Spawn)?;
        tenants.insert(key, client.clone());
        Ok(client)
    }

    pub async fn reload(&self) -> Result<usize> {
        let new_discovery = McpClient::spawn_from_config(&self.config)
            .await
            .context("failed to spawn discovery MCP instance during reload")?;
        let tools_count = new_discovery.tools.len();

        let old_discovery = {
            let mut discovery = self.discovery.write().await;
            std::mem::replace(&mut *discovery, new_discovery)
        };

        let old_tenants = {
            let mut tenants = self.tenants.lock().await;
            tenants
                .drain()
                .map(|(_, client)| client)
                .collect::<Vec<_>>()
        };

        old_discovery.shutdown().await;
        for client in old_tenants {
            client.shutdown().await;
        }

        Ok(tools_count)
    }

    fn tenant_key(
        &self,
        identity: Option<&IdentityContext>,
    ) -> std::result::Result<Option<String>, CallClientError> {
        tenant_key_for(self.mode, identity)
    }
}

fn redact_tenant_key(key: &str) -> String {
    let mut chars = key.chars();
    let prefix: String = chars.by_ref().take(12).collect();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

impl TenantMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Principal => "principal",
            Self::Session => "session",
        }
    }
}

fn tenant_key_for(
    mode: TenantMode,
    identity: Option<&IdentityContext>,
) -> std::result::Result<Option<String>, CallClientError> {
    match mode {
        TenantMode::Shared => Ok(None),
        TenantMode::Principal => {
            let identity = identity.ok_or(CallClientError::MissingIdentity(mode))?;
            let principal = identity
                .smith_user_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(identity.principal.as_str());
            Ok(Some(format!("principal:{principal}")))
        }
        TenantMode::Session => {
            let identity = identity.ok_or(CallClientError::MissingIdentity(mode))?;
            if identity.session.trim().is_empty() {
                return Err(CallClientError::MissingSession);
            }
            Ok(Some(format!("session:{}", identity.session)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tenant_key_for, CallClientError, IdentityContext, TenantMode};

    fn identity() -> IdentityContext {
        IdentityContext {
            principal: "user@example.com".to_string(),
            session: "session-123".to_string(),
            smith_user_id: Some("user-123".to_string()),
        }
    }

    #[test]
    fn shared_mode_uses_no_tenant_key() {
        assert!(tenant_key_for(TenantMode::Shared, None).unwrap().is_none());
    }

    #[test]
    fn principal_mode_prefers_smith_user_id() {
        assert_eq!(
            tenant_key_for(TenantMode::Principal, Some(&identity()))
                .unwrap()
                .as_deref(),
            Some("principal:user-123")
        );
    }

    #[test]
    fn session_mode_uses_session_claim() {
        assert_eq!(
            tenant_key_for(TenantMode::Session, Some(&identity()))
                .unwrap()
                .as_deref(),
            Some("session:session-123")
        );
    }

    #[test]
    fn principal_mode_requires_identity() {
        assert!(matches!(
            tenant_key_for(TenantMode::Principal, None),
            Err(CallClientError::MissingIdentity(TenantMode::Principal))
        ));
    }

    #[test]
    fn session_mode_requires_non_empty_session() {
        let mut identity = identity();
        identity.session.clear();
        assert!(matches!(
            tenant_key_for(TenantMode::Session, Some(&identity)),
            Err(CallClientError::MissingSession)
        ));
    }
}
