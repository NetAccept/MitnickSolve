//! Binary data plane for terminal streams.
//!
//! Separate from the JSON-RPC control socket: the client connects to a second
//! local socket, authenticates with the kernel token, opens a terminal stream,
//! and exchanges framed DATA/CREDIT/RESIZE/CLOSE messages.

use std::io::{self, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use super::{TryClone, local, read_token};

pub const DATA_MAGIC: [u8; 4] = *b"MSKD";
pub const DATA_VERSION: u8 = 1;
pub const DATA_HEADER_LEN: usize = 24;
pub const DATA_MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DataKind {
    Hello = 0x01,
    HelloAck = 0x02,
    Open = 0x03,
    OpenAck = 0x04,
    Data = 0x05,
    Credit = 0x06,
    Resize = 0x07,
    Signal = 0x08,
    Exit = 0x09,
    Close = 0x0a,
    Error = 0x0b,
    Ping = 0x0c,
    Pong = 0x0d,
}

impl TryFrom<u8> for DataKind {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, io::Error> {
        Ok(match value {
            0x01 => Self::Hello,
            0x02 => Self::HelloAck,
            0x03 => Self::Open,
            0x04 => Self::OpenAck,
            0x05 => Self::Data,
            0x06 => Self::Credit,
            0x07 => Self::Resize,
            0x08 => Self::Signal,
            0x09 => Self::Exit,
            0x0a => Self::Close,
            0x0b => Self::Error,
            0x0c => Self::Ping,
            0x0d => Self::Pong,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown data frame kind 0x{value:02x}"),
                ));
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrame {
    pub kind: DataKind,
    pub flags: u16,
    pub stream_id: u32,
    pub seq: u64,
    pub payload: Vec<u8>,
}

impl DataFrame {
    pub fn new(kind: DataKind, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            kind,
            flags: 0,
            stream_id,
            seq: 0,
            payload,
        }
    }

    pub fn json(kind: DataKind, stream_id: u32, value: &Value) -> Result<Self, serde_json::Error> {
        Ok(Self::new(kind, stream_id, serde_json::to_vec(value)?))
    }

    pub fn json_value(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_slice(&self.payload)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATA_HEADER_LEN + self.payload.len());
        out.extend_from_slice(&DATA_MAGIC);
        out.push(DATA_VERSION);
        out.push(self.kind as u8);
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode<R: Read>(reader: &mut R, max_frame: usize) -> io::Result<Option<Self>> {
        let mut header = [0u8; DATA_HEADER_LEN];
        let mut filled = 0;
        while filled < DATA_HEADER_LEN {
            match reader.read(&mut header[filled..])? {
                0 if filled == 0 => return Ok(None),
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "truncated data frame header",
                    ));
                }
                n => filled += n,
            }
        }
        if header[0..4] != DATA_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad data frame magic",
            ));
        }
        if header[4] != DATA_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported data frame version {}", header[4]),
            ));
        }
        let kind = DataKind::try_from(header[5])?;
        let flags = u16::from_be_bytes([header[6], header[7]]);
        let stream_id = u32::from_be_bytes(header[8..12].try_into().unwrap());
        let seq = u64::from_be_bytes(header[12..20].try_into().unwrap());
        let len = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if len > max_frame {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("data frame length {len} exceeds max {max_frame}"),
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

pub fn write_frame<W: Write>(writer: &mut W, frame: &DataFrame) -> io::Result<()> {
    writer.write_all(&frame.encode())?;
    writer.flush()
}

#[derive(Debug, Clone)]
pub enum DataEvent {
    Data(Vec<u8>),
    Exit,
    Closed,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TerminalOpen {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

pub struct DataClient {
    writer: Arc<Mutex<local::Stream>>,
    events: mpsc::Receiver<DataEvent>,
    pending_open: Arc<Mutex<Option<mpsc::Sender<Result<Value, String>>>>>,
    seq: AtomicU64,
    stream_id: u32,
    _reader: thread::JoinHandle<()>,
}

impl DataClient {
    pub fn connect(root: &std::path::Path) -> io::Result<Self> {
        let token = read_token(root)?;
        let stream = local::data_connect(root)?;
        let reader_stream = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));
        {
            let mut writer = writer
                .lock()
                .map_err(|_| io::Error::other("data writer poisoned"))?;
            let hello = DataFrame::json(
                DataKind::Hello,
                0,
                &json!({"token": token, "protocol": DATA_VERSION}),
            )
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            write_frame(&mut *writer, &hello)?;
        }

        let mut reader = BufReader::new(reader_stream);
        loop {
            let frame = DataFrame::decode(&mut reader, DATA_MAX_FRAME)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "data server closed")
            })?;
            match frame.kind {
                DataKind::HelloAck => break,
                DataKind::Error => {
                    let value = frame.json_value().unwrap_or(Value::Null);
                    return Err(io::Error::other(value.to_string()));
                }
                _ => {}
            }
        }

        let (event_tx, event_rx) = mpsc::channel();
        let pending_open: Arc<Mutex<Option<mpsc::Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(None));
        let pending_reader = pending_open.clone();
        let writer_reader = writer.clone();
        let reader = thread::spawn(move || {
            loop {
                let frame = match DataFrame::decode(&mut reader, DATA_MAX_FRAME) {
                    Ok(Some(frame)) => frame,
                    _ => {
                        let _ = event_tx.send(DataEvent::Closed);
                        break;
                    }
                };
                match frame.kind {
                    DataKind::Data => {
                        let _ = event_tx.send(DataEvent::Data(frame.payload));
                    }
                    DataKind::OpenAck => {
                        if let Some(sender) = pending_reader
                            .lock()
                            .ok()
                            .and_then(|mut pending| pending.take())
                        {
                            let _ = sender.send(Ok(frame.json_value().unwrap_or(Value::Null)));
                        }
                    }
                    DataKind::Error => {
                        let value = frame.json_value().unwrap_or(Value::Null);
                        let message = value.to_string();
                        let sent = pending_reader
                            .lock()
                            .ok()
                            .and_then(|mut pending| pending.take());
                        if let Some(sender) = sent {
                            let _ = sender.send(Err(message.clone()));
                        }
                        let _ = event_tx.send(DataEvent::Error(message));
                    }
                    DataKind::Exit => {
                        let _ = event_tx.send(DataEvent::Exit);
                    }
                    DataKind::Close => {
                        let _ = event_tx.send(DataEvent::Closed);
                        break;
                    }
                    DataKind::Ping => {
                        let pong = DataFrame::new(DataKind::Pong, 0, Vec::new());
                        if let Ok(mut writer) = writer_reader.lock() {
                            let _ = write_frame(&mut *writer, &pong);
                        }
                    }
                    DataKind::Credit | DataKind::Pong | DataKind::Resize | DataKind::Signal => {}
                    _ => {}
                }
            }
        });

        Ok(Self {
            writer,
            events: event_rx,
            pending_open,
            seq: AtomicU64::new(0),
            stream_id: 1,
            _reader: reader,
        })
    }

    pub fn open_terminal(&self, cols: u16, rows: u16) -> io::Result<TerminalOpen> {
        let (sender, receiver) = mpsc::channel();
        self.pending_open
            .lock()
            .map_err(|_| io::Error::other("pending open poisoned"))?
            .replace(sender);
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| io::Error::other("data writer poisoned"))?;
            let open = DataFrame::json(
                DataKind::Open,
                self.stream_id,
                &json!({"kind": "terminal", "cols": cols, "rows": rows}),
            )
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            write_frame(&mut *writer, &open)?;
        }
        let result = receiver
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "terminal open timeout"))?;
        let value = result.map_err(io::Error::other)?;
        let session_id = value
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing session_id"))?
            .to_string();
        Ok(TerminalOpen {
            session_id,
            cols: value
                .get("cols")
                .and_then(Value::as_u64)
                .unwrap_or(cols as u64) as u16,
            rows: value
                .get("rows")
                .and_then(Value::as_u64)
                .unwrap_or(rows as u64) as u16,
        })
    }

    pub fn send_data(&self, bytes: &[u8]) -> io::Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let frame = DataFrame {
            kind: DataKind::Data,
            flags: 0,
            stream_id: self.stream_id,
            seq,
            payload: bytes.to_vec(),
        };
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("data writer poisoned"))?;
        write_frame(&mut *writer, &frame)
    }

    pub fn credit(&self, bytes: usize) -> io::Result<()> {
        let frame = DataFrame::new(
            DataKind::Credit,
            self.stream_id,
            (bytes as u64).to_be_bytes().to_vec(),
        );
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("data writer poisoned"))?;
        write_frame(&mut *writer, &frame)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let frame = DataFrame::json(
            DataKind::Resize,
            self.stream_id,
            &json!({"cols": cols, "rows": rows}),
        )
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("data writer poisoned"))?;
        write_frame(&mut *writer, &frame)
    }

    pub fn close(&self) -> io::Result<()> {
        let frame = DataFrame::json(
            DataKind::Close,
            self.stream_id,
            &json!({"reason": "client_close"}),
        )
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("data writer poisoned"))?;
        write_frame(&mut *writer, &frame)
    }

    pub fn try_event(&self) -> Option<DataEvent> {
        self.events.try_recv().ok()
    }

    pub fn next_event_timeout(&self, timeout: Duration) -> Option<DataEvent> {
        self.events.recv_timeout(timeout).ok()
    }
}

impl Drop for DataClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = DataFrame {
            kind: DataKind::Data,
            flags: 0,
            stream_id: 1,
            seq: 7,
            payload: b"hello".to_vec(),
        };
        let mut cursor = Cursor::new(frame.encode());
        let back = DataFrame::decode(&mut cursor, DATA_MAX_FRAME)
            .unwrap()
            .unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = DataFrame::new(DataKind::Ping, 0, Vec::new()).encode();
        bytes[0] = b'X';
        assert!(DataFrame::decode(&mut Cursor::new(bytes), DATA_MAX_FRAME).is_err());
    }
}
