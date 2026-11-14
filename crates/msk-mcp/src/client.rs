//! Embedded `envd` client: spawns the Go binary, performs the HELLO handshake,
//! and carries MCP JSON-RPC over CONTROL frames on stdin/stdout.

use std::io::{self, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

use crate::frame::{EMBEDDED_MAX_FRAME, Frame, FrameKind};

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
