mod http;
mod mcp_client;
mod middleware;
mod tenancy;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use mcp_client::SpawnConfig;
use middleware::MiddlewareConfig;
use tenancy::{ClientPool, TenantMode};

#[derive(Parser)]
#[command(
    name = "mcp-sidecar",
    about = "Bridge any MCP server (stdio) to an HTTP API",
    after_help = "Pass the MCP server command after --:\n  mcp-sidecar -- npx @modelcontextprotocol/server-filesystem /data"
)]
struct Cli {
    /// HTTP listen port
    #[arg(long, default_value = "9100", env = "MCP_SIDECAR_PORT")]
    port: u16,

    /// Service name for logging
    #[arg(long, env = "MCP_SIDECAR_NAME")]
    name: Option<String>,

    /// Max seconds to wait for MCP initialize response
    #[arg(long, default_value = "10", env = "MCP_SIDECAR_INIT_TIMEOUT")]
    init_timeout: u64,

    /// Max seconds to wait for a tool call response (0 = no limit)
    #[arg(long, default_value = "60", env = "MCP_SIDECAR_CALL_TIMEOUT")]
    call_timeout: u64,

    /// Path to middleware TOML config (input/output transforms, filters)
    #[arg(long, env = "MCP_SIDECAR_MIDDLEWARE")]
    middleware: Option<PathBuf>,

    /// Optional API token for protecting sidecar APIs (Authorization: Bearer <token>)
    #[arg(long, env = "MCP_SIDECAR_API_TOKEN")]
    api_token: Option<String>,

    /// Allow unauthenticated sidecar API access (development only)
    #[arg(
        long,
        env = "MCP_SIDECAR_ALLOW_UNAUTHENTICATED",
        default_value_t = false
    )]
    allow_unauthenticated: bool,

    /// Secret used to verify daemon-issued x-oc-identity-token JWTs.
    #[arg(long, env = "MCP_SIDECAR_IDENTITY_SECRET")]
    identity_secret: Option<String>,

    /// Runtime isolation mode for MCP instances.
    #[arg(long, env = "MCP_SIDECAR_TENANCY", value_enum, default_value_t = TenantMode::Shared)]
    tenancy: TenantMode,

    /// Maximum isolated MCP instances to keep in the tenant pool.
    #[arg(long, env = "MCP_SIDECAR_MAX_TENANT_CLIENTS", default_value_t = 128)]
    max_tenant_clients: usize,

    /// Idle TTL for tenant-scoped MCP instances in seconds (0 disables idle eviction).
    #[arg(
        long,
        env = "MCP_SIDECAR_TENANT_IDLE_TTL_SECONDS",
        default_value_t = 900
    )]
    tenant_idle_ttl_seconds: u64,

    /// The MCP server command and arguments (everything after --)
    #[arg(trailing_var_arg = true, required = true)]
    command: Vec<String>,
}

pub struct AppState {
    pub clients: ClientPool,
    pub middleware: RwLock<Option<Arc<MiddlewareConfig>>>,
    pub middleware_path: Option<PathBuf>,
    pub api_token: Option<String>,
    pub identity_secret: Option<Vec<u8>>,
    /// Max duration for a single tool call (None = no limit).
    pub call_timeout: Option<Duration>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("mcp_sidecar=info")),
        )
        .init();

    let cli = Cli::parse();

    if cli.command.is_empty() {
        bail!("no MCP server command provided — pass it after --");
    }

    let program = &cli.command[0];
    let args: Vec<String> = cli.command[1..].to_vec();
    let service_name = cli.name.as_deref().unwrap_or(program);

    tracing::info!(service_name, port = cli.port, "starting mcp-sidecar");

    let api_token = cli.api_token.filter(|v| !v.trim().is_empty());
    let identity_secret = cli
        .identity_secret
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.into_bytes());
    if api_token.is_none() && !cli.allow_unauthenticated {
        bail!("MCP_SIDECAR_API_TOKEN is required unless MCP_SIDECAR_ALLOW_UNAUTHENTICATED=true");
    }
    if api_token.is_none() {
        tracing::warn!(
            "mcp-sidecar is running without API auth; set MCP_SIDECAR_API_TOKEN to secure APIs"
        );
    }
    if cli.tenancy != TenantMode::Shared && identity_secret.is_none() {
        bail!(
            "MCP_SIDECAR_IDENTITY_SECRET is required when MCP_SIDECAR_TENANCY is set to {}",
            cli.tenancy.as_str()
        );
    }

    // Load middleware config if provided
    let mw = match &cli.middleware {
        Some(path) => {
            let config =
                MiddlewareConfig::load(path).context("failed to load middleware config")?;
            Some(Arc::new(config))
        }
        None => None,
    };

    let spawn_config = SpawnConfig {
        program: program.clone(),
        args,
        init_timeout: Duration::from_secs(cli.init_timeout),
    };

    // Spawn the MCP server and perform handshake
    let discovery_client = mcp_client::McpClient::spawn_from_config(&spawn_config)
        .await
        .context("failed to start MCP server")?;

    let call_timeout = if cli.call_timeout > 0 {
        Some(Duration::from_secs(cli.call_timeout))
    } else {
        None
    };

    let state = Arc::new(AppState {
        clients: ClientPool::new(
            discovery_client,
            spawn_config.clone(),
            cli.tenancy,
            cli.max_tenant_clients,
            if cli.tenant_idle_ttl_seconds > 0 {
                Some(Duration::from_secs(cli.tenant_idle_ttl_seconds))
            } else {
                None
            },
        ),
        middleware: RwLock::new(mw),
        middleware_path: cli.middleware,
        api_token,
        identity_secret,
        call_timeout,
    });

    // Build HTTP router
    let app = http::router(state);

    // Start serving
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!(%addr, service_name, "HTTP server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
