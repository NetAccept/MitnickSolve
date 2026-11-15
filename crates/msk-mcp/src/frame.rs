//! MCP transport primitives for the embedded `envd` binding.
//!
//! Frames are transport-neutral; M0 uses them over a child process's stdio.
//! The same bytes will later travel over TLS/TCP or WebSocket.

use std::io::{self, Read};

use serde_json::Value;

pub const MAGIC: [u8; 4] = *b"MSDP";
pub const VERSION: u8 = 2;
pub const HEADER_LEN: usize = 24;
pub const MAX_FRAME: usize = 8 * 1024 * 1024;
pub const EMBEDDED_MAX_FRAME: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Hello = 0x01,
    HelloAck = 0x02,
    Open = 0x03,
    OpenAck = 0x04,
    Data = 0x05,
    Credit = 0x06,
    Seek = 0x07,
    Resize = 0x08,
    Signal = 0x09,
    Exit = 0x0a,
    Close = 0x0b,
    Error = 0x0c,
    Ping = 0x0d,
    Pong = 0x0e,
    Control = 0x0f,
}

impl TryFrom<u8> for FrameKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, io::Error> {
        Ok(match value {
            0x01 => Self::Hello,
            0x02 => Self::HelloAck,
            0x03 => Self::Open,
            0x04 => Self::OpenAck,
            0x05 => Self::Data,
            0x06 => Self::Credit,
            0x07 => Self::Seek,
            0x08 => Self::Resize,
            0x09 => Self::Signal,
            0x0a => Self::Exit,
            0x0b => Self::Close,
            0x0c => Self::Error,
            0x0d => Self::Ping,
            0x0e => Self::Pong,
            0x0f => Self::Control,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame kind 0x{value:02x}"),
                ));
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub flags: u16,
    pub stream_id: u32,
    pub seq: u64,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameKind, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            kind,
            flags: 0,
            stream_id,
            seq: 0,
            payload,
        }
    }

    pub fn control(stream_id: u32, value: &Value) -> Result<Self, serde_json::Error> {
        Ok(Self::new(
            FrameKind::Control,
            stream_id,
            serde_json::to_vec(value)?,
        ))
    }

    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Returns `Ok(None)` only on a clean EOF before any header byte.
    pub fn decode<R: Read + ?Sized>(reader: &mut R, max_frame: usize) -> io::Result<Option<Self>> {
        let mut header = [0u8; HEADER_LEN];
        let mut filled = 0;
        while filled < HEADER_LEN {
            match reader.read(&mut header[filled..])? {
                0 if filled == 0 => return Ok(None),
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated frame header",
                    ));
                }
                n => filled += n,
            }
        }

        if header[0..4] != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame magic",
            ));
        }
        if header[4] != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported frame version {}", header[4]),
            ));
        }

        let kind = FrameKind::try_from(header[5])?;
        let flags = u16::from_be_bytes([header[6], header[7]]);
        let stream_id = u32::from_be_bytes(header[8..12].try_into().unwrap());
        let seq = u64::from_be_bytes(header[12..20].try_into().unwrap());
        let len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if len > max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame length {len} exceeds max {max_frame}"),
            ));
        }

        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        Ok(Some(Self {
            kind,
            flags,
            stream_id,
            seq,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn roundtrip() {
        let frame = Frame {
            kind: FrameKind::Control,
            flags: 0,
            stream_id: 0,
            seq: 3,
            payload: br#"{"jsonrpc":"2.0"}"#.to_vec(),
        };
        let bytes = frame.encode();
        let mut cursor = Cursor::new(bytes);
        let back = Frame::decode(&mut cursor, MAX_FRAME).unwrap().unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn empty_payload_roundtrip() {
        let frame = Frame::new(FrameKind::Pong, 0, Vec::new());
        let mut cursor = Cursor::new(frame.encode());
        let back = Frame::decode(&mut cursor, MAX_FRAME).unwrap().unwrap();
        assert_eq!(back.kind, FrameKind::Pong);
        assert!(back.payload.is_empty());
    }

    #[test]
    fn clean_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(Frame::decode(&mut cursor, MAX_FRAME).unwrap().is_none());
    }

    #[test]
    fn truncated_header_is_an_error() {
        let mut cursor = Cursor::new(vec![b'M', b'S', b'D']);
        assert!(Frame::decode(&mut cursor, MAX_FRAME).is_err());
    }

    #[test]
    fn bad_magic_is_an_error() {
        let mut bytes = Frame::new(FrameKind::Ping, 0, Vec::new()).encode();
        bytes[0] = b'X';
        let mut cursor = Cursor::new(bytes);
        assert!(Frame::decode(&mut cursor, MAX_FRAME).is_err());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let frame = Frame::new(FrameKind::Control, 0, vec![0u8; 64]);
        let mut cursor = Cursor::new(frame.encode());
        assert!(Frame::decode(&mut cursor, 32).is_err());
    }
}
