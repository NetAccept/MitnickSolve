package server

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

// ValidateSandbox checks a deployment-provided launcher prefix. It is called
// once when a transport starts and again before every isolated launch, so a
// launcher deleted or replaced after startup cannot silently become a direct
// execution.
func ValidateSandbox(argv []string, dataRoot string) error {
	if len(argv) == 0 {
		return nil
	}
	if argv[0] == "" {
		return errors.New("sandbox launcher path is empty")
	}
	for _, arg := range argv {
		if strings.IndexByte(arg, 0) >= 0 {
			return errors.New("sandbox launcher contains a NUL byte")
		}
	}
	resolved, err := exec.LookPath(argv[0])
	if err != nil {
		return fmt.Errorf("sandbox launcher %q: %w", argv[0], err)
	}
	absolute, err := filepath.Abs(resolved)
	if err != nil {
		return fmt.Errorf("sandbox launcher path: %w", err)
	}
	info, err := os.Stat(absolute)
	if err != nil {
		return fmt.Errorf("sandbox launcher %q: %w", absolute, err)
	}
	if info.IsDir() {
		return fmt.Errorf("sandbox launcher %q is a directory", absolute)
	}
	if runtime.GOOS == "windows" {
		switch strings.ToLower(filepath.Ext(absolute)) {
		case ".bat", ".cmd", ".ps1", ".com":
			return fmt.Errorf("sandbox launcher %q is a script, not a PE executable", absolute)
		}
		if !isPEExecutable(absolute) {
			return fmt.Errorf("sandbox launcher %q is not a PE executable", absolute)
		}
	} else if info.Mode().Perm()&0o111 == 0 {
		return fmt.Errorf("sandbox launcher %q is not executable", absolute)
	}
	if dataRoot != "" {
		root, rootErr := filepath.EvalSymlinks(dataRoot)
		real, realErr := filepath.EvalSymlinks(absolute)
		if rootErr == nil && realErr == nil {
			rootAbs, _ := filepath.Abs(root)
			realAbs, _ := filepath.Abs(real)
			if within(realAbs, rootAbs) && !isFilesystemRoot(rootAbs) {
				return fmt.Errorf("sandbox launcher %q is inside the client-writable data root", absolute)
			}
		}
	}
	return nil
}

func within(path, root string) bool {
	relative, err := filepath.Rel(root, path)
	if err != nil || filepath.IsAbs(relative) {
		return false
	}
	return relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator))
}

func isFilesystemRoot(path string) bool {
	clean := filepath.Clean(path)
	volume := filepath.VolumeName(clean)
	return clean == volume+string(filepath.Separator) || clean == string(filepath.Separator)
}

func isPEExecutable(path string) bool {
	file, err := os.Open(path)
	if err != nil {
		return false
	}
	defer file.Close()
	var header [2]byte
	if _, err := file.Read(header[:]); err != nil {
		return false
	}
	return header[0] == 'M' && header[1] == 'Z'
}

// launchEnv returns a child environment with envd's own credentials removed.
// The launcher and every command share this policy: an untrusted challenge
// binary must not be able to read ENVD_TOKEN from the service process.
func launchEnv(extra ...string) []string {
	inherited := os.Environ()
	environment := make([]string, 0, len(inherited)+len(extra))
	for _, entry := range inherited {
		name, _, ok := strings.Cut(entry, "=")
		if ok && strings.HasPrefix(strings.ToUpper(name), "ENVD_") {
			continue
		}
		environment = append(environment, entry)
	}
	return append(environment, extra...)
}

func (s *Server) launch(env *Env, argv []string) ([]string, error) {
	if env.Profile == "isolated" {
		if err := ValidateSandbox(s.opts.Sandbox, s.opts.Root); err != nil {
			return nil, fmt.Errorf("sandbox launcher unavailable: %w", err)
		}
	}
	return env.launchArgv(argv, s.opts.Sandbox)
}
