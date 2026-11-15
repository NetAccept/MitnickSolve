//go:build !windows

package server

import (
	"errors"
	"os"
	"os/exec"
	"syscall"
)

// processGuard owns the process tree for one launch. On Unix the process group
// is the tree boundary, so the guard has no separate handle.
type processGuard struct{}

func newProcessGuard() (*processGuard, error) {
	return &processGuard{}, nil
}

// configurePipeProcess puts a pipe/exec child in its own process group. PTY
// children are already session leaders via Setsid and must not set Setpgid as
// well.
func configurePipeProcess(cmd *exec.Cmd) {
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.Setpgid = true
}

func (guard *processGuard) assign(pid int) error {
	return nil
}

func (guard *processGuard) close() {}

func killProcess(cmd *exec.Cmd) {
	if cmd == nil || cmd.Process == nil {
		return
	}
	killProcessID(cmd.Process.Pid)
}

func killProcessID(pid int) {
	if pid <= 0 {
		return
	}
	_ = syscall.Kill(-pid, syscall.SIGKILL)
	_ = syscall.Kill(pid, syscall.SIGKILL)
}

func processCancel(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return os.ErrProcessDone
	}
	if err := syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL); err != nil {
		if errors.Is(err, syscall.ESRCH) {
			return os.ErrProcessDone
		}
		return err
	}
	return nil
}
