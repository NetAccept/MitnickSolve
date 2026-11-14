//! Manifest, signature, artifact, and shared-library loading for kernel
//! components. This crate also hosts the minimal `llm.provider` vtable bridge.
//!
//! Safety model: signature/hash/ABI validation are install gates, not a
//! sandbox. In-process components must be first-party or audited; an
//! out-of-process host is required for untrusted code.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::io::Read;
use std::mem::{ManuallyDrop, size_of};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, VerifyingKey};
use msk_abi::{
    MS_ABI_VERSION, MS_COMPONENT_ENTRY, MS_ID_MAX_BYTES, MS_LLM_ERROR_CAPACITY,
    MS_LLM_MAX_ALLOC_BYTES, MS_LLM_MAX_ALLOC_ENTRIES, MS_LLM_MAX_OUTSTANDING_BYTES,
    MS_LLM_MAX_REQUEST_BYTES, MS_LLM_MAX_RESPONSE_BYTES, MS_LLM_VTABLE_ABI, MS_LOG_MAX_BYTES,
    MS_PORT_EVENT_SINK, MS_PORT_LLM, MS_PORT_SECRET_STORE, MS_PORT_STORE, MS_PORT_TRANSPORT,
    ms_abi_header, ms_bytes, ms_component, ms_host, ms_llm_vtable, ms_status,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
const MANIFEST_DOMAIN: &[u8] = b"msk-manifest-v1\0";
static HOST_ID: &[u8] = b"msk-kernel\0";
const HOST_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentManifest {
    pub component: ComponentInfo,
    #[serde(default)]
    pub requires: Option<Requires>,
    #[serde(default)]
    pub artifact: Option<Artifact>,
    /// Base64 Ed25519 signature over `canonical_bytes()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentInfo {
    pub id: String,
    pub version: String,
    pub abi: u32,
    pub port: String,
    pub publisher: String,
    #[serde(default = "default_thread_safety")]
    pub thread_safety: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub kernel: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub file: String,
    pub sha256: String,
}

/// Publisher id -> base64 Ed25519 public key (32 bytes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub publishers: HashMap<String, String>,
}

fn default_thread_safety() -> String {
    "serial".into()
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("unsupported ABI: {0}")]
    Abi(u32),
    #[error("sealed port cannot be implemented by a component: {0}")]
    SealedPort(String),
    #[error("unknown port: {0}")]
    UnknownPort(String),
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("manifest signature is missing")]
    SignatureMissing,
    #[error("manifest signature is invalid")]
    SignatureInvalid,
    #[error("publisher is not trusted: {0}")]
    UnknownPublisher(String),
    #[error("publisher key: {0}")]
    Key(String),
    #[error("artifact hash mismatch: expected {expected}, got {actual}")]
    ArtifactHash { expected: String, actual: String },
    #[error("dynamic load: {0}")]
    Load(String),
    #[error("component init failed: {0}")]
    Init(i32),
    #[error("component is not supported: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub enum LlmCallError {
    Status { code: i32, message: String },
    Protocol(String),
}

impl std::fmt::Display for LlmCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { code, message } if message.is_empty() => {
                write!(formatter, "component status {code}")
            }
            Self::Status { code, message } => {
                write!(formatter, "component status {code}: {message}")
            }
            Self::Protocol(message) => write!(formatter, "component protocol: {message}"),
        }
    }
}

impl std::error::Error for LlmCallError {}

pub fn validate_port(port: &str) -> Result<(), LoadError> {
    match port {
        "llm.provider" | "transport.http" | "store" | "event.sink" | "secret.store" => Ok(()),
        "policy" | "capability" | "scheduler" | "audit" | "wasm.runtime" => {
            Err(LoadError::SealedPort(port.into()))
        }
        other => Err(LoadError::UnknownPort(other.into())),
    }
}

fn port_constant(port: &str) -> Option<u32> {
    match port {
        "llm.provider" => Some(MS_PORT_LLM),
        "transport.http" => Some(MS_PORT_TRANSPORT),
        "store" => Some(MS_PORT_STORE),
        "event.sink" => Some(MS_PORT_EVENT_SINK),
        "secret.store" => Some(MS_PORT_SECRET_STORE),
        _ => None,
    }
}

impl ComponentManifest {
    pub fn validate(&self) -> Result<(), LoadError> {
        if self.component.abi != MS_ABI_VERSION {
            return Err(LoadError::Abi(self.component.abi));
        }
        if self.component.id.is_empty() || self.component.version.is_empty() {
            return Err(LoadError::Manifest("id and version are required".into()));
        }
        if self.component.publisher.is_empty() {
            return Err(LoadError::Manifest("publisher is required".into()));
        }
        if self.component.id.len() > MS_ID_MAX_BYTES
            || self.component.version.len() > MS_ID_MAX_BYTES
        {
            return Err(LoadError::Manifest("id/version is too long".into()));
        }
        validate_port(&self.component.port)?;
        match self.component.thread_safety.as_str() {
            "safe" | "serial" => {}
            other => {
                return Err(LoadError::Manifest(format!(
                    "unsupported thread_safety: {other}"
                )));
            }
        }
        if let Some(artifact) = &self.artifact {
            if artifact.sha256.len() != 64
                || !artifact.sha256.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(LoadError::Manifest(
                    "artifact sha256 must be 64 hex chars".into(),
                ));
            }
            if artifact.file.is_empty()
                || artifact.file.contains(['/', '\\', ':'])
                || Path::new(&artifact.file).is_absolute()
            {
                return Err(LoadError::Manifest(
                    "artifact file must be a plain file name".into(),
                ));
            }
        }
        Ok(())
    }

    /// Serializes the manifest with the signature field removed, prefixed by a
    /// domain tag so these bytes can never be confused with another protocol.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LoadError> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        let mut bytes = MANIFEST_DOMAIN.to_vec();
        bytes.extend(
            serde_json::to_vec(&unsigned).map_err(|err| LoadError::Manifest(err.to_string()))?,
        );
        Ok(bytes)
    }
}

impl TrustStore {
    pub fn from_json(text: &str) -> Result<Self, LoadError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(LoadError::Manifest("trust store is too large".into()));
        }
        serde_json::from_str(text).map_err(|err| LoadError::Manifest(err.to_string()))
    }

    pub fn verifying_key(&self, publisher: &str) -> Result<VerifyingKey, LoadError> {
        let encoded = self
            .publishers
            .get(publisher)
            .ok_or_else(|| LoadError::UnknownPublisher(publisher.to_string()))?;
        let bytes = BASE64
            .decode(encoded)
            .map_err(|err| LoadError::Key(err.to_string()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| LoadError::Key("expected a 32-byte Ed25519 key".into()))?;
        let key =
            VerifyingKey::from_bytes(&bytes).map_err(|err| LoadError::Key(err.to_string()))?;
        if key.is_weak() {
            return Err(LoadError::Key("weak Ed25519 key".into()));
        }
        Ok(key)
    }

    pub fn verify(&self, manifest: &ComponentManifest) -> Result<(), LoadError> {
        manifest.validate()?;
        let encoded = manifest
            .signature
            .as_deref()
            .ok_or(LoadError::SignatureMissing)?;
        let signature_bytes = BASE64
            .decode(encoded)
            .map_err(|err| LoadError::Manifest(format!("signature base64: {err}")))?;
        if signature_bytes.len() != 64 {
            return Err(LoadError::Manifest("signature must be 64 bytes".into()));
        }
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|err| LoadError::Manifest(format!("signature bytes: {err}")))?;
        let key = self.verifying_key(&manifest.component.publisher)?;
        key.verify_strict(&manifest.canonical_bytes()?, &signature)
            .map_err(|_| LoadError::SignatureInvalid)
    }
}

impl Artifact {
    /// Verifies the artifact's SHA-256 and returns its canonical path. Paths
    /// escaping `base_dir`, non-regular files, and oversized files are rejected.
    pub fn verify(&self, base_dir: &Path) -> Result<PathBuf, LoadError> {
        if self.file.is_empty()
            || self.file.contains(['/', '\\', ':'])
            || Path::new(&self.file).is_absolute()
        {
            return Err(LoadError::Manifest(
                "artifact file must be a plain file name".into(),
            ));
        }
        let canonical_base = base_dir.canonicalize()?;
        let candidate = canonical_base.join(&self.file);
        let metadata = std::fs::metadata(&candidate)?;
        if !metadata.is_file() {
            return Err(LoadError::Manifest("artifact is not a regular file".into()));
        }
        if metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(LoadError::Manifest("artifact is too large".into()));
        }
        let canonical_path = candidate.canonicalize()?;
        if !canonical_path.starts_with(&canonical_base) {
            return Err(LoadError::Manifest(
                "artifact escapes the component directory".into(),
            ));
        }
        let mut file = std::fs::File::open(&canonical_path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = hex_digest(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(&self.sha256) {
            return Err(LoadError::ArtifactHash {
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(canonical_path)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Verifies the signature, trust, artifact name, and artifact hash without
/// loading code. Used for process-host mode and install-time checks.
pub fn verify_component(
    manifest: &ComponentManifest,
    trust: &TrustStore,
    base_dir: &Path,
) -> Result<PathBuf, LoadError> {
    trust.verify(manifest)?;
    let artifact = manifest
        .artifact
        .as_ref()
        .ok_or_else(|| LoadError::Manifest("artifact is required for activation".into()))?;
    artifact.verify(base_dir)
}

/// A verified, loaded component library. Dropping it deliberately does not
/// unload the library: `dlclose`/`FreeLibrary` after component code has run is
/// unsafe and the architecture never unloads components.
pub struct LoadedComponent {
    _library: ManuallyDrop<libloading::Library>,
    component: ms_component,
    vtable: ms_llm_vtable,
    id: String,
    version: String,
}

impl LoadedComponent {
    pub fn load(
        manifest: &ComponentManifest,
        trust: &TrustStore,
        base_dir: &Path,
    ) -> Result<Self, LoadError> {
        if size_of::<usize>() != 8 {
            return Err(LoadError::Unsupported(
                "component activation currently requires a 64-bit target".into(),
            ));
        }
        let path = verify_component(manifest, trust, base_dir)?;

        let library = ManuallyDrop::new(
            unsafe { libloading::Library::new(&path) }
                .map_err(|err| LoadError::Load(err.to_string()))?,
        );
        let entry: libloading::Symbol<unsafe extern "C" fn() -> *const ms_component> =
            unsafe { library.get(MS_COMPONENT_ENTRY) }
                .map_err(|err| LoadError::Load(err.to_string()))?;
        let raw = unsafe { entry() };
        if raw.is_null() {
            return Err(LoadError::Load("component entry returned null".into()));
        }
        if (raw as usize) % std::mem::align_of::<ms_component>() != 0 {
            return Err(LoadError::Manifest(
                "component pointer is misaligned".into(),
            ));
        }
        let header = unsafe { ptr::read_unaligned(raw.cast::<ms_abi_header>()) };
        if header.abi_version != MS_ABI_VERSION {
            return Err(LoadError::Abi(header.abi_version));
        }
        if header.struct_size < size_of::<ms_component>() as u32 {
            return Err(LoadError::Manifest(
                "component struct_size is too small".into(),
            ));
        }
        let component = unsafe { ptr::read_unaligned(raw) };
        let expected_port = port_constant(&manifest.component.port)
            .ok_or_else(|| LoadError::UnknownPort(manifest.component.port.clone()))?;
        if component.port_kind != expected_port || component.port_kind != MS_PORT_LLM {
            return Err(LoadError::Unsupported(format!(
                "loader v1 only activates llm.provider, got port {}",
                component.port_kind
            )));
        }
        let id = read_bounded_cstr(component.header.id)?;
        let version = read_bounded_cstr(component.header.version)?;
        if id != manifest.component.id {
            return Err(LoadError::Manifest(format!(
                "component id mismatch: manifest {}, library {id}",
                manifest.component.id
            )));
        }
        if version != manifest.component.version {
            return Err(LoadError::Manifest(format!(
                "component version mismatch: manifest {}, library {version}",
                manifest.component.version
            )));
        }
        let vtable = read_llm_vtable(component.vtable)?;
        Ok(Self {
            _library: library,
            component,
            vtable,
            id,
            version,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn port_kind(&self) -> u32 {
        self.component.port_kind
    }

    pub fn vtable_abi(&self) -> u32 {
        self.vtable.header.abi_version
    }

    /// Calls `init` exactly once and returns a serialized call instance. On
    /// failure, `shutdown` is never called and the library stays loaded.
    pub fn into_llm_instance(
        self,
        config: HashMap<String, String>,
    ) -> Result<LlmInstance, LoadError> {
        if size_of::<usize>() != 8 {
            return Err(LoadError::Unsupported(
                "component activation currently requires a 64-bit target".into(),
            ));
        }
        let init = self
            .component
            .init
            .ok_or_else(|| LoadError::Manifest("component init is required".into()))?;
        let mut host_ctx = Box::new(HostCtx::new(config));
        let host_ctx_ptr = (&mut *host_ctx) as *mut HostCtx as *mut c_void;
        let host = Box::new(ms_host {
            header: ms_abi_header {
                abi_version: MS_ABI_VERSION,
                struct_size: size_of::<ms_host>() as u32,
                id: HOST_ID.as_ptr(),
                version: HOST_VERSION.as_ptr(),
            },
            ctx: host_ctx_ptr,
            alloc: Some(host_alloc),
            free: Some(host_free),
            log: Some(host_log),
            config_get: Some(host_config_get),
            now_unix_ms: Some(host_now_unix_ms),
        });
        let mut component_ctx = ptr::null_mut();
        let status = unsafe { init(&*host, &mut component_ctx) };
        if status != 0 {
            return Err(LoadError::Init(status));
        }
        Ok(LlmInstance {
            _library: self._library,
            component: self.component,
            vtable: self.vtable,
            ctx: component_ctx,
            host,
            host_ctx,
            id: self.id,
            version: self.version,
            initialized: true,
            call_lock: Mutex::new(()),
        })
    }
}

fn read_bounded_cstr(pointer: *const u8) -> Result<String, LoadError> {
    if pointer.is_null() {
        return Err(LoadError::Manifest("null component id/version".into()));
    }
    let mut bytes = Vec::new();
    for index in 0..=MS_ID_MAX_BYTES {
        if index == MS_ID_MAX_BYTES {
            return Err(LoadError::Manifest(
                "component id/version is not NUL-terminated within 256 bytes".into(),
            ));
        }
        let byte = unsafe { *pointer.add(index) };
        if byte == 0 {
            return String::from_utf8(bytes)
                .map_err(|err| LoadError::Manifest(format!("component id/version: {err}")));
        }
        bytes.push(byte);
    }
    unreachable!()
}

fn read_llm_vtable(pointer: *const c_void) -> Result<ms_llm_vtable, LoadError> {
    if pointer.is_null() {
        return Err(LoadError::Manifest("component vtable is null".into()));
    }
    if (pointer as usize) % std::mem::align_of::<ms_llm_vtable>() != 0 {
        return Err(LoadError::Manifest("component vtable is misaligned".into()));
    }
    let header = unsafe { ptr::read_unaligned(pointer.cast::<msk_abi::ms_vtable_header>()) };
    if header.abi_version != MS_LLM_VTABLE_ABI {
        return Err(LoadError::Manifest(format!(
            "unsupported LLM vtable ABI {}",
            header.abi_version
        )));
    }
    if header.struct_size < size_of::<ms_llm_vtable>() as u32 {
        return Err(LoadError::Manifest(
            "LLM vtable struct_size is too small".into(),
        ));
    }
    if header.port_kind != MS_PORT_LLM {
        return Err(LoadError::Manifest("LLM vtable port mismatch".into()));
    }
    if header.reserved != 0 {
        return Err(LoadError::Manifest(
            "LLM vtable reserved header is nonzero".into(),
        ));
    }
    let vtable = unsafe { ptr::read_unaligned(pointer.cast::<ms_llm_vtable>()) };
    if vtable.complete.is_none() {
        return Err(LoadError::Manifest("LLM vtable complete is null".into()));
    }
    if vtable.reserved.iter().any(|slot| !slot.is_null()) {
        return Err(LoadError::Manifest(
            "LLM vtable reserved slots must be null".into(),
        ));
    }
    Ok(vtable)
}

struct HostCtx {
    allocations: Mutex<HashMap<usize, usize>>,
    outstanding: AtomicUsize,
    config: HashMap<String, String>,
}

impl HostCtx {
    fn new(config: HashMap<String, String>) -> Self {
        Self {
            allocations: Mutex::new(HashMap::new()),
            outstanding: AtomicUsize::new(0),
            config,
        }
    }

    fn release(&self, pointer: *mut c_void) {
        if pointer.is_null() {
            return;
        }
        let removed = self
            .allocations
            .lock()
            .ok()
            .and_then(|mut allocations| allocations.remove(&(pointer as usize)));
        if let Some(length) = removed {
            self.outstanding.fetch_sub(length, Ordering::SeqCst);
            deallocate(pointer, length);
        }
    }
}

fn deallocate(pointer: *mut c_void, length: usize) {
    if length == 0 {
        return;
    }
    if let Ok(layout) = std::alloc::Layout::from_size_align(length, 16) {
        unsafe { std::alloc::dealloc(pointer.cast(), layout) };
    }
}

unsafe extern "C" fn host_alloc(ctx: *mut c_void, len: usize) -> *mut c_void {
    if ctx.is_null() || len == 0 || len > MS_LLM_MAX_ALLOC_BYTES {
        return ptr::null_mut();
    }
    let host = unsafe { &*(ctx.cast::<HostCtx>()) };
    let mut allocations = match host.allocations.lock() {
        Ok(allocations) => allocations,
        Err(_) => return ptr::null_mut(),
    };
    if allocations.len() >= MS_LLM_MAX_ALLOC_ENTRIES {
        return ptr::null_mut();
    }
    if host.outstanding.load(Ordering::SeqCst).saturating_add(len) > MS_LLM_MAX_OUTSTANDING_BYTES {
        return ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len, 16) else {
        return ptr::null_mut();
    };
    let pointer = unsafe { std::alloc::alloc(layout) };
    if pointer.is_null() {
        return ptr::null_mut();
    }
    allocations.insert(pointer as usize, len);
    host.outstanding.fetch_add(len, Ordering::SeqCst);
    pointer.cast()
}

unsafe extern "C" fn host_free(ctx: *mut c_void, pointer: *mut c_void, _len: usize) {
    if ctx.is_null() || pointer.is_null() {
        return;
    }
    let host = unsafe { &*(ctx.cast::<HostCtx>()) };
    host.release(pointer);
}

unsafe extern "C" fn host_log(ctx: *mut c_void, level: i32, msg: *const u8, len: usize) {
    if msg.is_null() || len == 0 {
        return;
    }
    let len = len.min(MS_LOG_MAX_BYTES);
    let bytes = unsafe { std::slice::from_raw_parts(msg, len) };
    let text: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let _ = ctx;
    eprintln!("component[{}]: {}", level_name(level), text);
}

fn level_name(level: i32) -> &'static str {
    match level {
        0 => "error",
        1 => "warn",
        2 => "info",
        3 => "debug",
        _ => "info",
    }
}

unsafe extern "C" fn host_config_get(
    ctx: *mut c_void,
    key: *const u8,
    key_len: usize,
    buf: *mut u8,
    len: *mut usize,
) -> i32 {
    if ctx.is_null() || key.is_null() || len.is_null() || key_len > MS_ID_MAX_BYTES {
        return ms_status::InvalidArgument as i32;
    }
    let host = unsafe { &*(ctx.cast::<HostCtx>()) };
    let key = match std::str::from_utf8(unsafe { std::slice::from_raw_parts(key, key_len) }) {
        Ok(key) => key,
        Err(_) => return ms_status::InvalidArgument as i32,
    };
    let Some(value) = host.config.get(key) else {
        return ms_status::NotFound as i32;
    };
    let required = value.len() + 1;
    if buf.is_null() || unsafe { *len } < required {
        unsafe { *len = required };
        return ms_status::InvalidArgument as i32;
    }
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr(), buf, value.len());
        *buf.add(value.len()) = 0;
        *len = required;
    }
    ms_status::Ok as i32
}

unsafe extern "C" fn host_now_unix_ms(_ctx: *mut c_void, out: *mut i64) -> i32 {
    if out.is_null() {
        return ms_status::InvalidArgument as i32;
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    unsafe { *out = millis };
    ms_status::Ok as i32
}

/// Serialized LLM component instance. Calls are serialized in v1 even when the
/// manifest says `thread_safety: safe`.
pub struct LlmInstance {
    _library: ManuallyDrop<libloading::Library>,
    #[allow(dead_code)]
    component: ms_component,
    vtable: ms_llm_vtable,
    ctx: *mut c_void,
    host: Box<ms_host>,
    host_ctx: Box<HostCtx>,
    id: String,
    version: String,
    initialized: bool,
    call_lock: Mutex<()>,
}

// The instance is only mutated through `&self` under `call_lock`, and the
// provider wraps it in a Mutex. The raw pointers are stable for the lifetime of
// the loaded library and host boxes.
unsafe impl Send for LlmInstance {}

impl LlmInstance {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn provider_id(&self) -> String {
        format!("component:{}", self.id)
    }

    pub fn outstanding_bytes(&self) -> usize {
        self.host_ctx.outstanding.load(Ordering::SeqCst)
    }

    pub fn complete(&self, request: &[u8]) -> Result<Vec<u8>, LlmCallError> {
        if request.len() > MS_LLM_MAX_REQUEST_BYTES {
            return Err(LlmCallError::Protocol("request exceeds 16 MiB".into()));
        }
        let Some(complete) = self.vtable.complete else {
            return Err(LlmCallError::Protocol("complete is null".into()));
        };
        let _guard = self
            .call_lock
            .lock()
            .map_err(|_| LlmCallError::Protocol("call lock poisoned".into()))?;
        let mut response = ms_bytes {
            ptr: ptr::null_mut(),
            len: 0,
        };
        let mut error = [0u8; MS_LLM_ERROR_CAPACITY];
        let mut error_len = 0usize;
        let status = unsafe {
            complete(
                self.ctx,
                request.as_ptr(),
                request.len(),
                &mut response,
                error.as_mut_ptr(),
                error.len(),
                &mut error_len,
            )
        };
        let message = error_message(&error, error_len);
        if status != 0 {
            // A registered buffer is safe to free even on error; an
            // unregistered pointer is never touched. This prevents a buggy
            // component from permanently exhausting the allocation quota.
            if !response.ptr.is_null() && self.registered_capacity(response.ptr).is_some() {
                self.release_response(response.ptr);
            }
            return Err(LlmCallError::Status {
                code: status,
                message,
            });
        }
        if response.ptr.is_null() {
            return Err(LlmCallError::Protocol("null response".into()));
        }
        let Some(capacity) = self.registered_capacity(response.ptr) else {
            return Err(LlmCallError::Protocol(
                "response was not allocated by the host".into(),
            ));
        };
        if response.len == 0 {
            self.release_response(response.ptr);
            return Err(LlmCallError::Protocol("empty response".into()));
        }
        if response.len > MS_LLM_MAX_RESPONSE_BYTES || response.len > capacity {
            self.release_response(response.ptr);
            return Err(LlmCallError::Protocol(format!(
                "response length {} exceeds {}",
                response.len,
                capacity.min(MS_LLM_MAX_RESPONSE_BYTES)
            )));
        }
        let bytes = unsafe { std::slice::from_raw_parts(response.ptr, response.len).to_vec() };
        self.release_response(response.ptr);
        Ok(bytes)
    }

    fn registered_capacity(&self, pointer: *mut u8) -> Option<usize> {
        self.host_ctx
            .allocations
            .lock()
            .ok()
            .and_then(|allocations| allocations.get(&(pointer as usize)).copied())
    }

    fn release_response(&self, pointer: *mut u8) {
        if let Some(free) = self.host.free {
            unsafe { free(self.host.ctx, pointer.cast(), 0) };
        }
    }
}

fn error_message(buffer: &[u8], length: usize) -> String {
    let length = length.min(buffer.len());
    String::from_utf8_lossy(&buffer[..length])
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(1024)
        .collect()
}

impl Drop for LlmInstance {
    fn drop(&mut self) {
        if self.initialized {
            if let Some(shutdown) = self.component.shutdown {
                unsafe { shutdown(self.ctx) };
            }
            self.initialized = false;
        }
        // host and host_ctx are dropped after this body; shutdown has returned.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn manifest(port: &str) -> ComponentManifest {
        ComponentManifest {
            component: ComponentInfo {
                id: "test.llm".into(),
                version: "1.0.0".into(),
                abi: MS_ABI_VERSION,
                port: port.into(),
                publisher: "acme".into(),
                thread_safety: "serial".into(),
            },
            requires: None,
            artifact: None,
            signature: None,
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn trust_for(key: &SigningKey) -> TrustStore {
        let mut trust = TrustStore::default();
        trust
            .publishers
            .insert("acme".into(), BASE64.encode(key.verifying_key().to_bytes()));
        trust
    }

    fn sign(mut manifest: ComponentManifest) -> ComponentManifest {
        let signing = key();
        let signature = signing.sign(&manifest.canonical_bytes().unwrap());
        manifest.signature = Some(BASE64.encode(signature.to_bytes()));
        manifest
    }

    #[test]
    fn open_port_is_allowed() {
        assert!(manifest("llm.provider").validate().is_ok());
    }

    #[test]
    fn sealed_port_is_rejected() {
        assert!(matches!(
            manifest("policy").validate(),
            Err(LoadError::SealedPort(_))
        ));
    }

    #[test]
    fn unknown_port_is_rejected() {
        assert!(matches!(
            manifest("made.up").validate(),
            Err(LoadError::UnknownPort(_))
        ));
    }

    #[test]
    fn signature_verifies_for_a_trusted_publisher() {
        let manifest = sign(manifest("llm.provider"));
        assert!(trust_for(&key()).verify(&manifest).is_ok());
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let mut manifest = sign(manifest("llm.provider"));
        manifest.component.version = "2.0.0".into();
        assert!(matches!(
            trust_for(&key()).verify(&manifest),
            Err(LoadError::SignatureInvalid)
        ));
    }

    #[test]
    fn untrusted_publisher_is_rejected() {
        let manifest = sign(manifest("llm.provider"));
        assert!(matches!(
            TrustStore::default().verify(&manifest),
            Err(LoadError::UnknownPublisher(_))
        ));
    }

    #[test]
    fn signature_is_required() {
        assert!(matches!(
            trust_for(&key()).verify(&manifest("llm.provider")),
            Err(LoadError::SignatureMissing)
        ));
    }

    #[test]
    fn artifact_hash_and_name_are_checked() {
        let dir = std::env::temp_dir().join(format!("msk-loader-{}", nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("component.bin"), b"hello").unwrap();
        let mut component_manifest = manifest("llm.provider");
        component_manifest.artifact = Some(Artifact {
            file: "component.bin".into(),
            sha256: hex_digest(&Sha256::digest(b"hello")),
        });
        assert!(
            component_manifest
                .artifact
                .as_ref()
                .unwrap()
                .verify(&dir)
                .is_ok()
        );

        component_manifest.artifact.as_mut().unwrap().sha256 = "0".repeat(64);
        assert!(matches!(
            component_manifest.artifact.as_ref().unwrap().verify(&dir),
            Err(LoadError::ArtifactHash { .. })
        ));
        component_manifest.artifact.as_mut().unwrap().file = "../escape".into();
        assert!(
            component_manifest
                .artifact
                .as_ref()
                .unwrap()
                .verify(&dir)
                .is_err()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }

    #[test]
    fn canonical_bytes_include_domain_and_exclude_signature() {
        let unsigned = manifest("llm.provider");
        let signed = sign(unsigned.clone());
        assert_eq!(
            unsigned.canonical_bytes().unwrap(),
            signed.canonical_bytes().unwrap()
        );
        assert!(
            unsigned
                .canonical_bytes()
                .unwrap()
                .starts_with(MANIFEST_DOMAIN)
        );
    }

    fn looks_like_component(name: &str) -> bool {
        name.contains("msk_test_component")
            && (name.ends_with(".dll") || name.ends_with(".so") || name.ends_with(".dylib"))
    }

    fn component_artifact() -> Option<PathBuf> {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .canonicalize()
            .ok()?;
        for profile in ["debug", "release"] {
            for directory in [target.join(profile), target.join(profile).join("deps")] {
                if let Ok(entries) = std::fs::read_dir(directory) {
                    for entry in entries.flatten() {
                        if looks_like_component(&entry.file_name().to_string_lossy()) {
                            return Some(entry.path());
                        }
                    }
                }
            }
            let build_dir = target.join(profile).join("build");
            if let Ok(packages) = std::fs::read_dir(build_dir) {
                for package in packages.flatten() {
                    let mut out_dirs = vec![package.path().join("out")];
                    if let Ok(children) = std::fs::read_dir(package.path()) {
                        for child in children.flatten() {
                            out_dirs.push(child.path().join("out"));
                        }
                    }
                    for out_dir in out_dirs {
                        if let Ok(files) = std::fs::read_dir(out_dir) {
                            for file in files.flatten() {
                                if looks_like_component(&file.file_name().to_string_lossy()) {
                                    return Some(file.path());
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn component_manifest(artifact: &Path) -> ComponentManifest {
        let file_name = artifact.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(artifact).unwrap();
        let mut manifest = manifest("llm.provider");
        manifest.artifact = Some(Artifact {
            file: file_name,
            sha256: hex_digest(&Sha256::digest(&bytes)),
        });
        sign(manifest)
    }

    #[test]
    fn loads_and_calls_the_test_llm_component() {
        let Some(path) = component_artifact() else {
            if std::env::var("CI").is_ok() {
                panic!("msk-test-component artifact is missing in CI");
            }
            eprintln!("msk-test-component artifact not built; skipping load test");
            return;
        };
        let manifest = component_manifest(&path);
        let loaded =
            LoadedComponent::load(&manifest, &trust_for(&key()), path.parent().unwrap()).unwrap();
        assert_eq!(loaded.id(), "test.llm");
        assert_eq!(loaded.port_kind(), MS_PORT_LLM);
        let instance = loaded.into_llm_instance(HashMap::new()).unwrap();
        let response = instance.complete(br#"{"mode":"ok"}"#).unwrap();
        let text = String::from_utf8(response).unwrap();
        assert!(text.contains("component hello"), "{text}");
        assert_eq!(instance.outstanding_bytes(), 0);
        drop(instance);
    }

    #[test]
    fn malformed_component_responses_are_rejected() {
        let Some(path) = component_artifact() else {
            return;
        };
        let manifest = component_manifest(&path);
        let loaded =
            LoadedComponent::load(&manifest, &trust_for(&key()), path.parent().unwrap()).unwrap();
        let instance = loaded.into_llm_instance(HashMap::new()).unwrap();
        assert!(matches!(
            instance.complete(br#"{"mode":"empty"}"#),
            Err(LlmCallError::Protocol(_))
        ));
        assert!(matches!(
            instance.complete(br#"{"mode":"oversize"}"#),
            Err(LlmCallError::Protocol(_))
        ));
        assert!(matches!(
            instance.complete(br#"{"mode":"unregistered"}"#),
            Err(LlmCallError::Protocol(_))
        ));
        assert!(matches!(
            instance.complete(br#"{"mode":"error"}"#),
            Err(LlmCallError::Status { code: -1, .. })
        ));
        assert_eq!(instance.outstanding_bytes(), 0);
    }

    #[test]
    fn manifest_mismatch_fails_before_init() {
        let Some(path) = component_artifact() else {
            return;
        };
        let mut manifest = component_manifest(&path);
        manifest.component.version = "9.9.9".into();
        let manifest = sign(manifest);
        let error = LoadedComponent::load(&manifest, &trust_for(&key()), path.parent().unwrap())
            .err()
            .expect("manifest mismatch should fail");
        assert!(matches!(error, LoadError::Manifest(_)));
    }
}
