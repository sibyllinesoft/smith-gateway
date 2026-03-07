mod compiler;
mod config;
mod http;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

use compiler::{compile_snapshot, CompiledSnapshot};
use config::Config;

#[derive(Parser)]
#[command(
    name = "api-sidecar",
    about = "Expose OpenAPI-described HTTP APIs through the smith tool shim contract"
)]
struct Cli {
    /// Path to the sidecar configuration file.
    #[arg(long, env = "API_SIDECAR_CONFIG")]
    config: PathBuf,

    /// Override the listen port from the config file.
    #[arg(long, env = "API_SIDECAR_PORT")]
    port: Option<u16>,

    /// Optional API token for protecting sidecar APIs (Authorization: Bearer <token>).
    #[arg(long, env = "API_SIDECAR_API_TOKEN")]
    api_token: Option<String>,

    /// Allow unauthenticated sidecar API access (development only).
    #[arg(
        long,
        env = "API_SIDECAR_ALLOW_UNAUTHENTICATED",
        default_value_t = false
    )]
    allow_unauthenticated: bool,
}

pub struct AppState {
    pub config_path: PathBuf,
    pub compiled: RwLock<Arc<CompiledSnapshot>>,
    pub client: reqwest::Client,
    pub api_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("api_sidecar=info")),
        )
        .init();

    let cli = Cli::parse();
    let api_token = cli.api_token.filter(|value| !value.trim().is_empty());
    if api_token.is_none() && !cli.allow_unauthenticated {
        bail!("API_SIDECAR_API_TOKEN is required unless API_SIDECAR_ALLOW_UNAUTHENTICATED=true");
    }
    if api_token.is_none() {
        tracing::warn!(
            "api-sidecar is running without API auth; set API_SIDECAR_API_TOKEN to secure APIs"
        );
    }

    let config = Config::load(&cli.config).context("failed to load api-sidecar config")?;
    let client = reqwest::Client::builder()
        .build()
        .context("failed to build HTTP client")?;
    let compiled = compile_snapshot(&config, &client)
        .await
        .context("failed to compile OpenAPI sidecar snapshot")?;

    let port = cli.port.or(config.service.port).unwrap_or(9100);
    tracing::info!(
        service_name = compiled.service_name,
        target_base_url = compiled.target_base_url,
        tools = compiled.tools.len(),
        port,
        "starting api-sidecar"
    );

    let state = Arc::new(AppState {
        config_path: cli.config,
        compiled: RwLock::new(Arc::new(compiled)),
        client,
        api_token,
    });

    let app = http::router(state);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, "HTTP server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
