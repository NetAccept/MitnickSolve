//! Embedded `envd` client: spawns the Go binary, performs the HELLO handshake,
//! and carries MCP JSON-RPC over CONTROL frames on stdin/stdout.

use std::io::{self, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

use crate::frame::{EMBEDDED_MAX_FRAME, Frame, FrameKind};

/// Common interface for embedded-stdio and HTTP service transports.
pub trait EnvTransport: Send {
    fn call_tool(&mut self, name: &str, arguments: Value) -> io::Result<Value>;
    fn request(&mut self, method: &str, params: Value) -> io::Result<Value>;
    fn shutdown(&mut self);
}

/// MCP JSON-RPC over HTTP(S), matching `envd --service`.
pub struct ServiceClient {
    base_url: String,
    token: String,
    agent: ureq::Agent,
    next_id: u64,
}

impl ServiceClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::new_with_ca(base_url, token, None).expect("no CA parsing without a bundle")
    }

    /// Creates a client that trusts the certificates in `ca_pem` instead of the
    /// Mozilla roots. The same bundle can be used for the data-plane TLS
    /// settings.
    pub fn new_with_ca(
        base_url: impl Into<String>,
        token: impl Into<String>,
        ca_pem: Option<Vec<u8>>,
    ) -> io::Result<Self> {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false);
        if let Some(pem) = ca_pem {
            let mut certificates = Vec::new();
            for item in ureq::tls::parse_pem(&pem) {
                match item {
                    Ok(ureq::tls::PemItem::Certificate(certificate)) => {
                        certificates.push(certificate)
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("parse envd CA bundle: {error}"),
                        ));
                    }
                }
            }
            if certificates.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "envd CA bundle contains no certificates",
                ));
            }
            let tls = ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::Specific(std::sync::Arc::new(
                    certificates,
                )))
                .build();
            builder = builder.tls_config(tls);
        }
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            agent: builder.build().into(),
            next_id: 0,
        })
    }

    pub fn initialize(&mut self) -> io::Result<Value> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "msk-kernel", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> io::Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    pub fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        self.next_id += 1;
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params
        });
        let authorization = format!("Bearer {}", self.token);
        let mut response = self
            .agent
            .post(format!("{}/mcp", self.base_url))
            .header("Authorization", &authorization)
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(io::Error::other)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(io::Error::other)?;
        if !(200..300).contains(&status) {
            return Err(io::Error::other(format!("envd HTTP {status}: {text}")));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        if let Some(error) = value.get("error") {
            return Err(io::Error::other(error.to_string()));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn shutdown(&mut self) {}
}

impl EnvTransport for ServiceClient {
    fn call_tool(&mut self, name: &str, arguments: Value) -> io::Result<Value> {
        ServiceClient::call_tool(self, name, arguments)
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        ServiceClient::request(self, method, params)
    }

    fn shutdown(&mut self) {
        ServiceClient::shutdown(self);
    }
}

impl EnvTransport for EnvClient {
    fn call_tool(&mut self, name: &str, arguments: Value) -> io::Result<Value> {
        EnvClient::call_tool(self, name, arguments)
    }

    fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        EnvClient::request(self, method, params)
    }

    fn shutdown(&mut self) {
        EnvClient::shutdown(self);
    }
}

#[derive(Debug, Clone)]
pub struct HelloAck {
    pub protocol: u32,
    pub server: String,
    pub max_frame: usize,
    pub credits_initial: u64,
}

pub struct EnvClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl EnvClient {
    pub fn spawn(envd: &Path, root: &Path) -> io::Result<Self> {
        let mut command = Command::new(envd);
        command
            .arg("--embedded")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("envd stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("envd stdout unavailable"))?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 0,
        })
    }

    pub fn handshake(&mut self, agent_id: &str) -> io::Result<HelloAck> {
        let hello = json!({
            "protocol": {"min": 2, "max": 2},
            "agent_id": agent_id,
            "token": "",
            "resume": [],
            "capabilities": ["pty", "files", "forward"]
        });
        let payload = serde_json::to_vec(&hello)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.send(&Frame::new(FrameKind::Hello, 0, payload))?;

        loop {
            let frame = self.recv()?;
            match frame.kind {
                FrameKind::HelloAck => {
                    let value = frame
                        .json()
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                    return Ok(HelloAck {
                        protocol: value.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u32,
                        server: value
                            .get("server")
                            .and_then(Value::as_str)
                            .unwrap_or("envd")
                            .to_string(),
                        max_frame: value
                            .get("max_frame")
                            .and_then(Value::as_u64)
                            .unwrap_or(EMBEDDED_MAX_FRAME as u64)
                            as usize,
                        credits_initial: value
                            .get("credits_initial")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    });
                }
                FrameKind::Error => {
                    let value = frame.json().unwrap_or_default();
                    return Err(io::Error::other(value.to_string()));
                }
                FrameKind::Ping => {
                    self.send(&Frame::new(FrameKind::Pong, 0, Vec::new()))?;
                }
                _ => {}
            }
        }
    }

    pub fn initialize(&mut self) -> io::Result<Value> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "msk-kernel", "version": env!("CARGO_PKG_VERSION")}
            }),
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> io::Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    pub fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        let frame = Frame::control(0, &message)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.send(&frame)?;

        loop {
            let frame = self.recv()?;
            match frame.kind {
                FrameKind::Control => {
                    let value = frame
                        .json()
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            return Err(io::Error::other(error.to_string()));
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                FrameKind::Ping => {
                    self.send(&Frame::new(FrameKind::Pong, 0, Vec::new()))?;
                }
                FrameKind::Error => {
                    let value = frame.json().unwrap_or_default();
                    return Err(io::Error::other(value.to_string()));
                }
                FrameKind::Close => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "envd closed the control stream",
                    ));
                }
                _ => {}
            }
        }
    }

    pub fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let frame = Frame::control(0, &message)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        self.send(&frame)
    }

    pub fn shutdown(&mut self) {
        let _ = self.send(&Frame::new(
            FrameKind::Close,
            0,
            br#"{"reason":"client_close"}"#.to_vec(),
        ));
        self.stdin.take();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn send(&mut self, frame: &Frame) -> io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "envd stdin closed"))?;
        stdin.write_all(&frame.encode())?;
        stdin.flush()
    }

    fn recv(&mut self) -> io::Result<Frame> {
        Frame::decode(&mut self.stdout, EMBEDDED_MAX_FRAME)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "envd ended the stream"))
    }
}

impl Drop for EnvClient {
    fn drop(&mut self) {
        self.stdin.take();
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn service_client_sends_bearer_auth_and_reads_result() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_string();
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer secret"),
                "{request}"
            );
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let mut client = ServiceClient::new(format!("http://{address}"), "secret");
        let result = client.request("ping", json!({})).unwrap();
        assert_eq!(result["ok"], true);
        server.join().unwrap();
    }

    #[test]
    fn service_client_trusts_a_supplied_ca_for_https() {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use rustls::{ServerConfig, ServerConnection, StreamOwned};
        use std::sync::Arc;

        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let server_key = KeyPair::generate().unwrap();
        let server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();
        let ca_pem = ca_cert.pem().into_bytes();
        let config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(server_cert.der().to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
                )
                .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let connection = ServerConnection::new(Arc::new(config)).unwrap();
            let mut stream = StreamOwned::new(connection, stream);
            while stream.conn.is_handshaking() {
                stream.conn.complete_io(&mut stream.sock).unwrap();
            }
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let mut client =
            ServiceClient::new_with_ca(format!("https://localhost:{port}"), "secret", Some(ca_pem))
                .unwrap();
        let result = client.request("ping", json!({})).unwrap();
        assert_eq!(result["ok"], true);
        server.join().unwrap();
    }
}
