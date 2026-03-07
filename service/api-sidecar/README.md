# api-sidecar

`api-sidecar` exposes OpenAPI-described HTTP APIs through the same shim contract used by `mcp-sidecar`.

It is designed for remote upstreams first: point it at a remote OpenAPI document or a remote service that exposes one at a standard path, and the sidecar compiles operations into tools that `mcp-index` can ingest.

## Golden Path

- OpenAPI 3.x is required
- Arazzo is optional
- HTTP+JSON only
- sidecar-managed auth
- one OpenAPI operation becomes one tool

## HTTP Surface

- `GET /health`
- `GET /tools`
- `POST /tools/:name`
- `POST /reload`

## Config Sources

OpenAPI can be loaded from:

- local file
- remote URL
- remote probe list

Default probe candidates:

- `/openapi.json`
- `/openapi.yaml`
- `/swagger/v1/swagger.json`
- `/v3/api-docs`

## Remote Demos

GitHub public reads:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/github-public.yaml
```

NHTSA public vehicle data:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/nhtsa-public.yaml
```

## Local Dev

Run the local mock API:

```bash
cargo run -p api-sidecar --bin mock-api
```

Then run the sidecar against the mock API:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/local-dev.yaml
```

## Supported v1 OpenAPI Subset

Inputs:

- path parameters
- query parameters
- allowlisted header parameters
- JSON request bodies with object schemas

Schema features:

- `type`
- `properties`
- `required`
- `enum`
- nested objects
- nested arrays
- local `$ref`
- simple object `allOf` merges

Explicitly not supported in v1:

- multipart uploads
- non-JSON request bodies
- non-JSON responses
- `oneOf`
- `anyOf`
- arbitrary remote `$ref`

## Auth

Outbound API auth is sidecar-managed.

Supported strategies:

- bearer token
- API key header
- API key query parameter

Secrets can come from inline config or environment variables.

## Notes

- Arazzo may be configured, but workflow compilation is not implemented yet in the current code.
- Reload recompiles the active tool snapshot atomically from the config file.
