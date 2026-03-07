// Package smithcli implements the dynamic Smith CLI.
//
// The CLI loads its command tree from `catalog` at startup and maps each
// server/tool pair into Cobra commands:
//
//	smith catalog list
//	smith github get-repo --owner octocat --repo Hello-World
//
// Global configuration can come from flags or environment variables:
//
//	SMITH_INDEX_URL
//	SMITH_API_TOKEN
//	SMITH_IDENTITY_TOKEN
//	SMITH_IDENTITY_TOKEN_FILE
//	SMITH_AUTHORIZED_ONLY
//	SMITH_TIMEOUT
//	SMITH_OUTPUT
//
// See the package examples for executable usage snippets.
package smithcli
