package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
	"github.com/cynosure279/mitnick/tools/envd/internal/server"
)

func main() {
	var (
		embedded bool
		root     string
		maxFrame int
	)
	flag.BoolVar(&embedded, "embedded", false, "serve the frame protocol over stdin/stdout (M0)")
	flag.StringVar(&root, "root", ".msk-envd", "envd data root")
	flag.IntVar(&maxFrame, "max-frame", protocol.EmbeddedMaxFrame, "maximum frame size in bytes")
	flag.Parse()

	if !embedded {
		fmt.Fprintln(os.Stderr, "envd M0 only supports --embedded")
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
	if err := server.RunEmbedded(context.Background(), server.Options{
		Root:     absRoot,
		MaxFrame: maxFrame,
	}); err != nil {
		fmt.Fprintln(os.Stderr, "envd:", err)
		os.Exit(1)
	}
}
