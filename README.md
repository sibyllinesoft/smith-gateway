# smith-tool-gateway

Tool-facing MCP services extracted from `smith-core`:

- `mcp-sidecar`: stdio MCP server -> HTTP shim
- `api-sidecar`: OpenAPI-described HTTP API -> HTTP shim
- `mcp-index`: unified MCP tool catalog and gateway
- `pg-auth-gateway`: Postgres wire proxy that validates Smith identity tokens and binds hardened RLS context
- `smith`: Cobra-based CLI that loads commands dynamically from `mcp-index`

`mcp-sidecar` can also verify `x-oc-identity-token` and inject a reserved `_smith_identity` argument into stdio MCP tool calls. That lets stdio tool servers bind verified end-user context server-side instead of trusting the harness.

## Development

```bash
cargo build --workspace
cargo test --workspace
go test ./...
```

## Design

- [RFC 0001: API Sidecar](/home/nathan/Projects/smith-tool-gateway/docs/rfcs/0001-api-sidecar.md)

## Component Docs

- [CLI](/home/nathan/Projects/smith-tool-gateway/docs/cli.md)
- [mcp-sidecar](/home/nathan/Projects/smith-tool-gateway/service/mcp-sidecar/README.md)
- [api-sidecar](/home/nathan/Projects/smith-tool-gateway/service/api-sidecar/README.md)

Run sidecar:

```bash
cargo run -p mcp-sidecar -- -- npx @modelcontextprotocol/server-filesystem /data
```

Run API sidecar:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/github-public.yaml
```

The API sidecar is designed for remote upstreams. It can load OpenAPI directly from a URL or probe standard remote paths such as `/openapi.json`, `/openapi.yaml`, `/swagger/v1/swagger.json`, and `/v3/api-docs`.

Remote demo configs:

- `service/api-sidecar/examples/github-public.yaml`: GitHub public REST reads via GitHub's official OpenAPI
- `service/api-sidecar/examples/nhtsa-public.yaml`: NHTSA public vehicle data via NHTSA's official OpenAPI

Local dev sanity check:

```bash
cargo run -p api-sidecar --bin mock-api
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/local-dev.yaml
```

Run index:

```bash
MCP_INDEX_UPSTREAMS=fs=http://localhost:9100 cargo run -p mcp-index
```

Run the Postgres auth gateway:

```bash
PG_AUTH_GATEWAY_READONLY_URL=postgresql://smith_readonly:smith-readonly-dev@localhost:5432/smith \
PG_AUTH_GATEWAY_GATEKEEPER_URL=postgresql://smith_gatekeeper:smith-gatekeeper-dev@localhost:5432/smith \
PG_AUTH_GATEWAY_IDENTITY_SECRET=change-me \
cargo run -p pg-auth-gateway
```

Run CLI:

```bash
go run ./cmd/smith --index-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" catalog list
go run ./cmd/smith --index-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" fs read_file --path /tmp/demo.txt
```

The CLI fetches `/api/tools?authorized=true` by default, so `mcp-index` can return only the tools allowed for the supplied identity token. Use `--authorized-only=false` if you want the raw catalog instead.

For larger catalogs, tune `mcp-index` discovery authorization with:

- `MCP_INDEX_AUTHZ_CONCURRENCY` (default `32`) for bounded parallel OPA checks
- `MCP_INDEX_AUTHZ_CACHE_TTL_SECONDS` (default `30`) for discovery decision reuse
- `MCP_INDEX_AUTHZ_CACHE_MAX_ENTRIES` (default `10000`) to bound cache memory

## Docker

Build images from this repo root:

```bash
docker build -f service/mcp-sidecar/Dockerfile -t mcp-sidecar:local .
docker build -f service/api-sidecar/Dockerfile -t api-sidecar:local .
docker build -f service/mcp-index/Dockerfile -t mcp-index:local .
docker build -f service/pg-auth-gateway/Dockerfile -t pg-auth-gateway:local .
```
