# api-sidecar

Most APIs already have OpenAPI specs. `api-sidecar` turns them into Smith tools without writing any MCP code — point it at an OpenAPI 3.x document (local file, URL, or auto-discovered from the target service), and it compiles each operation into a tool that catalog can discover and the CLI can call.

This means if a service already publishes an OpenAPI spec, you can add it to the Smith catalog by writing a short YAML config file and starting a sidecar. No custom adapter code, no MCP implementation.

## How it works

The sidecar is a compiler plus executor:

1. **Load** — Fetches the OpenAPI spec from a configured source (file, URL, or by probing standard paths on the target)
2. **Compile** — Turns each OpenAPI operation into a tool definition: path/query/header params and request body fields become tool input schema fields, auth gets handled by the sidecar config
3. **Serve** — Exposes the same HTTP contract as `mcp-sidecar` (`/health`, `/tools`, `/tools/:name`, `/reload`), so catalog treats it identically
4. **Execute** — When a tool is called, the sidecar maps the tool arguments back to HTTP request parameters, injects auth credentials, makes the upstream HTTP call, and returns the JSON response

One OpenAPI operation becomes one tool. The tool name comes from `operationId` (or is generated from the HTTP method and path). Tool descriptions come from the operation's `summary` or `description`.

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

## What it handles and what it skips

The sidecar deliberately supports a narrow subset of OpenAPI. The tradeoff: well-described JSON REST APIs work reliably out of the box, while exotic APIs need a custom MCP server.

**Supported inputs:**
- Path parameters
- Query parameters
- Allowlisted header parameters
- JSON request bodies with object schemas

**Supported schema features:**
- `type`, `properties`, `required`, `enum`
- Nested objects and arrays
- Local `$ref`
- Simple object `allOf` merges

**Deliberately not supported (v1):**
- Multipart uploads
- Non-JSON request or response bodies
- `oneOf`, `anyOf`
- Arbitrary remote `$ref`

Operations that use unsupported features are skipped at compile time with a warning in the `/health` diagnostics — they don't break the sidecar, they just don't become tools.

## OpenAPI source discovery

The sidecar can find an OpenAPI spec three ways:

1. **File** — local path in config
2. **URL** — direct URL to the spec document
3. **Probe** — tries standard paths on the target service and uses the first valid OpenAPI 3.x document it finds

Default probe paths:
- `/openapi.json`
- `/openapi.yaml`
- `/swagger/v1/swagger.json`
- `/v3/api-docs`

## Auth

Outbound API auth is managed by the sidecar — users never pass credentials as tool arguments.

Supported strategies:
- Bearer token
- API key header
- API key query parameter

Secrets come from inline config or environment variables.

## Reference

### HTTP endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check with compile diagnostics |
| `GET /tools` | List compiled tools |
| `POST /tools/:name` | Call a tool |
| `POST /reload` | Recompile tools from config atomically |

### Notes

- OpenAPI 3.x is required. Arazzo is optional (workflow compilation is not yet implemented).
- Reload recompiles the active tool snapshot atomically from the config file.
- If a reload fails, the sidecar keeps serving the last known good snapshot.
