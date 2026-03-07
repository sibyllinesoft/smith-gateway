# RFC 0001: API Sidecar

Status: Proposed

## Summary

Add a new `api-sidecar` service that exposes non-MCP HTTP APIs through the same shim contract used by `mcp-sidecar`.

The service compiles OpenAPI 3.x operations into tools and optionally compiles Arazzo workflows into higher-level tools. From the perspective of `mcp-index`, an API sidecar is just another upstream exposing:

- `GET /health`
- `GET /tools`
- `POST /tools/:name`
- `POST /reload`

This RFC intentionally defines a narrow golden path:

- OpenAPI 3.x is mandatory
- Arazzo is optional
- HTTP+JSON APIs only
- sidecar-managed auth only
- one OpenAPI operation maps to one tool

## Motivation

Many internal and third-party APIs are not MCP servers, but they already publish usable OpenAPI descriptions or can be made to do so with low friction. We want those APIs to appear in the Smith catalog and CLI as first-class tools without requiring each team to implement MCP directly.

The goal is not to build a universal API gateway. The goal is to provide a robust adapter for well-described HTTP APIs.

## Goals

- Let teams sidecar existing HTTP APIs into the Smith catalog without implementing MCP.
- Keep the `mcp-index` integration unchanged by matching the existing shim HTTP contract.
- Produce stable, predictable tool definitions from OpenAPI.
- Keep business arguments visible as tool inputs while hiding transport and auth concerns in sidecar config.
- Support optional Arazzo workflows for curated multi-step operations.
- Fail closed when specs are invalid or unsupported.

## Non-Goals

- Support every OpenAPI feature or edge case.
- Infer APIs from traffic or unstructured docs.
- Act as a generic reverse proxy.
- Support arbitrary content types in v1.
- Support multipart uploads, streaming, SSE, websockets, SOAP, or gRPC in v1.
- Implement user-delegated OAuth login flows in v1.
- Replace Envoy or a policy gateway.

## Service Responsibilities

`api-sidecar` is a compiler plus executor.

It has four responsibilities:

1. Load OpenAPI from a configured source.
2. Optionally load Arazzo from a configured source.
3. Compile operations and workflows into tool definitions.
4. Execute tool calls against the target HTTP API.

It does not own catalog aggregation, search, or cross-service authorization policy. Those remain in `mcp-index`.

## External Contract

The sidecar must match the existing shim shape already consumed by `mcp-index`.

### `GET /health`

Returns:

- sidecar status
- source metadata
- compile diagnostics
- tool count

Example:

```json
{
  "status": "ok",
  "server_info": {
    "name": "billing-api-sidecar",
    "version": "0.1.0",
    "target_base_url": "https://billing.internal"
  },
  "tools_count": 18,
  "source": {
    "openapi": "https://billing.internal/openapi.json",
    "arazzo": "/config/billing-workflows.yaml"
  },
  "diagnostics": {
    "warnings": [
      "skipped operation PATCH /invoices/{id}: requestBody content type application/merge-patch+json is not supported"
    ],
    "errors": []
  }
}
```

### `GET /tools`

Returns compiled tool definitions with the same shape emitted by `mcp-sidecar`.

Each tool includes:

- `name`
- `description`
- `inputSchema`

### `POST /tools/:name`

Accepts a JSON object matching the generated tool schema. Executes the compiled operation or workflow and returns normalized JSON.

### `POST /reload`

Reloads OpenAPI and Arazzo sources, recompiles the tool catalog, and swaps in the new compiled snapshot atomically.

## Configuration

Configuration should be explicit and file-backed. Environment interpolation is acceptable for secrets and deploy-time settings.

Example:

```yaml
service:
  name: billing
  port: 9100

target:
  base_url: https://billing.internal
  timeout_seconds: 30

openapi:
  source:
    mode: probe
    base_url: https://billing.internal
    candidates:
      - /openapi.json
      - /openapi.yaml
      - /swagger/v1/swagger.json
      - /v3/api-docs

arazzo:
  enabled: true
  source:
    mode: file
    path: /config/billing-workflows.yaml

auth:
  strategy: bearer
  token_env: BILLING_API_TOKEN

compile:
  include_tags:
    - invoices
    - customers
  exclude_operations:
    - internalReindex

overrides:
  operation_ids:
    getInvoice:
      tool_name: invoice_get
      description: Fetch one invoice by id
      response_pointer: /data
```

### OpenAPI Source Modes

Supported source modes:

- `file`
- `url`
- `probe`

Rules:

- OpenAPI is mandatory.
- If `probe` is used, the sidecar checks the configured candidates in order and uses the first successful OpenAPI 3.x document.
- If no valid document is found, the sidecar is unhealthy and exposes no tools.

### Arazzo Source Modes

Supported source modes:

- `file`
- `url`

Rules:

- Arazzo is optional.
- Missing or invalid Arazzo should not block operation-tool compilation unless `enabled: true` is paired with a strict mode in a future revision.

## Supported OpenAPI Subset

`api-sidecar` only compiles operations that fit the v1 golden path.

### Required

- OpenAPI 3.x document
- unique operation identity
  - prefer `operationId`
  - otherwise use generated `METHOD__normalized_path`
- JSON request and response handling

### Supported Inputs

- path parameters
- query parameters
- selected header parameters when explicitly allowlisted
- JSON request bodies

### Deferred or Rejected

- multipart forms
- file uploads
- XML bodies
- binary bodies
- cookies as tool inputs
- ambiguous polymorphism that cannot be rendered clearly

### Schema Support

Supported JSON Schema features:

- `type`
- `properties`
- `required`
- `enum`
- nested `object`
- nested `array`

Partially supported:

- `oneOf`
- `anyOf`
- `allOf`

Rule: if a schema shape cannot be compiled into a safe and understandable tool schema, the operation is skipped with a warning.

## Tool Naming

Naming must be stable and reviewable.

Rules:

1. Use `x-smith-tool-name` if present.
2. Else use `operationId` if present and unique.
3. Else use `method + normalized path`.

Examples:

- `getInvoice`
- `post__customers`
- `delete__invoices_by_id`

Names must be unique within one sidecar instance.

## Tool Description

Description precedence:

1. `x-smith-description`
2. OpenAPI `summary`
3. OpenAPI `description`
4. fallback description from method and path

## Input Schema Compilation

Each tool compiles to a single JSON object schema.

Field sources:

- path params become required fields
- query params become optional or required based on spec
- request body JSON object fields are merged into the tool schema

Rules:

- auth headers must not become public tool args
- transport headers must not become public tool args by default
- name collisions across path/query/body fields are compile errors unless overridden
- if request body is not a JSON object, the operation is skipped in v1

## Response Handling

The default return value is parsed JSON from the upstream response body.

Optional behavior:

- `x-smith-response-pointer` can select a JSON Pointer within the response
- a future `x-smith-result-template` may shape result output, but is out of scope for v1

If the upstream response is not JSON, the call fails in v1.

## Authentication

Authentication is sidecar-managed. Users should not pass credentials as tool inputs.

Supported v1 strategies:

- static bearer token
- API key in header
- API key in query string

Future candidates:

- OAuth client credentials
- mTLS
- delegated user identity propagation

OpenAPI security schemes may inform validation and warnings, but the sidecar config remains authoritative for execution.

## Arazzo Integration

Arazzo is for curated workflows, not for baseline API exposure.

Rules:

- OpenAPI operation tools are always compiled first.
- Arazzo workflows compile into additional tools.
- Workflow steps may reference compiled operation definitions.
- Workflow inputs compile into one JSON object schema.
- Workflow execution may pass data between steps using JSON Pointer extraction.
- Workflow output defaults to the final step result unless explicitly overridden.

## Vendor Extensions

The sidecar should support a small set of Smith-specific extensions:

- `x-smith-tool-name`
- `x-smith-description`
- `x-smith-hidden`
- `x-smith-response-pointer`
- `x-smith-cli-group`
- `x-smith-confirmation-required`

These extensions are the supported escape hatch for making generated tools usable without inventing a separate spec language.

## Failure Model

Compilation failures should be visible and actionable.

Rules:

- Invalid OpenAPI source: sidecar unhealthy, zero tools.
- Invalid Arazzo source: warning by default, operation tools still available.
- Unsupported operation: skipped with warning.
- Duplicate tool names: compile error unless overridden.
- Runtime upstream failures: surfaced to caller with upstream status and JSON body when possible.

The sidecar should keep the last known good compiled snapshot available if a reload fails.

## Implementation Shape

Add a new service:

- `service/api-sidecar`

Suggested module layout:

- `config.rs`
- `openapi_loader.rs`
- `arazzo_loader.rs`
- `compiler.rs`
- `executor.rs`
- `auth.rs`
- `http.rs`

Internal model:

- raw source documents
- compiled operation/workflow definitions
- generated tool list
- diagnostics bundle
- immutable active snapshot behind `Arc`

## v1 Delivery Plan

### Phase 1

- service skeleton
- config parsing
- OpenAPI load from file, URL, or probe list
- compile operation tools
- execute JSON HTTP operations
- support bearer and API-key auth
- expose `/health`, `/tools`, `/tools/:name`, `/reload`

### Phase 2

- Arazzo workflow compilation
- vendor extension support
- improved diagnostics
- response pointer selection

### Deferred

- multipart
- streaming
- OAuth client credentials
- delegated user auth
- advanced schema polymorphism
- non-JSON transports

## Open Questions

- Do we want a strict mode where invalid Arazzo blocks startup?
- Do we want header-parameter exposure to be opt-in globally or per operation only?
- Do we want generated tool names to preserve original casing or normalize to snake case?

## Decision

Proceed with `api-sidecar` as a narrow OpenAPI-first adapter. Treat OpenAPI as mandatory, Arazzo as optional, and optimize for stable tool generation rather than maximum protocol coverage.
