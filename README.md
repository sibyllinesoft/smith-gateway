# smith-tool-gateway

Smith Tool Gateway turns any tool — MCP servers, REST APIs, internal services — into a unified catalog that humans and AI agents can discover and call through a single interface. Instead of each consumer needing to know where tools live, what protocol they speak, and how to authenticate, the gateway handles all of that behind a consistent HTTP contract.

The system has three layers:

- **Sidecars** adapt tools to a common HTTP interface. `mcp-sidecar` wraps stdio MCP servers; `api-sidecar` compiles OpenAPI specs into tools. Both expose the same endpoints, so catalog doesn't care which kind of tool is behind them.
- **Catalog** aggregates all sidecar upstreams into a single tool registry with discovery, authorization filtering, and health tracking.
- **CLI** (`smith`) talks to catalog and dynamically generates commands for every tool in the registry — no hardcoded subcommands, no client-side tool knowledge.

There's also `pg-auth-gateway`, a Postgres wire proxy that validates Smith identity tokens and binds RLS context, but that's a supporting piece rather than the core flow.

## How the pieces fit together

```mermaid
graph LR
    mcp["MCP server<br/>(stdio)"] --> ms["mcp-sidecar"]
    api["REST API<br/>(OpenAPI)"] --> as["api-sidecar"]
    ms --> catalog
    as --> catalog
    catalog --> smith["smith CLI"]
```

Each sidecar registers as an upstream with catalog. Catalog merges their tool lists and serves them through `/api/tools`. The CLI fetches that list at startup and builds its command tree on the fly.

## Quick start

### Sidecars

Start an MCP sidecar wrapping a filesystem MCP server:

```bash
cargo run -p mcp-sidecar -- -- npx @modelcontextprotocol/server-filesystem /data
```

Start an API sidecar wrapping GitHub's public API via its OpenAPI spec:

```bash
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/github-public.yaml
```

Other demo configs:

```bash
# NHTSA public vehicle data
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/nhtsa-public.yaml

# Local mock API for development
cargo run -p api-sidecar --bin mock-api
API_SIDECAR_API_TOKEN=change-me \
cargo run -p api-sidecar -- --config service/api-sidecar/examples/local-dev.yaml
```

### Catalog

Start catalog, pointing it at one or more sidecar upstreams:

```bash
CATALOG_UPSTREAMS=fs=http://localhost:9100 cargo run -p catalog
```

For larger catalogs, tune discovery authorization:

- `CATALOG_AUTHZ_CONCURRENCY` (default `32`) — bounded parallel OPA checks
- `CATALOG_AUTHZ_CACHE_TTL_SECONDS` (default `30`) — discovery decision reuse
- `CATALOG_AUTHZ_CACHE_MAX_ENTRIES` (default `10000`) — bound cache memory

### CLI

Discover tools:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 catalog list
```

Call a tool:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 fs read_file --path /tmp/demo.txt
```

The CLI fetches `/api/tools?authorized=true` by default, so catalog only returns tools allowed for the supplied identity token. Pass `--identity-token` to authenticate, or use `--authorized-only=false` for the unfiltered catalog.

### Postgres auth gateway

```bash
PG_AUTH_GATEWAY_READONLY_URL=postgresql://smith_readonly:smith-readonly-dev@localhost:5432/smith \
PG_AUTH_GATEWAY_GATEKEEPER_URL=postgresql://smith_gatekeeper:smith-gatekeeper-dev@localhost:5432/smith \
PG_AUTH_GATEWAY_IDENTITY_SECRET=change-me \
cargo run -p pg-auth-gateway
```

## Development

```bash
cargo build --workspace
cargo test --workspace
go test ./...
```

## Component docs

- [CLI](docs/cli.md)
- [mcp-sidecar](service/mcp-sidecar/README.md)
- [api-sidecar](service/api-sidecar/README.md)
- [RFC 0001: API Sidecar](docs/api-sidecar.md)

## Docker

Build images from the repo root:

```bash
docker build -f service/mcp-sidecar/Dockerfile -t mcp-sidecar:local .
docker build -f service/api-sidecar/Dockerfile -t api-sidecar:local .
docker build -f service/catalog/Dockerfile -t catalog:local .
docker build -f service/pg-auth-gateway/Dockerfile -t pg-auth-gateway:local .
```

## Tradeoffs

The gateway deliberately stays simple in a few ways:

- **Sidecars are single-tool-server processes.** Each sidecar wraps exactly one MCP server or one OpenAPI spec. This means more processes to manage, but each one is stateless, independently deployable, and easy to reason about. The sidecar also consolidates functionality you'd need anyway in a service mesh deployment — health checks, auth token injection, reload, and a consistent HTTP contract — so the overhead is less "extra process" and more "purpose-built adapter that replaces generic infrastructure config."
- **Catalog is a registry, not a proxy.** Catalog knows where tools are but doesn't sit in the request path for tool calls — the CLI calls sidecars directly (via catalog-provided URLs). This keeps catalog simple and avoids making it a throughput bottleneck.
- **API sidecar supports a narrow OpenAPI subset.** It handles JSON-over-HTTP with path/query/header params and common schema features. It deliberately skips multipart uploads, non-JSON bodies, streaming, and complex polymorphism (`oneOf`/`anyOf`). The tradeoff is that a well-described REST API works reliably out of the box, while exotic APIs need a custom MCP server.
- **Auth is sidecar-managed, not user-delegated.** Sidecars hold API credentials and inject them on outbound requests. Users never pass raw credentials as tool arguments. This is simpler and safer for v1, but means OAuth login flows and per-user token delegation aren't supported yet.
