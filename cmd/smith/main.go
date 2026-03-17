package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"smith-tool-gateway/internal/smithcli"
)

var (
	version = "dev"
	commit  = "unknown"
	date    = ""
)

func main() {
	if wantsVersion(os.Args[1:]) {
		fmt.Fprintln(os.Stdout, buildVersion())
		return
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	cfg, err := smithcli.BootstrapConfig(os.Args[1:])
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	root, err := smithcli.BuildRootCmd(ctx, &cfg, buildVersion())
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	if err := root.ExecuteContext(ctx); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func buildVersion() string {
	if date == "" || date == "unknown" {
		return fmt.Sprintf("%s (%s)", version, commit)
	}
	return fmt.Sprintf("%s (%s, %s)", version, commit, date)
}

func wantsVersion(args []string) bool {
	return len(args) == 1 && (args[0] == "version" || args[0] == "--version")
}
