// Package protocol implements the MSDP frame layer used by envd's control and
// data planes. M0 uses it over stdio; the same bytes will travel over TLS/TCP
// and WebSocket later.
package protocol

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

const (
	Magic     = "MSDP"
	Version   = 2
	HeaderLen = 24

	DefaultMaxFrame  = 8 << 20
	EmbeddedMaxFrame = 256 << 10
)

type Kind uint8

const (
	KindHello    Kind = 0x01
	KindHelloAck Kind = 0x02
	KindOpen     Kind = 0x03
	KindOpenAck  Kind = 0x04
	KindData     Kind = 0x05
	KindCredit   Kind = 0x06
	KindSeek     Kind = 0x07
	KindResize   Kind = 0x08
	KindSignal   Kind = 0x09
	KindExit     Kind = 0x0a
	KindClose    Kind = 0x0b
	KindError    Kind = 0x0c
	KindPing     Kind = 0x0d
	KindPong     Kind = 0x0e
	KindControl  Kind = 0x0f
)

func (k Kind) String() string {
	switch k {
	case KindHello:
		return "HELLO"
	case KindHelloAck:
		return "HELLO_ACK"
	case KindOpen:
		return "OPEN"
	case KindOpenAck:
		return "OPEN_ACK"
	case KindData:
		return "DATA"
	case KindCredit:
		return "CREDIT"
	case KindSeek:
		return "SEEK"
	case KindResize:
		return "RESIZE"
	case KindSignal:
		return "SIGNAL"
	case KindExit:
		return "EXIT"
	case KindClose:
		return "CLOSE"
	case KindError:
		return "ERROR"
	case KindPing:
		return "PING"
	case KindPong:
		return "PONG"
	case KindControl:
		return "CONTROL"
	default:
		return fmt.Sprintf("0x%02x", uint8(k))
	}
}

var (
	ErrBadMagic      = errors.New("bad frame magic")
	ErrBadVersion    = errors.New("unsupported frame version")
	ErrFrameTooLarge = errors.New("frame too large")
)

type Frame struct {
	Kind     Kind
	Flags    uint16
	StreamID uint32
	Seq      uint64
	Payload  []byte
}

func WriteFrame(w io.Writer, frame Frame, maxFrame int) error {
	if maxFrame <= 0 {
		maxFrame = DefaultMaxFrame
	}
	if len(frame.Payload) > maxFrame {
		return ErrFrameTooLarge
	}

	var header [HeaderLen]byte
	copy(header[0:4], Magic)
	header[4] = Version
	header[5] = byte(frame.Kind)
	binary.BigEndian.PutUint16(header[6:8], frame.Flags)
	binary.BigEndian.PutUint32(header[8:12], frame.StreamID)
	binary.BigEndian.PutUint64(header[12:20], frame.Seq)
	binary.BigEndian.PutUint32(header[20:24], uint32(len(frame.Payload)))

	if _, err := w.Write(header[:]); err != nil {
		return err
	}
	if len(frame.Payload) > 0 {
		_, err := w.Write(frame.Payload)
		return err
	}
	return nil
}

func ReadFrame(r io.Reader, maxFrame int) (Frame, error) {
	if maxFrame <= 0 {
		maxFrame = DefaultMaxFrame
	}

	var header [HeaderLen]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return Frame{}, err
	}
	if string(header[0:4]) != Magic {
		return Frame{}, ErrBadMagic
	}
	if header[4] != Version {
		return Frame{}, ErrBadVersion
	}

	length := binary.BigEndian.Uint32(header[20:24])
	if int(length) > maxFrame {
		return Frame{}, ErrFrameTooLarge
	}

	payload := make([]byte, int(length))
	if _, err := io.ReadFull(r, payload); err != nil {
		return Frame{}, err
	}

	return Frame{
		Kind:     Kind(header[5]),
		Flags:    binary.BigEndian.Uint16(header[6:8]),
		StreamID: binary.BigEndian.Uint32(header[8:12]),
		Seq:      binary.BigEndian.Uint64(header[12:20]),
		Payload:  payload,
	}, nil
}
