//! envd-native MSDP data client for remote PTY/pipe streams.
//!
//! Plain TCP only in this slice: TLS data transport and WebSocket framing are
//! still pending. Output is byte-addressed by `seq`; the client credits bytes
//! back to the server as it receives them.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::frame::{Frame, FrameKind, MAX_FRAME};

#[derive(Debug, Clone)]
pub enum DataEvent {
    Data(Vec<u8>),
    Exit(Value),
    Closed,
    Error(String),
}

pub struct EnvDataClient {
    writer: Arc<Mutex<TcpStream>>,
    events: mpsc::Receiver<DataEvent>,
    buffer: Vec<u8>,
    session_id: String,
    closed: bool,
    last_error: Option<String>,
    exit: Option<Value>,
    _reader: thread::JoinHandle<()>,
}

impl EnvDataClient {
    pub fn connect(
        address: &str,
        token: &str,
        session_id: &str,
        cols: u16,
        rows: u16,
        since_seq: Option<u64>,
    ) -> io::Result<Self> {
        let stream = TcpStream::connect(address)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let writer = Arc::new(Mutex::new(stream));
        let mut read_stream = writer
            .lock()
            .map_err(|_| io::Error::other("data writer poisoned"))?
            .try_clone()?;

        send_json(
            &writer,
            FrameKind::Hello,
            json!({
                "token": token,
                "protocol": {"min": 2, "max": 2},
                "capabilities": ["flow.credit.v1", "resume.seq.v1"]
            }),
        )?;
        let ack = read_frame(&mut read_stream)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "envd data closed"))?;
        if ack.kind != FrameKind::HelloAck {
            return Err(frame_error("HELLO_ACK", &ack));
        }

        let mut open = json!({
            "session_id": session_id,
            "cols": cols,
            "rows": rows,
            "takeover": true
        });
        if let Some(since) = since_seq {
            open["since_seq"] = json!(since);
        }
        send_json(&writer, FrameKind::Open, open)?;
        let opened = read_frame(&mut read_stream)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "envd data closed"))?;
        if opened.kind != FrameKind::OpenAck {
            return Err(frame_error("OPEN_ACK", &opened));
        }
        read_stream.set_read_timeout(None)?;

        let (sender, receiver) = mpsc::channel();
        let reader_writer = writer.clone();
        let reader = thread::spawn(move || {
            loop {
                match read_frame(&mut read_stream) {
                    Ok(Some(frame)) => match frame.kind {
                        FrameKind::Data => {
                            if !frame.payload.is_empty() {
                                let credit = frame.payload.len() as u64;
                                let _ = send_bytes(
                                    &reader_writer,
                                    FrameKind::Credit,
                                    &credit.to_be_bytes(),
                                );
                            }
                            if sender.send(DataEvent::Data(frame.payload)).is_err() {
                                return;
                            }
                        }
                        FrameKind::Exit => {
                            let value = frame.json().unwrap_or(Value::Null);
                            let _ = sender.send(DataEvent::Exit(value));
                            return;
                        }
                        FrameKind::Error => {
                            let value = frame.json().unwrap_or(Value::Null);
                            let message = value
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("envd data error");
                            let _ = sender.send(DataEvent::Error(message.to_string()));
                            return;
                        }
                        FrameKind::Close => {
                            let _ = sender.send(DataEvent::Closed);
                            return;
                        }
                        FrameKind::Pong => {}
                        _ => {}
                    },
                    Ok(None) => {
                        let _ = sender.send(DataEvent::Closed);
                        return;
                    }
                    Err(error) => {
                        let _ = sender.send(DataEvent::Error(error.to_string()));
                        return;
                    }
                }
            }
        });

        Ok(Self {
            writer,
            events: receiver,
            buffer: Vec::new(),
            session_id: session_id.to_string(),
            closed: false,
            last_error: None,
            exit: None,
            _reader: reader,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn exit(&self) -> Option<&Value> {
        self.exit.as_ref()
    }

    pub fn send_data(&self, data: &[u8]) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "data view is closed",
            ));
        }
        send_bytes(&self.writer, FrameKind::Data, data)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        send_json(
            &self.writer,
            FrameKind::Resize,
            json!({"cols": cols, "rows": rows}),
        )
    }

    pub fn close(&self) -> io::Result<()> {
        send_bytes(&self.writer, FrameKind::Close, &[])
    }

    /// Returns up to `max_bytes`, collecting until the buffer is full, the
    /// stream closes, or the timeout expires.
    pub fn read(&mut self, max_bytes: usize, timeout: Duration) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + timeout;
        while self.buffer.len() < max_bytes && !self.closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self
                .events
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(DataEvent::Data(bytes)) => self.buffer.extend_from_slice(&bytes),
                Ok(DataEvent::Exit(value)) => {
                    self.exit = Some(value);
                    self.closed = true;
                }
                Ok(DataEvent::Closed) => self.closed = true,
                Ok(DataEvent::Error(message)) => {
                    self.last_error = Some(message);
                    self.closed = true;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => self.closed = true,
            }
        }
        let count = self.buffer.len().min(max_bytes);
        Ok(self.buffer.drain(..count).collect())
    }

    pub fn next_event_timeout(&mut self, timeout: Duration) -> Option<DataEvent> {
        match self.events.recv_timeout(timeout) {
            Ok(DataEvent::Data(bytes)) => {
                self.buffer.extend_from_slice(&bytes);
                Some(DataEvent::Data(bytes))
            }
            Ok(DataEvent::Exit(value)) => {
                self.exit = Some(value.clone());
                self.closed = true;
                Some(DataEvent::Exit(value))
            }
            Ok(DataEvent::Closed) => {
                self.closed = true;
                Some(DataEvent::Closed)
            }
            Ok(DataEvent::Error(message)) => {
                self.last_error = Some(message.clone());
                self.closed = true;
                Some(DataEvent::Error(message))
            }
            Err(_) => None,
        }
    }

    /// Remaining buffered bytes without reading the socket.
    pub fn buffered(&self) -> &[u8] {
        &self.buffer
    }
}

impl Drop for EnvDataClient {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn send_bytes(writer: &Arc<Mutex<TcpStream>>, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    let frame = Frame {
        kind,
        flags: 0,
        stream_id: 0,
        seq: 0,
        payload: payload.to_vec(),
    };
    let mut writer = writer
        .lock()
        .map_err(|_| io::Error::other("data writer poisoned"))?;
    writer.write_all(&frame.encode())?;
    writer.flush()
}

fn send_json(writer: &Arc<Mutex<TcpStream>>, kind: FrameKind, value: Value) -> io::Result<()> {
    let payload = serde_json::to_vec(&value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    send_bytes(writer, kind, &payload)
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Option<Frame>> {
    Frame::decode(stream, MAX_FRAME)
}

fn frame_error(expected: &str, frame: &Frame) -> io::Error {
    if frame.kind == FrameKind::Error {
        let value = frame.json().unwrap_or(Value::Null);
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("envd data error");
        io::Error::other(format!("envd data error: {message}"))
    } else {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("expected {expected}, got {:?}", frame.kind),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn handshake_data_and_credit_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(hello.kind, FrameKind::Hello);
            let ack = Frame::new(FrameKind::HelloAck, 0, br#"{"protocol":2}"#.to_vec());
            stream.write_all(&ack.encode()).unwrap();
            let open = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(open.kind, FrameKind::Open);
            let open_ack = Frame::new(
                FrameKind::OpenAck,
                0,
                br#"{"session_id":"ses_test","first_seq":0,"last_seq":0}"#.to_vec(),
            );
            stream.write_all(&open_ack.encode()).unwrap();
            let data = Frame {
                kind: FrameKind::Data,
                flags: 0,
                stream_id: 0,
                seq: 0,
                payload: b"hello-data".to_vec(),
            };
            stream.write_all(&data.encode()).unwrap();
            let credit = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(credit.kind, FrameKind::Credit);
            assert_eq!(
                u64::from_be_bytes(credit.payload[..8].try_into().unwrap()),
                10
            );
            let exit = Frame::new(FrameKind::Exit, 0, br#"{"exit_code":0}"#.to_vec());
            stream.write_all(&exit.encode()).unwrap();
            let _ = stream.read(&mut [0u8; 1]);
        });

        let mut client =
            EnvDataClient::connect(&address.to_string(), "secret", "ses_test", 80, 24, None)
                .unwrap();
        let data = client.read(64, Duration::from_secs(5)).unwrap();
        assert_eq!(data, b"hello-data");
        for _ in 0..100 {
            let _ = client.read(64, Duration::from_millis(50)).unwrap();
            if client.is_closed() {
                break;
            }
        }
        assert!(client.is_closed());
        assert_eq!(client.exit().unwrap()["exit_code"], 0);
        drop(client);
        server.join().unwrap();
    }
}
