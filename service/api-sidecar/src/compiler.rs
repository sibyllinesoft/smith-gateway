use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};
use url::Url;

use crate::config::{AuthConfig, Config, DocumentSource, OperationOverride};

#[derive(Debug, Clone)]
pub struct SourceMetadata {
    pub openapi: String,
    pub arazzo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PublicTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct CompiledTool {
    pub public: PublicTool,
    pub execution: ExecutionPlan,
}

#[derive(Debug, Clone)]
pub struct CompiledSnapshot {
    pub service_name: String,
    pub target_base_url: String,
    pub timeout: Duration,
    pub source: SourceMetadata,
    pub diagnostics: Diagnostics,
    pub tools: Vec<PublicTool>,
    pub tool_index: HashMap<String, CompiledTool>,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub method: Method,
    pub path_template: String,
    pub response_pointer: Option<String>,
    pub bindings: Vec<Binding>,
    pub body_fields: BTreeSet<String>,
    pub has_body: bool,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub arg_name: String,
    pub upstream_name: String,
    pub location: BindingLocation,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingLocation {
    Path,
    Query,
    Header,
    Body,
}

struct LoadedDocument {
    source_label: String,
    value: Value,
    base_url_hint: Option<String>,
}

pub async fn compile_snapshot(
    config: &Config,
    client: &reqwest::Client,
) -> Result<CompiledSnapshot> {
    let mut diagnostics = Diagnostics::default();
    let openapi = load_document(&config.openapi.source, client)
        .await
        .context("failed to load OpenAPI source")?;
    let arazzo = match &config.arazzo {
        Some(arazzo) if arazzo.enabled => match load_document(&arazzo.source, client).await {
            Ok(doc) => {
                diagnostics.warnings.push(
                    "Arazzo source loaded but workflow compilation is not implemented yet"
                        .to_string(),
                );
                Some(doc.source_label)
            }
            Err(err) => {
                diagnostics
                    .warnings
                    .push(format!("failed to load Arazzo source: {err}"));
                None
            }
        },
        _ => None,
    };

    validate_openapi_root(&openapi.value)?;
    let target_base_url = resolve_target_base_url(config, &openapi)?;
    let (tools, tool_index) =
        compile_tools(config, &openapi.value, &target_base_url, &mut diagnostics)?;

    Ok(CompiledSnapshot {
        service_name: config.service.name.clone(),
        target_base_url,
        timeout: Duration::from_secs(config.target.timeout_seconds.max(1)),
        source: SourceMetadata {
            openapi: openapi.source_label,
            arazzo,
        },
        diagnostics,
        tools,
        tool_index,
        auth: config.auth.clone(),
    })
}

pub async fn execute_tool(
    snapshot: &CompiledSnapshot,
    client: &reqwest::Client,
    tool_name: &str,
    arguments: Value,
) -> Result<Value> {
    let tool = snapshot
        .tool_index
        .get(tool_name)
        .ok_or_else(|| anyhow!("tool not found: {tool_name}"))?;

    let payload = arguments
        .as_object()
        .ok_or_else(|| anyhow!("tool arguments must be a JSON object"))?;
    validate_payload(&tool.public.input_schema, payload)
        .with_context(|| format!("invalid arguments for tool '{tool_name}'"))?;

    let mut url = Url::parse(&snapshot.target_base_url)
        .with_context(|| format!("invalid target base URL {}", snapshot.target_base_url))?;
    let path = render_path(&tool.execution.path_template, payload)?;
    url.set_path(&path);

    let mut query_pairs = Vec::new();
    if let Some((name, value)) = snapshot.auth.resolved_query_secret()? {
        query_pairs.push((name, value));
    }
    for binding in &tool.execution.bindings {
        if binding.location != BindingLocation::Query {
            continue;
        }
        if let Some(value) = payload.get(&binding.arg_name) {
            collect_query_values(&mut query_pairs, &binding.upstream_name, value)?;
        }
    }
    if !query_pairs.is_empty() {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query_pairs {
            pairs.append_pair(&name, &value);
        }
    }

    let mut request = client.request(tool.execution.method.clone(), url);
    request = request.timeout(snapshot.timeout);
    if let Some((header, value)) = snapshot.auth.resolved_secret()? {
        request = request.header(header, value);
    }
    for binding in &tool.execution.bindings {
        if binding.location != BindingLocation::Header {
            continue;
        }
        if let Some(value) = payload.get(&binding.arg_name) {
            request = request.header(&binding.upstream_name, scalar_to_string(value)?);
        }
    }

    if tool.execution.has_body {
        let mut body = Map::new();
        for field in &tool.execution.body_fields {
            if let Some(value) = payload.get(field) {
                body.insert(field.clone(), value.clone());
            }
        }
        request = request.json(&Value::Object(body));
    }

    let response = request
        .send()
        .await
        .context("failed to call upstream API")?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("failed to read upstream response body")?;

    if !status.is_success() {
        let body = if bytes.is_empty() {
            json!({ "error": format!("upstream returned {}", status) })
        } else {
            parse_json_or_error_body(&bytes)
        };
        bail!("upstream returned {}: {}", status, body);
    }

    let mut value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).context("upstream response was not valid JSON")?
    };

    if let Some(pointer) = &tool.execution.response_pointer {
        value = value.pointer(pointer).cloned().unwrap_or(Value::Null);
    }

    Ok(value)
}

fn parse_json_or_error_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| {
        json!({
            "error": String::from_utf8_lossy(bytes).to_string()
        })
    })
}

async fn load_document(
    source: &DocumentSource,
    client: &reqwest::Client,
) -> Result<LoadedDocument> {
    match source {
        DocumentSource::File { path } => {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            Ok(LoadedDocument {
                source_label: path.display().to_string(),
                value: parse_document(&raw)?,
                base_url_hint: None,
            })
        }
        DocumentSource::Url { url } => {
            let response = client
                .get(url)
                .send()
                .await
                .with_context(|| format!("failed to fetch {url}"))?;
            let status = response.status();
            if !status.is_success() {
                bail!("OpenAPI URL {url} returned {status}");
            }
            let raw = response
                .text()
                .await
                .context("failed to read OpenAPI response body")?;
            Ok(LoadedDocument {
                source_label: url.clone(),
                value: parse_document(&raw)?,
                base_url_hint: Some(base_origin(url)?),
            })
        }
        DocumentSource::Probe {
            base_url,
            candidates,
        } => {
            for candidate in candidates {
                let url = join_url(base_url, candidate)?;
                let response = match client.get(url.clone()).send().await {
                    Ok(response) => response,
                    Err(_) => continue,
                };
                if !response.status().is_success() {
                    continue;
                }
                let raw = response
                    .text()
                    .await
                    .context("failed to read OpenAPI probe body")?;
                if let Ok(value) = parse_document(&raw) {
                    return Ok(LoadedDocument {
                        source_label: url.to_string(),
                        value,
                        base_url_hint: Some(base_url.clone()),
                    });
                }
            }
            bail!("failed to discover OpenAPI document from configured probe candidates")
        }
    }
}

fn parse_document(raw: &str) -> Result<Value> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        Ok(serde_json::from_str(trimmed).context("invalid JSON document")?)
    } else {
        Ok(serde_yaml::from_str(trimmed).context("invalid YAML document")?)
    }
}

fn validate_openapi_root(root: &Value) -> Result<()> {
    let version = root
        .get("openapi")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing openapi version"))?;
    if !version.starts_with("3.") {
        bail!("only OpenAPI 3.x is supported, found {version}");
    }
    if root.get("paths").and_then(Value::as_object).is_none() {
        bail!("OpenAPI document is missing a paths object");
    }
    Ok(())
}

fn resolve_target_base_url(config: &Config, openapi: &LoadedDocument) -> Result<String> {
    if let Some(base_url) = config
        .target
        .base_url
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(base_url.trim_end_matches('/').to_string());
    }

    if let Some(server_url) = openapi
        .value
        .get("servers")
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(|server| server.get("url"))
        .and_then(Value::as_str)
    {
        if let Some(base) = &openapi.base_url_hint {
            let resolved = Url::parse(base)
                .and_then(|base_url| base_url.join(server_url))
                .context("failed to resolve OpenAPI server URL")?;
            return Ok(resolved.to_string().trim_end_matches('/').to_string());
        }
        return Ok(server_url.trim_end_matches('/').to_string());
    }

    if let DocumentSource::Probe { base_url, .. } = &config.openapi.source {
        return Ok(base_url.trim_end_matches('/').to_string());
    }

    bail!("target.base_url is required when the OpenAPI document has no usable servers entry")
}

fn compile_tools(
    config: &Config,
    root: &Value,
    target_base_url: &str,
    diagnostics: &mut Diagnostics,
) -> Result<(Vec<PublicTool>, HashMap<String, CompiledTool>)> {
    let paths = root
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("missing OpenAPI paths"))?;

    let mut tools = Vec::new();
    let mut tool_index = HashMap::new();
    let include_tags: BTreeSet<_> = config.compile.include_tags.iter().cloned().collect();
    let expose_headers: BTreeSet<_> = config
        .compile
        .expose_headers
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();

    for (path, path_item) in paths {
        let Some(path_obj) = path_item.as_object() else {
            diagnostics
                .warnings
                .push(format!("skipped path {path}: path item must be an object"));
            continue;
        };

        for method_name in ["get", "post", "put", "patch", "delete"] {
            let Some(operation) = path_obj.get(method_name) else {
                continue;
            };
            let Some(operation_obj) = operation.as_object() else {
                diagnostics.warnings.push(format!(
                    "skipped operation {} {}: operation must be an object",
                    method_name.to_uppercase(),
                    path
                ));
                continue;
            };

            let operation_id = operation_obj
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| generated_operation_id(method_name, path));

            if config.compile.exclude_operations.contains(&operation_id) {
                continue;
            }
            if !include_tags.is_empty() && !operation_matches_tags(operation_obj, &include_tags) {
                continue;
            }

            let override_cfg = config.overrides.operation_ids.get(&operation_id);
            if is_hidden(operation_obj, override_cfg) {
                continue;
            }

            match compile_operation(
                root,
                path,
                method_name,
                operation_obj,
                override_cfg,
                &expose_headers,
            ) {
                Ok(Some(tool)) => {
                    if tool_index.contains_key(&tool.public.name) {
                        diagnostics.warnings.push(format!(
                            "skipped duplicate tool name '{}' for operation {} {}",
                            tool.public.name,
                            method_name.to_uppercase(),
                            path
                        ));
                        continue;
                    }
                    tools.push(tool.public.clone());
                    tool_index.insert(tool.public.name.clone(), tool);
                }
                Ok(None) => {}
                Err(err) => diagnostics.warnings.push(format!(
                    "skipped operation {} {}: {}",
                    method_name.to_uppercase(),
                    path,
                    err
                )),
            }
        }
    }

    if tool_index.is_empty() {
        diagnostics.errors.push(format!(
            "no tools were compiled from OpenAPI source for target {}",
            target_base_url
        ));
    }

    tools.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((tools, tool_index))
}

fn compile_operation(
    root: &Value,
    path: &str,
    method_name: &str,
    operation: &Map<String, Value>,
    override_cfg: Option<&OperationOverride>,
    expose_headers: &BTreeSet<String>,
) -> Result<Option<CompiledTool>> {
    let mut properties = BTreeMap::new();
    let mut required = BTreeSet::new();
    let mut bindings = Vec::new();
    let mut body_fields = BTreeSet::new();
    let mut has_body = false;

    for parameter in collect_parameters(root, path, operation)? {
        let name = parameter
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("parameter missing name"))?;
        let location = parameter
            .get("in")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("parameter missing location"))?;
        let required_flag = parameter
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if location == "header" && !expose_headers.contains(&name.to_ascii_lowercase()) {
            continue;
        }

        let binding_location = match location {
            "path" => BindingLocation::Path,
            "query" => BindingLocation::Query,
            "header" => BindingLocation::Header,
            other => bail!("unsupported parameter location '{other}'"),
        };

        let schema = parameter
            .get("schema")
            .ok_or_else(|| anyhow!("parameter '{name}' is missing a schema"))?;
        let input_schema = compile_schema(root, schema)?;
        if !schema_is_scalar(&input_schema) {
            bail!("parameter '{name}' must compile to a scalar type");
        }

        insert_property(&mut properties, name, input_schema)?;
        if required_flag {
            required.insert(name.to_string());
        }
        bindings.push(Binding {
            arg_name: name.to_string(),
            upstream_name: name.to_string(),
            location: binding_location,
            required: required_flag,
        });
    }

    if let Some(request_body) = operation.get("requestBody") {
        let request_body = resolve_component(root, request_body)?;
        let body_schema = request_body
            .get("content")
            .and_then(Value::as_object)
            .and_then(pick_json_content)
            .ok_or_else(|| anyhow!("requestBody must define application/json content"))?
            .get("schema")
            .ok_or_else(|| anyhow!("requestBody JSON content is missing schema"))?;

        let compiled_body = compile_schema(root, body_schema)?;
        let Some(body_obj) = compiled_body.as_object() else {
            bail!("requestBody schema must compile to an object");
        };
        if body_obj.get("type").and_then(Value::as_str) != Some("object") {
            bail!("requestBody schema must be an object");
        }

        let body_properties = body_obj
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (name, schema) in body_properties {
            insert_property(&mut properties, &name, schema)?;
            body_fields.insert(name.clone());
            bindings.push(Binding {
                arg_name: name,
                upstream_name: String::new(),
                location: BindingLocation::Body,
                required: false,
            });
        }

        if let Some(required_fields) = body_obj.get("required").and_then(Value::as_array) {
            for field in required_fields {
                if let Some(field_name) = field.as_str() {
                    required.insert(field_name.to_string());
                }
            }
        }

        has_body = true;
    }

    let tool_name = override_cfg
        .and_then(|inner| inner.tool_name.clone())
        .or_else(|| {
            operation
                .get("x-smith-tool-name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| generated_operation_id(method_name, path));

    let description = override_cfg
        .and_then(|inner| inner.description.clone())
        .or_else(|| {
            operation
                .get("x-smith-description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            operation
                .get("summary")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            operation
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let response_pointer = override_cfg
        .and_then(|inner| inner.response_pointer.clone())
        .or_else(|| {
            operation
                .get("x-smith-response-pointer")
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let mut input_schema = Map::new();
    input_schema.insert("type".to_string(), Value::String("object".to_string()));
    input_schema.insert(
        "properties".to_string(),
        Value::Object(properties.into_iter().collect()),
    );
    if !required.is_empty() {
        input_schema.insert(
            "required".to_string(),
            Value::Array(required.into_iter().map(Value::String).collect()),
        );
    }

    Ok(Some(CompiledTool {
        public: PublicTool {
            name: tool_name,
            description,
            input_schema: Value::Object(input_schema),
        },
        execution: ExecutionPlan {
            method: Method::from_bytes(method_name.to_uppercase().as_bytes())?,
            path_template: path.to_string(),
            response_pointer,
            bindings,
            body_fields,
            has_body,
        },
    }))
}

fn collect_parameters(
    root: &Value,
    path: &str,
    operation: &Map<String, Value>,
) -> Result<Vec<Value>> {
    let path_parameters = root
        .pointer(&format!("/paths/{}/parameters", escape_pointer(path)))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let operation_parameters = operation
        .get("parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut dedup = BTreeMap::new();
    for raw in path_parameters
        .into_iter()
        .chain(operation_parameters.into_iter())
    {
        let resolved = resolve_component(root, &raw)?;
        let key = format!(
            "{}:{}",
            resolved.get("name").and_then(Value::as_str).unwrap_or(""),
            resolved.get("in").and_then(Value::as_str).unwrap_or("")
        );
        dedup.insert(key, resolved);
    }

    Ok(dedup.into_values().collect())
}

fn resolve_component(root: &Value, value: &Value) -> Result<Value> {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        if !reference.starts_with("#/") {
            bail!("only local $ref values are supported");
        }
        let target = root
            .pointer(&reference[1..])
            .ok_or_else(|| anyhow!("unresolved reference {reference}"))?;
        let mut resolved = resolve_component(root, target)?;
        if let Some(overrides) = value.as_object() {
            if let Some(resolved_obj) = resolved.as_object_mut() {
                for (key, value) in overrides {
                    if key == "$ref" {
                        continue;
                    }
                    resolved_obj.insert(key.clone(), value.clone());
                }
            }
        }
        return Ok(resolved);
    }
    Ok(value.clone())
}

fn compile_schema(root: &Value, schema: &Value) -> Result<Value> {
    let schema = resolve_component(root, schema)?;
    let Some(obj) = schema.as_object() else {
        bail!("schema must be an object");
    };

    if obj.get("oneOf").is_some() || obj.get("anyOf").is_some() {
        bail!("oneOf and anyOf are not supported");
    }

    if let Some(all_of) = obj.get("allOf").and_then(Value::as_array) {
        let mut merged = Map::new();
        merged.insert("type".to_string(), Value::String("object".to_string()));
        merged.insert("properties".to_string(), Value::Object(Map::new()));
        merged.insert("required".to_string(), Value::Array(Vec::new()));

        for part in all_of {
            let compiled = compile_schema(root, part)?;
            merge_object_schema(&mut merged, &compiled)?;
        }
        for (key, value) in obj {
            if key == "allOf" {
                continue;
            }
            merged.insert(key.clone(), value.clone());
        }
        return Ok(Value::Object(merged));
    }

    let schema_type = obj
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| {
            if obj.get("properties").is_some() {
                Some("object")
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow!("schema type is required"))?;

    let mut compiled = Map::new();
    compiled.insert("type".to_string(), Value::String(schema_type.to_string()));
    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        compiled.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }
    if let Some(enum_values) = obj.get("enum").and_then(Value::as_array) {
        compiled.insert("enum".to_string(), Value::Array(enum_values.clone()));
    }

    match schema_type {
        "string" | "integer" | "number" | "boolean" => {}
        "array" => {
            let items = obj
                .get("items")
                .ok_or_else(|| anyhow!("array schema is missing items"))?;
            compiled.insert("items".to_string(), compile_schema(root, items)?);
        }
        "object" => {
            let properties = obj
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut compiled_props = Map::new();
            for (name, value) in properties {
                compiled_props.insert(name, compile_schema(root, &value)?);
            }
            compiled.insert("properties".to_string(), Value::Object(compiled_props));
            if let Some(required) = obj.get("required").and_then(Value::as_array) {
                compiled.insert("required".to_string(), Value::Array(required.clone()));
            }
        }
        other => bail!("unsupported schema type '{other}'"),
    }

    Ok(Value::Object(compiled))
}

fn merge_object_schema(target: &mut Map<String, Value>, schema: &Value) -> Result<()> {
    let Some(source) = schema.as_object() else {
        bail!("allOf member must compile to an object schema");
    };
    if source.get("type").and_then(Value::as_str) != Some("object") {
        bail!("allOf members must be object schemas");
    }

    let target_props = target
        .entry("properties".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("invalid target properties"))?;
    let source_props = source
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (name, value) in source_props {
        if target_props.insert(name.clone(), value).is_some() {
            bail!("allOf introduced duplicate property '{name}'");
        }
    }

    let mut required: BTreeSet<String> = target
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if let Some(source_required) = source.get("required").and_then(Value::as_array) {
        for field in source_required {
            if let Some(field_name) = field.as_str() {
                required.insert(field_name.to_string());
            }
        }
    }
    target.insert(
        "required".to_string(),
        Value::Array(required.into_iter().map(Value::String).collect()),
    );
    Ok(())
}

fn insert_property(
    properties: &mut BTreeMap<String, Value>,
    name: &str,
    schema: Value,
) -> Result<()> {
    if properties.insert(name.to_string(), schema).is_some() {
        bail!("duplicate input field '{name}'");
    }
    Ok(())
}

fn schema_is_scalar(schema: &Value) -> bool {
    matches!(
        schema.get("type").and_then(Value::as_str),
        Some("string" | "integer" | "number" | "boolean")
    )
}

fn operation_matches_tags(operation: &Map<String, Value>, include_tags: &BTreeSet<String>) -> bool {
    operation
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .any(|tag| include_tags.contains(tag))
        })
        .unwrap_or(false)
}

fn is_hidden(operation: &Map<String, Value>, override_cfg: Option<&OperationOverride>) -> bool {
    override_cfg.and_then(|cfg| cfg.hidden).unwrap_or(false)
        || operation
            .get("x-smith-hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn generated_operation_id(method: &str, path: &str) -> String {
    let normalized = path
        .trim_matches('/')
        .replace('/', "__")
        .replace('{', "")
        .replace('}', "")
        .replace('-', "_");
    format!("{}__{}", method.to_lowercase(), normalized)
}

fn pick_json_content(content: &Map<String, Value>) -> Option<&Value> {
    content.get("application/json").or_else(|| {
        content
            .iter()
            .find(|(key, _)| key.ends_with("+json"))
            .map(|(_, value)| value)
    })
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn validate_payload(schema: &Value, payload: &Map<String, Value>) -> Result<()> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required {
            if let Some(name) = field.as_str() {
                if !payload.contains_key(name) {
                    bail!("missing required argument '{name}'");
                }
            }
        }
    }

    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    for (name, value) in payload {
        let Some(field_schema) = properties.get(name) else {
            bail!("unknown argument '{name}'");
        };
        validate_value(name, field_schema, value)?;
    }
    Ok(())
}

fn validate_value(name: &str, schema: &Value, value: &Value) -> Result<()> {
    let expected = schema
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("object");
    match expected {
        "string" if value.is_string() => Ok(()),
        "integer" if value.as_i64().is_some() || value.as_u64().is_some() => Ok(()),
        "number"
            if value.as_f64().is_some() || value.as_i64().is_some() || value.as_u64().is_some() =>
        {
            Ok(())
        }
        "boolean" if value.is_boolean() => Ok(()),
        "array" if value.is_array() => {
            if let Some(items) = schema.get("items") {
                if let Some(values) = value.as_array() {
                    for item in values {
                        validate_value(name, items, item)?;
                    }
                }
            }
            Ok(())
        }
        "object" if value.is_object() => {
            let Some(obj) = value.as_object() else {
                bail!("argument '{name}' must be of type object");
            };
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (field, nested) in obj {
                    let Some(nested_schema) = properties.get(field) else {
                        bail!("argument '{name}' contains unknown field '{field}'");
                    };
                    validate_value(field, nested_schema, nested)?;
                }
            }
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for field in required {
                    if let Some(field_name) = field.as_str() {
                        if !obj.contains_key(field_name) {
                            bail!("argument '{name}' is missing required field '{field_name}'");
                        }
                    }
                }
            }
            Ok(())
        }
        _ => bail!("argument '{name}' must be of type {expected}"),
    }
}

fn render_path(template: &str, payload: &Map<String, Value>) -> Result<String> {
    let mut rendered = template.to_string();
    for segment in extract_placeholders(template) {
        let value = payload
            .get(&segment)
            .ok_or_else(|| anyhow!("missing required path argument '{segment}'"))?;
        let encoded = urlencoding::encode(&scalar_to_string(value)?).to_string();
        rendered = rendered.replace(&format!("{{{segment}}}"), &encoded);
    }
    Ok(rendered)
}

fn extract_placeholders(template: &str) -> Vec<String> {
    let mut placeholders = Vec::new();
    let mut current = String::new();
    let mut in_placeholder = false;
    for ch in template.chars() {
        match ch {
            '{' => {
                in_placeholder = true;
                current.clear();
            }
            '}' if in_placeholder => {
                in_placeholder = false;
                placeholders.push(current.clone());
            }
            _ if in_placeholder => current.push(ch),
            _ => {}
        }
    }
    placeholders
}

fn scalar_to_string(value: &Value) -> Result<String> {
    match value {
        Value::String(inner) => Ok(inner.clone()),
        Value::Bool(inner) => Ok(inner.to_string()),
        Value::Number(inner) => Ok(inner.to_string()),
        _ => bail!("value must be a scalar"),
    }
}

fn collect_query_values(
    query_pairs: &mut Vec<(String, String)>,
    name: &str,
    value: &Value,
) -> Result<()> {
    match value {
        Value::Array(values) => {
            for value in values {
                query_pairs.push((name.to_string(), scalar_to_string(value)?));
            }
            Ok(())
        }
        _ => {
            query_pairs.push((name.to_string(), scalar_to_string(value)?));
            Ok(())
        }
    }
}

fn join_url(base: &str, path: &str) -> Result<Url> {
    Url::parse(base)
        .with_context(|| format!("invalid probe base URL {base}"))?
        .join(path)
        .with_context(|| format!("failed to join probe candidate {path}"))
}

fn base_origin(url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("invalid URL {url}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL {url} does not have a host"))?;
    let mut origin = format!("{}://{}", parsed.scheme(), host);
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    Ok(origin)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::extract::{Path, Query};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde::Deserialize;
    use serde_json::{json, Value};

    use super::{compile_snapshot, execute_tool, BindingLocation};
    use crate::config::Config;

    const TEST_OPENAPI_YAML: &str = r#"
openapi: 3.0.3
info:
  title: Local Test API
  version: 1.0.0
servers:
  - url: /
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

    #[derive(Debug, Deserialize)]
    struct WidgetQuery {
        verbose: Option<bool>,
    }

    #[derive(Debug, Deserialize)]
    struct EchoRequest {
        message: String,
        count: Option<i64>,
    }

    async fn spawn_test_server() -> SocketAddr {
        let app = Router::new()
            .route("/openapi.yaml", get(|| async { TEST_OPENAPI_YAML }))
            .route(
                "/api/widgets/:id",
                get(
                    |Path(id): Path<String>, Query(query): Query<WidgetQuery>| async move {
                        Json(json!({
                            "id": id,
                            "verbose": query.verbose.unwrap_or(false),
                        }))
                    },
                ),
            )
            .route(
                "/api/echo",
                post(|Json(body): Json<EchoRequest>| async move {
                    Json(json!({
                        "message": body.message,
                        "count": body.count.unwrap_or(1),
                    }))
                }),
            );

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve test api");
        });
        addr
    }

    #[tokio::test]
    async fn compile_snapshot_generates_tools_from_openapi() {
        let config: Config = serde_yaml::from_str(
            r#"
service:
  name: billing
target:
  base_url: https://billing.example.com
openapi:
  source:
    mode: file
    path: /tmp/spec.yaml
"#,
        )
        .expect("config parses");

        let spec = serde_json::json!({
            "openapi": "3.0.3",
            "paths": {
                "/invoices/{id}": {
                    "get": {
                        "operationId": "getInvoice",
                        "parameters": [
                            { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                            { "name": "expand", "in": "query", "schema": { "type": "string" } }
                        ]
                    }
                }
            }
        });

        let temp = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(
            temp.path(),
            serde_yaml::to_string(&spec).expect("serialize"),
        )
        .expect("write");

        let mut config = config;
        if let crate::config::DocumentSource::File { path } = &mut config.openapi.source {
            *path = temp.path().to_path_buf();
        }

        let client = reqwest::Client::new();
        let snapshot = compile_snapshot(&config, &client).await.expect("compile");

        let tool = snapshot.tool_index.get("getInvoice").expect("tool exists");
        assert_eq!(tool.execution.path_template, "/invoices/{id}");
        assert!(tool
            .execution
            .bindings
            .iter()
            .any(|binding| binding.location == BindingLocation::Path && binding.arg_name == "id"));
    }

    #[tokio::test]
    async fn probe_source_and_execute_tool_against_local_api() {
        let addr = spawn_test_server().await;
        let config: Config = serde_yaml::from_str(&format!(
            r#"
service:
  name: local-test
target:
  base_url: http://{addr}
openapi:
  source:
    mode: probe
    base_url: http://{addr}
    candidates:
      - /openapi.yaml
compile:
  include_tags:
    - widgets
    - echo
"#
        ))
        .expect("config parses");

        let client = reqwest::Client::new();
        let snapshot = compile_snapshot(&config, &client).await.expect("compile");

        let widget = execute_tool(
            &snapshot,
            &client,
            "getWidget",
            json!({
                "id": "abc",
                "verbose": true
            }),
        )
        .await
        .expect("execute widget");
        assert_eq!(widget["id"], Value::String("abc".to_string()));
        assert_eq!(widget["verbose"], Value::Bool(true));

        let echo = execute_tool(
            &snapshot,
            &client,
            "createEcho",
            json!({
                "message": "hello",
                "count": 3
            }),
        )
        .await
        .expect("execute echo");
        assert_eq!(echo["message"], Value::String("hello".to_string()));
        assert_eq!(echo["count"], Value::Number(3.into()));
    }
}
