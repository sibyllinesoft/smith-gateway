# CLI

The `smith` CLI exists so humans and scripts can call any tool in the catalog without knowing which sidecar hosts it, what protocol it speaks, or how it authenticates.

You don't write `smith` subcommands — they're generated at startup. The CLI fetches the tool catalog, and for each registered tool, it creates a command with flags derived from the tool's input schema. If a new sidecar registers a tool called `billing.get_invoice` with parameters `invoice_id` and `format`, you'll immediately see `smith billing get_invoice --invoice-id ... --format ...` without any code changes to the CLI itself.

## How it works

1. On startup, the CLI calls `GET /api/tools` on catalog
2. By default it requests only tools authorized for the current identity token
3. For each tool, it creates a Cobra command under the tool's server name (e.g., `smith fs read_file`, `smith github get_issue`)
4. Flags are generated from each tool's JSON input schema — types, required/optional, and descriptions all come from the schema

This means the CLI's command surface is always in sync with whatever's in the catalog. Add a sidecar, restart catalog, and the CLI picks up the new tools automatically.

## Quick start

List everything in the catalog:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" catalog list
```

Call a tool with generated flags:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" fs read_file --path /tmp/demo.txt
```

## More ways to pass arguments

Pass raw JSON when the generated flags aren't enough:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json '{"owner":"octocat","repo":"Hello-World","issue_number":1}'
```

Read arguments from a file:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json @issue-args.json
```

Read arguments from stdin (useful for piping):

```bash
printf '{"owner":"octocat","repo":"Hello-World","issue_number":1}\n' | \
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json @-
```

## Reference

### Flags

| Flag | Description |
|------|-------------|
| `--catalog-url` | Catalog service URL |
| `--index-url` | Alias for `--catalog-url` (compatibility) |
| `--api-token` | API token for catalog authentication |
| `--identity-token` | Identity token for authorization filtering |
| `--identity-token-file` | Read identity token from a file |
| `--authorized-only` | Only show authorized tools (default: true) |
| `--timeout` | Request timeout |
| `--output` | Output format |

### Environment variables

| Variable | Equivalent flag |
|----------|----------------|
| `SMITH_CATALOG_URL` | `--catalog-url` |
| `SMITH_INDEX_URL` | `--index-url` |
| `SMITH_API_TOKEN` | `--api-token` |
| `SMITH_IDENTITY_TOKEN` | `--identity-token` |
| `SMITH_IDENTITY_TOKEN_FILE` | `--identity-token-file` |
| `SMITH_AUTHORIZED_ONLY` | `--authorized-only` |
| `SMITH_TIMEOUT` | `--timeout` |
| `SMITH_OUTPUT` | `--output` |

### Notes

- The CLI asks for the authorized-only catalog by default. If catalog requires an identity token for discovery, provide `--identity-token` or `--identity-token-file`.
- `--index-url` and `SMITH_INDEX_URL` are supported as compatibility aliases.
- Generated flags come from the tool input schema, so the exact command surface depends on the current catalog contents.
