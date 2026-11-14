//! Minimal cdylib used by msk-loader's end-to-end ABI test. It exports the
//! stable entry symbol and a static description; it does not implement a port.

use msk_abi::{MS_ABI_VERSION, MS_PORT_LLM, ms_abi_header, ms_component};

static ID: &[u8] = b"test.component\0";
static VERSION: &[u8] = b"1.0.0\0";

#[unsafe(no_mangle)]
pub extern "C" fn ms_component_entry() -> *const ms_component {
    Box::into_raw(Box::new(ms_component {
        header: ms_abi_header {
            abi_version: MS_ABI_VERSION,
            struct_size: std::mem::size_of::<ms_abi_header>() as u32,
            id: ID.as_ptr(),
            version: VERSION.as_ptr(),
        },
        port_kind: MS_PORT_LLM,
        vtable: std::ptr::null(),
        init: None,
        shutdown: None,
    }))
}
