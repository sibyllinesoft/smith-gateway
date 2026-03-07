# mcp-sidecar

MCP servers speak stdio — they read JSON-RPC from stdin and write to stdout. That's great for local tool use, but catalog needs to discover and call tools over HTTP. `mcp-sidecar` bridges the gap: it launches an MCP server as a child process, talks stdio to it, and exposes the tools over HTTP so catalog can treat it like any other upstream.

This keeps MCP server authors free from HTTP concerns. They write a standard stdio MCP server, and the sidecar handles the rest.

## How it works

The sidecar starts the MCP server command you give it, manages the stdio connection, and translates between HTTP requests and MCP JSON-RPC messages. When catalog (or anything else) calls `GET /tools`, the sidecar queries the child MCP server and returns the tool list. When a tool is called via `POST /tools/:name`, the sidecar forwards the arguments to the child over stdio and returns the result as HTTP JSON.

## Quick start

Run the filesystem MCP server behind the sidecar:

```bash
MCP_SIDECAR_API_TOKEN=change-me \
cargo run -p mcp-sidecar -- -- npx @modelcontextprotocol/server-filesystem /data
```

Everything after `-- --` is the MCP server command. The sidecar launches it, discovers its tools, and serves them over HTTP on the default port.

## Identity injection

This is a security feature, not just a convenience. When `MCP_SIDECAR_IDENTITY_SECRET` is configured, the sidecar verifies the `x-oc-identity-token` header on incoming requests and injects a `_smith_identity` argument into every tool call it forwards to the MCP server.

The injected identity looks like:

```json
{
  "user_id": "user-123",
  "role": "member",
  "channel": "chat",
  "principal": "user@example.com",
  "session": "session-abc"
}
```

This means the MCP server receives verified end-user context without the calling harness being able to forge it. The tool server can use this for audit logging, per-user scoping, or authorization decisions — and it doesn't have to trust the caller to be honest about who's making the request.

## Why a sidecar?

The alternative would be embedding HTTP handling into every MCP server. That would mean every tool author needs to deal with HTTP routing, auth headers, health endpoints, and reload logic. The sidecar approach pushes all of that into one shared component:

- Tool authors write plain stdio MCP servers — the simplest possible interface
- The gateway gets a consistent HTTP contract across all tools
- Auth, middleware, and lifecycle management happen in one place

The tradeoff is an extra process per tool server and one more hop in the call path. In practice this is negligible — the sidecar is lightweight and the stdio communication is fast.

## Reference

### HTTP endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health check |
| `GET /tools` | List available tools |
| `POST /tools/:name` | Call a tool |
| `GET /resources` | List resources |
| `GET /resources/*uri` | Get a resource |
| `POST /reload` | Reload tools and middleware |

### Auth

The sidecar API can be protected with:

- `Authorization: Bearer <token>`
- `x-smith-token: <token>`

Configure with:

| Variable | Description |
|----------|-------------|
| `MCP_SIDECAR_API_TOKEN` | Required token for API access |
| `MCP_SIDECAR_ALLOW_UNAUTHENTICATED` | Allow unauthenticated access |
| `MCP_SIDECAR_IDENTITY_SECRET` | Secret for verifying identity tokens |

### Middleware

Optional middleware can be configured via `MCP_SIDECAR_MIDDLEWARE`. It supports:

- filters
- input transforms
- output transforms
- hidden tools

The middleware config is reloaded on `POST /reload`.
