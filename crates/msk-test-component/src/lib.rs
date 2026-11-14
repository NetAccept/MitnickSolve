//! Test cdylib for the component host. It implements a minimal `llm.provider`
//! vtable with fixed responses and deliberate failure modes selected by a
//! substring in the request bytes (`ok`, `empty`, `oversize`, `unregistered`,
//! `error`, `panic`).

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use msk_abi::{
    MS_ABI_VERSION, MS_LLM_VTABLE_ABI, MS_PORT_LLM, ms_abi_header, ms_bytes, ms_component, ms_host,
    ms_llm_vtable, ms_vtable_header,
};

static ID: &[u8] = b"test.llm\0";
static VERSION: &[u8] = b"1.0.0\0";
static RESPONSE: &[u8] =
    br#"{"schema_version":1,"text":"component hello","tool_calls":[],"usage_tokens":3}"#;
static UNREGISTERED: &[u8] = br#"{"schema_version":1,"text":"unregistered"}"#;
static ERROR_TEXT: &[u8] = b"test component error";

struct VtableWrap(ms_llm_vtable);
// The vtable is immutable static data; raw pointers inside it never change.
unsafe impl Sync for VtableWrap {}

static VTABLE: VtableWrap = VtableWrap(ms_llm_vtable {
    header: ms_vtable_header {
        abi_version: MS_LLM_VTABLE_ABI,
        struct_size: std::mem::size_of::<ms_llm_vtable>() as u32,
        port_kind: MS_PORT_LLM,
        reserved: 0,
    },
    complete: Some(component_complete),
    stream: None,
    reserved: [ptr::null(); 6],
});

struct ComponentWrap(ms_component);
unsafe impl Sync for ComponentWrap {}

static COMPONENT: ComponentWrap = ComponentWrap(ms_component {
    header: ms_abi_header {
        abi_version: MS_ABI_VERSION,
        struct_size: std::mem::size_of::<ms_component>() as u32,
        id: ID.as_ptr(),
        version: VERSION.as_ptr(),
    },
    port_kind: MS_PORT_LLM,
    vtable: &VTABLE.0 as *const ms_llm_vtable as *const c_void,
    init: Some(component_init),
    shutdown: Some(component_shutdown),
});

#[unsafe(no_mangle)]
pub extern "C" fn ms_component_entry() -> *const ms_component {
    &COMPONENT.0
}

struct Ctx {
    host: ms_host,
}

unsafe extern "C" fn component_init(host: *const ms_host, out: *mut *mut c_void) -> i32 {
    if host.is_null() || out.is_null() {
        return -1;
    }
    let host = unsafe { ptr::read_unaligned(host) };
    let context = Box::new(Ctx { host });
    unsafe { *out = Box::into_raw(context).cast() };
    0
}

unsafe extern "C" fn component_shutdown(ctx: *mut c_void) {
    if !ctx.is_null() {
        drop(unsafe { Box::from_raw(ctx.cast::<Ctx>()) });
    }
}

unsafe extern "C" fn component_complete(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    response: *mut ms_bytes,
    err: *mut u8,
    err_cap: usize,
    err_len: *mut usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || response.is_null() || err_len.is_null() {
            return -1;
        }
        unsafe {
            *response = ms_bytes {
                ptr: ptr::null_mut(),
                len: 0,
            };
            *err_len = 0;
        }
        let context = unsafe { &*(ctx.cast::<Ctx>()) };
        let request = if request.is_null() || request_len == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(request, request_len) }
        };
        let mode = detect_mode(request);
        if mode == "abort" {
            // Only used by the out-of-process host test; kills this host
            // process, not the kernel.
            std::process::exit(7);
        }
        if mode == "panic" {
            panic!("test component panic");
        }
        if mode == "error" {
            write_error(err, err_cap, err_len, ERROR_TEXT);
            return -1;
        }
        let payload = if mode == "unregistered" {
            UNREGISTERED
        } else {
            RESPONSE
        };
        if mode == "unregistered" {
            unsafe {
                *response = ms_bytes {
                    ptr: payload.as_ptr() as *mut u8,
                    len: payload.len(),
                };
            }
            return 0;
        }
        let Some(alloc) = context.host.alloc else {
            return -7;
        };
        let pointer = unsafe { alloc(context.host.ctx, payload.len()) };
        if pointer.is_null() {
            return -7;
        }
        unsafe {
            ptr::copy_nonoverlapping(payload.as_ptr(), pointer.cast::<u8>(), payload.len());
        }
        let length = match mode {
            "empty" => 0,
            "oversize" => payload.len() + 10,
            _ => payload.len(),
        };
        unsafe {
            *response = ms_bytes {
                ptr: pointer.cast(),
                len: length,
            };
        }
        0
    }));
    result.unwrap_or(-7)
}

fn detect_mode(request: &[u8]) -> &'static str {
    for mode in [
        "abort",
        "empty",
        "oversize",
        "unregistered",
        "error",
        "panic",
    ] {
        if request
            .windows(mode.len())
            .any(|window| window == mode.as_bytes())
        {
            return mode;
        }
    }
    "ok"
}

fn write_error(err: *mut u8, err_cap: usize, err_len: *mut usize, message: &[u8]) {
    if err.is_null() || err_len.is_null() {
        return;
    }
    let length = message.len().min(err_cap);
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), err, length);
        *err_len = length;
    }
}
