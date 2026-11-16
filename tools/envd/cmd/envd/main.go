package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
	"github.com/cynosure279/mitnick/tools/envd/internal/server"
)

func main() {
	var (
		embedded   bool
		service    bool
		root       string
		maxFrame   int
		listen     string
		dataListen string
		token      string
		tlsCert    string
		tlsKey     string
		sandboxCmd string
	)
	var sandboxArgs stringList
	var netAllow stringList
	flag.BoolVar(&embedded, "embedded", false, "serve the frame protocol over stdin/stdout")
	flag.BoolVar(&service, "service", false, "serve MCP over HTTP and MSDP over TCP")
	flag.StringVar(&root, "root", ".msk-envd", "envd data root")
	flag.IntVar(&maxFrame, "max-frame", protocol.EmbeddedMaxFrame, "maximum frame size in bytes")
	flag.StringVar(&listen, "listen", "127.0.0.1:7800", "service control listen address")
	flag.StringVar(&dataListen, "data-listen", "127.0.0.1:7801", "service data listen address (empty disables)")
	flag.StringVar(&token, "token", "", "service bearer token (defaults to ENVD_TOKEN)")
	flag.StringVar(&tlsCert, "tls-cert", "", "TLS certificate for service mode")
	flag.StringVar(&tlsKey, "tls-key", "", "TLS private key for service mode")
	flag.StringVar(&sandboxCmd, "sandbox-command", "", "executable prefix for the isolated profile")
	flag.Var(&sandboxArgs, "sandbox-arg", "argument for --sandbox-command (repeatable; use --sandbox-arg=value)")
	flag.Var(&netAllow, "net-allow", "egress allowlist entry: CIDR or domain suffix (repeatable/comma-separated; empty allows all)")
	flag.Parse()

	if embedded && service {
		fmt.Fprintln(os.Stderr, "envd: --embedded and --service are mutually exclusive")
		os.Exit(2)
	}
	if !embedded && !service {
		fmt.Fprintln(os.Stderr, "envd: choose --embedded or --service")
		os.Exit(2)
	}

	absRoot, err := filepath.Abs(root)
	if err != nil {
		fmt.Fprintln(os.Stderr, "envd:", err)
		os.Exit(1)
	}
	if err := os.MkdirAll(absRoot, 0o700); err != nil {
		fmt.Fprintln(os.Stderr, "envd:", err)
		os.Exit(1)
	}
	sandbox, err := resolveSandbox(sandboxCmd, sandboxArgs, absRoot)
	if err != nil {
		fmt.Fprintln(os.Stderr, "envd:", err)
		os.Exit(2)
	}
	if len(sandbox) > 0 {
		fmt.Fprintf(os.Stderr, "envd: isolated launcher: %s\n", sandbox[0])
	}

	if embedded {
		if err := server.RunEmbedded(context.Background(), server.Options{
			Root:     absRoot,
			MaxFrame: maxFrame,
			Sandbox:  sandbox,
			NetAllow: netAllow,
		}); err != nil {
			fmt.Fprintln(os.Stderr, "envd:", err)
			os.Exit(1)
		}
		return
	}

	if token == "" {
		token = os.Getenv("ENVD_TOKEN")
	}
	if token == "" {
		fmt.Fprintln(os.Stderr, "envd: service mode requires --token or ENVD_TOKEN")
		os.Exit(2)
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	fmt.Fprintf(os.Stderr, "envd: service control listening on %s\n", listen)
	if dataListen != "" {
		fmt.Fprintf(os.Stderr, "envd: service data listening on %s\n", dataListen)
	}
	if err := server.RunService(ctx, server.ServiceOptions{
		Options: server.Options{
			Root:     absRoot,
			MaxFrame: maxFrame,
			Sandbox:  sandbox,
			NetAllow: netAllow,
		},
		Addr:     listen,
		DataAddr: dataListen,
		Token:    token,
		TLSCert:  tlsCert,
		TLSKey:   tlsKey,
	}); err != nil {
		fmt.Fprintln(os.Stderr, "envd:", err)
		os.Exit(1)
	}
}

// stringList collects repeated flag values in order.
type stringList []string

func (values *stringList) String() string {
	return strings.Join(*values, " ")
}

func (values *stringList) Set(value string) error {
	*values = append(*values, value)
	return nil
}

// resolveSandbox validates the deployment-provided launcher once at startup.
// An isolated request can never fall back to an unwrapped process.
func resolveSandbox(command string, args []string, dataRoot string) ([]string, error) {
	if command == "" {
		if len(args) > 0 {
			return nil, errors.New("--sandbox-arg requires --sandbox-command")
		}
		return nil, nil
	}
	path, err := exec.LookPath(command)
	if err != nil {
		return nil, fmt.Errorf("sandbox command %q: %w", command, err)
	}
	launch := make([]string, 0, len(args)+1)
	launch = append(launch, path)
	launch = append(launch, args...)
	if err := server.ValidateSandbox(launch, dataRoot); err != nil {
		return nil, err
	}
	return server.NormalizeSandbox(launch), nil
}
