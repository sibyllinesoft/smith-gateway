use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::task::JoinSet;

use crate::oauth;
use crate::poller::{DiscoveryAuthzCacheEntry, DiscoveryAuthzCacheKey, IndexState, ToolEntry};

type AppState = Arc<IndexState>;

#[derive(Debug, Clone, Deserialize)]
struct IdentityTokenClaims {
    channel: String,
    principal: String,
    #[serde(default)]
    smith_user_id: Option<String>,
    smith_user_role: String,
}

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
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().to_string())
}

fn require_api_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    let Some(expected) = state.api_token.as_ref() else {
        return Ok(());
    };

    let provided = extract_token(headers).unwrap_or_default();
    if provided == *expected {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing or invalid API token" })),
        ))
    }
}

fn extract_identity_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-oc-identity-token")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn verify_identity_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<(String, IdentityTokenClaims)>, (StatusCode, Json<Value>)> {
    let Some(secret) = state.identity_secret.as_ref() else {
        return Ok(None);
    };

    let Some(token) = extract_identity_token(headers) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "missing x-oc-identity-token" })),
        ));
    };

    let decoded = decode::<IdentityTokenClaims>(
        &token,
        &DecodingKey::from_secret(secret),
        &Validation::default(),
    )
    .map_err(|err| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": format!("invalid identity token: {err}") })),
        )
    })?;

    Ok(Some((token, decoded.claims)))
}

async fn authorize_tool_call(
    state: &AppState,
    claims: &IdentityTokenClaims,
    qualified_tool_name: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    let input = json!({
        "user_id": claims.smith_user_id.clone().unwrap_or_default(),
        "role": claims.smith_user_role,
        "agent_id": "",
        "source": claims.channel,
        "channel_id": "",
        "thread_id": "",
        "trigger": "chat",
        "metadata": {
            "trusted": true,
            "principal": claims.principal,
        },
        "tool": qualified_tool_name,
    });

    let resp = state
        .client
        .post(format!("{}/v1/data/smith/tool_access/allow", state.opa_url))
        .json(&json!({ "input": input }))
        .send()
        .await
        .map_err(|err| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": format!("OPA authorization failed: {err}") })),
            )
        })?;

    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|err| json!({ "error": format!("failed to parse OPA response: {err}") }));
    if body.get("result").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": format!(
                    "tool '{}' is not allowed for signed role '{}'",
                    qualified_tool_name,
                    claims.smith_user_role
                )
            })),
        ))
    }
}

fn discovery_cache_key(
    claims: &IdentityTokenClaims,
    qualified_tool_name: &str,
) -> DiscoveryAuthzCacheKey {
    DiscoveryAuthzCacheKey {
        channel: claims.channel.clone(),
        principal: claims.principal.clone(),
        smith_user_id: claims.smith_user_id.clone(),
        smith_user_role: claims.smith_user_role.clone(),
        tool: qualified_tool_name.to_string(),
    }
}

async fn lookup_discovery_authz_cache(
    state: &AppState,
    key: &DiscoveryAuthzCacheKey,
) -> Option<bool> {
    let now = Instant::now();

    {
        let cache = state.discovery_authz_cache.read().await;
        if let Some(entry) = cache.get(key) {
            if entry.expires_at > now {
                return Some(entry.allowed);
            }
        } else {
            return None;
        }
    }

    let mut cache = state.discovery_authz_cache.write().await;
    if cache.get(key).is_some_and(|entry| entry.expires_at <= now) {
        cache.remove(key);
    }
    None
}

fn prune_discovery_authz_cache(
    cache: &mut std::collections::HashMap<DiscoveryAuthzCacheKey, DiscoveryAuthzCacheEntry>,
    now: Instant,
    max_entries: usize,
) {
    cache.retain(|_, entry| entry.expires_at > now);

    while cache.len() > max_entries {
        let Some(evict_key) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.expires_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        cache.remove(&evict_key);
    }
}

async fn store_discovery_authz_cache(state: &AppState, key: DiscoveryAuthzCacheKey, allowed: bool) {
    let now = Instant::now();
    let expires_at = now + state.authz_cache_ttl;
    let mut cache = state.discovery_authz_cache.write().await;
    cache.insert(
        key,
        DiscoveryAuthzCacheEntry {
            allowed,
            expires_at,
        },
    );
    prune_discovery_authz_cache(&mut cache, now, state.authz_cache_max_entries);
}

async fn authorize_tool_discovery(
    state: &AppState,
    claims: &IdentityTokenClaims,
    qualified_tool_name: &str,
) -> Result<bool, (StatusCode, Json<Value>)> {
    let key = discovery_cache_key(claims, qualified_tool_name);
    if let Some(allowed) = lookup_discovery_authz_cache(state, &key).await {
        return Ok(allowed);
    }

    let allowed = match authorize_tool_call(state, claims, qualified_tool_name).await {
        Ok(()) => true,
        Err((StatusCode::FORBIDDEN, _)) => false,
        Err(err) => return Err(err),
    };
    store_discovery_authz_cache(state, key, allowed).await;
    Ok(allowed)
}

fn tool_discovery_claims(
    state: &AppState,
    headers: &HeaderMap,
    authorized: bool,
) -> Result<Option<IdentityTokenClaims>, (StatusCode, Json<Value>)> {
    if !authorized {
        return Ok(None);
    }

    Ok(verify_identity_token(state, headers)?.map(|(_, claims)| claims))
}

async fn filter_tools_for_discovery(
    state: &AppState,
    claims: Option<&IdentityTokenClaims>,
    tools: Vec<ToolEntry>,
) -> Result<Vec<ToolEntry>, (StatusCode, Json<Value>)> {
    let Some(claims) = claims else {
        return Ok(tools);
    };

    enum DiscoveryAuthOutcome {
        Allowed(ToolEntry),
        Denied,
        Error((StatusCode, Json<Value>)),
    }

    let total = tools.len();
    if total == 0 {
        return Ok(tools);
    }

    let concurrency = state.authz_concurrency.max(1).min(total);
    let mut remaining = tools.into_iter().enumerate();
    let mut in_flight = JoinSet::new();
    let mut outcomes: Vec<Option<DiscoveryAuthOutcome>> = (0..total).map(|_| None).collect();

    let spawn_check = |set: &mut JoinSet<(usize, DiscoveryAuthOutcome)>,
                       idx: usize,
                       tool: ToolEntry| {
        let state = Arc::clone(state);
        let claims = claims.to_owned();

        set.spawn(async move {
            let qualified_name = format!("{}__{}", tool.server, tool.name);
            let outcome = match authorize_tool_discovery(&state, &claims, &qualified_name).await {
                Ok(true) => DiscoveryAuthOutcome::Allowed(tool),
                Ok(false) => DiscoveryAuthOutcome::Denied,
                Err(err) => DiscoveryAuthOutcome::Error(err),
            };
            (idx, outcome)
        });
    };

    for _ in 0..concurrency {
        if let Some((idx, tool)) = remaining.next() {
            spawn_check(&mut in_flight, idx, tool);
        }
    }

    while let Some(result) = in_flight.join_next().await {
        let (idx, outcome) = result.map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("authorization task failed: {err}") })),
            )
        })?;
        outcomes[idx] = Some(outcome);

        if let Some((next_idx, next_tool)) = remaining.next() {
            spawn_check(&mut in_flight, next_idx, next_tool);
        }
    }

    let mut allowed = Vec::with_capacity(total);
    for outcome in outcomes.into_iter().flatten() {
        match outcome {
            DiscoveryAuthOutcome::Allowed(tool) => allowed.push(tool),
            DiscoveryAuthOutcome::Denied => {}
            DiscoveryAuthOutcome::Error(err) => return Err(err),
        }
    }

    Ok(allowed)
}

pub fn router(state: Arc<IndexState>) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/health", get(health))
        .route("/api/servers", get(servers))
        .route("/api/tools", get(tools))
        .route("/api/tools/search", get(tools_search))
        .route("/api/tools/call", post(tools_call))
        .route("/api/auth/start", get(auth_start))
        .route("/api/auth/callback", get(auth_callback))
        .with_state(state)
}

// ── GET / ─────────────────────────────────────────────────────────────

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

// ── GET /health ─────────────────────────────────────────────────────────

async fn health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;

    let servers = state.servers.read().await;
    let total = servers.len();
    let healthy = servers.iter().filter(|s| s.healthy).count();
    let tools_total: usize = servers
        .iter()
        .filter(|s| s.healthy)
        .map(|s| s.tools_count)
        .sum();

    Ok(Json(json!({
        "status": "ok",
        "servers_total": total,
        "servers_healthy": healthy,
        "servers_unhealthy": total - healthy,
        "tools_total": tools_total,
    })))
}

// ── GET /api/servers ────────────────────────────────────────────────────

async fn servers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;
    let servers = state.servers.read().await;
    Ok(Json(json!(*servers)))
}

// ── GET /api/tools ──────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct ToolsQuery {
    #[serde(default)]
    authorized: bool,
}

async fn tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ToolsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;
    let claims = tool_discovery_claims(&state, &headers, params.authorized)?;

    let servers = state.servers.read().await;
    let all_tools: Vec<_> = servers
        .iter()
        .filter(|s| s.healthy)
        .flat_map(|s| s.tools.iter().cloned())
        .collect();
    drop(servers);

    let all_tools = filter_tools_for_discovery(&state, claims.as_ref(), all_tools).await?;
    Ok(Json(json!(all_tools)))
}

// ── GET /api/tools/search?q=&server= ────────────────────────────────────

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    server: Option<String>,
    #[serde(default)]
    authorized: bool,
}

async fn tools_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;
    let claims = tool_discovery_claims(&state, &headers, params.authorized)?;

    let has_query = params.q.as_ref().is_some_and(|q| !q.trim().is_empty());
    let has_server = params.server.as_ref().is_some_and(|s| !s.trim().is_empty());

    if !has_query && !has_server {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "provide at least one of: q, server" })),
        ));
    }

    let search_index = state.search_index.read().await;

    let results = if has_query {
        let q = params.q.as_ref().unwrap();
        search_index.search(q, params.server.as_deref(), 50)
    } else {
        search_index.list_server(params.server.as_ref().unwrap())
    };

    let results = filter_tools_for_discovery(&state, claims.as_ref(), results).await?;
    Ok(Json(json!(results)))
}

// ── POST /api/tools/call ──────────────────────────────────────────────

#[derive(Deserialize)]
struct ToolCallRequest {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

async fn tools_call(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ToolCallRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;
    let verified_identity = verify_identity_token(&state, &headers)?;

    let servers = state.servers.read().await;

    // Find the server
    let server = servers
        .iter()
        .find(|s| s.name == req.server)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("server '{}' not found", req.server) })),
            )
        })?;

    if !server.healthy {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": format!("server '{}' is not healthy", req.server) })),
        ));
    }

    // Validate tool exists on this server
    if !server.tools.iter().any(|t| t.name == req.tool) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                json!({ "error": format!("tool '{}' not found on server '{}'", req.tool, req.server) }),
            ),
        ));
    }

    let upstream_url = format!("{}/tools/{}", server.url, req.tool);
    // Drop the read lock before making the HTTP call
    drop(servers);

    if let Some((_, claims)) = verified_identity.as_ref() {
        authorize_tool_call(&state, claims, &format!("{}__{}", req.server, req.tool)).await?;
    }

    let resp = state
        .call_client
        .post(&upstream_url)
        .headers(upstream_auth_headers(
            state.upstream_api_token.as_deref(),
            verified_identity.as_ref().map(|(token, _)| token.as_str()),
        ))
        .json(&req.arguments)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("upstream request failed: {e}") })),
            )
        })?;

    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .unwrap_or_else(|e| json!({ "error": format!("failed to parse upstream response: {e}") }));

    if status.is_success() {
        Ok(Json(body))
    } else {
        Err((
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(body),
        ))
    }
}

// ── GET /api/auth/start?server= ─────────────────────────────────────────

#[derive(Deserialize)]
struct AuthStartQuery {
    server: String,
}

async fn auth_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<AuthStartQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_api_token(&state, &headers)?;

    let provider = state.oauth.providers.get(&params.server).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("no OAuth provider for server '{}'", params.server) })),
        )
    })?;

    let state_token = uuid::Uuid::new_v4().to_string();
    state
        .oauth
        .insert_pending(state_token.clone(), params.server.clone())
        .await;

    let redirect_uri = format!("{}/api/auth/callback", state.base_url);
    let url = oauth::build_auth_url(provider, &redirect_uri, &state_token);

    Ok(Json(json!({ "url": url })))
}

// ── GET /api/auth/callback?code=&state= ─────────────────────────────────

#[derive(Deserialize)]
struct AuthCallbackQuery {
    code: String,
    state: String,
}

async fn auth_callback(
    State(state): State<AppState>,
    Query(params): Query<AuthCallbackQuery>,
) -> Html<String> {
    // Validate CSRF state token
    let pending = match state.oauth.take_pending(&params.state).await {
        Some(p) => p,
        None => return auth_error_page("Invalid or expired state token. Please try again."),
    };

    let provider = match state.oauth.providers.get(&pending.server) {
        Some(p) => p.clone(),
        None => return auth_error_page("OAuth provider configuration not found."),
    };

    let redirect_uri = format!("{}/api/auth/callback", state.base_url);

    // Exchange code for tokens
    let refresh_token =
        match oauth::exchange_code(&state.client, &provider, &params.code, &redirect_uri).await {
            Ok(t) => t,
            Err(e) => return auth_error_page(&format!("Token exchange failed: {e}")),
        };

    // Write credentials to shared volume
    if let Err(e) = oauth::write_credentials(&provider, &refresh_token).await {
        return auth_error_page(&format!("Failed to write credentials: {e}"));
    }

    // Trigger reload on the upstream shim
    let upstream = state.upstreams.iter().find(|u| u.name == pending.server);
    if let Some(upstream) = upstream {
        let reload_url = format!("{}/reload", upstream.url);
        match state
            .client
            .post(&reload_url)
            .headers(upstream_auth_headers(
                state.upstream_api_token.as_deref(),
                None,
            ))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(server = %pending.server, "triggered shim reload");
            }
            Ok(resp) => {
                tracing::warn!(server = %pending.server, status = %resp.status(), "shim reload returned non-200");
            }
            Err(e) => {
                tracing::warn!(server = %pending.server, error = %e, "failed to trigger shim reload");
            }
        }
    }

    auth_success_page(&pending.server)
}

fn auth_success_page(server: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="UTF-8"><title>OAuth Complete</title>
<style>body{{font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#0a0a0a;color:#e5e5e5}}
.card{{text-align:center;padding:40px;border-radius:12px;background:#1a1a1a;border:1px solid #333}}
.check{{color:#22c55e;font-size:48px;margin-bottom:16px}}
p{{margin:8px 0;color:#a3a3a3}}</style></head>
<body><div class="card">
<div class="check">&#10003;</div>
<h2>Connected to {server}</h2>
<p>This window will close automatically.</p>
</div>
<script>
window.opener?.postMessage({{ type: 'oauth-complete', server: '{server}' }}, '*');
setTimeout(() => window.close(), 1500);
</script></body></html>"#,
        server = server,
    ))
}

fn auth_error_page(error: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html><head><meta charset="UTF-8"><title>OAuth Error</title>
<style>body{{font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0;background:#0a0a0a;color:#e5e5e5}}
.card{{text-align:center;padding:40px;border-radius:12px;background:#1a1a1a;border:1px solid #333;max-width:400px}}
.x{{color:#ef4444;font-size:48px;margin-bottom:16px}}
p{{margin:8px 0;color:#a3a3a3;word-break:break-word}}</style></head>
<body><div class="card">
<div class="x">&#10007;</div>
<h2>Authentication Failed</h2>
<p>{error}</p>
<p style="margin-top:20px"><a href="javascript:window.close()" style="color:#60a5fa">Close window</a></p>
</div></body></html>"#,
        error = error,
    ))
}

fn upstream_auth_headers(token: Option<&str>, identity_token: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        if let Ok(value) = format!("Bearer {token}").parse() {
            headers.insert(header::AUTHORIZATION, value);
        }
        if let Ok(value) = token.parse() {
            headers.insert("x-smith-token", value);
        }
    }
    if let Some(identity_token) = identity_token {
        if let Ok(value) = identity_token.parse() {
            headers.insert("x-oc-identity-token", value);
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use super::prune_discovery_authz_cache;
    use crate::poller::{DiscoveryAuthzCacheEntry, DiscoveryAuthzCacheKey};

    fn key(tool: &str) -> DiscoveryAuthzCacheKey {
        DiscoveryAuthzCacheKey {
            channel: "chat".to_string(),
            principal: "user@example.com".to_string(),
            smith_user_id: Some("u-1".to_string()),
            smith_user_role: "member".to_string(),
            tool: tool.to_string(),
        }
    }

    #[test]
    fn prune_discovery_authz_cache_drops_expired_entries() {
        let now = Instant::now();
        let mut cache = HashMap::from([
            (
                key("server__expired"),
                DiscoveryAuthzCacheEntry {
                    allowed: true,
                    expires_at: now - Duration::from_secs(1),
                },
            ),
            (
                key("server__fresh"),
                DiscoveryAuthzCacheEntry {
                    allowed: false,
                    expires_at: now + Duration::from_secs(30),
                },
            ),
        ]);

        prune_discovery_authz_cache(&mut cache, now, 10);

        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&key("server__fresh")));
    }

    #[test]
    fn prune_discovery_authz_cache_enforces_max_entries() {
        let now = Instant::now();
        let mut cache = HashMap::from([
            (
                key("server__oldest"),
                DiscoveryAuthzCacheEntry {
                    allowed: true,
                    expires_at: now + Duration::from_secs(5),
                },
            ),
            (
                key("server__middle"),
                DiscoveryAuthzCacheEntry {
                    allowed: true,
                    expires_at: now + Duration::from_secs(15),
                },
            ),
            (
                key("server__newest"),
                DiscoveryAuthzCacheEntry {
                    allowed: true,
                    expires_at: now + Duration::from_secs(30),
                },
            ),
        ]);

        prune_discovery_authz_cache(&mut cache, now, 2);

        assert_eq!(cache.len(), 2);
        assert!(!cache.contains_key(&key("server__oldest")));
        assert!(cache.contains_key(&key("server__middle")));
        assert!(cache.contains_key(&key("server__newest")));
    }
}
