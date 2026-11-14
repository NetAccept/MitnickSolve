package server

import (
	"sync"
)

// outputLog is an append-only, byte-addressable mirror of a session's output.
// seq is the absolute byte offset of the first byte in the stream. It trims
// the oldest bytes when it exceeds capacity; readers below first get a gap
// rather than silently clamped data.
type outputLog struct {
	mu       sync.Mutex
	data     []byte
	first    uint64
	last     uint64
	capacity int
	finished bool
	notify   chan struct{}
}

func newOutputLog(capacity int) *outputLog {
	if capacity <= 0 {
		capacity = 4 << 20
	}
	return &outputLog{
		capacity: capacity,
		notify:   make(chan struct{}),
	}
}

func (log *outputLog) wakeLocked() {
	close(log.notify)
	log.notify = make(chan struct{})
}

func (log *outputLog) append(chunk []byte) {
	if len(chunk) == 0 {
		return
	}
	log.mu.Lock()
	defer log.mu.Unlock()
	log.data = append(log.data, chunk...)
	log.last += uint64(len(chunk))
	if len(log.data) > log.capacity {
		drop := len(log.data) - log.capacity
		copy(log.data, log.data[drop:])
		log.data = log.data[:len(log.data)-drop]
		log.first += uint64(drop)
	}
	log.wakeLocked()
}

func (log *outputLog) finish() {
	log.mu.Lock()
	defer log.mu.Unlock()
	if !log.finished {
		log.finished = true
		log.wakeLocked()
	}
}

func (log *outputLog) bounds() (first, last uint64, finished bool, notify <-chan struct{}) {
	log.mu.Lock()
	defer log.mu.Unlock()
	return log.first, log.last, log.finished, log.notify
}

// read returns a copy of at most max bytes starting at seq. A seq below first
// is a gap; a seq at or beyond last returns no data and the caller waits.
func (log *outputLog) read(seq uint64, max int) (data []byte, gap bool, first, last uint64) {
	log.mu.Lock()
	defer log.mu.Unlock()
	first, last = log.first, log.last
	if seq < first {
		return nil, true, first, last
	}
	if seq >= last || max <= 0 {
		return nil, false, first, last
	}
	offset := seq - first
	end := offset + uint64(max)
	if end > uint64(len(log.data)) {
		end = uint64(len(log.data))
	}
	data = append([]byte(nil), log.data[offset:end]...)
	return data, false, first, last
}

func (log *outputLog) isFinished() bool {
	log.mu.Lock()
	defer log.mu.Unlock()
	return log.finished
}
