package smithcli

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"time"
)

func ExampleBootstrapConfig() {
	cfg, err := BootstrapConfig([]string{
		"--catalog-url", "http://catalog.internal",
		"--authorized-only=false",
		"--output", "raw",
	})
	if err != nil {
		panic(err)
	}

	fmt.Println(cfg.BaseURL)
	fmt.Println(cfg.AuthorizedOnly)
	fmt.Println(cfg.Output)
	// Output:
	// http://catalog.internal
	// false
	// raw
}

func ExampleBuildRootCmd_catalogList() {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/tools":
			_, _ = io.WriteString(w, `[{"name":"get_widget","description":"Fetch one widget","server":"demo","input_schema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}]`)
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()

	cfg := Config{
		BaseURL:        server.URL,
		AuthorizedOnly: false,
		Timeout:        time.Second,
		Output:         "json",
	}

	root, err := BuildRootCmd(context.Background(), &cfg)
	if err != nil {
		panic(err)
	}

	var out bytes.Buffer
	root.SetOut(&out)
	root.SetErr(io.Discard)
	root.SetArgs([]string{"catalog", "list"})
	if err := root.Execute(); err != nil {
		panic(err)
	}

	fmt.Println(strings.TrimSpace(out.String()))
	// Output:
	// demo  get_widget  Fetch one widget
}

func ExampleBuildRootCmd_executeTool() {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/api/tools":
			_, _ = io.WriteString(w, `[{"name":"get_widget","description":"Fetch one widget","server":"demo","input_schema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}]`)
		case "/api/tools/call":
			_, _ = io.WriteString(w, `{"id":"abc","name":"Widget abc"}`)
		default:
			http.NotFound(w, r)
		}
	}))
	defer server.Close()

	cfg := Config{
		BaseURL:        server.URL,
		AuthorizedOnly: false,
		Timeout:        time.Second,
		Output:         "json",
	}

	root, err := BuildRootCmd(context.Background(), &cfg)
	if err != nil {
		panic(err)
	}

	var out bytes.Buffer
	root.SetOut(&out)
	root.SetErr(io.Discard)
	root.SetArgs([]string{"demo", "get_widget", "--id", "abc"})
	if err := root.Execute(); err != nil {
		panic(err)
	}

	fmt.Println(strings.TrimSpace(out.String()))
	// Output:
	// {
	//   "id": "abc",
	//   "name": "Widget abc"
	// }
}
