mod http;
mod oauth;
mod poller;
mod search;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use oauth::OAuthState;
use poller::{parse_upstreams, spawn_poller, IndexState};

#[derive(Parser)]
#[command(
    name = "catalog",
    about = "MCP server registry — polls shims, serves unified tool index"
)]
struct Cli {
    /// HTTP listen port
    #[arg(long, default_value = "9200", env = "CATALOG_PORT")]
    port: u16,

    /// Upstream MCP shim instances: name=url,name=url
    #[arg(long, env = "CATALOG_UPSTREAMS")]
    upstreams: String,

    /// Poll interval in seconds
    #[arg(long, default_value = "30", env = "CATALOG_POLL_INTERVAL")]
    poll_interval: u64,

    /// Directory for OAuth credential files (shared with MCP servers)
    #[arg(long, default_value = "/credentials", env = "CATALOG_CREDENTIALS_DIR")]
    credentials_dir: PathBuf,

    /// Base URL for OAuth redirect URIs
    #[arg(
        long,
        default_value = "http://localhost:9200",
        env = "CATALOG_BASE_URL"
    )]
    base_url: String,

    /// Optional API token for protecting catalog APIs (Authorization: Bearer <token>)
    #[arg(long, env = "CATALOG_API_TOKEN")]
    api_token: Option<String>,

    /// API token used by catalog when calling upstream sidecar instances
    #[arg(long, env = "CATALOG_UPSTREAM_API_TOKEN")]
    upstream_api_token: Option<String>,

    /// Allow unauthenticated public API access (development only)
    #[arg(long, env = "CATALOG_ALLOW_UNAUTHENTICATED", default_value_t = false)]
    allow_unauthenticated: bool,

    /// Secret used to verify daemon-issued x-oc-identity-token JWTs.
    #[arg(long, env = "CATALOG_IDENTITY_SECRET")]
    identity_secret: Option<String>,

    /// OPA endpoint for tool authorization decisions.
    #[arg(
        long,
        env = "CATALOG_OPA_URL",
        default_value = "http://opa-management:8181"
    )]
    opa_url: String,

    /// Max concurrent OPA checks when filtering discovery results.
    #[arg(long, env = "CATALOG_AUTHZ_CONCURRENCY", default_value_t = 32)]
    authz_concurrency: usize,

    /// TTL in seconds for cached discovery authorization decisions.
    #[arg(long, env = "CATALOG_AUTHZ_CACHE_TTL_SECONDS", default_value_t = 30)]
    authz_cache_ttl_seconds: u64,

    /// Max cached discovery authorization decisions kept in memory.
    #[arg(
        long,
        env = "CATALOG_AUTHZ_CACHE_MAX_ENTRIES",
        default_value_t = 10_000
    )]
    authz_cache_max_entries: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("catalog=info")),
        )
        .init();

    let cli = Cli::parse();

    let upstreams = parse_upstreams(&cli.upstreams);
    if upstreams.is_empty() {
        bail!("no upstreams provided — set --upstreams or CATALOG_UPSTREAMS");
    }

    // Build OAuth providers from environment
    let mut providers = HashMap::new();
    if let Some(google) = oauth::google_provider_from_env(&cli.credentials_dir) {
        tracing::info!("Google OAuth provider configured");
        // google-analytics shares the same OAuth credentials as google
        providers.insert("google-analytics".to_string(), google.clone());
        providers.insert("google".to_string(), google);
    }

    let oauth_state = Arc::new(OAuthState::new(providers));

    let api_token = cli.api_token.filter(|v| !v.trim().is_empty());
    let upstream_api_token = cli.upstream_api_token.filter(|v| !v.trim().is_empty());
    let identity_secret = cli
        .identity_secret
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.into_bytes());

    if api_token.is_none() && !cli.allow_unauthenticated {
        bail!("CATALOG_API_TOKEN is required unless CATALOG_ALLOW_UNAUTHENTICATED=true");
    }
    if api_token.is_none() {
        tracing::warn!("catalog is running without API auth; set CATALOG_API_TOKEN to secure APIs");
    }

    tracing::info!(
        port = cli.port,
        upstreams = upstreams.len(),
        poll_interval = cli.poll_interval,
        oauth_providers = oauth_state.providers.len(),
        api_token_enabled = api_token.is_some(),
        upstream_api_token_enabled = upstream_api_token.is_some(),
        identity_token_verification = identity_secret.is_some(),
        authz_concurrency = cli.authz_concurrency,
        authz_cache_ttl_seconds = cli.authz_cache_ttl_seconds,
        authz_cache_max_entries = cli.authz_cache_max_entries,
        allow_unauthenticated = cli.allow_unauthenticated,
        "starting catalog"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    let call_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to build tool-call HTTP client");

    let state = Arc::new(IndexState {
        servers: RwLock::new(Vec::new()),
        search_index: RwLock::new(search::ToolIndex::empty()),
        discovery_authz_cache: RwLock::new(HashMap::new()),
        upstreams,
        client,
        call_client,
        oauth: oauth_state,
        base_url: cli.base_url.trim_end_matches('/').to_string(),
        api_token,
        upstream_api_token,
        identity_secret,
        opa_url: cli.opa_url.trim_end_matches('/').to_string(),
        authz_concurrency: cli.authz_concurrency.max(1),
        authz_cache_ttl: Duration::from_secs(cli.authz_cache_ttl_seconds),
        authz_cache_max_entries: cli.authz_cache_max_entries.max(1),
    });

    spawn_poller(Arc::clone(&state), Duration::from_secs(cli.poll_interval));

    let app = http::router(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!(%addr, "HTTP server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
