//! Versioned C ABI shared by the kernel and load-time core components.
//!
//! Nothing in this crate may use Rust-specific types across the boundary:
//! only `#[repr(C)]` structs, opaque pointers, and integer status codes.

#![allow(non_camel_case_types)]

use std::ffi::c_void;

pub const MS_ABI_VERSION: u32 = 1;

/// Symbol every core-component shared library exports. It returns a pointer
/// to a static `ms_component`.
pub const MS_COMPONENT_ENTRY: &[u8] = b"ms_component_entry\0";

pub const MS_PORT_LLM: u32 = 1;
pub const MS_PORT_TRANSPORT: u32 = 2;
pub const MS_PORT_STORE: u32 = 3;
pub const MS_PORT_EVENT_SINK: u32 = 4;
pub const MS_PORT_SECRET_STORE: u32 = 5;

#[repr(C)]
pub struct ms_abi_header {
    pub abi_version: u32,
    pub struct_size: u32,
    pub id: *const u8,
    pub version: *const u8,
}

pub type ms_alloc_fn = unsafe extern "C" fn(ctx: *mut c_void, len: usize) -> *mut c_void;
pub type ms_free_fn = unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut c_void, len: usize);
pub type ms_log_fn = unsafe extern "C" fn(ctx: *mut c_void, level: i32, msg: *const u8, len: usize);

#[repr(C)]
pub struct ms_host {
    pub header: ms_abi_header,
    pub ctx: *mut c_void,
    pub alloc: Option<ms_alloc_fn>,
    pub free: Option<ms_free_fn>,
    pub log: Option<ms_log_fn>,
}

#[repr(C)]
pub struct ms_component {
    pub header: ms_abi_header,
    pub port_kind: u32,
    pub vtable: *const c_void,
    pub init:
        Option<unsafe extern "C" fn(host: *const ms_host, out_ctx: *mut *mut c_void) -> ms_status>,
    pub shutdown: Option<unsafe extern "C" fn(ctx: *mut c_void)>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_are_stable() {
        assert_eq!(ms_status::Ok as i32, 0);
        assert_eq!(ms_status::PermissionDenied as i32, -2);
    }

    #[test]
    fn host_is_c_layout() {
        assert!(std::mem::size_of::<ms_host>() >= std::mem::size_of::<ms_abi_header>());
    }
}
