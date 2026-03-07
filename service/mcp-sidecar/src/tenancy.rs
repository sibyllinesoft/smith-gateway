use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    tenants: Mutex<HashMap<String, TenantEntry>>,
    config: SpawnConfig,
    mode: TenantMode,
    max_tenant_clients: usize,
    idle_ttl: Option<Duration>,
}

struct TenantEntry {
    client: Arc<McpClient>,
    last_used_at: Instant,
}

impl ClientPool {
    pub fn new(
        discovery: Arc<McpClient>,
        config: SpawnConfig,
        mode: TenantMode,
        max_tenant_clients: usize,
        idle_ttl: Option<Duration>,
    ) -> Self {
        Self {
            discovery: RwLock::new(discovery),
            tenants: Mutex::new(HashMap::new()),
            config,
            mode,
            max_tenant_clients: max_tenant_clients.max(1),
            idle_ttl,
        }
    }

    pub fn mode(&self) -> TenantMode {
        self.mode
    }

    pub async fn discovery_client(&self) -> Arc<McpClient> {
        self.discovery.read().await.clone()
    }

    pub async fn active_tenant_clients(&self) -> usize {
        self.prune_idle_clients().await;
        self.tenants.lock().await.len()
    }

    pub async fn call_client(
        &self,
        identity: Option<&IdentityContext>,
    ) -> std::result::Result<Arc<McpClient>, CallClientError> {
        let Some(key) = self.tenant_key(identity)? else {
            return Ok(self.discovery_client().await);
        };

        let now = Instant::now();
        let mut expired_clients = Vec::new();
        let mut tenants = self.tenants.lock().await;
        prune_idle_entries(&mut tenants, self.idle_ttl, now, &mut expired_clients);
        if let Some(entry) = tenants.get_mut(&key) {
            entry.last_used_at = now;
            let client = entry.client.clone();
            drop(tenants);
            shutdown_clients(expired_clients).await;
            return Ok(client);
        }

        if tenants.len() >= self.max_tenant_clients {
            let error = CallClientError::Capacity {
                limit: self.max_tenant_clients,
                mode: self.mode,
            };
            drop(tenants);
            shutdown_clients(expired_clients).await;
            return Err(error);
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
        tenants.insert(
            key,
            TenantEntry {
                client: client.clone(),
                last_used_at: now,
            },
        );
        drop(tenants);
        shutdown_clients(expired_clients).await;
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
                .map(|(_, entry)| entry.client)
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

    async fn prune_idle_clients(&self) -> usize {
        let mut expired_clients = Vec::new();
        {
            let mut tenants = self.tenants.lock().await;
            prune_idle_entries(
                &mut tenants,
                self.idle_ttl,
                Instant::now(),
                &mut expired_clients,
            );
        }
        let pruned_count = expired_clients.len();
        shutdown_clients(expired_clients).await;
        pruned_count
    }
}

fn prune_idle_entries(
    tenants: &mut HashMap<String, TenantEntry>,
    idle_ttl: Option<Duration>,
    now: Instant,
    expired_clients: &mut Vec<Arc<McpClient>>,
) {
    let Some(idle_ttl) = idle_ttl else {
        return;
    };

    let expired_keys = tenants
        .iter()
        .filter_map(|(key, entry)| {
            is_entry_idle(entry.last_used_at, now, idle_ttl).then_some(key.clone())
        })
        .collect::<Vec<_>>();

    for key in expired_keys {
        if let Some(entry) = tenants.remove(&key) {
            expired_clients.push(entry.client);
        }
    }
}

fn is_entry_idle(last_used_at: Instant, now: Instant, idle_ttl: Duration) -> bool {
    now.saturating_duration_since(last_used_at) >= idle_ttl
}

async fn shutdown_clients(clients: Vec<Arc<McpClient>>) {
    for client in clients {
        client.shutdown().await;
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
    use std::time::{Duration, Instant};

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

    #[test]
    fn idle_check_respects_ttl_boundary() {
        let start = Instant::now();
        assert!(!super::is_entry_idle(
            start,
            start + Duration::from_secs(29),
            Duration::from_secs(30)
        ));
        assert!(super::is_entry_idle(
            start,
            start + Duration::from_secs(30),
            Duration::from_secs(30)
        ));
    }
}
