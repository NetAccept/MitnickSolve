package protocol

import (
	"bytes"
	"testing"
)

func TestFrameRoundTrip(t *testing.T) {
	var buf bytes.Buffer
	want := Frame{
		Kind:     KindControl,
		StreamID: 0,
		Seq:      3,
		Payload:  []byte(`{"jsonrpc":"2.0"}`),
	}
	if err := WriteFrame(&buf, want, EmbeddedMaxFrame); err != nil {
		t.Fatalf("write: %v", err)
	}
	got, err := ReadFrame(&buf, EmbeddedMaxFrame)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if got.Kind != want.Kind || got.StreamID != want.StreamID || got.Seq != want.Seq {
		t.Fatalf("header mismatch: got %+v want %+v", got, want)
	}
	if !bytes.Equal(got.Payload, want.Payload) {
		t.Fatalf("payload mismatch: got %q want %q", got.Payload, want.Payload)
	}
}

func TestBadMagic(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteFrame(&buf, Frame{Kind: KindPing}, EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	data := buf.Bytes()
	data[0] = 'X'
	if _, err := ReadFrame(bytes.NewReader(data), EmbeddedMaxFrame); err != ErrBadMagic {
		t.Fatalf("got %v, want ErrBadMagic", err)
	}
}

func TestOversizedFrame(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteFrame(&buf, Frame{Kind: KindControl, Payload: bytes.Repeat([]byte{1}, 64)}, 32); err != ErrFrameTooLarge {
		t.Fatalf("write: got %v, want ErrFrameTooLarge", err)
	}
}

func TestTruncatedFrame(t *testing.T) {
	var buf bytes.Buffer
	if err := WriteFrame(&buf, Frame{Kind: KindPing}, EmbeddedMaxFrame); err != nil {
		t.Fatal(err)
	}
	data := buf.Bytes()
	if _, err := ReadFrame(bytes.NewReader(data[:len(data)-1]), EmbeddedMaxFrame); err == nil {
		t.Fatal("expected truncated frame error")
	}
}
