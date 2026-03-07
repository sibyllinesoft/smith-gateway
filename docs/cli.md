# CLI

The `smith` CLI loads the tool catalog from `catalog` at startup and exposes each server/tool pair as Cobra commands.

## What It Does

- loads `/api/tools` from `catalog`
- optionally asks `catalog` for the identity-filtered catalog
- creates one top-level command per server
- creates one subcommand per tool
- generates flags from each tool's JSON input schema

## Usage

List the loaded catalog:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" catalog list
```

Call a tool with generated flags:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" fs read_file --path /tmp/demo.txt
```

Call a tool with raw JSON arguments:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json '{"owner":"octocat","repo":"Hello-World","issue_number":1}'
```

Read tool arguments from a file:

```bash
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json @issue-args.json
```

Read tool arguments from stdin:

```bash
printf '{"owner":"octocat","repo":"Hello-World","issue_number":1}\n' | \
go run ./cmd/smith --catalog-url http://localhost:9200 --identity-token "$IDENTITY_TOKEN" github get_issue --args-json @-
```

## Configuration

Flags:

- `--catalog-url`
- `--index-url`
- `--api-token`
- `--identity-token`
- `--identity-token-file`
- `--authorized-only`
- `--timeout`
- `--output`

Environment variables:

- `SMITH_CATALOG_URL`
- `SMITH_INDEX_URL`
- `SMITH_API_TOKEN`
- `SMITH_IDENTITY_TOKEN`
- `SMITH_IDENTITY_TOKEN_FILE`
- `SMITH_AUTHORIZED_ONLY`
- `SMITH_TIMEOUT`
- `SMITH_OUTPUT`

## Notes

- The CLI asks for the authorized-only catalog by default.
- If `catalog` is configured to require an identity token for discovery, provide `--identity-token` or `--identity-token-file`.
- `--index-url` and `SMITH_INDEX_URL` remain supported as compatibility aliases.
- Generated flags come from the tool input schema, so the exact command surface depends on the current catalog.
