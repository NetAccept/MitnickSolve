//go:build windows

package server

import (
	"fmt"
	"os"
	"os/exec"
	"unsafe"

	"golang.org/x/sys/windows"
)

// processGuard owns a Job Object with KILL_ON_JOB_CLOSE. Closing the job kills
// every process still assigned to it, including descendants.
type processGuard struct {
	job windows.Handle
}

func newProcessGuard() (*processGuard, error) {
	job, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return nil, err
	}
	var info windows.JOBOBJECT_EXTENDED_LIMIT_INFORMATION
	info.BasicLimitInformation.LimitFlags = windows.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
	if _, err := windows.SetInformationJobObject(
		job,
		windows.JobObjectExtendedLimitInformation,
		uintptr(unsafe.Pointer(&info)),
		uint32(unsafe.Sizeof(info)),
	); err != nil {
		_ = windows.CloseHandle(job)
		return nil, err
	}
	return &processGuard{job: job}, nil
}

func configurePipeProcess(cmd *exec.Cmd) {}

func (guard *processGuard) assign(pid int) error {
	if guard == nil || guard.job == 0 {
		return nil
	}
	if pid <= 0 {
		return os.ErrProcessDone
	}
	process, err := windows.OpenProcess(
		windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE,
		false,
		uint32(pid),
	)
	if err != nil {
		return fmt.Errorf("open process for job: %w", err)
	}
	defer windows.CloseHandle(process)
	return windows.AssignProcessToJobObject(guard.job, process)
}

func (guard *processGuard) close() {
	if guard == nil || guard.job == 0 {
		return
	}
	_ = windows.CloseHandle(guard.job)
	guard.job = 0
}

func killProcess(cmd *exec.Cmd) {
	if cmd == nil || cmd.Process == nil {
		return
	}
	_ = cmd.Process.Kill()
}

func killProcessID(pid int) {
	if pid <= 0 {
		return
	}
	process, err := windows.OpenProcess(windows.PROCESS_TERMINATE, false, uint32(pid))
	if err != nil {
		return
	}
	defer windows.CloseHandle(process)
	_ = windows.TerminateProcess(process, 1)
}

func processCancel(cmd *exec.Cmd) error {
	if cmd == nil || cmd.Process == nil {
		return os.ErrProcessDone
	}
	return cmd.Process.Kill()
}
