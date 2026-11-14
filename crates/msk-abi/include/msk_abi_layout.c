#include "msk_abi.h"

size_t msk_c_sizeof_header(void) { return sizeof(ms_abi_header); }
size_t msk_c_offsetof_header_id(void) { return offsetof(ms_abi_header, id); }
size_t msk_c_offsetof_header_version(void) { return offsetof(ms_abi_header, version); }

size_t msk_c_sizeof_host(void) { return sizeof(ms_host); }
size_t msk_c_offsetof_host_ctx(void) { return offsetof(ms_host, ctx); }
size_t msk_c_offsetof_host_alloc(void) { return offsetof(ms_host, alloc); }
size_t msk_c_offsetof_host_free(void) { return offsetof(ms_host, free); }
size_t msk_c_offsetof_host_log(void) { return offsetof(ms_host, log); }
size_t msk_c_offsetof_host_config_get(void) { return offsetof(ms_host, config_get); }
size_t msk_c_offsetof_host_now_unix_ms(void) { return offsetof(ms_host, now_unix_ms); }

size_t msk_c_sizeof_component(void) { return sizeof(ms_component); }
size_t msk_c_offsetof_component_port_kind(void) { return offsetof(ms_component, port_kind); }
size_t msk_c_offsetof_component_vtable(void) { return offsetof(ms_component, vtable); }
size_t msk_c_offsetof_component_init(void) { return offsetof(ms_component, init); }
size_t msk_c_offsetof_component_shutdown(void) { return offsetof(ms_component, shutdown); }

size_t msk_c_sizeof_vtable_header(void) { return sizeof(ms_vtable_header); }
size_t msk_c_sizeof_bytes(void) { return sizeof(ms_bytes); }
size_t msk_c_sizeof_llm_vtable(void) { return sizeof(ms_llm_vtable); }
size_t msk_c_offsetof_llm_vtable_complete(void) { return offsetof(ms_llm_vtable, complete); }
size_t msk_c_offsetof_llm_vtable_stream(void) { return offsetof(ms_llm_vtable, stream); }
size_t msk_c_offsetof_llm_vtable_reserved(void) { return offsetof(ms_llm_vtable, reserved); }
