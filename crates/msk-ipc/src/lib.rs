//! Versioned JSON-RPC messages between the kernel daemon and its frontends.
//!
//! Transport is a newline-delimited JSON stream over a local socket: a Unix
//! domain socket or a Windows named pipe. This crate contains the wire types,
//! the local-socket helpers, and a small synchronous client.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

pub use interprocess::TryClone;
use serde::{Deserialize, Serialize};

pub mod data;

pub const JSONRPC_VERSION: &str = "2.0";
pub const IPC_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    pub params: serde_json::Value,
}

impl RpcNotification {
    pub fn new(method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.into(),
            method: method.into(),
            params,
        }
    }
}

pub mod local {
    use std::io;
    use std::path::Path;

    #[cfg(unix)]
    use interprocess::local_socket::GenericFilePath;
    #[cfg(windows)]
    use interprocess::local_socket::GenericNamespaced;
    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{ListenerOptions, Name};

    pub use interprocess::local_socket::{Listener as LocalSocketListener, Stream};

    pub fn socket_name(root: &Path) -> io::Result<Name<'static>> {
        #[cfg(windows)]
        {
            let key = stable_hash(&root.to_string_lossy());
            let name = format!("msk-kernel-{key:016x}");
            Ok(name.to_ns_name::<GenericNamespaced>()?.into_owned())
        }
        #[cfg(unix)]
        {
            let path = root.join("kernel.sock");
            Ok(path
                .to_string_lossy()
                .as_ref()
                .to_fs_name::<GenericFilePath>()?
                .into_owned())
        }
    }

    pub fn listener(root: &Path) -> io::Result<LocalSocketListener> {
        let listener = ListenerOptions::new()
            .name(socket_name(root)?)
            .try_overwrite(true)
            .create_sync()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let socket = root.join("kernel.sock");
            if socket.exists() {
                let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(listener)
    }

    pub fn connect(root: &Path) -> io::Result<Stream> {
        Stream::connect(socket_name(root)?)
    }

    pub fn data_socket_name(root: &Path) -> io::Result<Name<'static>> {
        #[cfg(windows)]
        {
            let key = stable_hash(&root.to_string_lossy());
            let name = format!("msk-kernel-data-{key:016x}");
            Ok(name.to_ns_name::<GenericNamespaced>()?.into_owned())
        }
        #[cfg(unix)]
        {
            let path = root.join("kernel.data");
            Ok(path
                .to_string_lossy()
                .as_ref()
                .to_fs_name::<GenericFilePath>()?
                .into_owned())
        }
    }

    pub fn data_listener(root: &Path) -> io::Result<LocalSocketListener> {
        let listener = ListenerOptions::new()
            .name(data_socket_name(root)?)
            .try_overwrite(true)
            .create_sync()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let socket = root.join("kernel.data");
            if socket.exists() {
                let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
            }
        }
        Ok(listener)
    }

    pub fn data_connect(root: &Path) -> io::Result<Stream> {
        Stream::connect(data_socket_name(root)?)
    }

    pub fn data_incoming(
        listener: &LocalSocketListener,
    ) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        listener.incoming()
    }

    pub fn data_display_name(root: &Path) -> String {
        #[cfg(windows)]
        {
            format!(
                "msk-kernel-data-{:016x}",
                stable_hash(&root.to_string_lossy())
            )
        }
        #[cfg(unix)]
        {
            root.join("kernel.data").display().to_string()
        }
    }

    pub fn incoming(
        listener: &LocalSocketListener,
    ) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        listener.incoming()
    }

    pub fn display_name(root: &Path) -> String {
        #[cfg(windows)]
        {
            format!("msk-kernel-{:016x}", stable_hash(&root.to_string_lossy()))
        }
        #[cfg(unix)]
        {
            root.join("kernel.sock").display().to_string()
        }
    }

    #[cfg(windows)]
    fn stable_hash(text: &str) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
        hash
    }
}

pub fn token_path(root: &Path) -> PathBuf {
    root.join("ipc.token")
}

/// Canonicalizes a path and strips the Windows `\\?\` verbatim prefix, which
/// cmd.exe and some tools do not accept as a working directory.
pub fn normalize_root(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(stripped) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    canonical
}

pub fn write_token(root: &Path, token: &str) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    let path = token_path(root);
    std::fs::write(&path, token.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn read_token(root: &Path) -> io::Result<String> {
    Ok(std::fs::read_to_string(token_path(root))?
        .trim()
        .to_string())
}

pub fn write_json_line<W: Write>(writer: &mut W, value: &serde_json::Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    writer.write_all(b"\n")?;
    writer.flush()
}

pub fn read_json_line<R: BufRead>(reader: &mut R) -> io::Result<Option<serde_json::Value>> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    serde_json::from_str(line.trim())
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub struct Client {
    root: PathBuf,
    token: String,
    writer: Arc<Mutex<local::Stream>>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<serde_json::Value>>>>,
    notifications: mpsc::Receiver<serde_json::Value>,
    next_id: u64,
    _reader: thread::JoinHandle<()>,
}

impl Client {
    pub fn connect(root: &Path) -> io::Result<Self> {
        let root = normalize_root(root);
        let token = read_token(&root)?;
        let stream = local::connect(&root)?;
        let reader_stream = stream.try_clone()?;
        let writer = Arc::new(Mutex::new(stream));
        let pending: Arc<Mutex<HashMap<u64, mpsc::Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (notification_tx, notification_rx) = mpsc::channel();
        let pending_reader = pending.clone();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(reader_stream);
            loop {
                match read_json_line(&mut reader) {
                    Ok(Some(value)) => {
                        if let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) {
                            let sender = pending_reader
                                .lock()
                                .ok()
                                .and_then(|mut map| map.remove(&id));
                            if let Some(sender) = sender {
                                let _ = sender.send(value);
                                continue;
                            }
                        }
                        let _ = notification_tx.send(value);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        });
        Ok(Self {
            root: root.to_path_buf(),
            token,
            writer,
            pending,
            notifications: notification_rx,
            next_id: 1,
            _reader: reader,
        })
    }

    pub fn hello(&mut self) -> io::Result<serde_json::Value> {
        self.request(
            methods::HELLO,
            serde_json::json!({
                "token": self.token,
                "client": "msk-client",
                "version": IPC_PROTOCOL_VERSION
            }),
        )
    }

    pub fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> io::Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let (sender, receiver) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| io::Error::other("pending map poisoned"))?
            .insert(id, sender);
        let request = RpcRequest::new(id, method, params);
        let value = serde_json::to_value(request)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        {
            let mut writer = self
                .writer
                .lock()
                .map_err(|_| io::Error::other("writer poisoned"))?;
            write_json_line(&mut *writer, &value)?;
        }
        let response = receiver
            .recv_timeout(Duration::from_secs(30))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "IPC response timeout"))?;
        if let Some(error) = response.get("error") {
            return Err(io::Error::other(error.to_string()));
        }
        Ok(response
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }

    pub fn subscribe(&mut self, task: Option<&str>) -> io::Result<u64> {
        let result = self.request(
            methods::EVENTS_SUBSCRIBE,
            serde_json::json!({"task_id": task}),
        )?;
        Ok(result
            .get("subscriber_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0))
    }

    pub fn next_notification_timeout(&self, timeout: Duration) -> Option<serde_json::Value> {
        self.notifications.recv_timeout(timeout).ok()
    }

    pub fn try_notification(&self) -> Option<serde_json::Value> {
        self.notifications.try_recv().ok()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

pub mod methods {
    pub const HELLO: &str = "kernel.hello";
    pub const TASK_CREATE: &str = "task.create";
    pub const TASK_CANCEL: &str = "task.cancel";
    pub const TASK_STATE: &str = "task.state";
    pub const TASK_LIST: &str = "task.list";
    pub const APPROVAL_LIST: &str = "approval.list";
    pub const APPROVAL_RESPOND: &str = "approval.respond";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    pub const TERMINAL_OPEN: &str = "terminal.open";
    pub const TERMINAL_WRITE: &str = "terminal.write";
    pub const TERMINAL_READ: &str = "terminal.read";
    pub const TERMINAL_RESIZE: &str = "terminal.resize";
    pub const TERMINAL_CLOSE: &str = "terminal.close";
}

pub mod events {
    pub const TASK_STATE: &str = "task.state";
    pub const TOOL_CALL: &str = "tool.call";
    pub const TOOL_RESULT: &str = "tool.result";
    pub const POLICY_DECISION: &str = "policy.decision";
    pub const APPROVAL_REQUEST: &str = "approval.request";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let req = RpcRequest::new(7, methods::TASK_CREATE, serde_json::json!({"goal": "x"}));
        let text = serde_json::to_string(&req).unwrap();
        let back: RpcRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.method, "task.create");
    }

    #[test]
    fn error_response_omits_result() {
        let res = RpcResponse::err(1, -32601, "method not found");
        let text = serde_json::to_string(&res).unwrap();
        assert!(!text.contains("result"));
        assert!(text.contains("method not found"));
    }

    #[test]
    fn notification_roundtrip() {
        let notification = RpcNotification::new("event", serde_json::json!({"x": 1}));
        let text = serde_json::to_string(&notification).unwrap();
        let back: RpcNotification = serde_json::from_str(&text).unwrap();
        assert_eq!(back.method, "event");
        assert_eq!(back.params["x"], 1);
    }

    #[test]
    fn json_lines_roundtrip() {
        let value = serde_json::json!({"jsonrpc": "2.0", "method": "ping"});
        let mut buffer = Vec::new();
        write_json_line(&mut buffer, &value).unwrap();
        let mut reader = std::io::Cursor::new(buffer);
        let back = read_json_line(&mut reader).unwrap().unwrap();
        assert_eq!(back, value);
    }
}
