package server

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"encoding/base64"
	"encoding/json"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"

	"github.com/cynosure279/mitnick/tools/envd/internal/protocol"
)

func analysisServer(t *testing.T) (*Server, string, string) {
	t.Helper()
	root := t.TempDir()
	s := New(Options{Root: root, MaxFrame: protocol.EmbeddedMaxFrame})
	created := mustTool(t, s, "env_create", `{"name":"analysis"}`)
	return s, created["env_id"].(string), created["workspace"].(string)
}

func analysisTool(t *testing.T, fn func(json.RawMessage) toolResult, args any) map[string]any {
	t.Helper()
	raw, err := json.Marshal(args)
	if err != nil {
		t.Fatal(err)
	}
	result := fn(raw)
	data, ok := decodeTool(t, result)
	if !ok {
		t.Fatalf("tool failed: %+v", result.StructuredContent)
	}
	return data
}

func analysisError(t *testing.T, fn func(json.RawMessage) toolResult, args any) map[string]any {
	t.Helper()
	raw, err := json.Marshal(args)
	if err != nil {
		t.Fatal(err)
	}
	return toolError(t, fn(raw))
}

func writeTestFile(t *testing.T, workspace, name string, data []byte) {
	t.Helper()
	path := filepath.Join(workspace, filepath.FromSlash(name))
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestToolFileHash(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "sample.txt", []byte("hello world"))

	data := analysisTool(t, s.toolFileHash,
		map[string]any{"env_id": envID, "path": "sample.txt"})
	if data["size"].(float64) != 11 {
		t.Fatalf("size = %v", data["size"])
	}
	hashes := data["hashes"].(map[string]any)
	const wantSHA256 = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
	if hashes["sha256"] != wantSHA256 {
		t.Fatalf("sha256 = %v", hashes["sha256"])
	}

	multi := analysisTool(t, s.toolFileHash, map[string]any{
		"env_id":     envID,
		"path":       "sample.txt",
		"algorithms": []string{"md5", "sha1", "sha256"},
	})
	multiHashes := multi["hashes"].(map[string]any)
	if multiHashes["md5"] != "5eb63bbbe01eeed093cb22bb8f5acdc3" {
		t.Fatalf("md5 = %v", multiHashes["md5"])
	}
	if multiHashes["sha1"] != "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed" {
		t.Fatalf("sha1 = %v", multiHashes["sha1"])
	}

	detail := analysisError(t, s.toolFileHash, map[string]any{
		"env_id": envID, "path": "sample.txt", "algorithms": []string{"crc32"},
	})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("unknown algorithm error = %v", detail)
	}
	detail = analysisError(t, s.toolFileHash, map[string]any{
		"env_id": envID, "path": "sample.txt", "max_bytes": 5,
	})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("max_bytes error = %v", detail)
	}
	detail = analysisError(t, s.toolFileHash, map[string]any{
		"env_id": envID, "path": "../outside.txt",
	})
	if detail["name"] != "PERMISSION_DENIED" {
		t.Fatalf("traversal error = %v", detail)
	}
}

func TestToolFileType(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "image.png",
		append([]byte{0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a}, make([]byte, 32)...))
	writeTestFile(t, workspace, "note.txt", []byte("hello plain text\n"))
	writeTestFile(t, workspace, "blob.b64", []byte(strings.Repeat("QUJD", 20)))
	writeTestFile(t, workspace, "prog.elf", []byte{0x7f, 'E', 'L', 'F', 2, 1, 1})
	writeTestFile(t, workspace, "bundle.zip", []byte("PK\x03\x04rest-of-file"))
	writeTestFile(t, workspace, "archive.tar",
		append(append(make([]byte, 257), []byte("ustar")...), make([]byte, 100)...))

	tests := []struct {
		path string
		kind string
		mime string
	}{
		{"image.png", "png", "image/png"},
		{"note.txt", "text", "text/plain"},
		{"blob.b64", "base64", "text/plain"},
		{"prog.elf", "elf", "application/octet-stream"},
		{"bundle.zip", "zip", "application/zip"},
		{"archive.tar", "tar", "application/octet-stream"},
	}
	for _, test := range tests {
		data := analysisTool(t, s.toolFileType,
			map[string]any{"env_id": envID, "path": test.path})
		if data["kind"] != test.kind {
			t.Errorf("%s kind = %v, want %s", test.path, data["kind"], test.kind)
		}
		mime := data["mime"].(string)
		if !strings.Contains(mime, test.mime) {
			t.Errorf("%s mime = %q, want %q", test.path, mime, test.mime)
		}
		if data["magic_hex"] == "" || data["magic_ascii"] == "" {
			t.Errorf("%s missing magic", test.path)
		}
	}

	detail := analysisError(t, s.toolFileType, map[string]any{"env_id": envID, "path": "."})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("directory error = %v", detail)
	}
}

func TestToolFileStrings(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	content := append([]byte("\x00HELLO\x00\x00"),
		[]byte("W\x00I\x00D\x00E\x00\x00\x00T\x00E\x00S\x00T\x00")...)
	writeTestFile(t, workspace, "strings.bin", content)

	data := analysisTool(t, s.toolFileStrings, map[string]any{
		"env_id": envID, "path": "strings.bin", "encoding": "both",
	})
	if data["count"].(float64) != 3 {
		t.Fatalf("count = %v (%v)", data["count"], data["strings"])
	}
	found := map[string]float64{}
	for _, item := range data["strings"].([]any) {
		entry := item.(map[string]any)
		found[entry["text"].(string)] = entry["offset"].(float64)
	}
	if found["HELLO"] != 1 {
		t.Fatalf("HELLO offset = %v", found["HELLO"])
	}
	if found["WIDE"] != 8 {
		t.Fatalf("WIDE offset = %v", found["WIDE"])
	}
	if found["TEST"] != 18 {
		t.Fatalf("TEST offset = %v", found["TEST"])
	}

	limited := analysisTool(t, s.toolFileStrings, map[string]any{
		"env_id": envID, "path": "strings.bin", "encoding": "both", "limit": 1,
	})
	if limited["count"].(float64) != 1 || limited["truncated"] != true {
		t.Fatalf("limit result = %v", limited)
	}

	short := analysisTool(t, s.toolFileStrings, map[string]any{
		"env_id": envID, "path": "strings.bin", "encoding": "both", "min_len": 8,
	})
	if short["count"].(float64) != 0 {
		t.Fatalf("min_len result = %v", short["strings"])
	}

	window := analysisTool(t, s.toolFileStrings, map[string]any{
		"env_id": envID, "path": "strings.bin", "offset": 1, "length": 5,
	})
	if window["count"].(float64) != 1 {
		t.Fatalf("window result = %v", window["strings"])
	}
	if window["strings"].([]any)[0].(map[string]any)["text"] != "HELLO" {
		t.Fatalf("window string = %v", window["strings"])
	}

	detail := analysisError(t, s.toolFileStrings, map[string]any{
		"env_id": envID, "path": "strings.bin", "encoding": "ebcdic",
	})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("encoding error = %v", detail)
	}
}

func TestToolFileHexdump(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "hello.txt", []byte("Hello, world!\n"))

	data := analysisTool(t, s.toolFileHexdump, map[string]any{
		"env_id": envID, "path": "hello.txt", "width": 8,
	})
	if data["length"].(float64) != 14 || data["eof"] != true {
		t.Fatalf("hexdump header = %v", data)
	}
	lines := data["lines"].([]any)
	if len(lines) != 2 {
		t.Fatalf("lines = %d", len(lines))
	}
	first := lines[0].(map[string]any)
	if first["offset"].(float64) != 0 || first["hex"] != "48 65 6c 6c 6f 2c 20 77" {
		t.Fatalf("first line = %v", first)
	}
	if first["ascii"] != "Hello, w" {
		t.Fatalf("first ascii = %v", first["ascii"])
	}
	second := lines[1].(map[string]any)
	if second["offset"].(float64) != 8 || second["ascii"] != "orld!." {
		t.Fatalf("second line = %v", second)
	}

	window := analysisTool(t, s.toolFileHexdump, map[string]any{
		"env_id": envID, "path": "hello.txt", "offset": 7, "length": 5, "width": 16,
	})
	if window["eof"] != false || window["length"].(float64) != 5 {
		t.Fatalf("window header = %v", window)
	}
	if window["lines"].([]any)[0].(map[string]any)["hex"] != "77 6f 72 6c 64" {
		t.Fatalf("window hex = %v", window["lines"])
	}
}

func TestToolFileEntropy(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "zeros.bin", make([]byte, 8192))
	pattern := make([]byte, 4096)
	for index := range pattern {
		pattern[index] = byte(index % 256)
	}
	writeTestFile(t, workspace, "pattern.bin", pattern)

	zeros := analysisTool(t, s.toolFileEntropy, map[string]any{
		"env_id": envID, "path": "zeros.bin",
	})
	if zeros["overall"].(float64) != 0 {
		t.Fatalf("zeros overall = %v", zeros["overall"])
	}
	if zeros["classification"] != "text/low entropy" {
		t.Fatalf("zeros classification = %v", zeros["classification"])
	}
	if len(zeros["chunks"].([]any)) != 2 {
		t.Fatalf("zeros chunks = %v", zeros["chunks"])
	}

	random := analysisTool(t, s.toolFileEntropy, map[string]any{
		"env_id": envID, "path": "pattern.bin", "chunk_size": 4096,
	})
	if random["overall"].(float64) != 8 {
		t.Fatalf("pattern overall = %v", random["overall"])
	}
	if random["classification"] != "likely packed/encrypted" {
		t.Fatalf("pattern classification = %v", random["classification"])
	}
	chunk := random["chunks"].([]any)[0].(map[string]any)
	if chunk["entropy"].(float64) != 8 || chunk["offset"].(float64) != 0 {
		t.Fatalf("pattern chunk = %v", chunk)
	}
}

func TestToolFileSearch(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "docs/a.txt", []byte("hello world\nsecond line\n"))
	writeTestFile(t, workspace, "docs/b.log", []byte("HELLO again\n"))
	writeTestFile(t, workspace, "docs/sub/c.go", []byte("package main\nfunc hello() {}\n"))
	writeTestFile(t, workspace, "docs/bin.dat",
		append([]byte("hello\x00"), bytes.Repeat([]byte{0}, 32)...))
	writeTestFile(t, workspace, ".hidden/secret.txt", []byte("hello hidden\n"))

	main := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "hello", "case_sensitive": true,
	})
	if main["count"].(float64) != 2 {
		t.Fatalf("case-sensitive matches = %v", main["matches"])
	}
	if main["files_walked"].(float64) != 4 {
		t.Fatalf("files_walked = %v", main["files_walked"])
	}
	paths := map[string]bool{}
	for _, item := range main["matches"].([]any) {
		entry := item.(map[string]any)
		paths[entry["path"].(string)] = true
		if entry["path"] == "docs/a.txt" {
			if entry["line"].(float64) != 1 || entry["column"].(float64) != 1 {
				t.Fatalf("a.txt position = %v", entry)
			}
		}
	}
	if !paths["docs/a.txt"] || !paths["docs/sub/c.go"] {
		t.Fatalf("paths = %v", paths)
	}

	insensitive := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "hello",
	})
	if insensitive["count"].(float64) != 3 {
		t.Fatalf("case-insensitive matches = %v", insensitive["matches"])
	}

	globbed := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "hello", "glob": "*.log",
	})
	if globbed["count"].(float64) != 1 {
		t.Fatalf("glob matches = %v", globbed["matches"])
	}

	regexed := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "h.llo", "regex": true,
	})
	if regexed["count"].(float64) != 3 {
		t.Fatalf("regex matches = %v", regexed["matches"])
	}

	named := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "a.txt", "mode": "name", "glob": "*.txt",
	})
	if named["count"].(float64) != 1 {
		t.Fatalf("name matches = %v", named["matches"])
	}

	capped := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "docs", "pattern": "hello", "max_results": 1,
	})
	if capped["count"].(float64) != 1 || capped["truncated"] != true {
		t.Fatalf("capped matches = %v", capped)
	}

	hiddenOnly := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": ".", "pattern": "hidden",
	})
	if hiddenOnly["count"].(float64) != 0 {
		t.Fatalf("hidden excluded matches = %v", hiddenOnly["matches"])
	}
	withHidden := analysisTool(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": ".", "pattern": "hidden", "include_hidden": true,
	})
	if withHidden["count"].(float64) != 1 {
		t.Fatalf("hidden included matches = %v", withHidden["matches"])
	}

	detail := analysisError(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "pattern": "[", "regex": true,
	})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("regex error = %v", detail)
	}
	detail = analysisError(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "pattern": "x", "glob": "[",
	})
	if detail["name"] != "INVALID_ARGUMENT" {
		t.Fatalf("glob error = %v", detail)
	}
	detail = analysisError(t, s.toolFileSearch, map[string]any{
		"env_id": envID, "root": "..", "pattern": "x",
	})
	if detail["name"] != "PERMISSION_DENIED" {
		t.Fatalf("root error = %v", detail)
	}
}

type zipTestEntry struct {
	name string
	data []byte
	mode os.FileMode
}

func writeTestZip(t *testing.T, path string, entries []zipTestEntry) {
	t.Helper()
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	writer := zip.NewWriter(file)
	for _, entry := range entries {
		header := &zip.FileHeader{Name: entry.name, Method: zip.Deflate}
		if entry.mode != 0 {
			header.SetMode(entry.mode)
		}
		body, err := writer.CreateHeader(header)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := body.Write(entry.data); err != nil {
			t.Fatal(err)
		}
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
}

type tarTestEntry struct {
	header tar.Header
	data   []byte
}

func writeTestTar(t *testing.T, path string, gzipped bool, entries []tarTestEntry) {
	t.Helper()
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	var sink io.Writer = file
	var gz *gzip.Writer
	if gzipped {
		gz = gzip.NewWriter(file)
		sink = gz
	}
	writer := tar.NewWriter(sink)
	for _, entry := range entries {
		header := entry.header
		if header.Size == 0 && len(entry.data) > 0 {
			header.Size = int64(len(entry.data))
		}
		if err := writer.WriteHeader(&header); err != nil {
			t.Fatal(err)
		}
		if len(entry.data) > 0 {
			if _, err := writer.Write(entry.data); err != nil {
				t.Fatal(err)
			}
		}
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	if gz != nil {
		if err := gz.Close(); err != nil {
			t.Fatal(err)
		}
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
}

func writeTestGzip(t *testing.T, path string, data []byte) {
	t.Helper()
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	writer := gzip.NewWriter(file)
	name := filepath.Base(path)
	if strings.EqualFold(filepath.Ext(name), ".gz") {
		name = strings.TrimSuffix(name, filepath.Ext(name))
	}
	writer.Name = name
	if _, err := writer.Write(data); err != nil {
		t.Fatal(err)
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestToolArchiveList(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestZip(t, filepath.Join(workspace, "bundle.zip"), []zipTestEntry{
		{name: "dir/one.txt", data: []byte("alpha")},
		{name: "two.txt", data: []byte("beta")},
	})
	writeTestTar(t, filepath.Join(workspace, "plain.tar"), false, []tarTestEntry{
		{header: tar.Header{Name: "dir/", Typeflag: tar.TypeDir, Mode: 0o755}},
		{header: tar.Header{Name: "one.txt", Typeflag: tar.TypeReg, Mode: 0o644}, data: []byte("alpha")},
	})
	writeTestTar(t, filepath.Join(workspace, "bundle.tar.gz"), true, []tarTestEntry{
		{header: tar.Header{Name: "one.txt", Typeflag: tar.TypeReg, Mode: 0o644}, data: []byte("alpha")},
	})
	writeTestGzip(t, filepath.Join(workspace, "payload.txt.gz"), []byte("hello gzip"))
	writeTestFile(t, workspace, "not-archive.bin", []byte("plain bytes"))

	zipData := analysisTool(t, s.toolArchiveList,
		map[string]any{"env_id": envID, "path": "bundle.zip"})
	if zipData["format"] != "zip" || zipData["count"].(float64) != 2 {
		t.Fatalf("zip list = %v", zipData)
	}
	if zipData["total_uncompressed"].(float64) != 9 {
		t.Fatalf("zip total = %v", zipData["total_uncompressed"])
	}

	tarData := analysisTool(t, s.toolArchiveList,
		map[string]any{"env_id": envID, "path": "plain.tar"})
	if tarData["format"] != "tar" || tarData["count"].(float64) != 2 {
		t.Fatalf("tar list = %v", tarData)
	}

	tarGzData := analysisTool(t, s.toolArchiveList,
		map[string]any{"env_id": envID, "path": "bundle.tar.gz"})
	if tarGzData["format"] != "tar.gz" || tarGzData["count"].(float64) != 1 {
		t.Fatalf("tar.gz list = %v", tarGzData)
	}

	gzipData := analysisTool(t, s.toolArchiveList,
		map[string]any{"env_id": envID, "path": "payload.txt.gz"})
	if gzipData["format"] != "gzip" || gzipData["count"].(float64) != 1 {
		t.Fatalf("gzip list = %v", gzipData)
	}
	entry := gzipData["entries"].([]any)[0].(map[string]any)
	if entry["name"] != "payload.txt" || entry["size"].(float64) != 10 {
		t.Fatalf("gzip entry = %v", entry)
	}

	detail := analysisError(t, s.toolArchiveList,
		map[string]any{"env_id": envID, "path": "not-archive.bin"})
	if detail["name"] != "UNSUPPORTED" {
		t.Fatalf("unsupported archive error = %v", detail)
	}
}

func TestToolArchiveExtract(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestZip(t, filepath.Join(workspace, "bundle.zip"), []zipTestEntry{
		{name: "dir/a.txt", data: []byte("alpha")},
		{name: "top.txt", data: []byte("beta")},
		{name: "../evil.txt", data: []byte("bad")},
		{name: "/abs.txt", data: []byte("bad")},
		{name: "link", data: []byte("target"), mode: os.ModeSymlink | 0o777},
	})

	data := analysisTool(t, s.toolArchiveExtract,
		map[string]any{"env_id": envID, "path": "bundle.zip", "dest": "extract"})
	if data["count"].(float64) != 2 || data["skipped"].(float64) != 3 {
		t.Fatalf("extract result = %v", data)
	}
	if data["total_bytes"].(float64) != 9 {
		t.Fatalf("extract total = %v", data["total_bytes"])
	}
	names := map[string]bool{}
	for _, item := range data["extracted"].([]any) {
		names[item.(map[string]any)["name"].(string)] = true
	}
	if !names["dir/a.txt"] || !names["top.txt"] {
		t.Fatalf("extracted names = %v", names)
	}
	if content, err := os.ReadFile(filepath.Join(workspace, "extract", "dir", "a.txt")); err != nil ||
		string(content) != "alpha" {
		t.Fatalf("dir/a.txt = %q, %v", content, err)
	}
	if _, err := os.Lstat(filepath.Join(workspace, "evil.txt")); !os.IsNotExist(err) {
		t.Fatalf("traversal wrote outside dest: %v", err)
	}
	if _, err := os.Lstat(filepath.Join(workspace, "abs.txt")); !os.IsNotExist(err) {
		t.Fatalf("absolute entry wrote outside dest: %v", err)
	}
	if _, err := os.Lstat(filepath.Join(workspace, "extract", "link")); !os.IsNotExist(err) {
		t.Fatalf("symlink entry was created: %v", err)
	}

	again := analysisTool(t, s.toolArchiveExtract,
		map[string]any{"env_id": envID, "path": "bundle.zip", "dest": "extract"})
	if again["count"].(float64) != 0 {
		t.Fatalf("overwrite=false extracted = %v", again["extracted"])
	}
	overwritten := analysisTool(t, s.toolArchiveExtract, map[string]any{
		"env_id": envID, "path": "bundle.zip", "dest": "extract", "overwrite": true,
	})
	if overwritten["count"].(float64) != 2 {
		t.Fatalf("overwrite=true extracted = %v", overwritten["extracted"])
	}

	capped := analysisTool(t, s.toolArchiveExtract, map[string]any{
		"env_id": envID, "path": "bundle.zip", "dest": "capped", "max_entries": 1,
	})
	if capped["count"].(float64) != 1 || capped["truncated"] != true {
		t.Fatalf("max_entries result = %v", capped)
	}

	writeTestZip(t, filepath.Join(workspace, "small.zip"), []zipTestEntry{
		{name: "a.txt", data: []byte("1234")},
		{name: "b.txt", data: []byte("56")},
	})
	limited := analysisTool(t, s.toolArchiveExtract, map[string]any{
		"env_id": envID, "path": "small.zip", "dest": "limited", "max_total_bytes": 5,
	})
	if limited["count"].(float64) != 1 || limited["truncated"] != true {
		t.Fatalf("max_total_bytes result = %v", limited)
	}
	if limited["total_bytes"].(float64) != 4 {
		t.Fatalf("max_total_bytes total = %v", limited["total_bytes"])
	}

	writeTestTar(t, filepath.Join(workspace, "bundle.tar"), false, []tarTestEntry{
		{header: tar.Header{Name: "t.txt", Typeflag: tar.TypeReg, Mode: 0o644}, data: []byte("tar")},
		{header: tar.Header{
			Name: "slink", Typeflag: tar.TypeSymlink, Linkname: "t.txt", Mode: 0o777,
		}},
	})
	tarData := analysisTool(t, s.toolArchiveExtract,
		map[string]any{"env_id": envID, "path": "bundle.tar", "dest": "tarout"})
	if tarData["count"].(float64) != 1 || tarData["skipped"].(float64) != 1 {
		t.Fatalf("tar extract = %v", tarData)
	}
	if _, err := os.Lstat(filepath.Join(workspace, "tarout", "slink")); !os.IsNotExist(err) {
		t.Fatalf("tar symlink entry was created: %v", err)
	}

	writeTestGzip(t, filepath.Join(workspace, "note.txt.gz"), []byte("gzip body"))
	gzipData := analysisTool(t, s.toolArchiveExtract,
		map[string]any{"env_id": envID, "path": "note.txt.gz", "dest": "gzout"})
	if gzipData["count"].(float64) != 1 {
		t.Fatalf("gzip extract = %v", gzipData)
	}
	extracted := gzipData["extracted"].([]any)[0].(map[string]any)
	if extracted["name"] != "note.txt" {
		t.Fatalf("gzip extracted name = %v", extracted["name"])
	}
	if content, err := os.ReadFile(filepath.Join(workspace, "gzout", "note.txt")); err != nil ||
		string(content) != "gzip body" {
		t.Fatalf("gzip content = %q, %v", content, err)
	}

	writeTestFile(t, workspace, "not-archive.bin", []byte("plain bytes"))
	detail := analysisError(t, s.toolArchiveExtract,
		map[string]any{"env_id": envID, "path": "not-archive.bin"})
	if detail["name"] != "UNSUPPORTED" {
		t.Fatalf("unsupported extract error = %v", detail)
	}
}

func TestToolBinaryInfo(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "text.txt", []byte("not a binary"))

	detail := analysisError(t, s.toolBinaryInfo,
		map[string]any{"env_id": envID, "path": "text.txt"})
	if detail["name"] != "UNSUPPORTED" {
		t.Fatalf("text binary info error = %v", detail)
	}

	executable, err := os.Executable()
	if err != nil {
		t.Skipf("os.Executable: %v", err)
	}
	target := filepath.Join(workspace, "sample.bin")
	if err := os.Link(executable, target); err != nil {
		source, openErr := os.Open(executable)
		if openErr != nil {
			t.Skipf("open executable: %v", openErr)
		}
		destination, createErr := os.Create(target)
		if createErr != nil {
			_ = source.Close()
			t.Skipf("create copy: %v", createErr)
		}
		if _, copyErr := io.Copy(destination, source); copyErr != nil {
			_ = source.Close()
			_ = destination.Close()
			t.Skipf("copy executable: %v", copyErr)
		}
		_ = source.Close()
		_ = destination.Close()
	}

	data := analysisTool(t, s.toolBinaryInfo,
		map[string]any{"env_id": envID, "path": "sample.bin"})
	format, _ := data["format"].(string)
	switch format {
	case "elf", "pe", "macho":
	default:
		t.Fatalf("format = %v", data["format"])
	}
	if data["arch"] == "" || data["bits"].(float64) == 0 {
		t.Fatalf("machine fields = %v", data)
	}
	if len(data["sections"].([]any)) == 0 {
		t.Fatalf("no sections: %v", data)
	}
	security := data["security"].(map[string]any)
	for _, key := range []string{
		"nx", "pie", "relro", "canary", "fortify", "aslr", "dynamic_base", "cfg", "integrity",
	} {
		if _, ok := security[key]; !ok {
			t.Fatalf("security missing %s: %v", key, security)
		}
	}
}

func TestToolSecretScan(t *testing.T) {
	s, envID, workspace := analysisServer(t)
	writeTestFile(t, workspace, "secrets.txt", []byte(strings.Join([]string{
		"aws = AKIAABCDEFGHIJKLMNOP",
		"jwt = eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghij",
		"password = supersecretvalue123",
		"authorization: Bearer abcdefghijklmnopqrstuvwxyz",
		"github = ghp_abcdefghijklmnopqrstuvwxyz123456",
		"slack = xoxb-1234567890-abcdefghij",
		"db = postgres://user:pass@example.com/db",
		"-----BEGIN RSA PRIVATE KEY-----",
	}, "\n")))
	writeTestFile(t, workspace, "binary.bin",
		append([]byte("\x00AKIAABCDEFGHIJKLMNOP"), bytes.Repeat([]byte{0}, 16)...))
	writeTestFile(t, workspace, ".hidden.txt", []byte("AKIAABCDEFGHIJKLMNOP"))

	data := analysisTool(t, s.toolSecretScan,
		map[string]any{"env_id": envID, "root": "."})
	if data["files_scanned"].(float64) != 1 {
		t.Fatalf("files_scanned = %v", data["files_scanned"])
	}
	patterns := map[string]bool{}
	for _, item := range data["findings"].([]any) {
		finding := item.(map[string]any)
		patterns[finding["pattern"].(string)] = true
		preview := finding["preview"].(string)
		for _, secret := range []string{
			"supersecretvalue123", "AKIAABCDEFGHIJKLMNOP",
			"abcdefghijklmnopqrstuvwxyz", "ghp_abcdefghijklmnopqrstuvwxyz123456",
		} {
			if strings.Contains(preview, secret) {
				t.Fatalf("preview leaked %q: %q", secret, preview)
			}
		}
	}
	for _, name := range []string{
		"aws_access_key", "jwt", "generic_secret", "bearer_token", "github_token",
		"slack_token", "connection_string", "pem_private_key",
	} {
		if !patterns[name] {
			t.Errorf("missing pattern %s in %v", name, patterns)
		}
	}

	writeTestFile(t, workspace, "second.txt", []byte("token = anothervalue123\n"))
	limited := analysisTool(t, s.toolSecretScan,
		map[string]any{"env_id": envID, "root": ".", "max_files": 1})
	if limited["files_scanned"].(float64) != 1 || limited["truncated"] != true {
		t.Fatalf("max_files result = %v", limited)
	}

	withHidden := analysisTool(t, s.toolSecretScan,
		map[string]any{"env_id": envID, "root": ".", "include_hidden": true})
	if withHidden["files_scanned"].(float64) < 2 {
		t.Fatalf("hidden scan files = %v", withHidden["files_scanned"])
	}
}

func TestToolTransform(t *testing.T) {
	s := New(Options{Root: t.TempDir(), MaxFrame: protocol.EmbeddedMaxFrame})

	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "base64_encode", "data": "hello"}); data["result"] != "aGVsbG8=" {
		t.Fatalf("base64_encode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "base64_decode", "data": "aGVsbG8="}); data["result"] != "hello" {
		t.Fatalf("base64_decode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "base64url_encode", "data": "a?b"}); data["result"] != "YT9i" {
		t.Fatalf("base64url_encode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "base64url_decode", "data": "YT9i"}); data["result"] != "a?b" {
		t.Fatalf("base64url_decode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "hex_encode", "data": "hi"}); data["result"] != "6869" {
		t.Fatalf("hex_encode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "hex_decode", "data": "6869"}); data["result"] != "hi" {
		t.Fatalf("hex_decode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "url_encode", "data": "a b&c"}); data["result"] != "a+b%26c" {
		t.Fatalf("url_encode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "url_decode", "data": "a+b%26c"}); data["result"] != "a b&c" {
		t.Fatalf("url_decode = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "html_escape", "data": "<a&b>"}); data["result"] != "&lt;a&amp;b&gt;" {
		t.Fatalf("html_escape = %v", data["result"])
	}
	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "html_unescape", "data": "&lt;a&amp;b&gt;"}); data["result"] != "<a&b>" {
		t.Fatalf("html_unescape = %v", data["result"])
	}

	hashed := analysisTool(t, s.toolTransform, map[string]any{
		"op": "hash", "data": "abc", "algorithms": []string{"md5", "sha256"},
	})
	hashes := hashed["hashes"].(map[string]any)
	if hashes["md5"] != "900150983cd24fb0d6963f7d28e17f72" {
		t.Fatalf("hash md5 = %v", hashes["md5"])
	}
	if hashes["sha256"] != "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad" {
		t.Fatalf("hash sha256 = %v", hashes["sha256"])
	}

	maced := analysisTool(t, s.toolTransform, map[string]any{
		"op":        "hmac",
		"data":      "The quick brown fox jumps over the lazy dog",
		"key":       "key",
		"algorithm": "sha256",
	})
	hmacInfo := maced["hmac"].(map[string]any)
	if hmacInfo["algorithm"] != "sha256" {
		t.Fatalf("hmac algorithm = %v", hmacInfo["algorithm"])
	}
	if hmacInfo["hex"] != "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8" {
		t.Fatalf("hmac hex = %v", hmacInfo["hex"])
	}

	header := `{"alg":"HS256","typ":"JWT"}`
	payload := `{"sub":"1234567890","name":"John Doe"}`
	token := base64.RawURLEncoding.EncodeToString([]byte(header)) + "." +
		base64.RawURLEncoding.EncodeToString([]byte(payload)) + ".signature"
	jwtData := analysisTool(t, s.toolTransform, map[string]any{
		"op": "jwt_decode", "data": token,
	})
	jwt := jwtData["jwt"].(map[string]any)
	if jwt["header"].(map[string]any)["alg"] != "HS256" {
		t.Fatalf("jwt header = %v", jwt["header"])
	}
	if jwt["payload"].(map[string]any)["sub"] != "1234567890" {
		t.Fatalf("jwt payload = %v", jwt["payload"])
	}
	if jwt["signature"] != "signature" {
		t.Fatalf("jwt signature = %v", jwt["signature"])
	}
	if !strings.Contains(jwtData["note"].(string), "not verified") {
		t.Fatalf("jwt note = %v", jwtData["note"])
	}

	compressed := analysisTool(t, s.toolTransform,
		map[string]any{"op": "gzip", "data": "hello gzip"})
	encoded := compressed["result_base64"].(string)
	if encoded == "" {
		t.Fatal("gzip produced no data")
	}
	restored := analysisTool(t, s.toolTransform,
		map[string]any{"op": "gunzip", "data_base64": encoded})
	if restored["result"] != "hello gzip" {
		t.Fatalf("gunzip = %v", restored["result"])
	}

	if data := analysisTool(t, s.toolTransform,
		map[string]any{"op": "rot13", "data": "Hello, World!"}); data["result"] != "Uryyb, Jbeyq!" {
		t.Fatalf("rot13 = %v", data["result"])
	}
	xored := analysisTool(t, s.toolTransform,
		map[string]any{"op": "xor", "data": "abc", "xor_key": "k"})
	if xored["result_base64"] != "CgkI" {
		t.Fatalf("xor = %v", xored["result_base64"])
	}

	uuidData := analysisTool(t, s.toolTransform, map[string]any{"op": "uuid"})
	uuidPattern := regexp.MustCompile(
		`^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`)
	if !uuidPattern.MatchString(uuidData["result"].(string)) {
		t.Fatalf("uuid = %v", uuidData["result"])
	}

	randomData := analysisTool(t, s.toolTransform,
		map[string]any{"op": "random_bytes", "length": 16})
	decoded, err := base64.StdEncoding.DecodeString(randomData["result_base64"].(string))
	if err != nil || len(decoded) != 16 {
		t.Fatalf("random_bytes = %v (%v)", randomData["result_base64"], err)
	}

	errors := []map[string]any{
		{"op": "nope"},
		{"op": "base64_decode", "data": "!!!"},
		{"op": "jwt_decode", "data": "not-a-jwt"},
		{"op": "gunzip", "data": "not gzip"},
		{"op": "random_bytes", "length": 0},
		{"op": "hash", "data": "x", "algorithm": "crc32"},
		{"op": "hmac", "data": "x", "key": "k", "algorithm": "md5"},
		{"op": "xor", "data": "x"},
	}
	for _, args := range errors {
		detail := analysisError(t, s.toolTransform, args)
		if detail["name"] != "INVALID_ARGUMENT" {
			t.Errorf("%v error = %v", args["op"], detail)
		}
	}
}
