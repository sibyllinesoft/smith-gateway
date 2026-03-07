package smithcli

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestBootstrapConfigAllowsUnknownToolFlags(t *testing.T) {
	t.Setenv("SMITH_AUTHORIZED_ONLY", "false")

	cfg, err := BootstrapConfig([]string{
		"--index-url", "http://example.test",
		"server",
		"tool",
		"--tool-flag", "value",
	})
	if err != nil {
		t.Fatalf("BootstrapConfig returned error: %v", err)
	}

	if cfg.BaseURL != "http://example.test" {
		t.Fatalf("expected index URL to be parsed, got %q", cfg.BaseURL)
	}
	if cfg.AuthorizedOnly {
		t.Fatalf("expected authorized-only to remain false")
	}
}

func TestBootstrapConfigLoadsIdentityTokenFromFile(t *testing.T) {
	dir := t.TempDir()
	tokenPath := filepath.Join(dir, "identity.jwt")
	if err := os.WriteFile(tokenPath, []byte(" signed-token \n"), 0o600); err != nil {
		t.Fatalf("write token file: %v", err)
	}

	t.Setenv("SMITH_AUTHORIZED_ONLY", "true")

	cfg, err := BootstrapConfig([]string{
		"--identity-token-file", tokenPath,
	})
	if err != nil {
		t.Fatalf("BootstrapConfig returned error: %v", err)
	}

	if cfg.IdentityToken != "signed-token" {
		t.Fatalf("expected token from file, got %q", cfg.IdentityToken)
	}
}

func TestMissingRequiredProperties(t *testing.T) {
	schema := &JSONSchema{
		Required: []string{"alpha", "beta"},
	}

	got := missingRequiredProperties(schema, map[string]any{
		"alpha": "present",
	})
	want := []string{"beta"}

	if !reflect.DeepEqual(got, want) {
		t.Fatalf("missingRequiredProperties = %#v, want %#v", got, want)
	}
}
