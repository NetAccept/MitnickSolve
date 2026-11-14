//! Versioned C ABI shared by the kernel and load-time core components.
//!
//! Nothing in this crate may use Rust-specific types across the boundary:
//! only `#[repr(C)]` structs, opaque pointers, and integer status codes.
//! `struct_size` is the total size of the containing struct and is validated
//! before any optional field is read.

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub const MS_ABI_VERSION: u32 = 1;
pub const MS_LLM_VTABLE_ABI: u32 = 1;
pub const MS_LLM_JSON_SCHEMA: u32 = 1;

pub const MS_LLM_MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
pub const MS_LLM_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MS_LLM_MAX_TOOL_CALLS: usize = 128;
pub const MS_LLM_ERROR_CAPACITY: usize = 1024;
pub const MS_LLM_MAX_ALLOC_BYTES: usize = 64 * 1024 * 1024;
pub const MS_LLM_MAX_OUTSTANDING_BYTES: usize = 128 * 1024 * 1024;
pub const MS_LLM_MAX_ALLOC_ENTRIES: usize = 4096;
pub const MS_LOG_MAX_BYTES: usize = 8 * 1024;
pub const MS_ID_MAX_BYTES: usize = 256;

pub const MS_PORT_LLM: u32 = 1;
pub const MS_PORT_TRANSPORT: u32 = 2;
pub const MS_PORT_STORE: u32 = 3;
pub const MS_PORT_EVENT_SINK: u32 = 4;
pub const MS_PORT_SECRET_STORE: u32 = 5;

/// Symbol every core-component shared library exports. It returns a pointer
/// to a process-lifetime `ms_component`.
pub const MS_COMPONENT_ENTRY: &[u8] = b"ms_component_entry\0";

#[repr(C)]
pub struct ms_abi_header {
    pub abi_version: u32,
    /// Total size of the containing struct, not just this header.
    pub struct_size: u32,
    pub id: *const u8,
    pub version: *const u8,
}

pub type ms_alloc_fn = unsafe extern "C" fn(ctx: *mut c_void, len: usize) -> *mut c_void;
pub type ms_free_fn = unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut c_void, len: usize);
pub type ms_log_fn = unsafe extern "C" fn(ctx: *mut c_void, level: i32, msg: *const u8, len: usize);
pub type ms_config_get_fn = unsafe extern "C" fn(
    ctx: *mut c_void,
    key: *const u8,
    key_len: usize,
    buf: *mut u8,
    len: *mut usize,
) -> i32;
pub type ms_now_unix_ms_fn = unsafe extern "C" fn(ctx: *mut c_void, out: *mut i64) -> i32;

#[repr(C)]
pub struct ms_host {
    pub header: ms_abi_header,
    pub ctx: *mut c_void,
    pub alloc: Option<ms_alloc_fn>,
    pub free: Option<ms_free_fn>,
    pub log: Option<ms_log_fn>,
    /// Appended in ABI v1. `*len` is capacity on input and required length on
    /// `MS_STATUS_INVALID_ARGUMENT`; the value is NUL-terminated when copied.
    pub config_get: Option<ms_config_get_fn>,
    pub now_unix_ms: Option<ms_now_unix_ms_fn>,
}

pub type ms_init_fn = unsafe extern "C" fn(host: *const ms_host, out_ctx: *mut *mut c_void) -> i32;
pub type ms_shutdown_fn = unsafe extern "C" fn(ctx: *mut c_void);

#[repr(C)]
pub struct ms_component {
    pub header: ms_abi_header,
    pub port_kind: u32,
    pub vtable: *const c_void,
    pub init: Option<ms_init_fn>,
    pub shutdown: Option<ms_shutdown_fn>,
}

#[repr(C)]
pub struct ms_vtable_header {
    pub abi_version: u32,
    /// Total size of the containing vtable.
    pub struct_size: u32,
    pub port_kind: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ms_bytes {
    pub ptr: *mut u8,
    pub len: usize,
}

pub type ms_llm_complete_fn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    response: *mut ms_bytes,
    err: *mut u8,
    err_cap: usize,
    err_len: *mut usize,
) -> i32;

pub type ms_llm_delta_fn = unsafe extern "C" fn(sink_ctx: *mut c_void, ptr: *const u8, len: usize);

pub type ms_llm_stream_fn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    sink: Option<ms_llm_delta_fn>,
    sink_ctx: *mut c_void,
    response: *mut ms_bytes,
    err: *mut u8,
    err_cap: usize,
    err_len: *mut usize,
) -> i32;

#[repr(C)]
pub struct ms_llm_vtable {
    pub header: ms_vtable_header,
    pub complete: Option<ms_llm_complete_fn>,
    /// Optional; NULL in the minimal v1 host. Reserved for a later revision.
    pub stream: Option<ms_llm_stream_fn>,
    pub reserved: [*const c_void; 6],
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ms_status {
    Ok = 0,
    InvalidArgument = -1,
    PermissionDenied = -2,
    NotFound = -3,
    Timeout = -4,
    Cancelled = -5,
    Unsupported = -6,
    Internal = -7,
}

impl TryFrom<i32> for ms_status {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Self::Ok,
            -1 => Self::InvalidArgument,
            -2 => Self::PermissionDenied,
            -3 => Self::NotFound,
            -4 => Self::Timeout,
            -5 => Self::Cancelled,
            -6 => Self::Unsupported,
            -7 => Self::Internal,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn status_values_are_stable_and_fallible() {
        assert_eq!(ms_status::Ok as i32, 0);
        assert_eq!(ms_status::PermissionDenied as i32, -2);
        assert_eq!(ms_status::try_from(-7), Ok(ms_status::Internal));
        assert!(ms_status::try_from(-42).is_err());
        assert_eq!(
            size_of::<Option<unsafe extern "C" fn()>>(),
            size_of::<*const c_void>()
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn abi_layout_is_stable_on_64_bit() {
        assert_eq!(size_of::<ms_abi_header>(), 24);
        assert_eq!(offset_of!(ms_abi_header, id), 8);
        assert_eq!(offset_of!(ms_abi_header, version), 16);

        assert_eq!(size_of::<ms_host>(), 72);
        assert_eq!(offset_of!(ms_host, ctx), 24);
        assert_eq!(offset_of!(ms_host, alloc), 32);
        assert_eq!(offset_of!(ms_host, free), 40);
        assert_eq!(offset_of!(ms_host, log), 48);
        assert_eq!(offset_of!(ms_host, config_get), 56);
        assert_eq!(offset_of!(ms_host, now_unix_ms), 64);

        assert_eq!(size_of::<ms_component>(), 56);
        assert_eq!(offset_of!(ms_component, port_kind), 24);
        assert_eq!(offset_of!(ms_component, vtable), 32);
        assert_eq!(offset_of!(ms_component, init), 40);
        assert_eq!(offset_of!(ms_component, shutdown), 48);

        assert_eq!(size_of::<ms_vtable_header>(), 16);
        assert_eq!(size_of::<ms_bytes>(), 16);
        assert_eq!(size_of::<ms_llm_vtable>(), 80);
        assert_eq!(offset_of!(ms_llm_vtable, complete), 16);
        assert_eq!(offset_of!(ms_llm_vtable, stream), 24);
        assert_eq!(offset_of!(ms_llm_vtable, reserved), 32);
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn abi_layout_is_stable_on_32_bit() {
        assert_eq!(size_of::<ms_abi_header>(), 16);
        assert_eq!(size_of::<ms_host>(), 40);
        assert_eq!(size_of::<ms_component>(), 32);
        assert_eq!(size_of::<ms_bytes>(), 8);
        assert_eq!(size_of::<ms_llm_vtable>(), 48);
    }

    unsafe extern "C" {
        fn msk_c_sizeof_header() -> usize;
        fn msk_c_offsetof_header_id() -> usize;
        fn msk_c_offsetof_header_version() -> usize;
        fn msk_c_sizeof_host() -> usize;
        fn msk_c_offsetof_host_ctx() -> usize;
        fn msk_c_offsetof_host_alloc() -> usize;
        fn msk_c_offsetof_host_free() -> usize;
        fn msk_c_offsetof_host_log() -> usize;
        fn msk_c_offsetof_host_config_get() -> usize;
        fn msk_c_offsetof_host_now_unix_ms() -> usize;
        fn msk_c_sizeof_component() -> usize;
        fn msk_c_offsetof_component_port_kind() -> usize;
        fn msk_c_offsetof_component_vtable() -> usize;
        fn msk_c_offsetof_component_init() -> usize;
        fn msk_c_offsetof_component_shutdown() -> usize;
        fn msk_c_sizeof_vtable_header() -> usize;
        fn msk_c_sizeof_bytes() -> usize;
        fn msk_c_sizeof_llm_vtable() -> usize;
        fn msk_c_offsetof_llm_vtable_complete() -> usize;
        fn msk_c_offsetof_llm_vtable_stream() -> usize;
        fn msk_c_offsetof_llm_vtable_reserved() -> usize;
    }

    #[test]
    fn c_header_matches_rust_layout() {
        unsafe {
            assert_eq!(msk_c_sizeof_header(), size_of::<ms_abi_header>());
            assert_eq!(msk_c_offsetof_header_id(), offset_of!(ms_abi_header, id));
            assert_eq!(
                msk_c_offsetof_header_version(),
                offset_of!(ms_abi_header, version)
            );
            assert_eq!(msk_c_sizeof_host(), size_of::<ms_host>());
            assert_eq!(msk_c_offsetof_host_ctx(), offset_of!(ms_host, ctx));
            assert_eq!(msk_c_offsetof_host_alloc(), offset_of!(ms_host, alloc));
            assert_eq!(msk_c_offsetof_host_free(), offset_of!(ms_host, free));
            assert_eq!(msk_c_offsetof_host_log(), offset_of!(ms_host, log));
            assert_eq!(
                msk_c_offsetof_host_config_get(),
                offset_of!(ms_host, config_get)
            );
            assert_eq!(
                msk_c_offsetof_host_now_unix_ms(),
                offset_of!(ms_host, now_unix_ms)
            );
            assert_eq!(msk_c_sizeof_component(), size_of::<ms_component>());
            assert_eq!(
                msk_c_offsetof_component_port_kind(),
                offset_of!(ms_component, port_kind)
            );
            assert_eq!(
                msk_c_offsetof_component_vtable(),
                offset_of!(ms_component, vtable)
            );
            assert_eq!(
                msk_c_offsetof_component_init(),
                offset_of!(ms_component, init)
            );
            assert_eq!(
                msk_c_offsetof_component_shutdown(),
                offset_of!(ms_component, shutdown)
            );
            assert_eq!(msk_c_sizeof_vtable_header(), size_of::<ms_vtable_header>());
            assert_eq!(msk_c_sizeof_bytes(), size_of::<ms_bytes>());
            assert_eq!(msk_c_sizeof_llm_vtable(), size_of::<ms_llm_vtable>());
            assert_eq!(
                msk_c_offsetof_llm_vtable_complete(),
                offset_of!(ms_llm_vtable, complete)
            );
            assert_eq!(
                msk_c_offsetof_llm_vtable_stream(),
                offset_of!(ms_llm_vtable, stream)
            );
            assert_eq!(
                msk_c_offsetof_llm_vtable_reserved(),
                offset_of!(ms_llm_vtable, reserved)
            );
        }
    }
}
