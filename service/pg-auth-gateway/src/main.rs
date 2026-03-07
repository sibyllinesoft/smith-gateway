use std::fmt::Debug;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::Parser;
use futures::stream;
use futures::Sink;
use futures::SinkExt;
use jsonwebtoken::{decode, DecodingKey, Validation};
use pgwire::api::auth::{
    finish_authentication, protocol_negotiation, save_startup_parameters_to_metadata,
    DefaultServerParameterProvider, StartupHandler,
};
use pgwire::api::query::SimpleQueryHandler;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::{ClientInfo, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::startup::Authentication;
use pgwire::messages::PgWireBackendMessage;
use pgwire::messages::PgWireFrontendMessage;
use pgwire::tokio::process_socket;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_postgres::{Client, NoTls};
use tracing::{error, warn};

const META_SMITH_USER_ID: &str = "x-smith-user-id";
const META_SMITH_USER_ROLE: &str = "x-smith-user-role";
const META_XOC_CHANNEL: &str = "x-oc-channel";
const META_XOC_PRINCIPAL: &str = "x-oc-principal";
const META_XOC_SESSION: &str = "x-oc-session";

#[derive(Debug, Clone, Parser)]
#[command(name = "pg-auth-gateway")]
struct Cli {
    #[arg(
        long,
        env = "PG_AUTH_GATEWAY_LISTEN_ADDR",
        default_value = "0.0.0.0:5432"
    )]
    listen_addr: SocketAddr,

    #[arg(long, env = "PG_AUTH_GATEWAY_READONLY_URL")]
    readonly_url: String,

    #[arg(long, env = "PG_AUTH_GATEWAY_GATEKEEPER_URL")]
    gatekeeper_url: String,

    #[arg(long, env = "PG_AUTH_GATEWAY_IDENTITY_SECRET")]
    identity_secret: String,

    #[arg(long, env = "PG_AUTH_GATEWAY_BIND_TTL_SECS", default_value_t = 300)]
    bind_ttl_secs: i32,
}

#[derive(Debug, Clone, Deserialize)]
struct IdentityTokenClaims {
    channel: String,
    principal: String,
    session: String,
    #[serde(default)]
    smith_user_id: Option<String>,
    smith_user_role: String,
}

#[derive(Debug, Clone)]
struct QueryIdentity {
    channel: String,
    principal: String,
    session: String,
    smith_user_id: Option<String>,
    smith_user_role: String,
}

impl QueryIdentity {
    fn from_client<C>(client: &C) -> PgWireResult<Self>
    where
        C: ClientInfo,
    {
        let smith_user_role = client
            .metadata()
            .get(META_SMITH_USER_ROLE)
            .cloned()
            .ok_or_else(|| user_error("42501", "missing verified smith role"))?;

        Ok(Self {
            channel: client
                .metadata()
                .get(META_XOC_CHANNEL)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string()),
            principal: client
                .metadata()
                .get(META_XOC_PRINCIPAL)
                .cloned()
                .unwrap_or_default(),
            session: client
                .metadata()
                .get(META_XOC_SESSION)
                .cloned()
                .unwrap_or_default(),
            smith_user_id: client.metadata().get(META_SMITH_USER_ID).cloned(),
            smith_user_role,
        })
    }
}

#[derive(Clone)]
struct GatewayState {
    readonly_url: String,
    gatekeeper_url: String,
    decoding_key: DecodingKey,
    validation: Validation,
    bind_ttl_secs: i32,
}

struct TokenStartupHandler {
    state: Arc<GatewayState>,
    parameters: DefaultServerParameterProvider,
}

struct GatewayQueryHandler {
    state: Arc<GatewayState>,
}

struct GatewayServerHandlers {
    startup: Arc<TokenStartupHandler>,
    query: Arc<GatewayQueryHandler>,
}

impl PgWireServerHandlers for GatewayServerHandlers {
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        self.startup.clone()
    }

    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        self.query.clone()
    }
}

#[async_trait]
impl StartupHandler for TokenStartupHandler {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                protocol_negotiation(client, startup).await?;
                save_startup_parameters_to_metadata(client, startup);
                client.set_state(pgwire::api::PgWireConnectionState::AuthenticationInProgress);
                client
                    .send(PgWireBackendMessage::Authentication(
                        Authentication::CleartextPassword,
                    ))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(pwd) => {
                let pwd = pwd.into_password()?;
                let token = pwd.password;
                let token = std::str::from_utf8(token.as_bytes())
                    .map_err(|_| user_error("28P01", "identity token must be valid UTF-8"))?;

                let decoded = decode::<IdentityTokenClaims>(
                    token,
                    &self.state.decoding_key,
                    &self.state.validation,
                )
                .map_err(|err| user_error("28P01", &format!("invalid identity token: {err}")))?;

                client
                    .metadata_mut()
                    .insert(META_XOC_CHANNEL.to_string(), decoded.claims.channel.clone());
                client.metadata_mut().insert(
                    META_XOC_PRINCIPAL.to_string(),
                    decoded.claims.principal.clone(),
                );
                client
                    .metadata_mut()
                    .insert(META_XOC_SESSION.to_string(), decoded.claims.session.clone());
                client.metadata_mut().insert(
                    META_SMITH_USER_ROLE.to_string(),
                    decoded.claims.smith_user_role.clone(),
                );
                if let Some(user_id) = decoded.claims.smith_user_id {
                    client
                        .metadata_mut()
                        .insert(META_SMITH_USER_ID.to_string(), user_id);
                }

                finish_authentication(client, &self.parameters).await?;
            }
            _ => {}
        }

        Ok(())
    }
}

#[async_trait]
impl SimpleQueryHandler for GatewayQueryHandler {
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let identity = QueryIdentity::from_client(client)?;
        run_query(&self.state, &identity, query).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pg_auth_gateway=info,pgwire=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let state = Arc::new(GatewayState {
        readonly_url: cli.readonly_url,
        gatekeeper_url: cli.gatekeeper_url,
        decoding_key: DecodingKey::from_secret(cli.identity_secret.as_bytes()),
        validation: Validation::default(),
        bind_ttl_secs: cli.bind_ttl_secs,
    });

    let mut parameters = DefaultServerParameterProvider::default();
    parameters.is_superuser = false;
    parameters.default_transaction_read_only = true;

    let handlers = Arc::new(GatewayServerHandlers {
        startup: Arc::new(TokenStartupHandler {
            state: state.clone(),
            parameters,
        }),
        query: Arc::new(GatewayQueryHandler { state }),
    });

    let listener = TcpListener::bind(cli.listen_addr)
        .await
        .with_context(|| format!("failed to bind {}", cli.listen_addr))?;
    tracing::info!(listen_addr = %cli.listen_addr, "pg auth gateway listening");

    loop {
        let (socket, peer_addr) = listener.accept().await.context("accept failed")?;
        let handlers = handlers.clone();

        tokio::spawn(async move {
            if let Err(err) = process_socket(socket, None, handlers).await {
                warn!(peer_addr = %peer_addr, error = %err, "pgwire session failed");
            }
        });
    }
}

async fn run_query(
    state: &GatewayState,
    identity: &QueryIdentity,
    query: &str,
) -> PgWireResult<Vec<Response>> {
    let readonly = connect_postgres(&state.readonly_url, "readonly").await?;
    let gatekeeper = connect_postgres(&state.gatekeeper_url, "gatekeeper").await?;

    let binding_row = readonly
        .query_one(
            "SELECT backend_pid, backend_start FROM public.current_backend_binding_key()",
            &[],
        )
        .await
        .map_err(api_error)?;

    let backend_pid: i32 = binding_row.try_get("backend_pid").map_err(api_error)?;
    let backend_start: String = binding_row.try_get("backend_start").map_err(api_error)?;

    gatekeeper
        .query_one(
            "SELECT public.bind_rls_session($1, $2, $3, $4, $5)",
            &[
                &backend_pid,
                &backend_start,
                &identity.smith_user_id,
                &identity.smith_user_role,
                &state.bind_ttl_secs,
            ],
        )
        .await
        .map_err(api_error)?;

    tracing::info!(
        channel = %identity.channel,
        principal = %identity.principal,
        session = %identity.session,
        user_id = identity.smith_user_id.as_deref().unwrap_or(""),
        role = %identity.smith_user_role,
        "bound readonly postgres session"
    );

    let query_result = readonly.simple_query(query).await.map_err(api_error);

    if let Err(err) = gatekeeper
        .query_one(
            "SELECT public.unbind_rls_session($1, $2)",
            &[&backend_pid, &backend_start],
        )
        .await
    {
        error!(
            backend_pid,
            backend_start,
            error = %err,
            "failed to remove readonly postgres binding"
        );
    }

    let messages = query_result?;
    build_responses(messages)
}

async fn connect_postgres(url: &str, label: &str) -> PgWireResult<Client> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .map_err(api_error)?;

    tokio::spawn({
        let label = label.to_string();
        async move {
            if let Err(err) = connection.await {
                warn!(connection = %label, error = %err, "postgres connection task exited");
            }
        }
    });

    Ok(client)
}

fn build_responses(
    messages: Vec<tokio_postgres::SimpleQueryMessage>,
) -> PgWireResult<Vec<Response>> {
    let mut responses = Vec::new();
    let mut current_schema: Option<Arc<Vec<FieldInfo>>> = None;
    let mut current_rows = Vec::new();

    for message in messages {
        match message {
            tokio_postgres::SimpleQueryMessage::RowDescription(columns) => {
                current_schema = Some(Arc::new(
                    columns
                        .iter()
                        .map(|column| {
                            FieldInfo::new(
                                column.name().to_string(),
                                None,
                                None,
                                Type::TEXT,
                                FieldFormat::Text,
                            )
                        })
                        .collect(),
                ));
                current_rows.clear();
            }
            tokio_postgres::SimpleQueryMessage::Row(row) => {
                let schema = current_schema
                    .as_ref()
                    .cloned()
                    .ok_or_else(|| api_error(anyhow!("row returned before row description")))?;
                let mut encoder = DataRowEncoder::new(schema);
                for idx in 0..row.len() {
                    let value = row.try_get(idx).map_err(api_error)?;
                    let value = value.map(ToOwned::to_owned);
                    encoder.encode_field(&value).map_err(api_error)?;
                }
                current_rows.push(Ok(encoder.take_row()));
            }
            tokio_postgres::SimpleQueryMessage::CommandComplete(rows) => {
                if let Some(schema) = current_schema.take() {
                    let mut query_response = QueryResponse::new(schema, stream::iter(current_rows));
                    query_response.set_command_tag("SELECT");
                    responses.push(Response::Query(query_response));
                    current_rows = Vec::new();
                } else {
                    responses.push(Response::Execution(Tag::new("OK").with_rows(rows as usize)));
                }
            }
            _ => {}
        }
    }

    Ok(responses)
}

fn user_error(code: &str, message: &str) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        code.to_string(),
        message.to_string(),
    )))
}

fn api_error(err: impl Into<anyhow::Error>) -> PgWireError {
    let err = err.into();
    PgWireError::ApiError(Box::new(std::io::Error::other(err.to_string())))
}
