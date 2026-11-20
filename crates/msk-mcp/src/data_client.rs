//! envd-native MSDP data client for remote PTY/pipe streams.
//!
//! Supports plain TCP and TLS 1.3. A single I/O thread owns the stream, so the
//! same incremental frame decoder works for both transports; callers enqueue
//! frames instead of borrowing the socket. Output is byte-addressed by `seq`;
//! the client credits bytes back to the server as it receives them.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use serde_json::{Value, json};

use crate::frame::{Frame, FrameKind, HEADER_LEN, MAGIC, MAX_FRAME, VERSION};

const READ_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone)]
pub enum DataEvent {
    Data(Vec<u8>),
    Exit(Value),
    Closed,
    Error(String),
}

/// TLS settings for the remote data plane. Verification uses the supplied CA
/// PEM when present, otherwise the Mozilla root store; there is no insecure
/// mode.
#[derive(Debug, Clone)]
pub struct TlsOptions {
    pub server_name: String,
    pub ca_pem: Option<Vec<u8>>,
}

impl TlsOptions {
    pub fn new(server_name: impl Into<String>) -> Self {
        Self {
            server_name: server_name.into(),
            ca_pem: None,
        }
    }

    pub fn with_ca_pem(mut self, pem: Vec<u8>) -> Self {
        self.ca_pem = Some(pem);
        self
    }
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

enum Transport {
    Plain(TcpStream),
    Tls(StreamOwned<ClientConnection, TcpStream>),
}

impl Transport {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.set_read_timeout(timeout),
            Self::Tls(stream) => stream.sock.set_read_timeout(timeout),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

struct CreditState {
    credit: u64,
    closed: bool,
}

struct InputCredit {
    state: Mutex<CreditState>,
    condvar: Condvar,
}

impl InputCredit {
    fn new(credit: u64) -> Self {
        Self {
            state: Mutex::new(CreditState {
                credit,
                closed: false,
            }),
            condvar: Condvar::new(),
        }
    }

    fn grant(&self, delta: u64) {
        if delta == 0 {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.credit = state.credit.saturating_add(delta).min(4 << 20);
            self.condvar.notify_all();
        }
    }

    fn take(&self, want: usize) -> usize {
        if want == 0 {
            return 0;
        }
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return 0,
        };
        while state.credit == 0 && !state.closed {
            state = match self.condvar.wait(state) {
                Ok(state) => state,
                Err(_) => return 0,
            };
        }
        if state.closed {
            return 0;
        }
        let granted = state.credit.min(want as u64) as usize;
        state.credit -= granted as u64;
        granted
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.condvar.notify_all();
        }
    }
}

pub struct EnvDataClient {
    outbound: mpsc::Sender<Vec<u8>>,
    input_credit: Arc<InputCredit>,
    events: mpsc::Receiver<DataEvent>,
    buffer: Vec<u8>,
    session_id: String,
    closed: bool,
    last_error: Option<String>,
    exit: Option<Value>,
    _io: thread::JoinHandle<()>,
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
        Self::connect_with(address, token, session_id, cols, rows, since_seq, None)
    }

    pub fn connect_tls(
        address: &str,
        token: &str,
        session_id: &str,
        cols: u16,
        rows: u16,
        since_seq: Option<u64>,
        tls: &TlsOptions,
    ) -> io::Result<Self> {
        Self::connect_with(address, token, session_id, cols, rows, since_seq, Some(tls))
    }

    fn connect_with(
        address: &str,
        token: &str,
        session_id: &str,
        cols: u16,
        rows: u16,
        since_seq: Option<u64>,
        tls: Option<&TlsOptions>,
    ) -> io::Result<Self> {
        let tcp = TcpStream::connect(address)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut stream = match tls {
            None => Transport::Plain(tcp),
            Some(options) => Transport::Tls(tls_stream(tcp, options)?),
        };

        send_frame(
            &mut stream,
            FrameKind::Hello,
            &serde_json::to_vec(&json!({
                "token": token,
                "protocol": {"min": 2, "max": 2},
                "capabilities": ["flow.credit.v1", "resume.seq.v1"]
            }))
            .map_err(io::Error::other)?,
        )?;
        let ack = read_frame(&mut stream)?
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
        send_frame(
            &mut stream,
            FrameKind::Open,
            &serde_json::to_vec(&open).map_err(io::Error::other)?,
        )?;
        let opened = read_frame(&mut stream)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "envd data closed"))?;
        if opened.kind != FrameKind::OpenAck {
            return Err(frame_error("OPEN_ACK", &opened));
        }

        // The handshake is done; short read timeouts let the I/O thread service
        // the outbound queue while no input is arriving.
        stream.set_read_timeout(Some(READ_POLL))?;
        let stream: Box<dyn ReadWrite> = Box::new(stream);

        let input_credit = Arc::new(InputCredit::new(65536));
        let reader_credit = input_credit.clone();
        let (event_tx, event_rx) = mpsc::channel();
        let (outbound, outbound_rx) = mpsc::channel();
        let io = thread::spawn(move || {
            io_loop(stream, outbound_rx, event_tx, reader_credit);
        });

        Ok(Self {
            outbound,
            input_credit,
            events: event_rx,
            buffer: Vec::new(),
            session_id: session_id.to_string(),
            closed: false,
            last_error: None,
            exit: None,
            _io: io,
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
        let mut offset = 0;
        while offset < data.len() {
            let want = (data.len() - offset).min(64 * 1024);
            let granted = self.input_credit.take(want);
            if granted == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "data view closed while waiting for input credit",
                ));
            }
            self.send_frame(FrameKind::Data, &data[offset..offset + granted])?;
            offset += granted;
        }
        Ok(())
    }

    /// Repositions the view cursor within the retained replay window. Replayed
    /// bytes arrive as normal DATA; an out-of-window cursor produces an ERROR.
    pub fn seek(&self, seq: u64) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "data view is closed",
            ));
        }
        self.send_frame(FrameKind::Seek, &seq.to_be_bytes())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.send_frame(
            FrameKind::Resize,
            &serde_json::to_vec(&json!({"cols": cols, "rows": rows})).map_err(io::Error::other)?,
        )
    }

    pub fn close(&self) -> io::Result<()> {
        let result = self.send_frame(FrameKind::Close, &[]);
        self.input_credit.close();
        result
    }

    fn send_frame(&self, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
        self.outbound
            .send(Frame::new(kind, 0, payload.to_vec()).encode())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "data I/O thread stopped"))
    }

    /// Returns up to `max_bytes`, collecting until the buffer is full, the
    /// stream closes, or the timeout expires.
    pub fn read(&mut self, max_bytes: usize, timeout: Duration) -> io::Result<Vec<u8>> {
        let now = Instant::now();
        let deadline = now
            .checked_add(timeout)
            .unwrap_or_else(|| now + Duration::from_secs(3600));
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

fn io_loop(
    mut stream: Box<dyn ReadWrite>,
    outbound: mpsc::Receiver<Vec<u8>>,
    events: mpsc::Sender<DataEvent>,
    input_credit: Arc<InputCredit>,
) {
    let mut decoder = FrameDecoder::default();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        loop {
            match outbound.try_recv() {
                Ok(bytes) => {
                    if let Err(error) = stream.write_all(&bytes).and_then(|_| stream.flush()) {
                        input_credit.close();
                        let _ = events.send(DataEvent::Error(format!("data write: {error}")));
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    input_credit.close();
                    return;
                }
            }
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                input_credit.close();
                let _ = events.send(DataEvent::Closed);
                return;
            }
            Ok(count) => {
                decoder.push(&buffer[..count]);
                loop {
                    let frame = match decoder.next() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(error) => {
                            input_credit.close();
                            let _ = events.send(DataEvent::Error(error.to_string()));
                            return;
                        }
                    };
                    let (keep_going, reply) = handle_frame(frame, &events, &input_credit);
                    if let Some(reply) = reply {
                        if let Err(error) = stream.write_all(&reply).and_then(|_| stream.flush()) {
                            input_credit.close();
                            let _ = events.send(DataEvent::Error(format!("data write: {error}")));
                            return;
                        }
                    }
                    if !keep_going {
                        return;
                    }
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) => {
                input_credit.close();
                let _ = events.send(DataEvent::Error(error.to_string()));
                return;
            }
        }
    }
}

/// Returns whether the view continues, plus an optional frame to write back.
fn handle_frame(
    frame: Frame,
    events: &mpsc::Sender<DataEvent>,
    input_credit: &InputCredit,
) -> (bool, Option<Vec<u8>>) {
    match frame.kind {
        FrameKind::Data => {
            let reply = if frame.payload.is_empty() {
                None
            } else {
                Some(
                    Frame::new(
                        FrameKind::Credit,
                        0,
                        (frame.payload.len() as u64).to_be_bytes().to_vec(),
                    )
                    .encode(),
                )
            };
            (events.send(DataEvent::Data(frame.payload)).is_ok(), reply)
        }
        FrameKind::Credit => {
            if frame.payload.len() == 8 {
                input_credit.grant(u64::from_be_bytes(frame.payload[..8].try_into().unwrap()));
            }
            (true, None)
        }
        FrameKind::Exit => {
            let value = frame.json().unwrap_or(Value::Null);
            input_credit.close();
            let _ = events.send(DataEvent::Exit(value));
            (false, None)
        }
        FrameKind::Error => {
            let value = frame.json().unwrap_or(Value::Null);
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("envd data error");
            input_credit.close();
            let _ = events.send(DataEvent::Error(message.to_string()));
            (false, None)
        }
        FrameKind::Close => {
            input_credit.close();
            let _ = events.send(DataEvent::Closed);
            (false, None)
        }
        FrameKind::Ping => (
            true,
            Some(Frame::new(FrameKind::Pong, 0, Vec::new()).encode()),
        ),
        FrameKind::Pong => (true, None),
        _ => (true, None),
    }
}

fn send_frame(stream: &mut dyn ReadWrite, kind: FrameKind, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&Frame::new(kind, 0, payload.to_vec()).encode())?;
    stream.flush()
}

fn read_frame(stream: &mut dyn ReadWrite) -> io::Result<Option<Frame>> {
    Frame::decode(stream, MAX_FRAME)
}

fn tls_stream(
    mut tcp: TcpStream,
    options: &TlsOptions,
) -> io::Result<StreamOwned<ClientConnection, TcpStream>> {
    let mut roots = RootCertStore::empty();
    if let Some(pem) = &options.ca_pem {
        let mut reader = io::Cursor::new(pem);
        for certificate in rustls_pemfile::certs(&mut reader) {
            roots
                .add(CertificateDer::from(certificate?))
                .map_err(io::Error::other)?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no trusted TLS roots for the envd data plane",
        ));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(options.server_name.clone()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid TLS server name: {}", options.server_name),
        )
    })?;
    let mut connection = ClientConnection::new(Arc::new(config), name).map_err(io::Error::other)?;
    while connection.is_handshaking() {
        connection.complete_io(&mut tcp)?;
    }
    Ok(StreamOwned::new(connection, tcp))
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

/// Incremental MSDP frame parser. A timed read may end in the middle of a
/// frame; buffered bytes are retained and completed by the next read.
#[derive(Default)]
struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    fn next(&mut self) -> io::Result<Option<Frame>> {
        if self.buffer.len() < HEADER_LEN {
            return Ok(None);
        }
        let header = &self.buffer[..HEADER_LEN];
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
        let length = u32::from_be_bytes(header[20..24].try_into().unwrap()) as usize;
        if length > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame length {length} exceeds max {MAX_FRAME}"),
            ));
        }
        let total = HEADER_LEN + length;
        if self.buffer.len() < total {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_LEN..total].to_vec();
        self.buffer.drain(..total);
        Ok(Some(Frame {
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
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn input_credit_blocks_until_granted_and_wakes_on_close() {
        let credit = Arc::new(InputCredit::new(0));
        let waiter = credit.clone();
        let handle = thread::spawn(move || waiter.take(10));
        thread::sleep(Duration::from_millis(30));
        credit.grant(4);
        assert_eq!(handle.join().unwrap(), 4);

        let closed = Arc::new(InputCredit::new(0));
        let waiter = closed.clone();
        let handle = thread::spawn(move || waiter.take(10));
        thread::sleep(Duration::from_millis(30));
        closed.close();
        assert_eq!(handle.join().unwrap(), 0);
    }

    #[test]
    fn frame_decoder_handles_split_and_multiple_frames() {
        let first = Frame::new(FrameKind::Data, 0, b"hello".to_vec()).encode();
        let second = Frame::new(FrameKind::Exit, 0, br#"{"exit_code":0}"#.to_vec()).encode();
        let mut decoder = FrameDecoder::default();
        decoder.push(&first[..7]);
        assert!(decoder.next().unwrap().is_none());
        decoder.push(&first[7..]);
        decoder.push(&second);
        assert_eq!(decoder.next().unwrap().unwrap().payload, b"hello");
        assert_eq!(decoder.next().unwrap().unwrap().kind, FrameKind::Exit);
        assert!(decoder.next().unwrap().is_none());
    }

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

    fn tls_identity() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();
        (
            ca_cert.pem().into_bytes(),
            server_cert.der().to_vec(),
            server_key.serialize_der(),
        )
    }

    #[test]
    fn tls_handshake_and_data_roundtrip() {
        use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ServerConfig, ServerConnection};

        let (ca_pem, cert_der, key_der) = tls_identity();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(cert_der)],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
                )
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = StreamOwned::new(connection, stream);
            while stream.conn.is_handshaking() {
                stream.conn.complete_io(&mut stream.sock).unwrap();
            }
            let hello = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(hello.kind, FrameKind::Hello);
            let ack = Frame::new(FrameKind::HelloAck, 0, br#"{"protocol":2}"#.to_vec());
            stream.write_all(&ack.encode()).unwrap();
            let open = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(open.kind, FrameKind::Open);
            let open_ack = Frame::new(
                FrameKind::OpenAck,
                0,
                br#"{"session_id":"ses_tls","first_seq":0,"last_seq":0}"#.to_vec(),
            );
            stream.write_all(&open_ack.encode()).unwrap();
            let data = Frame::new(FrameKind::Data, 0, b"tls-data".to_vec());
            stream.write_all(&data.encode()).unwrap();
            let credit = Frame::decode(&mut stream, MAX_FRAME).unwrap().unwrap();
            assert_eq!(credit.kind, FrameKind::Credit);
            let exit = Frame::new(FrameKind::Exit, 0, br#"{"exit_code":0}"#.to_vec());
            stream.write_all(&exit.encode()).unwrap();
            let _ = stream.read(&mut [0u8; 1]);
        });

        let mut client = EnvDataClient::connect_tls(
            &address.to_string(),
            "secret",
            "ses_tls",
            80,
            24,
            None,
            &TlsOptions::new("localhost").with_ca_pem(ca_pem),
        )
        .unwrap();
        assert_eq!(
            client.read(64, Duration::from_secs(5)).unwrap(),
            b"tls-data"
        );
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
