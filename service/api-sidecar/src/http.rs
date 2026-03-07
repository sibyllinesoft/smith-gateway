use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::compiler::{compile_snapshot, execute_tool};
use crate::config::Config;
use crate::AppState;

type SharedState = Arc<AppState>;

fn extract_token(headers: &HeaderMap) -> Option<String> {
    if let Some(authz) = headers.get(header::AUTHORIZATION) {
        if let Ok(authz) = authz.to_str() {
            if let Some(rest) = authz.strip_prefix("Bearer ") {
                return Some(rest.trim().to_string());
            }
            if let Some(rest) = authz.strip_prefix("bearer ") {
                return Some(rest.trim().to_string());
            }
        }
    }

    headers
        .get("x-smith-token")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
}

fn require_api_token(state: &SharedState, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(expected) = state.api_token.as_ref() else {
        return Ok(());
    };

    let provided = extract_token(headers).unwrap_or_default();
    if provided == *expected {
        Ok(())
    } else {
        Err(AppError::Unauthorized(
            "missing or invalid API token".to_string(),
        ))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/tools", get(list_tools))
        .route("/tools/:name", post(call_tool))
        .route("/reload", post(reload))
        .with_state(state)
}

async fn health(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_api_token(&state, &headers)?;

    let compiled = state.compiled.read().await.clone();
    let warnings = compiled.diagnostics.warnings.clone();
    let errors = compiled.diagnostics.errors.clone();

    Ok(Json(json!({
        "status": if errors.is_empty() { "ok" } else { "degraded" },
        "server_info": {
            "name": compiled.service_name,
            "version": env!("CARGO_PKG_VERSION"),
            "target_base_url": compiled.target_base_url,
        },
        "tools_count": compiled.tools.len(),
        "source": {
            "openapi": compiled.source.openapi,
            "arazzo": compiled.source.arazzo,
        },
        "diagnostics": {
            "warnings": warnings,
            "errors": errors,
        }
    })))
}

async fn list_tools(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_api_token(&state, &headers)?;

    let compiled = state.compiled.read().await.clone();
    let tools: Vec<Value> = compiled
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })
        })
        .collect();

    Ok(Json(json!(tools)))
}

async fn call_tool(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    require_api_token(&state, &headers)?;

    let compiled = state.compiled.read().await.clone();
    if !compiled.tool_index.contains_key(&name) {
        return Err(AppError::NotFound(format!("tool not found: {name}")));
    }

    let result = execute_tool(&compiled, &state.client, &name, body)
        .await
        .map_err(map_execute_error)?;
    Ok(Json(result))
}

async fn reload(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_api_token(&state, &headers)?;

    let config =
        Config::load(&state.config_path).map_err(|err| AppError::Internal(err.to_string()))?;
    let compiled = compile_snapshot(&config, &state.client)
        .await
        .map_err(|err| AppError::Internal(err.to_string()))?;
    let tools_count = compiled.tools.len();

    *state.compiled.write().await = Arc::new(compiled);

    Ok(Json(json!({
        "status": "reloaded",
        "tools_count": tools_count,
    })))
}

fn map_execute_error(error: anyhow::Error) -> AppError {
    let message = error.to_string();
    if message.contains("tool not found") {
        AppError::NotFound(message)
    } else if message.contains("invalid arguments") || message.contains("missing required") {
        AppError::ToolError(message)
    } else if message.contains("upstream returned") {
        AppError::Upstream(message)
    } else {
        AppError::BadGateway(message)
    }
}

enum AppError {
    Unauthorized(String),
    NotFound(String),
    ToolError(String),
    Upstream(String),
    BadGateway(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = match self {
            Self::Unauthorized(message) => (StatusCode::UNAUTHORIZED, json!({ "error": message })),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, json!({ "error": message })),
            Self::ToolError(message) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": message }),
            ),
            Self::Upstream(message) => (StatusCode::BAD_GATEWAY, json!({ "error": message })),
            Self::BadGateway(message) => (StatusCode::BAD_GATEWAY, json!({ "error": message })),
            Self::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": message }),
            ),
        };
        (status, Json(body)).into_response()
    }
}
