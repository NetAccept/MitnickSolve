//! WASM tool runtime.
//!
//! M0 uses the same JSON-in-linear-memory ABI as the envd drafts: the host
//! writes the input at offset 0, the guest exports `run(ptr, len) -> i64`
//! returning `(output_ptr << 32) | output_len`, and the host reads the result.
//! A guest may import `msk.tool_call` to reach the kernel's gated host bridge
//! (M0: envd `exec`). Fresh stores, fuel, and a memory cap are applied per call.

use std::path::Path;
use std::sync::Arc;

use msk_core::Capability;
use serde::{Deserialize, Serialize};
use wasmtime::{
    Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
};

const MAX_MEMORY: usize = 64 * 1024 * 1024;
const MAX_INPUT: usize = 256 * 1024;
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const OUTPUT_OFFSET: usize = 4096;
const DEFAULT_FUEL: u64 = 50_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub max_memory_bytes: Option<u64>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl ToolManifest {
    pub fn validate(&self) -> Result<(), WasmError> {
        if self.name.is_empty() {
            return Err(WasmError::InvalidManifest("empty tool name".into()));
        }
        if self.input_schema.is_null() {
            return Err(WasmError::InvalidManifest("missing input schema".into()));
        }
        Ok(())
    }
}

/// The tool identity and capabilities granted by the kernel for one invocation.
/// The guest cannot add capabilities; it only receives what the kernel grants.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    pub tool: String,
    pub capabilities: Vec<Capability>,
}

impl ToolContext {
    pub fn new(tool: impl Into<String>, capabilities: Vec<Capability>) -> Self {
        Self {
            tool: tool.into(),
            capabilities,
        }
    }

    pub fn allows(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// Kernel-side bridge for host imports. M0 wires this to envd `exec`; the host
/// implementation must check `ctx` before performing any side effect.
pub trait ToolHost: Send + Sync {
    fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String>;
}

#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("trap: {0}")]
    Trap(String),
    #[error("timeout")]
    Timeout,
    #[error("engine: {0}")]
    Engine(String),
}

pub trait WasmToolRuntime: Send + Sync {
    fn run(
        &self,
        manifest: &ToolManifest,
        ctx: &ToolContext,
        input: &[u8],
    ) -> Result<Vec<u8>, WasmError>;
}

struct HostState {
    host: Arc<dyn ToolHost>,
    ctx: ToolContext,
    limits: StoreLimits,
}

pub struct WasmtimeRuntime {
    engine: Engine,
    module: Module,
    host: Arc<dyn ToolHost>,
    fuel: u64,
}

impl WasmtimeRuntime {
    pub fn from_file(host: Arc<dyn ToolHost>, path: impl AsRef<Path>) -> Result<Self, WasmError> {
        let bytes =
            std::fs::read(path.as_ref()).map_err(|err| WasmError::Engine(err.to_string()))?;
        Self::from_bytes(host, &bytes)
    }

    pub fn from_wat(host: Arc<dyn ToolHost>, wat: &str) -> Result<Self, WasmError> {
        Self::from_bytes(host, wat.as_bytes())
    }

    pub fn from_bytes(host: Arc<dyn ToolHost>, bytes: &[u8]) -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|err| WasmError::Engine(err.to_string()))?;
        let module =
            Module::new(&engine, bytes).map_err(|err| WasmError::Engine(err.to_string()))?;
        Ok(Self {
            engine,
            module,
            host,
            fuel: DEFAULT_FUEL,
        })
    }

    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    pub fn run(&self, input: &[u8]) -> Result<Vec<u8>, WasmError> {
        self.run_with_context(&ToolContext::default(), input)
    }

    pub fn run_with_context(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, WasmError> {
        if input.len() > MAX_INPUT {
            return Err(WasmError::Trap("input exceeds 256 KiB".into()));
        }

        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        linker
            .func_wrap(
                "msk",
                "tool_call",
                |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| -> wasmtime::Result<i64> {
                    let input = read_guest(&mut caller, ptr, len)?;
                    let output = {
                        let state = caller.data();
                        state.host.call(&state.ctx, &input)
                    }
                    .map_err(wasmtime::Error::msg)?;
                    write_guest(&mut caller, &output)
                },
            )
            .map_err(|err| WasmError::Engine(err.to_string()))?;

        let limits = StoreLimitsBuilder::new().memory_size(MAX_MEMORY).build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                host: self.host.clone(),
                ctx: ctx.clone(),
                limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.fuel)
            .map_err(|err| WasmError::Engine(err.to_string()))?;

        let instance = linker
            .instantiate(&mut store, &self.module)
            .map_err(|err| WasmError::Trap(err.to_string()))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Trap("missing memory export".into()))?;

        ensure_memory(&mut store, &memory, input.len())?;
        memory
            .write(&mut store, 0, input)
            .map_err(|err| WasmError::Trap(err.to_string()))?;

        let run = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "run")
            .map_err(|err| WasmError::Trap(err.to_string()))?;
        let packed = run
            .call(&mut store, (0, input.len() as i32))
            .map_err(|err| WasmError::Trap(format!("{err:#}")))?;

        let ptr = ((packed as u64) >> 32) as u32 as usize;
        let len = ((packed as u64) & 0xffff_ffff) as u32 as usize;
        if len > MAX_OUTPUT {
            return Err(WasmError::Trap("output exceeds 4 MiB".into()));
        }
        let data = memory.data(&store);
        let end = ptr
            .checked_add(len)
            .ok_or_else(|| WasmError::Trap("output pointer overflow".into()))?;
        if end > data.len() {
            return Err(WasmError::Trap("output out of bounds".into()));
        }
        Ok(data[ptr..end].to_vec())
    }
}

impl WasmToolRuntime for WasmtimeRuntime {
    fn run(
        &self,
        _manifest: &ToolManifest,
        ctx: &ToolContext,
        input: &[u8],
    ) -> Result<Vec<u8>, WasmError> {
        self.run_with_context(ctx, input)
    }
}

/// Placeholder used before the runtime is configured.
pub struct UnsupportedRuntime;

impl WasmToolRuntime for UnsupportedRuntime {
    fn run(
        &self,
        _manifest: &ToolManifest,
        _ctx: &ToolContext,
        _input: &[u8],
    ) -> Result<Vec<u8>, WasmError> {
        Err(WasmError::Unsupported(
            "wasmtime runtime is not configured".into(),
        ))
    }
}

fn ensure_memory(
    store: &mut Store<HostState>,
    memory: &Memory,
    needed: usize,
) -> Result<(), WasmError> {
    let current = memory.data(&*store).len();
    if needed > current {
        let pages = ((needed - current) as u64).div_ceil(65536);
        memory
            .grow(&mut *store, pages)
            .map_err(|err| WasmError::Trap(err.to_string()))?;
    }
    Ok(())
}

fn read_guest(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> wasmtime::Result<Vec<u8>> {
    if ptr < 0 || len < 0 || len as usize > MAX_INPUT {
        return Err(wasmtime::Error::msg("invalid guest buffer"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("missing memory export"))?;
    let mut buf = vec![0u8; len as usize];
    memory.read(&mut *caller, ptr as usize, &mut buf)?;
    Ok(buf)
}

fn write_guest(caller: &mut Caller<'_, HostState>, data: &[u8]) -> wasmtime::Result<i64> {
    if data.len() > MAX_OUTPUT {
        return Err(wasmtime::Error::msg("host output exceeds 4 MiB"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(|export| export.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("missing memory export"))?;

    let needed = OUTPUT_OFFSET + data.len();
    let current = memory.data(&*caller).len();
    if needed > current {
        let pages = ((needed - current) as u64).div_ceil(65536);
        memory.grow(&mut *caller, pages)?;
    }
    memory.write(&mut *caller, OUTPUT_OFFSET, data)?;
    Ok(((OUTPUT_OFFSET as i64) << 32) | data.len() as i64)
}

#[cfg(test)]
const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 16)
  (func (export "run") (param i32 i32) (result i64)
    (memory.copy (i32.const 4096) (local.get 0) (local.get 1))
    (i64.or
      (i64.shl (i64.const 4096) (i64.const 32))
      (i64.extend_i32_u (local.get 1)))))
"#;

#[cfg(test)]
const HOST_CALL_WAT: &str = r#"
(module
  (import "msk" "tool_call" (func $tool_call (param i32 i32) (result i64)))
  (memory (export "memory") 16)
  (func (export "run") (param i32 i32) (result i64)
    (call $tool_call (local.get 0) (local.get 1))))
"#;

#[cfg(test)]
const LOOP_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "run") (param i32 i32) (result i64)
    (loop $l (br $l))
    (i64.const 0)))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopHost;

    impl ToolHost for NoopHost {
        fn call(&self, _ctx: &ToolContext, _input: &[u8]) -> Result<Vec<u8>, String> {
            Err("no host configured".into())
        }
    }

    struct PrefixHost;

    impl ToolHost for PrefixHost {
        fn call(&self, _ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = b"host:".to_vec();
            out.extend_from_slice(input);
            Ok(out)
        }
    }

    struct CapabilityHost;

    impl ToolHost for CapabilityHost {
        fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
            if !ctx.allows(Capability::ExecSession) {
                return Err("capability denied: exec.session".into());
            }
            Ok(input.to_vec())
        }
    }

    #[test]
    fn echo_module_roundtrip() {
        let runtime = WasmtimeRuntime::from_wat(Arc::new(NoopHost), ECHO_WAT).unwrap();
        assert_eq!(runtime.run(b"hello").unwrap(), b"hello");
    }

    #[test]
    fn host_import_roundtrip() {
        let runtime = WasmtimeRuntime::from_wat(Arc::new(PrefixHost), HOST_CALL_WAT).unwrap();
        assert_eq!(runtime.run(b"abc").unwrap(), b"host:abc");
    }

    #[test]
    fn capabilities_are_checked_by_the_host() {
        let runtime = WasmtimeRuntime::from_wat(Arc::new(CapabilityHost), HOST_CALL_WAT).unwrap();

        let allowed = ToolContext::new("exec", vec![Capability::ExecSession]);
        assert_eq!(runtime.run_with_context(&allowed, b"ok").unwrap(), b"ok");

        let denied = ToolContext::new("exec", vec![]);
        assert!(runtime.run_with_context(&denied, b"ok").is_err());
    }

    #[test]
    fn fuel_exhaustion_is_an_error() {
        let runtime = WasmtimeRuntime::from_wat(Arc::new(NoopHost), LOOP_WAT)
            .unwrap()
            .with_fuel(10_000);
        assert!(runtime.run(b"").is_err());
    }

    #[test]
    fn manifest_requires_name() {
        let manifest = ToolManifest {
            name: String::new(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: Vec::new(),
            max_memory_bytes: None,
            timeout_ms: None,
        };
        assert!(manifest.validate().is_err());
    }
}
