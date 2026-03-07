# api-sidecar

Most APIs already have OpenAPI specs. `api-sidecar` turns them into Smith tools without writing any MCP code — point it at an OpenAPI 3.x document (local file, URL, or auto-discovered from the target service), and it compiles each operation into a tool that catalog can discover and the CLI can call.

This means if a service already publishes an OpenAPI spec, you can add it to the Smith catalog by writing a short YAML config file and starting a sidecar. No custom adapter code, no MCP implementation.

The scope is deliberately narrow: OpenAPI 3.x, HTTP+JSON, platform-managed auth, one operation per tool. This isn't a universal API gateway — it's a reliable adapter for well-described HTTP APIs.

## How it works

The sidecar is a compiler plus executor:

1. **Load** — Fetches the OpenAPI spec from a configured source (file, URL, or by probing standard paths on the target)
2. **Compile** — Turns each OpenAPI operation into a tool definition: path/query/header params and request body fields become tool input schema fields, auth gets handled by the sidecar config
3. **Serve** — Exposes the same HTTP contract as `mcp-sidecar` (`/health`, `/tools`, `/tools/:name`, `/reload`), so catalog treats it identically
4. **Execute** — When a tool is called, the sidecar maps the tool arguments back to HTTP request parameters, injects auth credentials, makes the upstream HTTP call, and returns the JSON response

It does not own catalog aggregation, search, or cross-service authorization policy. Those remain in `catalog`.

## Quick start

Run against GitHub's public API:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/github-public.yaml
```

Run against NHTSA's public vehicle data API:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/nhtsa-public.yaml
```

Local dev with the included mock API:

```bash
cargo run -p api-sidecar --bin mock-api
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/local-dev.yaml
```

## HTTP contract

The sidecar matches the shim shape that `catalog` already consumes, so each instance registers as an upstream without additional translation.

### `GET /health`

Returns sidecar status, source metadata, compile diagnostics, and tool count.

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

Returns compiled tool definitions with the same shape emitted by `mcp-sidecar`. Each tool includes `name`, `description`, and `inputSchema`.

### `POST /tools/:name`

Accepts a JSON object matching the generated tool schema. Executes the compiled operation or workflow and returns normalized JSON.

### `POST /reload`

Reloads OpenAPI and Arazzo sources, recompiles the tool catalog, and swaps in the new compiled snapshot atomically. If the reload fails, the sidecar keeps serving the last known good snapshot.

## Configuration

Configuration is file-backed. Environment interpolation is supported for secrets and deploy-time settings.

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

### OpenAPI source modes

- `file` — local path
- `url` — direct URL to spec document
- `probe` — checks configured candidate paths in order, uses the first valid OpenAPI 3.x document

OpenAPI is mandatory. If no valid document is found, the sidecar is unhealthy and exposes no tools.

### Arazzo source modes

- `file` — local path
- `url` — direct URL

Arazzo is optional. Missing or invalid Arazzo does not block operation-tool compilation.

## OpenAPI support

The sidecar compiles operations that fit its supported subset. Operations that don't fit are skipped with a warning — they don't break the sidecar, they just don't become tools.

### Supported inputs

- Path parameters
- Query parameters
- Allowlisted header parameters
- JSON request bodies with object schemas

### Supported schema features

- `type`, `properties`, `required`, `enum`
- Nested objects and arrays
- Local `$ref`
- `allOf` (simple object merges)

### Not supported (v1)

- Multipart uploads, file uploads
- Non-JSON request or response bodies (XML, binary)
- `oneOf`, `anyOf`
- Arbitrary remote `$ref`
- Cookies as tool inputs

If a schema shape can't be compiled into a clear tool schema, the operation is skipped with a warning.

## Tool naming

Tool names are stable and deterministic. Precedence:

1. `x-smith-tool-name` if present
2. `operationId` if present and unique
3. Generated from `method + normalized path` (e.g., `post__customers`, `delete__invoices_by_id`)

Names must be unique within one sidecar instance. Duplicates are a compile error unless overridden.

## Tool descriptions

Description precedence:

1. `x-smith-description`
2. OpenAPI `summary`
3. OpenAPI `description`
4. Fallback from method and path

## Input schema compilation

Each tool compiles to a single JSON object schema:

- Path params become required fields
- Query params become optional or required based on the spec
- Request body JSON object fields are merged into the tool schema
- Auth and transport headers never become tool arguments
- Name collisions across path/query/body fields are compile errors unless overridden
- If the request body is not a JSON object, the operation is skipped

## Response handling

The default return value is parsed JSON from the upstream response body. `x-smith-response-pointer` can select a sub-path within the response via JSON Pointer. Non-JSON responses fail the call.

## Auth

Outbound API auth is managed by the sidecar or broader platform layer — users never pass credentials as tool arguments.

Supported strategies:
- Bearer token
- API key header
- API key query parameter

Secrets come from inline config or environment variables. OpenAPI security schemes may inform validation and warnings, but the sidecar config is authoritative for execution.

This is about credential ownership, not forced runtime sharing. A deployment can still bind execution to verified user or session identity and have the sidecar resolve the correct credential context internally. The important boundary is that raw upstream secrets stay out of tool arguments.

Future candidates: OAuth client credentials, mTLS, delegated identity-bound credential lookup.

## Arazzo integration

Arazzo is for curated workflows, not for baseline API exposure. Workflow compilation is not yet implemented in the current code.

When implemented:

- OpenAPI operation tools are always compiled first
- Arazzo workflows compile into additional tools
- Workflow steps may reference compiled operation definitions
- Workflow inputs compile into one JSON object schema
- Workflow execution passes data between steps using JSON Pointer extraction
- Workflow output defaults to the final step result unless overridden

## Vendor extensions

Smith-specific OpenAPI extensions for customizing generated tools:

| Extension | Purpose |
|-----------|---------|
| `x-smith-tool-name` | Override generated tool name |
| `x-smith-description` | Override tool description |
| `x-smith-hidden` | Hide operation from tool list |
| `x-smith-response-pointer` | Select sub-path from response |
| `x-smith-cli-group` | CLI grouping hint |
| `x-smith-confirmation-required` | Mark tool as requiring confirmation |

These are the escape hatch for making generated tools usable without inventing a separate spec language.

## Failure behavior

- **Invalid OpenAPI source**: sidecar unhealthy, zero tools
- **Invalid Arazzo source**: warning, operation tools still available
- **Unsupported operation**: skipped with warning in diagnostics
- **Duplicate tool names**: compile error unless overridden
- **Runtime upstream failures**: surfaced to caller with upstream status and JSON body when possible
- **Failed reload**: last known good snapshot stays active
