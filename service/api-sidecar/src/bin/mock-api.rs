use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};

const OPENAPI_YAML: &str = r#"
openapi: 3.0.3
info:
  title: Local Mock API
  version: 1.0.0
servers:
  - url: http://127.0.0.1:9160
paths:
  /openapi.yaml:
    get:
      operationId: getOpenApiDocument
      tags: [meta]
      responses:
        '200':
          description: OpenAPI document
  /api/widgets/{id}:
    get:
      operationId: getWidget
      tags: [widgets]
      parameters:
        - name: id
          in: path
          required: true
          schema:
            type: string
        - name: verbose
          in: query
          required: false
          schema:
            type: boolean
      responses:
        '200':
          description: Widget response
          content:
            application/json:
              schema:
                type: object
  /api/echo:
    post:
      operationId: createEcho
      tags: [echo]
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [message]
              properties:
                message:
                  type: string
                count:
                  type: integer
      responses:
        '200':
          description: Echo response
          content:
            application/json:
              schema:
                type: object
"#;

#[derive(Parser)]
#[command(
    name = "mock-api",
    about = "Local mock API for api-sidecar development"
)]
struct Cli {
    #[arg(long, env = "MOCK_API_PORT", default_value_t = 9160)]
    port: u16,
}

#[derive(Debug, Deserialize)]
struct WidgetQuery {
    verbose: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct EchoRequest {
    message: String,
    count: Option<i64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let app = Router::new()
        .route("/openapi.yaml", get(openapi))
        .route("/api/widgets/:id", get(get_widget))
        .route("/api/echo", post(post_echo));

    let addr = SocketAddr::from(([127, 0, 0, 1], cli.port));
    println!("mock-api listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn openapi() -> &'static str {
    OPENAPI_YAML
}

async fn get_widget(Path(id): Path<String>, Query(query): Query<WidgetQuery>) -> Json<Value> {
    Json(json!({
        "id": id,
        "name": format!("Widget {id}"),
        "verbose": query.verbose.unwrap_or(false),
    }))
}

async fn post_echo(Json(body): Json<EchoRequest>) -> Json<Value> {
    Json(json!({
        "message": body.message,
        "count": body.count.unwrap_or(1),
    }))
}
