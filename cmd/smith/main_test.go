package main

import "testing"

func TestBuildVersionWithoutDate(t *testing.T) {
	version = "v1.2.3"
	commit = "abc123"
	date = ""
	t.Cleanup(func() {
		version = "dev"
		commit = "unknown"
		date = ""
	})

	if got := buildVersion(); got != "v1.2.3 (abc123)" {
		t.Fatalf("buildVersion() = %q", got)
	}
}

func TestWantsVersion(t *testing.T) {
	tests := []struct {
		name string
		args []string
		want bool
	}{
		{name: "subcommand", args: []string{"version"}, want: true},
		{name: "flag", args: []string{"--version"}, want: true},
		{name: "extra args", args: []string{"version", "--json"}, want: false},
		{name: "empty", args: nil, want: false},
	}

	for _, tt := range tests {
		if got := wantsVersion(tt.args); got != tt.want {
			t.Fatalf("%s: wantsVersion(%v) = %v, want %v", tt.name, tt.args, got, tt.want)
		}
	}
}
