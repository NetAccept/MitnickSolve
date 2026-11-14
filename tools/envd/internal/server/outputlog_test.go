package server

import (
	"bytes"
	"testing"
	"time"
)

func TestOutputLogAppendReadTrimGap(t *testing.T) {
	log := newOutputLog(8)
	log.append([]byte("hello"))
	data, gap, first, last := log.read(0, 5)
	if gap || string(data) != "hello" || first != 0 || last != 5 {
		t.Fatalf("initial read: data=%q gap=%v first=%d last=%d", data, gap, first, last)
	}
	log.append([]byte("world!!"))
	data, gap, first, last = log.read(5, 8)
	if gap || !bytes.Equal(data, []byte("world!!")) || first != 4 || last != 12 {
		t.Fatalf("trimmed read: data=%q gap=%v first=%d last=%d", data, gap, first, last)
	}
	_, gap, first, _ = log.read(0, 4)
	if !gap || first != 4 {
		t.Fatalf("expected gap at 0, first=%d gap=%v", first, gap)
	}
	done, _, finished, notify := log.bounds()
	if done != 4 || finished {
		t.Fatalf("bounds: first=%d finished=%v", done, finished)
	}
	go log.finish()
	select {
	case <-notify:
	case <-time.After(time.Second):
		t.Fatal("finish did not wake waiters")
	}
	if _, _, finished, _ := log.bounds(); !finished {
		t.Fatal("log did not finish")
	}
}
