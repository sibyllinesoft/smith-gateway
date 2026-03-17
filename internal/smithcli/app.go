package smithcli

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strconv"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

const (
	defaultCatalogURL = "http://localhost:9200"
	defaultTimeout    = 30 * time.Second
)

type Config struct {
	BaseURL           string
	APIToken          string
	IdentityToken     string
	IdentityTokenFile string
	AuthorizedOnly    bool
	Timeout           time.Duration
	Output            string
}

type Tool struct {
	Name        string      `json:"name"`
	Description string      `json:"description,omitempty"`
	InputSchema *JSONSchema `json:"input_schema,omitempty"`
	Server      string      `json:"server"`
}

type JSONSchema struct {
	Type       string                        `json:"type,omitempty"`
	Properties map[string]JSONSchemaProperty `json:"properties,omitempty"`
	Required   []string                      `json:"required,omitempty"`
}

type JSONSchemaProperty struct {
	Type        string `json:"type,omitempty"`
	Description string `json:"description,omitempty"`
}

type Client struct {
	baseURL        string
	apiToken       string
	identityToken  string
	authorizedOnly bool
	httpClient     *http.Client
}

type apiError struct {
	Status int
	Body   string
}

func (e *apiError) Error() string {
	if e.Body == "" {
		return fmt.Sprintf("catalog API returned HTTP %d", e.Status)
	}
	return fmt.Sprintf("catalog API returned HTTP %d: %s", e.Status, e.Body)
}

type argumentValue interface {
	pflag.Value
	IsSet() bool
	Value() any
}

type stringValue struct {
	set   bool
	value string
}

func (v *stringValue) String() string { return v.value }
func (v *stringValue) Set(raw string) error {
	v.value = raw
	v.set = true
	return nil
}
func (v *stringValue) Type() string { return "string" }
func (v *stringValue) IsSet() bool  { return v.set }
func (v *stringValue) Value() any   { return v.value }

type intValue struct {
	set   bool
	value int64
}

func (v *intValue) String() string {
	if !v.set {
		return ""
	}
	return strconv.FormatInt(v.value, 10)
}
func (v *intValue) Set(raw string) error {
	parsed, err := strconv.ParseInt(raw, 10, 64)
	if err != nil {
		return fmt.Errorf("invalid integer %q", raw)
	}
	v.value = parsed
	v.set = true
	return nil
}
func (v *intValue) Type() string { return "integer" }
func (v *intValue) IsSet() bool  { return v.set }
func (v *intValue) Value() any   { return v.value }

type floatValue struct {
	set   bool
	value float64
}

func (v *floatValue) String() string {
	if !v.set {
		return ""
	}
	return strconv.FormatFloat(v.value, 'f', -1, 64)
}
func (v *floatValue) Set(raw string) error {
	parsed, err := strconv.ParseFloat(raw, 64)
	if err != nil {
		return fmt.Errorf("invalid number %q", raw)
	}
	v.value = parsed
	v.set = true
	return nil
}
func (v *floatValue) Type() string { return "number" }
func (v *floatValue) IsSet() bool  { return v.set }
func (v *floatValue) Value() any   { return v.value }

type boolValue struct {
	set   bool
	value bool
}

func (v *boolValue) String() string {
	if !v.set {
		return ""
	}
	return strconv.FormatBool(v.value)
}
func (v *boolValue) Set(raw string) error {
	parsed, err := strconv.ParseBool(raw)
	if err != nil {
		return fmt.Errorf("invalid boolean %q", raw)
	}
	v.value = parsed
	v.set = true
	return nil
}
func (v *boolValue) Type() string { return "boolean" }
func (v *boolValue) IsSet() bool  { return v.set }
func (v *boolValue) Value() any   { return v.value }

type jsonValue struct {
	set   bool
	value any
}

func (v *jsonValue) String() string {
	if !v.set {
		return ""
	}
	data, err := json.Marshal(v.value)
	if err != nil {
		return ""
	}
	return string(data)
}
func (v *jsonValue) Set(raw string) error {
	parsed, err := decodeJSONValue(strings.NewReader(raw))
	if err != nil {
		return err
	}
	v.value = parsed
	v.set = true
	return nil
}
func (v *jsonValue) Type() string { return "json" }
func (v *jsonValue) IsSet() bool  { return v.set }
func (v *jsonValue) Value() any   { return v.value }

func BootstrapConfig(args []string) (Config, error) {
	cfg := Config{
		BaseURL:           firstEnvOrDefault([]string{"SMITH_CATALOG_URL", "SMITH_INDEX_URL"}, defaultCatalogURL),
		APIToken:          strings.TrimSpace(os.Getenv("SMITH_API_TOKEN")),
		IdentityToken:     strings.TrimSpace(os.Getenv("SMITH_IDENTITY_TOKEN")),
		IdentityTokenFile: strings.TrimSpace(os.Getenv("SMITH_IDENTITY_TOKEN_FILE")),
		AuthorizedOnly:    envBoolOrDefault("SMITH_AUTHORIZED_ONLY", true),
		Timeout:           envDurationOrDefault("SMITH_TIMEOUT", defaultTimeout),
		Output:            envOrDefault("SMITH_OUTPUT", "json"),
	}

	fs := pflag.NewFlagSet("smith-bootstrap", pflag.ContinueOnError)
	fs.ParseErrorsWhitelist.UnknownFlags = true
	fs.SetInterspersed(true)
	fs.StringVar(&cfg.BaseURL, "catalog-url", cfg.BaseURL, "catalog base URL")
	fs.StringVar(&cfg.BaseURL, "index-url", cfg.BaseURL, "catalog base URL (deprecated alias)")
	fs.StringVar(&cfg.APIToken, "api-token", cfg.APIToken, "catalog API token")
	fs.StringVar(&cfg.IdentityToken, "identity-token", cfg.IdentityToken, "signed identity token")
	fs.StringVar(&cfg.IdentityTokenFile, "identity-token-file", cfg.IdentityTokenFile, "path to a signed identity token")
	fs.BoolVar(&cfg.AuthorizedOnly, "authorized-only", cfg.AuthorizedOnly, "only load tools allowed for the current identity")
	fs.DurationVar(&cfg.Timeout, "timeout", cfg.Timeout, "HTTP timeout")
	fs.StringVar(&cfg.Output, "output", cfg.Output, "output format: json or raw")
	if err := fs.Parse(args); err != nil {
		return Config{}, err
	}

	if err := finalizeConfig(&cfg); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

func BuildRootCmd(ctx context.Context, cfg *Config, version string) (*cobra.Command, error) {
	client := NewClient(*cfg)
	tools, err := client.FetchTools(ctx)
	if err != nil {
		return nil, err
	}

	root := &cobra.Command{
		Use:           "smith",
		Short:         "Dynamic CLI for the smith MCP tool catalog",
		Long:          "Smith loads the tool catalog from catalog at startup and exposes each server/tool pair as Cobra commands.",
		SilenceUsage:  true,
		SilenceErrors: true,
	}
	if strings.TrimSpace(version) != "" {
		root.Version = strings.TrimSpace(version)
	}
	bindGlobalFlags(root.PersistentFlags(), cfg)
	root.AddCommand(newCatalogCmd(tools))
	attachToolCommands(root, client, tools, cfg)
	root.SetContext(ctx)
	return root, nil
}

func NewClient(cfg Config) *Client {
	return &Client{
		baseURL:        strings.TrimRight(cfg.BaseURL, "/"),
		apiToken:       strings.TrimSpace(cfg.APIToken),
		identityToken:  strings.TrimSpace(cfg.IdentityToken),
		authorizedOnly: cfg.AuthorizedOnly,
		httpClient: &http.Client{
			Timeout: cfg.Timeout,
		},
	}
}

func (c *Client) FetchTools(ctx context.Context) ([]Tool, error) {
	endpoint, err := url.Parse(c.baseURL + "/api/tools")
	if err != nil {
		return nil, err
	}
	query := endpoint.Query()
	query.Set("authorized", strconv.FormatBool(c.authorizedOnly))
	endpoint.RawQuery = query.Encode()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint.String(), nil)
	if err != nil {
		return nil, err
	}
	c.applyHeaders(req)

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, decodeAPIError(resp)
	}

	var tools []Tool
	if err := json.NewDecoder(resp.Body).Decode(&tools); err != nil {
		return nil, err
	}

	sort.Slice(tools, func(i, j int) bool {
		if tools[i].Server != tools[j].Server {
			return tools[i].Server < tools[j].Server
		}
		return tools[i].Name < tools[j].Name
	})
	return tools, nil
}

func (c *Client) CallTool(ctx context.Context, server, tool string, args map[string]any) (any, error) {
	body, err := json.Marshal(map[string]any{
		"server":    server,
		"tool":      tool,
		"arguments": args,
	})
	if err != nil {
		return nil, err
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/api/tools/call", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	c.applyHeaders(req)

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return nil, decodeAPIError(resp)
	}

	var result any
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, err
	}
	return result, nil
}

func (c *Client) applyHeaders(req *http.Request) {
	if c.apiToken != "" {
		req.Header.Set("Authorization", "Bearer "+c.apiToken)
		req.Header.Set("x-smith-token", c.apiToken)
	}
	if c.identityToken != "" {
		req.Header.Set("x-oc-identity-token", c.identityToken)
	}
}

func attachToolCommands(root *cobra.Command, client *Client, tools []Tool, cfg *Config) {
	serverCommands := map[string]*cobra.Command{}

	for _, tool := range tools {
		serverCmd := serverCommands[tool.Server]
		if serverCmd == nil {
			serverCmd = &cobra.Command{
				Use:   tool.Server,
				Short: fmt.Sprintf("Tools exposed by %s", tool.Server),
			}
			root.AddCommand(serverCmd)
			serverCommands[tool.Server] = serverCmd
		}
		serverCmd.AddCommand(newToolCmd(client, cfg, tool))
	}
}

func newToolCmd(client *Client, cfg *Config, tool Tool) *cobra.Command {
	use := tool.Name
	short := tool.Description
	if short == "" {
		short = fmt.Sprintf("Call %s on %s", tool.Name, tool.Server)
	}

	type boundFlag struct {
		name  string
		value argumentValue
	}

	bound := make([]boundFlag, 0, len(tool.InputSchema.GetProperties()))
	var rawArgs string

	cmd := &cobra.Command{
		Use:   use,
		Short: short,
		Long:  buildToolLongHelp(tool),
		Args:  cobra.NoArgs,
		RunE: func(cmd *cobra.Command, args []string) error {
			payload := map[string]any{}
			if rawArgs != "" {
				parsed, err := loadJSONObject(rawArgs)
				if err != nil {
					return err
				}
				for key, value := range parsed {
					payload[key] = value
				}
			}

			for _, entry := range bound {
				if entry.value.IsSet() {
					payload[entry.name] = entry.value.Value()
				}
			}

			if missing := missingRequiredProperties(tool.InputSchema, payload); len(missing) > 0 {
				return fmt.Errorf("missing required arguments: %s", strings.Join(missing, ", "))
			}

			result, err := client.CallTool(cmd.Context(), tool.Server, tool.Name, payload)
			if err != nil {
				return err
			}
			return writeOutput(cmd.OutOrStdout(), cfg.Output, result)
		},
	}

	cmd.Flags().StringVar(&rawArgs, "args-json", "", "raw JSON object for tool arguments, or @file / @- for stdin")
	for _, name := range sortedPropertyNames(tool.InputSchema) {
		prop := tool.InputSchema.Properties[name]
		value, usage := newArgumentValue(prop)
		cmd.Flags().Var(value, name, usageForProperty(name, prop, usage, requiredSet(tool.InputSchema)[name]))
		if prop.Type == "boolean" {
			if flag := cmd.Flags().Lookup(name); flag != nil {
				flag.NoOptDefVal = "true"
			}
		}
		bound = append(bound, boundFlag{name: name, value: value})
	}

	return cmd
}

func newCatalogCmd(tools []Tool) *cobra.Command {
	cmd := &cobra.Command{
		Use:   "catalog",
		Short: "Inspect the loaded tool catalog",
	}

	cmd.AddCommand(&cobra.Command{
		Use:   "list",
		Short: "List the tools loaded from catalog",
		RunE: func(cmd *cobra.Command, args []string) error {
			w := tabwriter.NewWriter(cmd.OutOrStdout(), 2, 2, 2, ' ', 0)
			for _, tool := range tools {
				desc := strings.TrimSpace(tool.Description)
				if desc == "" {
					desc = "-"
				}
				if _, err := fmt.Fprintf(w, "%s\t%s\t%s\n", tool.Server, tool.Name, desc); err != nil {
					return err
				}
			}
			return w.Flush()
		},
	})

	return cmd
}

func bindGlobalFlags(fs *pflag.FlagSet, cfg *Config) {
	fs.StringVar(&cfg.BaseURL, "catalog-url", cfg.BaseURL, "catalog base URL")
	fs.StringVar(&cfg.BaseURL, "index-url", cfg.BaseURL, "catalog base URL (deprecated alias)")
	fs.StringVar(&cfg.APIToken, "api-token", cfg.APIToken, "catalog API token")
	fs.StringVar(&cfg.IdentityToken, "identity-token", cfg.IdentityToken, "signed identity token")
	fs.StringVar(&cfg.IdentityTokenFile, "identity-token-file", cfg.IdentityTokenFile, "path to a signed identity token")
	fs.BoolVar(&cfg.AuthorizedOnly, "authorized-only", cfg.AuthorizedOnly, "only load tools allowed for the current identity")
	fs.DurationVar(&cfg.Timeout, "timeout", cfg.Timeout, "HTTP timeout")
	fs.StringVar(&cfg.Output, "output", cfg.Output, "output format: json or raw")
}

func finalizeConfig(cfg *Config) error {
	cfg.BaseURL = strings.TrimRight(strings.TrimSpace(cfg.BaseURL), "/")
	if cfg.BaseURL == "" {
		cfg.BaseURL = defaultCatalogURL
	}

	cfg.Output = strings.ToLower(strings.TrimSpace(cfg.Output))
	switch cfg.Output {
	case "", "json":
		cfg.Output = "json"
	case "raw":
	default:
		return fmt.Errorf("unsupported output format %q", cfg.Output)
	}

	if cfg.IdentityToken == "" && cfg.IdentityTokenFile != "" {
		data, err := os.ReadFile(cfg.IdentityTokenFile)
		if err != nil {
			return err
		}
		cfg.IdentityToken = strings.TrimSpace(string(data))
	}

	if cfg.Timeout <= 0 {
		cfg.Timeout = defaultTimeout
	}

	return nil
}

func buildToolLongHelp(tool Tool) string {
	parts := []string{
		fmt.Sprintf("Server: %s", tool.Server),
		fmt.Sprintf("Tool: %s", tool.Name),
	}
	if desc := strings.TrimSpace(tool.Description); desc != "" {
		parts = append(parts, "", desc)
	}
	if schema := tool.InputSchema; schema != nil && len(schema.Properties) > 0 {
		parts = append(parts, "", "Arguments are generated from the tool input schema.")
		if len(schema.Required) > 0 {
			req := append([]string(nil), schema.Required...)
			sort.Strings(req)
			parts = append(parts, "Required: "+strings.Join(req, ", "))
		}
	}
	return strings.Join(parts, "\n")
}

func newArgumentValue(prop JSONSchemaProperty) (argumentValue, string) {
	switch prop.Type {
	case "integer":
		return &intValue{}, "integer"
	case "number":
		return &floatValue{}, "number"
	case "boolean":
		return &boolValue{}, "boolean"
	case "array", "object":
		return &jsonValue{}, "JSON"
	default:
		return &stringValue{}, "string"
	}
}

func usageForProperty(name string, prop JSONSchemaProperty, kind string, required bool) string {
	parts := []string{fmt.Sprintf("%s (%s)", name, kind)}
	if required {
		parts = append(parts, "required")
	}
	if desc := strings.TrimSpace(prop.Description); desc != "" {
		parts = append(parts, desc)
	}
	return strings.Join(parts, "; ")
}

func sortedPropertyNames(schema *JSONSchema) []string {
	if schema == nil || len(schema.Properties) == 0 {
		return nil
	}
	names := make([]string, 0, len(schema.Properties))
	for name := range schema.Properties {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

func requiredSet(schema *JSONSchema) map[string]bool {
	set := make(map[string]bool)
	if schema == nil {
		return set
	}
	for _, name := range schema.Required {
		set[name] = true
	}
	return set
}

func missingRequiredProperties(schema *JSONSchema, payload map[string]any) []string {
	if schema == nil || len(schema.Required) == 0 {
		return nil
	}
	missing := make([]string, 0)
	for _, name := range schema.Required {
		if _, ok := payload[name]; !ok {
			missing = append(missing, name)
		}
	}
	sort.Strings(missing)
	return missing
}

func loadJSONObject(raw string) (map[string]any, error) {
	reader, err := jsonInputReader(raw)
	if err != nil {
		return nil, err
	}

	value, err := decodeJSONValue(reader)
	if err != nil {
		return nil, err
	}

	obj, ok := value.(map[string]any)
	if !ok {
		return nil, errors.New("tool arguments must decode to a JSON object")
	}
	return obj, nil
}

func jsonInputReader(raw string) (io.Reader, error) {
	if !strings.HasPrefix(raw, "@") {
		return strings.NewReader(raw), nil
	}

	path := strings.TrimPrefix(raw, "@")
	if path == "-" {
		data, err := io.ReadAll(os.Stdin)
		if err != nil {
			return nil, err
		}
		return bytes.NewReader(data), nil
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return bytes.NewReader(data), nil
}

func decodeJSONValue(reader io.Reader) (any, error) {
	decoder := json.NewDecoder(reader)
	decoder.UseNumber()

	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, err
	}
	return value, nil
}

func writeOutput(w io.Writer, mode string, value any) error {
	encoder := json.NewEncoder(w)
	if mode == "json" {
		encoder.SetIndent("", "  ")
	}
	return encoder.Encode(value)
}

func decodeAPIError(resp *http.Response) error {
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return &apiError{Status: resp.StatusCode}
	}
	return &apiError{
		Status: resp.StatusCode,
		Body:   strings.TrimSpace(string(body)),
	}
}

func envOrDefault(name, fallback string) string {
	value := strings.TrimSpace(os.Getenv(name))
	if value == "" {
		return fallback
	}
	return value
}

func firstEnvOrDefault(names []string, fallback string) string {
	for _, name := range names {
		if value := strings.TrimSpace(os.Getenv(name)); value != "" {
			return value
		}
	}
	return fallback
}

func envBoolOrDefault(name string, fallback bool) bool {
	value := strings.TrimSpace(os.Getenv(name))
	if value == "" {
		return fallback
	}
	parsed, err := strconv.ParseBool(value)
	if err != nil {
		return fallback
	}
	return parsed
}

func envDurationOrDefault(name string, fallback time.Duration) time.Duration {
	value := strings.TrimSpace(os.Getenv(name))
	if value == "" {
		return fallback
	}
	parsed, err := time.ParseDuration(value)
	if err != nil {
		return fallback
	}
	return parsed
}

func (s *JSONSchema) GetProperties() map[string]JSONSchemaProperty {
	if s == nil {
		return nil
	}
	return s.Properties
}
