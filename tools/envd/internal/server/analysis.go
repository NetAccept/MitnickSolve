package server

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/gzip"
	"crypto/hmac"
	"crypto/md5"
	"crypto/rand"
	"crypto/sha1"
	"crypto/sha256"
	"crypto/sha512"
	"debug/elf"
	"debug/macho"
	"debug/pe"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"hash"
	"html"
	"io"
	"io/fs"
	"math"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
	"unicode/utf8"
)

const (
	maxHashBytes    = int64(512 << 20)
	maxStringsBytes = int64(16 << 20)
	maxHexdumpBytes = int64(16 << 10)
	maxEntropyBytes = int64(4 << 20)
	maxSearchFile   = int64(8 << 20)
	maxSecretFile   = int64(4 << 20)
)

var errDecompressionBudget = errors.New("decompressed archive exceeds the byte budget")

// budgetReader stops a decompression stream at a fixed number of bytes. It is
// used for tar.gz and gzip members so a small archive cannot inflate for
// gigabytes while members are skipped or listed.
type budgetReader struct {
	reader    io.Reader
	remaining int64
}

func (b *budgetReader) Read(p []byte) (int, error) {
	if b.remaining <= 0 {
		return 0, errDecompressionBudget
	}
	if int64(len(p)) > b.remaining {
		p = p[:b.remaining]
	}
	n, err := b.reader.Read(p)
	b.remaining -= int64(n)
	return n, err
}

func cleanWorkspaceRelative(rel string) (string, error) {
	if rel == "" || rel == "." {
		return ".", nil
	}
	if filepath.IsAbs(rel) {
		return "", errors.New("absolute paths are not allowed")
	}
	clean := filepath.Clean(rel)
	if clean == ".." || strings.HasPrefix(clean, ".."+string(os.PathSeparator)) {
		return "", errors.New("path escapes the workspace")
	}
	return clean, nil
}

// envFilePath resolves a path inside the environment workspace without opening
// it. The path is verified through os.Root before it is returned; callers that
// need race-free access should prefer openEnvFile.
func (s *Server) envFilePath(envID, rel string) (string, toolResult, bool) {
	if rel == "" {
		return "", errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is required"), false
	}
	env, err := s.getEnv(envID)
	if err != nil {
		return "", errResult(codeNotFound, "NOT_FOUND", err.Error()), false
	}
	clean, err := cleanWorkspaceRelative(rel)
	if err != nil {
		return "", errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	root, err := os.OpenRoot(env.Workspace)
	if err != nil {
		return "", errResult(codeInternal, "INTERNAL", err.Error()), false
	}
	info, err := root.Stat(clean)
	_ = root.Close()
	if err != nil {
		if os.IsNotExist(err) {
			return "", errResult(codeNotFound, "NOT_FOUND", err.Error()), false
		}
		return "", errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	if !info.Mode().IsRegular() {
		return "", errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"path is not a regular file"), false
	}
	return filepath.Join(env.Workspace, clean), toolResult{}, true
}

// openEnvFile opens a regular file inside an environment workspace. os.Root
// rejects symlink/junction escapes and reserved device names; Stat before Open
// rejects FIFOs, sockets and devices without blocking on them.
func (s *Server) openEnvFile(envID, rel string) (*os.File, os.FileInfo, toolResult, bool) {
	if rel == "" {
		return nil, nil, errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is required"), false
	}
	env, err := s.getEnv(envID)
	if err != nil {
		return nil, nil, errResult(codeNotFound, "NOT_FOUND", err.Error()), false
	}
	clean, err := cleanWorkspaceRelative(rel)
	if err != nil {
		return nil, nil, errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	root, err := os.OpenRoot(env.Workspace)
	if err != nil {
		return nil, nil, errResult(codeInternal, "INTERNAL", err.Error()), false
	}
	defer root.Close()
	info, err := root.Stat(clean)
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil, errResult(codeNotFound, "NOT_FOUND", err.Error()), false
		}
		return nil, nil, errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	if !info.Mode().IsRegular() {
		return nil, nil, errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"path is not a regular file"), false
	}
	file, err := root.Open(clean)
	if err != nil {
		return nil, nil, errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	return file, info, toolResult{}, true
}

// readFileRange reads at most length bytes starting at offset. A short read at
// EOF is not an error.
func readFileRange(file *os.File, offset, length int64) ([]byte, error) {
	if offset > 0 {
		if _, err := file.Seek(offset, io.SeekStart); err != nil {
			return nil, err
		}
	}
	if length < 0 {
		length = 0
	}
	buf := make([]byte, length)
	n, err := io.ReadFull(file, buf)
	if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
		return nil, err
	}
	return buf[:n], nil
}

func clampInt(value, fallback, min, max int) int {
	if value <= 0 {
		return fallback
	}
	if value < min {
		return min
	}
	if value > max {
		return max
	}
	return value
}

func clampInt64(value, fallback, min, max int64) int64 {
	if value <= 0 {
		return fallback
	}
	if value < min {
		return min
	}
	if value > max {
		return max
	}
	return value
}

// prepareEnvDir opens the workspace as an os.Root and creates a relative
// destination directory through it. Root methods reject symlink/junction
// escapes, Windows device names and traversal, so callers can create and open
// extraction targets without a separate path check.
func (s *Server) prepareEnvDir(envID, rel string) (*os.Root, string, toolResult, bool) {
	env, err := s.getEnv(envID)
	if err != nil {
		return nil, "", errResult(codeNotFound, "NOT_FOUND", err.Error()), false
	}
	clean, err := cleanWorkspaceRelative(rel)
	if err != nil {
		return nil, "", errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	root, err := os.OpenRoot(env.Workspace)
	if err != nil {
		return nil, "", errResult(codeInternal, "INTERNAL", err.Error()), false
	}
	if clean != "." {
		if err := root.MkdirAll(clean, 0o700); err != nil {
			_ = root.Close()
			return nil, "", errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
		}
	}
	info, err := root.Stat(clean)
	if err != nil || !info.IsDir() {
		_ = root.Close()
		if err == nil {
			err = errors.New("destination is not a directory")
		}
		return nil, "", errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error()), false
	}
	return root, clean, toolResult{}, true
}

// toolFileHash streams a file through one or more digest algorithms.
func (s *Server) toolFileHash(raw json.RawMessage) toolResult {
	var args struct {
		EnvID      string   `json:"env_id"`
		Path       string   `json:"path"`
		Algorithms []string `json:"algorithms"`
		MaxBytes   int64    `json:"max_bytes"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Path == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is required")
	}
	if args.MaxBytes == 0 {
		args.MaxBytes = maxHashBytes
	}
	if args.MaxBytes < 1 || args.MaxBytes > maxHashBytes {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			fmt.Sprintf("max_bytes must be between 1 and %d", maxHashBytes))
	}
	if len(args.Algorithms) == 0 {
		args.Algorithms = []string{"sha256"}
	}

	hashers := make(map[string]hash.Hash, len(args.Algorithms))
	writers := make([]io.Writer, 0, len(args.Algorithms))
	for _, name := range args.Algorithms {
		algorithm := strings.ToLower(strings.TrimSpace(name))
		if _, exists := hashers[algorithm]; exists {
			continue
		}
		var digest hash.Hash
		switch algorithm {
		case "md5":
			digest = md5.New()
		case "sha1":
			digest = sha1.New()
		case "sha256":
			digest = sha256.New()
		case "sha512":
			digest = sha512.New()
		default:
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"unknown hash algorithm: "+name)
		}
		hashers[algorithm] = digest
		writers = append(writers, digest)
	}

	file, info, res, ok := s.openEnvFile(args.EnvID, args.Path)
	if !ok {
		return res
	}
	defer file.Close()
	if info.Size() > args.MaxBytes {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			fmt.Sprintf("file size %d exceeds max_bytes %d", info.Size(), args.MaxBytes))
	}

	started := time.Now()
	size, err := io.Copy(io.MultiWriter(writers...), io.LimitReader(file, args.MaxBytes+1))
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	if size > args.MaxBytes {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			fmt.Sprintf("file grew beyond max_bytes %d while hashing", args.MaxBytes))
	}
	hashes := make(map[string]string, len(hashers))
	for algorithm, digest := range hashers {
		hashes[algorithm] = hex.EncodeToString(digest.Sum(nil))
	}

	return okResult(fmt.Sprintf("hashed %d bytes", size), map[string]any{
		"path":        args.Path,
		"size":        size,
		"hashes":      hashes,
		"duration_ms": time.Since(started).Milliseconds(),
	})
}

// toolFileType identifies a file from its first 4 KiB.
func (s *Server) toolFileType(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	file, info, res, ok := s.openEnvFile(args.EnvID, args.Path)
	if !ok {
		return res
	}
	defer file.Close()

	head := make([]byte, 4096)
	n, err := io.ReadFull(file, head)
	if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	head = head[:n]

	magic := head
	if len(magic) > 16 {
		magic = magic[:16]
	}
	kind := detectFileKind(head)
	return okResult(kind, map[string]any{
		"path":        args.Path,
		"size":        info.Size(),
		"mime":        http.DetectContentType(head),
		"magic_hex":   hex.EncodeToString(magic),
		"magic_ascii": magicASCII(magic),
		"kind":        kind,
	})
}

func magicASCII(data []byte) string {
	var builder strings.Builder
	for _, b := range data {
		if b >= 0x20 && b <= 0x7e {
			builder.WriteByte(b)
		} else {
			builder.WriteByte('.')
		}
	}
	return builder.String()
}

func detectFileKind(head []byte) string {
	switch {
	case bytes.HasPrefix(head, []byte{0x7f, 'E', 'L', 'F'}):
		return "elf"
	case bytes.HasPrefix(head, []byte{'M', 'Z'}):
		return "pe"
	case bytes.HasPrefix(head, []byte{0xfe, 0xed, 0xfa, 0xce}),
		bytes.HasPrefix(head, []byte{0xfe, 0xed, 0xfa, 0xcf}),
		bytes.HasPrefix(head, []byte{0xce, 0xfa, 0xed, 0xfe}),
		bytes.HasPrefix(head, []byte{0xcf, 0xfa, 0xed, 0xfe}),
		bytes.HasPrefix(head, []byte{0xca, 0xfe, 0xba, 0xbe}),
		bytes.HasPrefix(head, []byte{0xbe, 0xba, 0xfe, 0xca}):
		return "macho"
	case bytes.HasPrefix(head, []byte("PK\x03\x04")),
		bytes.HasPrefix(head, []byte("PK\x05\x06")),
		bytes.HasPrefix(head, []byte("PK\x07\x08")):
		return "zip"
	case bytes.HasPrefix(head, []byte{0x1f, 0x8b}):
		return "gzip"
	case len(head) >= 262 && string(head[257:262]) == "ustar":
		return "tar"
	case bytes.HasPrefix(head, []byte{'7', 'z', 0xbc, 0xaf, 0x27, 0x1c}):
		return "7z"
	case bytes.HasPrefix(head, []byte{'R', 'a', 'r', '!', 0x1a, 0x07}):
		return "rar"
	case bytes.HasPrefix(head, []byte("%PDF-")):
		return "pdf"
	case bytes.HasPrefix(head, []byte{0x89, 'P', 'N', 'G', 0x0d, 0x0a, 0x1a, 0x0a}):
		return "png"
	case bytes.HasPrefix(head, []byte{0xff, 0xd8, 0xff}):
		return "jpeg"
	case bytes.HasPrefix(head, []byte("GIF87a")), bytes.HasPrefix(head, []byte("GIF89a")):
		return "gif"
	case bytes.HasPrefix(head, []byte("SQLite format 3\x00")):
		return "sqlite"
	case bytes.HasPrefix(head, []byte{0x00, 0x61, 0x73, 0x6d}):
		return "wasm"
	case looksBase64(head):
		return "base64"
	case len(head) > 0 && utf8.Valid(head) && !bytes.ContainsRune(head, '\x00'):
		return "text"
	default:
		return "unknown"
	}
}

func looksBase64(head []byte) bool {
	if len(head) < 24 {
		return false
	}
	seen := 0
	for _, c := range head {
		switch {
		case c == '\r' || c == '\n' || c == ' ' || c == '\t':
			continue
		case (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') ||
			(c >= '0' && c <= '9') || c == '+' || c == '/' || c == '=':
			seen++
		default:
			return false
		}
	}
	return seen >= 24
}

type fileStringHit struct {
	offset int64
	text   string
}

// toolFileStrings extracts printable ASCII and UTF-16LE runs from a file.
func (s *Server) toolFileStrings(raw json.RawMessage) toolResult {
	var args struct {
		EnvID    string `json:"env_id"`
		Path     string `json:"path"`
		MinLen   int    `json:"min_len"`
		Encoding string `json:"encoding"`
		Limit    int    `json:"limit"`
		Offset   int64  `json:"offset"`
		Length   int64  `json:"length"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Offset < 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "offset must be non-negative")
	}
	encoding := strings.ToLower(strings.TrimSpace(args.Encoding))
	if encoding == "" {
		encoding = "ascii"
	}
	switch encoding {
	case "ascii", "utf16le", "both":
	default:
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"encoding must be ascii, utf16le, or both")
	}
	minLen := clampInt(args.MinLen, 4, 4, 64)
	limit := clampInt(args.Limit, 200, 1, 500)
	length := clampInt64(args.Length, 4<<20, 1, maxStringsBytes)

	file, _, res, ok := s.openEnvFile(args.EnvID, args.Path)
	if !ok {
		return res
	}
	defer file.Close()
	data, err := readFileRange(file, args.Offset, length)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	hits := make([]fileStringHit, 0)
	if encoding == "ascii" || encoding == "both" {
		hits = append(hits, findASCIIStrings(data, minLen, limit)...)
	}
	if encoding == "utf16le" || encoding == "both" {
		hits = append(hits, findUTF16LEStrings(data, minLen, limit)...)
	}
	sort.SliceStable(hits, func(i, j int) bool {
		return hits[i].offset < hits[j].offset
	})

	matched := make([]map[string]any, 0, minInt(len(hits), limit))
	truncated := len(hits) > limit
	for _, hit := range hits {
		if len(matched) >= limit {
			break
		}
		text := hit.text
		if len(text) > 256 {
			text = text[:256] + "…"
		}
		matched = append(matched, map[string]any{
			"offset": args.Offset + hit.offset,
			"text":   text,
		})
	}

	return okResult(fmt.Sprintf("%d strings", len(matched)), map[string]any{
		"strings":   matched,
		"count":     len(matched),
		"truncated": truncated,
	})
}

func findASCIIStrings(data []byte, minLen, limit int) []fileStringHit {
	hits := make([]fileStringHit, 0, minInt(limit, 1024))
	start := -1
	for i := 0; i <= len(data); i++ {
		printable := i < len(data) && (data[i] == '\t' || (data[i] >= 0x20 && data[i] <= 0x7e))
		if printable {
			if start < 0 {
				start = i
			}
			continue
		}
		if start >= 0 {
			if i-start >= minLen {
				hits = append(hits, fileStringHit{offset: int64(start), text: string(data[start:i])})
				if len(hits) >= limit {
					return hits
				}
			}
			start = -1
		}
	}
	return hits
}

func findUTF16LEStrings(data []byte, minLen, limit int) []fileStringHit {
	hits := make([]fileStringHit, 0, minInt(limit, 1024))
	i := 0
	for i+1 < len(data) {
		low, high := data[i], data[i+1]
		if high == 0 && (low == '\t' || (low >= 0x20 && low <= 0x7e)) {
			start := i
			var builder strings.Builder
			for i+1 < len(data) && data[i+1] == 0 &&
				(data[i] == '\t' || (data[i] >= 0x20 && data[i] <= 0x7e)) {
				builder.WriteByte(data[i])
				i += 2
			}
			if builder.Len() >= minLen {
				hits = append(hits, fileStringHit{offset: int64(start), text: builder.String()})
				if len(hits) >= limit {
					return hits
				}
			}
			continue
		}
		i++
	}
	return hits
}

func minInt(a, b int) int {
	if a < b {
		return a
	}
	return b
}

// toolFileHexdump renders a byte range as canonical 16-column rows.
func (s *Server) toolFileHexdump(raw json.RawMessage) toolResult {
	var args struct {
		EnvID  string `json:"env_id"`
		Path   string `json:"path"`
		Offset int64  `json:"offset"`
		Length int64  `json:"length"`
		Width  int    `json:"width"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Offset < 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "offset must be non-negative")
	}
	length := clampInt64(args.Length, 4096, 1, maxHexdumpBytes)
	width := clampInt(args.Width, 16, 8, 32)

	file, info, res, ok := s.openEnvFile(args.EnvID, args.Path)
	if !ok {
		return res
	}
	defer file.Close()
	data, err := readFileRange(file, args.Offset, length)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	lines := make([]map[string]any, 0, (len(data)+width-1)/width)
	for start := 0; start < len(data); start += width {
		end := start + width
		if end > len(data) {
			end = len(data)
		}
		chunk := data[start:end]
		hexParts := make([]string, 0, len(chunk))
		var ascii strings.Builder
		for _, b := range chunk {
			hexParts = append(hexParts, fmt.Sprintf("%02x", b))
			if b >= 0x20 && b <= 0x7e {
				ascii.WriteByte(b)
			} else {
				ascii.WriteByte('.')
			}
		}
		lines = append(lines, map[string]any{
			"offset": args.Offset + int64(start),
			"hex":    strings.Join(hexParts, " "),
			"ascii":  ascii.String(),
		})
	}

	return okResult(fmt.Sprintf("%d bytes", len(data)), map[string]any{
		"offset": args.Offset,
		"length": len(data),
		"eof":    args.Offset+int64(len(data)) >= info.Size(),
		"lines":  lines,
	})
}

// toolFileEntropy computes Shannon entropy per chunk and overall.
func (s *Server) toolFileEntropy(raw json.RawMessage) toolResult {
	var args struct {
		EnvID     string `json:"env_id"`
		Path      string `json:"path"`
		Offset    int64  `json:"offset"`
		Length    int64  `json:"length"`
		ChunkSize int    `json:"chunk_size"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Offset < 0 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "offset must be non-negative")
	}
	length := clampInt64(args.Length, 256<<10, 1, maxEntropyBytes)
	chunkSize := clampInt(args.ChunkSize, 4096, 4096, 65536)

	file, info, res, ok := s.openEnvFile(args.EnvID, args.Path)
	if !ok {
		return res
	}
	defer file.Close()
	data, err := readFileRange(file, args.Offset, length)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}

	chunks := make([]map[string]any, 0, (len(data)+chunkSize-1)/chunkSize)
	minimum, maximum := 0.0, 0.0
	for start := 0; start < len(data); start += chunkSize {
		end := start + chunkSize
		if end > len(data) {
			end = len(data)
		}
		value := roundEntropy(shannonEntropy(data[start:end]))
		if len(chunks) == 0 {
			minimum, maximum = value, value
		} else {
			if value < minimum {
				minimum = value
			}
			if value > maximum {
				maximum = value
			}
		}
		chunks = append(chunks, map[string]any{
			"offset":  args.Offset + int64(start),
			"entropy": value,
		})
	}

	overall := shannonEntropy(data)
	classification := "mixed"
	switch {
	case overall >= 7.2:
		classification = "likely packed/encrypted"
	case overall <= 3.5:
		classification = "text/low entropy"
	}

	return okResult(classification, map[string]any{
		"size":           info.Size(),
		"overall":        roundEntropy(overall),
		"min":            minimum,
		"max":            maximum,
		"chunks":         chunks,
		"classification": classification,
	})
}

func shannonEntropy(data []byte) float64 {
	if len(data) == 0 {
		return 0
	}
	var counts [256]int
	for _, b := range data {
		counts[b]++
	}
	total := float64(len(data))
	entropy := 0.0
	for _, count := range counts {
		if count == 0 {
			continue
		}
		probability := float64(count) / total
		entropy -= probability * math.Log2(probability)
	}
	return entropy
}

func roundEntropy(value float64) float64 {
	return math.Round(value*10000) / 10000
}

// toolFileSearch walks the workspace and matches file names or text content.
func (s *Server) toolFileSearch(raw json.RawMessage) toolResult {
	var args struct {
		EnvID          string `json:"env_id"`
		Root           string `json:"root"`
		Pattern        string `json:"pattern"`
		Mode           string `json:"mode"`
		Glob           string `json:"glob"`
		CaseSensitive  bool   `json:"case_sensitive"`
		Regex          bool   `json:"regex"`
		MaxResults     int    `json:"max_results"`
		MaxFileBytes   int64  `json:"max_file_bytes"`
		IncludeHidden  bool   `json:"include_hidden"`
		MaxFilesWalked int    `json:"max_files_walked"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Pattern == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "pattern is required")
	}
	if len(args.Pattern) > 512 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "pattern exceeds 512 characters")
	}
	mode := strings.ToLower(strings.TrimSpace(args.Mode))
	if mode == "" {
		mode = "content"
	}
	if mode != "name" && mode != "content" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "mode must be name or content")
	}
	if args.Root == "" {
		args.Root = "."
	}
	if args.Glob != "" {
		if _, err := filepath.Match(args.Glob, ""); err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "invalid glob: "+err.Error())
		}
	}
	maxResults := clampInt(args.MaxResults, 100, 1, 500)
	maxFileBytes := clampInt64(args.MaxFileBytes, 1<<20, 1, maxSearchFile)
	maxFilesWalked := clampInt(args.MaxFilesWalked, 10000, 1, 100000)

	var matcher *regexp.Regexp
	needle := args.Pattern
	if args.Regex {
		if len(args.Pattern) > 256 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "regex exceeds 256 characters")
		}
		expression := args.Pattern
		if !args.CaseSensitive {
			expression = "(?i)" + expression
		}
		compiled, err := regexp.Compile(expression)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "invalid regex: "+err.Error())
		}
		matcher = compiled
	} else if !args.CaseSensitive {
		needle = strings.ToLower(needle)
	}

	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	cleanRoot, err := cleanWorkspaceRelative(args.Root)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	root, err := os.OpenRoot(env.Workspace)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	defer root.Close()
	rootFS := root.FS()
	if _, err := fs.Stat(rootFS, cleanRoot); err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}

	matches := make([]map[string]any, 0, maxResults)
	truncated := false
	filesWalked := 0

	walkErr := fs.WalkDir(rootFS, cleanRoot, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			if path == cleanRoot {
				return walkErr
			}
			return nil
		}
		if path == cleanRoot && entry.IsDir() {
			return nil
		}
		relative := path
		name := entry.Name()

		if !args.IncludeHidden && path != cleanRoot && strings.HasPrefix(name, ".") {
			if entry.IsDir() {
				return fs.SkipDir
			}
			return nil
		}
		if entry.Type()&fs.ModeSymlink != 0 {
			if entry.IsDir() {
				return fs.SkipDir
			}
			return nil
		}
		if !entry.IsDir() {
			if filesWalked >= maxFilesWalked {
				truncated = true
				return fs.SkipAll
			}
			filesWalked++
		}

		if mode == "name" {
			if !matchWorkspaceGlob(args.Glob, relative, name) {
				return nil
			}
			if !nameMatches(name, matcher, needle, args.CaseSensitive) {
				return nil
			}
			size := int64(0)
			if info, err := entry.Info(); err == nil {
				size = info.Size()
			}
			matches = append(matches, map[string]any{
				"path":    relative,
				"line":    0,
				"column":  0,
				"preview": "",
				"size":    size,
			})
			if len(matches) >= maxResults {
				truncated = true
				return fs.SkipAll
			}
			return nil
		}

		if entry.IsDir() || entry.Type()&fs.ModeType != 0 {
			return nil
		}
		if !matchWorkspaceGlob(args.Glob, relative, name) {
			return nil
		}
		file, err := root.Open(filepath.FromSlash(path))
		if err != nil {
			return nil
		}
		data, readErr := io.ReadAll(io.LimitReader(file, maxFileBytes))
		_ = file.Close()
		if readErr != nil {
			return nil
		}
		head := data
		if len(head) > 8<<10 {
			head = head[:8<<10]
		}
		if bytes.IndexByte(head, 0) >= 0 {
			return nil
		}

		size := int64(len(data))
		if info, err := entry.Info(); err == nil {
			size = info.Size()
		}
		for index, lineBytes := range bytes.Split(data, []byte{'\n'}) {
			line := string(lineBytes)
			column, found := findMatch(line, matcher, needle, args.CaseSensitive)
			if !found {
				continue
			}
			preview := strings.TrimSpace(line)
			preview = strings.ToValidUTF8(preview, "?")
			if len(preview) > 120 {
				preview = preview[:120]
			}
			matches = append(matches, map[string]any{
				"path":    relative,
				"line":    index + 1,
				"column":  column + 1,
				"preview": preview,
				"size":    size,
			})
			if len(matches) >= maxResults {
				truncated = true
				return fs.SkipAll
			}
		}
		return nil
	})
	if walkErr != nil {
		return errResult(codeInternal, "INTERNAL", walkErr.Error())
	}

	return okResult(fmt.Sprintf("%d matches in %d files", len(matches), filesWalked),
		map[string]any{
			"matches":      matches,
			"count":        len(matches),
			"truncated":    truncated,
			"files_walked": filesWalked,
		})
}

func matchWorkspaceGlob(glob, relative, name string) bool {
	if glob == "" {
		return true
	}
	if matched, err := filepath.Match(glob, relative); err == nil && matched {
		return true
	}
	if matched, err := filepath.Match(glob, name); err == nil && matched {
		return true
	}
	return false
}

func nameMatches(name string, matcher *regexp.Regexp, needle string, caseSensitive bool) bool {
	_, found := findMatch(name, matcher, needle, caseSensitive)
	return found
}

// findMatch returns the byte offset of the first match in value.
func findMatch(value string, matcher *regexp.Regexp, needle string, caseSensitive bool) (int, bool) {
	if matcher != nil {
		location := matcher.FindStringIndex(value)
		if location == nil {
			return 0, false
		}
		return location[0], true
	}
	if caseSensitive {
		index := strings.Index(value, needle)
		return index, index >= 0
	}
	index := strings.Index(strings.ToLower(value), needle)
	return index, index >= 0
}

// toolArchiveList lists zip, tar, tar.gz, and single-entry gzip archives.
func (s *Server) toolArchiveList(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	full, res, ok := s.envFilePath(args.EnvID, args.Path)
	if !ok {
		return res
	}
	info, err := os.Stat(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if info.IsDir() {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is a directory")
	}
	format, err := detectArchiveFormat(full)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	if format == "" {
		return errResult(codeUnsupported, "UNSUPPORTED",
			"unsupported archive format: only zip, tar, tar.gz, and gz are supported")
	}

	entries := make([]map[string]any, 0)
	total := int64(0)
	truncated := false

	switch format {
	case "zip":
		reader, err := zip.OpenReader(full)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open zip: "+err.Error())
		}
		defer reader.Close()
		for _, entry := range reader.File {
			if len(entries) >= 1000 {
				truncated = true
				break
			}
			isDir := entry.FileInfo().IsDir() || strings.HasSuffix(entry.Name, "/")
			size := int64(entry.UncompressedSize64)
			entries = append(entries, map[string]any{
				"name":            entry.Name,
				"size":            size,
				"compressed_size": int64(entry.CompressedSize64),
				"mode":            entry.Mode().String(),
				"modified":        entry.ModTime().UTC().Format(time.RFC3339),
				"is_dir":          isDir,
			})
			total += size
		}
	case "tar", "tar.gz":
		var source io.Reader
		var gz *gzip.Reader
		file, err := os.Open(full)
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		defer file.Close()
		if format == "tar.gz" {
			gz, err = gzip.NewReader(file)
			if err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open gzip: "+err.Error())
			}
			defer gz.Close()
			source = &budgetReader{reader: gz, remaining: 128 << 20}
		} else {
			source = file
		}
		tarReader := tar.NewReader(source)
		for {
			header, err := tarReader.Next()
			if errors.Is(err, io.EOF) {
				break
			}
			if errors.Is(err, errDecompressionBudget) {
				return errResult(codeResourceExhausted, "RESOURCE_EXHAUSTED",
					"decompressed archive exceeds 128 MiB")
			}
			if err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
					"read tar: "+err.Error())
			}
			if len(entries) >= 1000 {
				truncated = true
				break
			}
			isDir := header.Typeflag == tar.TypeDir
			entries = append(entries, map[string]any{
				"name":            header.Name,
				"size":            header.Size,
				"compressed_size": int64(0),
				"mode":            header.FileInfo().Mode().String(),
				"modified":        header.ModTime.UTC().Format(time.RFC3339),
				"is_dir":          isDir,
			})
			total += header.Size
		}
	case "gzip":
		file, err := os.Open(full)
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		defer file.Close()
		reader, err := gzip.NewReader(file)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open gzip: "+err.Error())
		}
		defer reader.Close()
		modified := reader.ModTime
		if modified.IsZero() {
			modified = info.ModTime()
		}
		size := gzipISIZE(file, info.Size())
		entries = append(entries, map[string]any{
			"name":            gzipEntryName(full, reader.Name),
			"size":            size,
			"compressed_size": info.Size(),
			"mode":            os.FileMode(0o644).String(),
			"modified":        modified.UTC().Format(time.RFC3339),
			"is_dir":          false,
		})
		total = size
	}

	return okResult(fmt.Sprintf("%s: %d entries", format, len(entries)), map[string]any{
		"format":             format,
		"entries":            entries,
		"count":              len(entries),
		"total_uncompressed": total,
		"truncated":          truncated,
	})
}

func detectArchiveFormat(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	head := make([]byte, 512)
	n, err := io.ReadFull(file, head)
	if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
		return "", err
	}
	head = head[:n]
	switch {
	case bytes.HasPrefix(head, []byte("PK\x03\x04")),
		bytes.HasPrefix(head, []byte("PK\x05\x06")),
		bytes.HasPrefix(head, []byte("PK\x07\x08")):
		return "zip", nil
	case bytes.HasPrefix(head, []byte{0x1f, 0x8b}):
		tarred, err := gzipContainsTar(path)
		if err != nil {
			return "", err
		}
		if tarred {
			return "tar.gz", nil
		}
		return "gzip", nil
	case isTarHeader(head):
		return "tar", nil
	default:
		return "", nil
	}
}

func isTarHeader(head []byte) bool {
	return len(head) >= 262 && string(head[257:262]) == "ustar"
}

func gzipContainsTar(path string) (bool, error) {
	file, err := os.Open(path)
	if err != nil {
		return false, err
	}
	defer file.Close()
	reader, err := gzip.NewReader(file)
	if err != nil {
		return false, nil
	}
	defer reader.Close()
	head := make([]byte, 512)
	n, err := io.ReadFull(reader, head)
	if err != nil && !errors.Is(err, io.EOF) && !errors.Is(err, io.ErrUnexpectedEOF) {
		return false, nil
	}
	return isTarHeader(head[:n]), nil
}

func gzipISIZE(file *os.File, size int64) int64 {
	if size < 4 {
		return 0
	}
	var footer [4]byte
	if _, err := file.ReadAt(footer[:], size-4); err != nil {
		return 0
	}
	return int64(binary.LittleEndian.Uint32(footer[:]))
}

func gzipEntryName(path, headerName string) string {
	if headerName != "" {
		cleaned := strings.ReplaceAll(headerName, "\\", "/")
		if index := strings.LastIndexByte(cleaned, '/'); index >= 0 {
			cleaned = cleaned[index+1:]
		}
		if cleaned != "" {
			return cleaned
		}
	}
	base := filepath.Base(path)
	if strings.EqualFold(filepath.Ext(base), ".gz") {
		base = strings.TrimSuffix(base, filepath.Ext(base))
	}
	return base
}

// toolArchiveExtract writes archive members into the workspace. This is a
// risky tool: it can create directories and run the attacker-controlled
// uncompressed size of a malicious archive against the destination tree, so it
// enforces zip-slip rejection, a symlink/device skip policy, and hard entry and
// byte limits.
func (s *Server) toolArchiveExtract(raw json.RawMessage) toolResult {
	var args struct {
		EnvID         string `json:"env_id"`
		Path          string `json:"path"`
		Dest          string `json:"dest"`
		Overwrite     bool   `json:"overwrite"`
		MaxEntries    int    `json:"max_entries"`
		MaxTotalBytes int64  `json:"max_total_bytes"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	full, res, ok := s.envFilePath(args.EnvID, args.Path)
	if !ok {
		return res
	}
	info, err := os.Stat(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if info.IsDir() {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is a directory")
	}
	format, err := detectArchiveFormat(full)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	if format == "" {
		return errResult(codeUnsupported, "UNSUPPORTED",
			"unsupported archive format: only zip, tar, tar.gz, and gz are supported")
	}

	destRel := args.Dest
	if destRel == "" {
		destRel = "extract"
	}
	root, destRel, res, ok := s.prepareEnvDir(args.EnvID, destRel)
	if !ok {
		return res
	}
	defer root.Close()
	sourceRel, _ := cleanWorkspaceRelative(args.Path)

	maxEntries := clampInt(args.MaxEntries, 1000, 1, 1000)
	maxTotal := args.MaxTotalBytes
	if maxTotal <= 0 {
		maxTotal = 512 << 20
	}
	if maxTotal > 512<<20 {
		maxTotal = 512 << 20
	}

	extracted := make([]map[string]any, 0)
	total := int64(0)
	skipped := 0
	processed := 0
	truncated := false
	stop := false

	extract := func(name string, isDir bool, declaredSize int64, open func() (io.ReadCloser, error)) {
		if stop {
			return
		}
		processed++
		if processed > maxEntries {
			truncated = true
			stop = true
			skipped++
			return
		}
		relative, err := cleanArchiveName(name)
		if err != nil {
			skipped++
			return
		}
		targetRel := filepath.Join(destRel, relative)
		if sourceRel != "." && filepath.Clean(targetRel) == filepath.Clean(sourceRel) {
			skipped++
			return
		}
		if isDir {
			if err := root.MkdirAll(targetRel, 0o700); err != nil {
				skipped++
			}
			return
		}
		remaining := maxTotal - total
		if remaining <= 0 {
			truncated = true
			stop = true
			skipped++
			return
		}
		if declaredSize >= 0 && declaredSize > remaining {
			truncated = true
			stop = true
			skipped++
			return
		}
		if existing, err := root.Lstat(targetRel); err == nil {
			if existing.Mode()&os.ModeSymlink != 0 || existing.IsDir() || !args.Overwrite {
				skipped++
				return
			}
		} else if !os.IsNotExist(err) {
			skipped++
			return
		}
		body, err := open()
		if err != nil {
			skipped++
			return
		}
		defer body.Close()
		if err := root.MkdirAll(filepath.Dir(targetRel), 0o700); err != nil {
			skipped++
			return
		}
		tmpRel := fmt.Sprintf("%s.msk-tmp-%d-%d", targetRel, time.Now().UnixNano(), processed)
		output, err := root.OpenFile(tmpRel, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			skipped++
			return
		}
		written, copyErr := io.Copy(output, io.LimitReader(body, remaining+1))
		closeErr := output.Close()
		if copyErr != nil || closeErr != nil || written > remaining {
			_ = root.Remove(tmpRel)
			skipped++
			if written > remaining {
				truncated = true
				stop = true
			}
			return
		}
		if args.Overwrite {
			_ = root.Remove(targetRel)
		}
		if err := root.Rename(tmpRel, targetRel); err != nil {
			_ = root.Remove(tmpRel)
			skipped++
			return
		}
		total += written
		extracted = append(extracted, map[string]any{
			"name":  relative,
			"bytes": written,
		})
	}

	switch format {
	case "zip":
		reader, err := zip.OpenReader(full)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open zip: "+err.Error())
		}
		defer reader.Close()
		for _, entry := range reader.File {
			if stop {
				break
			}
			mode := entry.Mode()
			isDir := entry.FileInfo().IsDir() || strings.HasSuffix(entry.Name, "/")
			if mode&os.ModeSymlink != 0 || (mode&os.ModeType != 0 && !isDir) {
				skipped++
				continue
			}
			if isDir {
				extract(entry.Name, true, 0, nil)
				continue
			}
			current := entry
			extract(entry.Name, false, int64(current.UncompressedSize64), func() (io.ReadCloser, error) {
				return current.Open()
			})
		}
	case "tar", "tar.gz":
		file, err := os.Open(full)
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		defer file.Close()
		var source io.Reader = file
		var gz *gzip.Reader
		if format == "tar.gz" {
			gz, err = gzip.NewReader(file)
			if err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open gzip: "+err.Error())
			}
			defer gz.Close()
			source = &budgetReader{reader: gz, remaining: maxTotal + 32<<20}
		}
		tarReader := tar.NewReader(source)
		for {
			header, err := tarReader.Next()
			if errors.Is(err, io.EOF) {
				break
			}
			if errors.Is(err, errDecompressionBudget) {
				return errResult(codeResourceExhausted, "RESOURCE_EXHAUSTED",
					"decompressed archive exceeds the extraction budget")
			}
			if err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
					"read tar: "+err.Error())
			}
			if stop {
				break
			}
			switch header.Typeflag {
			case tar.TypeSymlink, tar.TypeLink, tar.TypeChar, tar.TypeBlock, tar.TypeFifo:
				skipped++
				continue
			case tar.TypeDir:
				extract(header.Name, true, 0, nil)
				continue
			}
			current := header
			extract(header.Name, false, current.Size, func() (io.ReadCloser, error) {
				return io.NopCloser(tarReader), nil
			})
		}
	case "gzip":
		file, err := os.Open(full)
		if err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		defer file.Close()
		reader, err := gzip.NewReader(file)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "open gzip: "+err.Error())
		}
		defer reader.Close()
		extract(gzipEntryName(full, reader.Name), false, -1, func() (io.ReadCloser, error) {
			return io.NopCloser(reader), nil
		})
	}

	return okResult(fmt.Sprintf("extracted %d files", len(extracted)), map[string]any{
		"dest":        filepath.ToSlash(destRel),
		"extracted":   extracted,
		"skipped":     skipped,
		"count":       len(extracted),
		"total_bytes": total,
		"truncated":   truncated,
	})
}

// cleanArchiveName normalizes an archive member name and rejects absolute
// paths, drive designators, and traversal components.
func cleanArchiveName(name string) (string, error) {
	if name == "" {
		return "", errors.New("empty entry name")
	}
	name = strings.ReplaceAll(name, "\\", "/")
	if strings.HasPrefix(name, "/") || filepath.IsAbs(name) {
		return "", errors.New("absolute entry path")
	}
	if filepath.VolumeName(name) != "" {
		return "", errors.New("entry path has a volume")
	}
	parts := strings.Split(name, "/")
	cleaned := make([]string, 0, len(parts))
	for _, part := range parts {
		if part == "" || part == "." {
			continue
		}
		if part == ".." {
			return "", errors.New("entry path traverses upward")
		}
		if strings.ContainsAny(part, ":\x00") {
			return "", errors.New("entry path has an invalid character")
		}
		if strings.HasSuffix(part, ".") || strings.HasSuffix(part, " ") {
			return "", errors.New("entry path has a trailing dot or space")
		}
		base := part
		if index := strings.IndexByte(base, '.'); index >= 0 {
			base = base[:index]
		}
		switch strings.ToUpper(base) {
		case "CON", "PRN", "AUX", "NUL",
			"COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
			"LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9":
			return "", errors.New("entry path uses a reserved device name")
		}
		cleaned = append(cleaned, part)
	}
	if len(cleaned) == 0 {
		return "", errors.New("empty entry name")
	}
	return strings.Join(cleaned, "/"), nil
}

// toolBinaryInfo inspects ELF, PE, and Mach-O metadata and hardening flags.
func (s *Server) toolBinaryInfo(raw json.RawMessage) toolResult {
	var args struct {
		EnvID string `json:"env_id"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	full, res, ok := s.envFilePath(args.EnvID, args.Path)
	if !ok {
		return res
	}
	info, err := os.Stat(full)
	if err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	if info.IsDir() {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "path is a directory")
	}
	file, err := os.Open(full)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	defer file.Close()

	if parsed, err := elf.NewFile(file); err == nil {
		return elfBinaryResult(parsed)
	}
	if parsed, err := pe.NewFile(file); err == nil {
		return peBinaryResult(parsed)
	}
	if parsed, err := macho.NewFile(file); err == nil {
		return machoBinaryResult(parsed)
	}
	return errResult(codeUnsupported, "UNSUPPORTED",
		"not an ELF, PE, or Mach-O binary")
}

func binaryResult(format, arch string, bits int, endian string, entry uint64,
	sections []map[string]any, imports []string, security map[string]any) toolResult {
	if len(sections) > 200 {
		sections = sections[:200]
	}
	if len(imports) > 200 {
		imports = imports[:200]
	}
	return okResult(fmt.Sprintf("%s %s %d-bit", format, arch, bits), map[string]any{
		"format":   format,
		"arch":     arch,
		"bits":     bits,
		"endian":   endian,
		"entry":    entry,
		"sections": sections,
		"imports":  imports,
		"security": security,
	})
}

func elfBinaryResult(file *elf.File) toolResult {
	bits := 0
	switch file.Class {
	case elf.ELFCLASS32:
		bits = 32
	case elf.ELFCLASS64:
		bits = 64
	}
	endian := "unknown"
	switch file.Data {
	case elf.ELFDATA2LSB:
		endian = "little"
	case elf.ELFDATA2MSB:
		endian = "big"
	}
	sections := make([]map[string]any, 0, len(file.Sections))
	for _, section := range file.Sections {
		sections = append(sections, map[string]any{
			"name":    section.Name,
			"address": section.Addr,
			"size":    section.Size,
			"flags":   uint64(section.Flags),
		})
	}
	imports := make([]string, 0)
	if symbols, err := file.ImportedSymbols(); err == nil {
		for _, symbol := range symbols {
			imports = append(imports, symbol.Name)
		}
	}
	return binaryResult("elf", file.Machine.String(), bits, endian, file.Entry,
		sections, limitImports(imports, 200), elfSecurity(file))
}

func elfSecurity(file *elf.File) map[string]any {
	nx := true
	relro := "none"
	for _, program := range file.Progs {
		switch program.Type {
		case elf.PT_GNU_STACK:
			nx = program.Flags&elf.PF_X == 0
		case elf.PT_GNU_RELRO:
			relro = "partial"
		}
	}
	bindNow := false
	if values, err := file.DynValue(elf.DT_BIND_NOW); err == nil && len(values) > 0 {
		bindNow = true
	}
	if values, err := file.DynValue(elf.DT_FLAGS); err == nil {
		for _, value := range values {
			if elf.DynFlag(value)&elf.DF_BIND_NOW != 0 {
				bindNow = true
			}
		}
	}
	if values, err := file.DynValue(elf.DT_FLAGS_1); err == nil {
		for _, value := range values {
			if elf.DynFlag1(value)&elf.DF_1_NOW != 0 {
				bindNow = true
			}
		}
	}
	if relro == "partial" && bindNow {
		relro = "full"
	}
	canary, fortify := elfSymbolFlags(file)
	pie := file.Type == elf.ET_DYN
	return map[string]any{
		"nx":           nx,
		"pie":          pie,
		"relro":        relro,
		"canary":       canary,
		"fortify":      fortify,
		"aslr":         pie,
		"dynamic_base": false,
		"cfg":          false,
		"integrity":    false,
	}
}

func elfSymbolFlags(file *elf.File) (canary, fortify bool) {
	check := func(symbols []elf.Symbol) {
		for _, symbol := range symbols {
			if symbol.Name == "__stack_chk_fail" || symbol.Name == "__stack_chk_guard" {
				canary = true
			}
			if strings.Contains(symbol.Name, "_chk") {
				fortify = true
			}
			if canary && fortify {
				return
			}
		}
	}
	if symbols, err := file.Symbols(); err == nil {
		check(symbols)
	}
	if !(canary && fortify) {
		if symbols, err := file.DynamicSymbols(); err == nil {
			check(symbols)
		}
	}
	return canary, fortify
}

func peBinaryResult(file *pe.File) toolResult {
	bits := 0
	entry := uint64(0)
	characteristics := uint16(0)
	switch header := file.OptionalHeader.(type) {
	case *pe.OptionalHeader32:
		bits = 32
		entry = uint64(header.ImageBase) + uint64(header.AddressOfEntryPoint)
		characteristics = header.DllCharacteristics
	case *pe.OptionalHeader64:
		bits = 64
		entry = header.ImageBase + uint64(header.AddressOfEntryPoint)
		characteristics = header.DllCharacteristics
	}
	sections := make([]map[string]any, 0, len(file.Sections))
	for _, section := range file.Sections {
		sections = append(sections, map[string]any{
			"name":    section.Name,
			"address": section.VirtualAddress,
			"size":    section.Size,
			"flags":   section.Characteristics,
		})
	}
	imports := make([]string, 0)
	if symbols, err := file.ImportedSymbols(); err == nil {
		imports = append(imports, symbols...)
	}
	dynamicBase := characteristics&pe.IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE != 0
	security := map[string]any{
		"nx":           characteristics&pe.IMAGE_DLLCHARACTERISTICS_NX_COMPAT != 0,
		"pie":          false,
		"relro":        "none",
		"canary":       false,
		"fortify":      false,
		"aslr":         dynamicBase,
		"dynamic_base": dynamicBase,
		"cfg":          characteristics&pe.IMAGE_DLLCHARACTERISTICS_GUARD_CF != 0,
		"integrity":    characteristics&pe.IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY != 0,
	}
	return binaryResult("pe", peArch(file.Machine), bits, "little", entry,
		sections, limitImports(imports, 200), security)
}

func peArch(machine uint16) string {
	switch machine {
	case pe.IMAGE_FILE_MACHINE_AMD64:
		return "amd64"
	case pe.IMAGE_FILE_MACHINE_I386:
		return "386"
	case pe.IMAGE_FILE_MACHINE_ARM:
		return "arm"
	case pe.IMAGE_FILE_MACHINE_ARM64:
		return "arm64"
	default:
		return fmt.Sprintf("machine_0x%x", machine)
	}
}

func machoBinaryResult(file *macho.File) toolResult {
	bits := 32
	if file.Magic == macho.Magic64 {
		bits = 64
	}
	endian := "big"
	if file.ByteOrder == binary.LittleEndian {
		endian = "little"
	}
	sections := make([]map[string]any, 0, len(file.Sections))
	for _, section := range file.Sections {
		sections = append(sections, map[string]any{
			"name":    section.Name,
			"address": section.Addr,
			"size":    section.Size,
			"flags":   uint64(section.Flags),
		})
	}
	imports := make([]string, 0)
	if file.Symtab != nil {
		valid := true
		if file.Dysymtab != nil {
			start := int64(file.Dysymtab.Iundefsym)
			count := int64(file.Dysymtab.Nundefsym)
			valid = start >= 0 && count >= 0 && start+count <= int64(len(file.Symtab.Syms))
		}
		if valid {
			if symbols, err := file.ImportedSymbols(); err == nil {
				imports = append(imports, symbols...)
			}
		}
	}
	imports = limitImports(imports, 200)

	const mhPIE = 0x200000
	pie := file.Flags&mhPIE != 0
	canary := false
	fortify := false
	for _, symbol := range imports {
		if strings.Contains(symbol, "stack_chk_fail") {
			canary = true
		}
		if strings.Contains(symbol, "_chk") {
			fortify = true
		}
	}
	security := map[string]any{
		"nx":           true,
		"pie":          pie,
		"relro":        "none",
		"canary":       canary,
		"fortify":      fortify,
		"aslr":         pie,
		"dynamic_base": false,
		"cfg":          false,
		"integrity":    false,
	}
	return binaryResult("macho", machoArch(file.Cpu), bits, endian, 0,
		sections, imports, security)
}

func machoArch(cpu macho.Cpu) string {
	switch cpu {
	case macho.Cpu386:
		return "386"
	case macho.CpuAmd64:
		return "amd64"
	case macho.CpuArm:
		return "arm"
	case macho.CpuArm64:
		return "arm64"
	case macho.CpuPpc:
		return "ppc"
	case macho.CpuPpc64:
		return "ppc64"
	default:
		return fmt.Sprintf("cpu_0x%x", uint32(cpu))
	}
}

func limitImports(imports []string, limit int) []string {
	out := make([]string, 0, minInt(len(imports), limit))
	seen := make(map[string]bool, len(out))
	for _, name := range imports {
		if name == "" || seen[name] {
			continue
		}
		seen[name] = true
		out = append(out, name)
		if len(out) >= limit {
			break
		}
	}
	return out
}

type secretPattern struct {
	name  string
	regex *regexp.Regexp
	group int
}

var secretPatterns = []secretPattern{
	{"aws_access_key", regexp.MustCompile(`\bAKIA[0-9A-Z]{16}\b`), 0},
	{"pem_private_key", regexp.MustCompile(
		`-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----`), 0},
	{"jwt", regexp.MustCompile(
		`\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*`), 0},
	{"github_token", regexp.MustCompile(`\bgh[pousr]_[A-Za-z0-9]{20,}\b`), 0},
	{"slack_token", regexp.MustCompile(`\bxox[abposr]-[A-Za-z0-9-]{10,}\b`), 0},
	{"bearer_token", regexp.MustCompile(
		`(?i)\bbearer\s+([A-Za-z0-9._~+/=-]{12,})`), 1},
	{"connection_string", regexp.MustCompile(
		`(?i)\b(?:mongodb(?:\+srv)?|postgres(?:ql)?|mysql|redis|amqps?|mssql|sqlserver)://[^\s"'<>]+`), 0},
	{"generic_secret", regexp.MustCompile(
		`(?i)\b(?:api[_-]?key|secret|token|password|passwd|pwd)\b\s*[:=]\s*["']?([A-Za-z0-9_\-./+=]{6,})`), 1},
}

// toolSecretScan walks text files and reports credential-shaped strings.
// Previews are redacted; the raw secret is never returned.
func (s *Server) toolSecretScan(raw json.RawMessage) toolResult {
	var args struct {
		EnvID         string `json:"env_id"`
		Root          string `json:"root"`
		Glob          string `json:"glob"`
		MaxFiles      int    `json:"max_files"`
		MaxFileBytes  int64  `json:"max_file_bytes"`
		IncludeHidden bool   `json:"include_hidden"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	if args.Root == "" {
		args.Root = "."
	}
	if args.Glob != "" {
		if _, err := filepath.Match(args.Glob, ""); err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "invalid glob: "+err.Error())
		}
	}
	maxFiles := clampInt(args.MaxFiles, 1000, 1, 5000)
	maxFileBytes := clampInt64(args.MaxFileBytes, 512<<10, 1, maxSecretFile)

	env, err := s.getEnv(args.EnvID)
	if err != nil {
		return errResult(codeNotFound, "NOT_FOUND", err.Error())
	}
	cleanRoot, err := cleanWorkspaceRelative(args.Root)
	if err != nil {
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}
	root, err := os.OpenRoot(env.Workspace)
	if err != nil {
		return errResult(codeInternal, "INTERNAL", err.Error())
	}
	defer root.Close()
	rootFS := root.FS()
	if _, err := fs.Stat(rootFS, cleanRoot); err != nil {
		if os.IsNotExist(err) {
			return errResult(codeNotFound, "NOT_FOUND", err.Error())
		}
		return errResult(codePermissionDenied, "PERMISSION_DENIED", err.Error())
	}

	findings := make([]map[string]any, 0)
	filesScanned := 0
	truncated := false

	walkErr := fs.WalkDir(rootFS, cleanRoot, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			if path == cleanRoot {
				return walkErr
			}
			return nil
		}
		if entry.IsDir() {
			if path != cleanRoot && !args.IncludeHidden && strings.HasPrefix(entry.Name(), ".") {
				return fs.SkipDir
			}
			return nil
		}
		if entry.Type()&fs.ModeSymlink != 0 || entry.Type()&fs.ModeType != 0 {
			return nil
		}
		if !args.IncludeHidden && strings.HasPrefix(entry.Name(), ".") {
			return nil
		}
		relative := path
		if !matchWorkspaceGlob(args.Glob, relative, entry.Name()) {
			return nil
		}
		if filesScanned >= maxFiles {
			truncated = true
			return fs.SkipAll
		}
		file, err := root.Open(filepath.FromSlash(path))
		if err != nil {
			return nil
		}
		data, readErr := io.ReadAll(io.LimitReader(file, maxFileBytes))
		_ = file.Close()
		if readErr != nil {
			return nil
		}
		head := data
		if len(head) > 8<<10 {
			head = head[:8<<10]
		}
		if bytes.IndexByte(head, 0) >= 0 {
			return nil
		}
		filesScanned++

		for index, lineBytes := range bytes.Split(data, []byte{'\n'}) {
			line := string(lineBytes)
			for _, pattern := range secretPatterns {
				locations := pattern.regex.FindAllStringSubmatchIndex(line, -1)
				for _, location := range locations {
					secret := ""
					if pattern.group*2+1 < len(location) && location[pattern.group*2] >= 0 {
						secret = line[location[pattern.group*2]:location[pattern.group*2+1]]
					}
					if secret == "" {
						continue
					}
					preview := redactSecret(secret)
					if pattern.name == "connection_string" {
						preview = redactConnectionString(secret)
					}
					if pattern.group > 0 {
						start, end := location[pattern.group*2], location[pattern.group*2+1]
						if start >= 0 && end >= start {
							preview = line[location[0]:start] + preview + line[end:location[1]]
						}
					}
					preview = strings.TrimSpace(preview)
					if len(preview) > 160 {
						preview = preview[:160]
					}
					findings = append(findings, map[string]any{
						"path":    relative,
						"line":    index + 1,
						"pattern": pattern.name,
						"preview": preview,
					})
					if len(findings) >= 500 {
						truncated = true
						return fs.SkipAll
					}
				}
			}
		}
		return nil
	})
	if walkErr != nil {
		return errResult(codeInternal, "INTERNAL", walkErr.Error())
	}

	return okResult(fmt.Sprintf("%d findings in %d files", len(findings), filesScanned),
		map[string]any{
			"findings":      findings,
			"files_scanned": filesScanned,
			"truncated":     truncated,
		})
}

// redactSecret keeps at most two leading characters for short secrets and at
// most four leading and two trailing characters for longer values.
func redactSecret(secret string) string {
	if len(secret) <= 4 {
		return strings.Repeat("*", len(secret))
	}
	if len(secret) <= 16 {
		return secret[:2] + strings.Repeat("*", len(secret)-2)
	}
	hidden := len(secret) - 6
	if hidden < 8 {
		hidden = 8
	}
	return secret[:4] + strings.Repeat("*", hidden) + secret[len(secret)-2:]
}

// redactConnectionString keeps the scheme and host but hides the password.
func redactConnectionString(value string) string {
	parsed, err := url.Parse(value)
	if err != nil || parsed.Host == "" {
		return redactSecret(value)
	}
	if parsed.User != nil {
		username := parsed.User.Username()
		if _, hasPassword := parsed.User.Password(); hasPassword {
			parsed.User = url.UserPassword(username, "***")
		} else {
			parsed.User = url.User(username)
		}
	}
	return parsed.String()
}

// toolTransform performs encoding, hashing, compression, and token inspection.
// It needs no environment because it only operates on caller-provided data.
func (s *Server) toolTransform(raw json.RawMessage) toolResult {
	var args struct {
		Op         string   `json:"op"`
		Data       string   `json:"data"`
		DataBase64 string   `json:"data_base64"`
		Key        string   `json:"key"`
		KeyBase64  string   `json:"key_base64"`
		Algorithm  string   `json:"algorithm"`
		Algorithms []string `json:"algorithms"`
		Length     int      `json:"length"`
		XorKey     string   `json:"xor_key"`
	}
	if err := json.Unmarshal(raw, &args); err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
	}
	op := strings.ToLower(strings.TrimSpace(args.Op))
	if op == "" {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "op is required")
	}
	input, err := transformInput(args.Data, args.DataBase64)
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "data: "+err.Error())
	}

	const maxTransformOutput = 64 << 10
	out := map[string]any{"op": op}
	textResult := func(value string) toolResult {
		if len(value) > maxTransformOutput {
			return errResult(codeResourceExhausted, "RESOURCE_EXHAUSTED",
				"transform output exceeds 64 KiB; write the data to a file and use file/exec tools")
		}
		out["result"] = value
		out["bytes"] = len(value)
		return okResult(op, out)
	}
	binaryOutput := func(value []byte) toolResult {
		if len(value) > maxTransformOutput {
			return errResult(codeResourceExhausted, "RESOURCE_EXHAUSTED",
				"transform output exceeds 64 KiB; write the data to a file and use file/exec tools")
		}
		out["result_base64"] = base64.StdEncoding.EncodeToString(value)
		if isPrintableText(value) {
			out["result"] = string(value)
		}
		out["bytes"] = len(value)
		return okResult(op, out)
	}

	switch op {
	case "base64_encode":
		return textResult(base64.StdEncoding.EncodeToString(input))
	case "base64_decode":
		decoded, err := decodeBase64Flexible(input)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "base64: "+err.Error())
		}
		return binaryOutput(decoded)
	case "base64url_encode":
		return textResult(base64.URLEncoding.EncodeToString(input))
	case "base64url_decode":
		decoded, err := decodeBase64URL(strings.TrimSpace(string(input)))
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "base64url: "+err.Error())
		}
		return binaryOutput(decoded)
	case "hex_encode":
		return textResult(hex.EncodeToString(input))
	case "hex_decode":
		decoded, err := hex.DecodeString(strings.TrimSpace(string(input)))
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "hex: "+err.Error())
		}
		return binaryOutput(decoded)
	case "url_encode":
		return textResult(url.QueryEscape(string(input)))
	case "url_decode":
		decoded, err := url.QueryUnescape(string(input))
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "url: "+err.Error())
		}
		return textResult(decoded)
	case "html_escape":
		return textResult(html.EscapeString(string(input)))
	case "html_unescape":
		return textResult(html.UnescapeString(string(input)))
	case "hash":
		algorithms := args.Algorithms
		if len(algorithms) == 0 {
			algorithms = []string{args.Algorithm}
		}
		hashes := make(map[string]string)
		order := make([]string, 0, len(algorithms))
		for _, name := range algorithms {
			algorithm := strings.ToLower(strings.TrimSpace(name))
			if algorithm == "" {
				algorithm = "sha256"
			}
			if _, exists := hashes[algorithm]; exists {
				continue
			}
			digest, err := hashBytes(algorithm, input)
			if err != nil {
				return errResult(codeInvalidArgument, "INVALID_ARGUMENT", err.Error())
			}
			hashes[algorithm] = digest
			order = append(order, algorithm)
		}
		out["hashes"] = hashes
		out["bytes"] = len(input)
		if len(order) > 0 {
			out["result"] = hashes[order[0]]
		}
		return okResult(op, out)
	case "hmac":
		algorithm := strings.ToLower(strings.TrimSpace(args.Algorithm))
		if algorithm == "" {
			algorithm = "sha256"
		}
		var factory func() hash.Hash
		switch algorithm {
		case "sha1":
			factory = sha1.New
		case "sha256":
			factory = sha256.New
		case "sha512":
			factory = sha512.New
		default:
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"hmac algorithm must be sha1, sha256, or sha512")
		}
		key, err := transformInput(args.Key, args.KeyBase64)
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "key: "+err.Error())
		}
		mac := hmac.New(factory, key)
		_, _ = mac.Write(input)
		sum := mac.Sum(nil)
		out["hmac"] = map[string]any{
			"algorithm": algorithm,
			"hex":       hex.EncodeToString(sum),
			"base64":    base64.StdEncoding.EncodeToString(sum),
		}
		out["result"] = hex.EncodeToString(sum)
		out["bytes"] = len(input)
		return okResult(op, out)
	case "jwt_decode":
		return transformJWT(out, input)
	case "gzip":
		var buffer bytes.Buffer
		writer := gzip.NewWriter(&buffer)
		if _, err := writer.Write(input); err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "gzip: "+err.Error())
		}
		if err := writer.Close(); err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "gzip: "+err.Error())
		}
		return binaryOutput(buffer.Bytes())
	case "gunzip":
		reader, err := gzip.NewReader(bytes.NewReader(input))
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "gunzip: "+err.Error())
		}
		defer reader.Close()
		decoded, err := io.ReadAll(io.LimitReader(reader, 64<<10+1))
		if err != nil {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "gunzip: "+err.Error())
		}
		if len(decoded) > 64<<20 {
			return errResult(codeResourceExhausted, "RESOURCE_EXHAUSTED",
				"gunzip output exceeds 64 MiB")
		}
		return binaryOutput(decoded)
	case "rot13":
		return textResult(rot13(string(input)))
	case "xor":
		if args.XorKey == "" {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "xor_key is required")
		}
		key := []byte(args.XorKey)
		decoded := make([]byte, len(input))
		for index := range input {
			decoded[index] = input[index] ^ key[index%len(key)]
		}
		return binaryOutput(decoded)
	case "uuid":
		var value [16]byte
		if _, err := rand.Read(value[:]); err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		value[6] = (value[6] & 0x0f) | 0x40
		value[8] = (value[8] & 0x3f) | 0x80
		uuid := fmt.Sprintf("%x-%x-%x-%x-%x",
			value[0:4], value[4:6], value[6:8], value[8:10], value[10:16])
		out["bytes"] = 16
		return textResult(uuid)
	case "random_bytes":
		if args.Length < 1 || args.Length > 4096 {
			return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
				"length must be between 1 and 4096")
		}
		value := make([]byte, args.Length)
		if _, err := rand.Read(value); err != nil {
			return errResult(codeInternal, "INTERNAL", err.Error())
		}
		return binaryOutput(value)
	default:
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "unknown op: "+args.Op)
	}
}

func transformInput(data, encoded string) ([]byte, error) {
	if encoded == "" {
		return []byte(data), nil
	}
	decoded, err := base64.StdEncoding.DecodeString(encoded)
	if err != nil {
		return nil, err
	}
	return decoded, nil
}

func decodeBase64Flexible(data []byte) ([]byte, error) {
	value := strings.TrimSpace(string(data))
	if decoded, err := base64.StdEncoding.DecodeString(value); err == nil {
		return decoded, nil
	}
	return base64.RawStdEncoding.DecodeString(value)
}

func decodeBase64URL(value string) ([]byte, error) {
	if decoded, err := base64.RawURLEncoding.DecodeString(value); err == nil {
		return decoded, nil
	}
	return base64.URLEncoding.DecodeString(value)
}

func hashBytes(algorithm string, data []byte) (string, error) {
	switch algorithm {
	case "md5":
		sum := md5.Sum(data)
		return hex.EncodeToString(sum[:]), nil
	case "sha1":
		sum := sha1.Sum(data)
		return hex.EncodeToString(sum[:]), nil
	case "sha256":
		sum := sha256.Sum256(data)
		return hex.EncodeToString(sum[:]), nil
	case "sha512":
		sum := sha512.Sum512(data)
		return hex.EncodeToString(sum[:]), nil
	default:
		return "", fmt.Errorf("unknown hash algorithm: %s", algorithm)
	}
}

func transformJWT(out map[string]any, token []byte) toolResult {
	parts := strings.Split(strings.TrimSpace(string(token)), ".")
	if len(parts) != 2 && len(parts) != 3 {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT",
			"jwt must have two or three dot-separated parts")
	}
	header, err := decodeBase64URL(parts[0])
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "jwt header: "+err.Error())
	}
	payload, err := decodeBase64URL(parts[1])
	if err != nil {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "jwt payload: "+err.Error())
	}
	if !json.Valid(header) {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "jwt header is not JSON")
	}
	if !json.Valid(payload) {
		return errResult(codeInvalidArgument, "INVALID_ARGUMENT", "jwt payload is not JSON")
	}
	signature := ""
	if len(parts) == 3 {
		signature = parts[2]
	}
	out["jwt"] = map[string]any{
		"header":    json.RawMessage(header),
		"payload":   json.RawMessage(payload),
		"signature": signature,
	}
	out["bytes"] = len(token)
	out["note"] = "signature is not verified"
	return okResult("jwt_decode", out)
}

func isPrintableText(data []byte) bool {
	if !utf8.Valid(data) || bytes.IndexByte(data, 0) >= 0 {
		return false
	}
	for _, r := range string(data) {
		if r != '\n' && r != '\r' && r != '\t' && (r < 0x20 || r == 0x7f) {
			return false
		}
	}
	return true
}

func rot13(value string) string {
	data := []byte(value)
	for index, c := range data {
		switch {
		case c >= 'a' && c <= 'z':
			data[index] = 'a' + (c-'a'+13)%26
		case c >= 'A' && c <= 'Z':
			data[index] = 'A' + (c-'A'+13)%26
		}
	}
	return string(data)
}
