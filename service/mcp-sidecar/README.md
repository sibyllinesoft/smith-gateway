# mcp-sidecar

`mcp-sidecar` bridges a stdio MCP server to a small HTTP shim that `catalog` can poll and call.

## HTTP Surface

- `GET /health`
- `GET /tools`
- `POST /tools/:name`
- `GET /resources`
- `GET /resources/*uri`
- `POST /reload`

This is the contract `catalog` already understands, so each sidecar instance can be registered as an upstream without additional translation.

## Typical Usage

Run the filesystem MCP server behind the sidecar:

```bash
MCP_SIDECAR_API_TOKEN=change-me \
cargo run -p mcp-sidecar -- -- npx @modelcontextprotocol/server-filesystem /data
```

## Auth

The sidecar API itself can be protected with:

- `Authorization: Bearer <token>`
- `x-smith-token: <token>`

Configure with:

- `MCP_SIDECAR_API_TOKEN`
- `MCP_SIDECAR_ALLOW_UNAUTHENTICATED`

## Verified Identity Injection

If `MCP_SIDECAR_IDENTITY_SECRET` is configured, the sidecar verifies `x-oc-identity-token` and injects a reserved `_smith_identity` argument into stdio MCP tool calls.

Injected shape:

```json
{
  "user_id": "user-123",
  "role": "member",
  "channel": "chat",
  "principal": "user@example.com",
  "session": "session-abc"
}
```

That lets tool servers bind end-user context server-side without trusting the calling harness.

## Middleware

Optional middleware config can be supplied with `MCP_SIDECAR_MIDDLEWARE`.

It supports:

- filters
- input transforms
- output transforms
- hidden tools

The middleware config is reloaded on `POST /reload`.
