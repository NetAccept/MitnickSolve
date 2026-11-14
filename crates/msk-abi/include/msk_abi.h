#ifndef MSK_ABI_H
#define MSK_ABI_H

#include <stddef.h>
#include <stdint.h>

#define MS_ABI_VERSION 1
#define MS_LLM_VTABLE_ABI 1
#define MS_PORT_LLM 1
#define MS_PORT_TRANSPORT 2
#define MS_PORT_STORE 3
#define MS_PORT_EVENT_SINK 4
#define MS_PORT_SECRET_STORE 5

typedef int32_t ms_status;
#define MS_STATUS_OK 0
#define MS_STATUS_INVALID_ARGUMENT (-1)
#define MS_STATUS_PERMISSION_DENIED (-2)
#define MS_STATUS_NOT_FOUND (-3)
#define MS_STATUS_TIMEOUT (-4)
#define MS_STATUS_CANCELLED (-5)
#define MS_STATUS_UNSUPPORTED (-6)
#define MS_STATUS_INTERNAL (-7)

typedef struct ms_abi_header {
    uint32_t abi_version;
    /* Total size of the containing struct, not just this header. */
    uint32_t struct_size;
    const uint8_t *id;
    const uint8_t *version;
} ms_abi_header;

typedef struct ms_host ms_host;
typedef struct ms_component ms_component;
typedef struct ms_llm_vtable ms_llm_vtable;

typedef void *(*ms_alloc_fn)(void *ctx, size_t len);
typedef void (*ms_free_fn)(void *ctx, void *ptr, size_t len);
typedef void (*ms_log_fn)(void *ctx, int32_t level, const uint8_t *msg, size_t len);
typedef int32_t (*ms_config_get_fn)(void *ctx, const uint8_t *key, size_t key_len,
                                    uint8_t *buf, size_t *len);
typedef int32_t (*ms_now_unix_ms_fn)(void *ctx, int64_t *out);

struct ms_host {
    ms_abi_header header;
    void *ctx;
    ms_alloc_fn alloc;
    ms_free_fn free;
    ms_log_fn log;
    ms_config_get_fn config_get;
    ms_now_unix_ms_fn now_unix_ms;
};

typedef int32_t (*ms_init_fn)(const ms_host *host, void **out_ctx);
typedef void (*ms_shutdown_fn)(void *ctx);

struct ms_component {
    ms_abi_header header;
    uint32_t port_kind;
    const void *vtable;
    ms_init_fn init;
    ms_shutdown_fn shutdown;
};

typedef struct ms_vtable_header {
    uint32_t abi_version;
    uint32_t struct_size;
    uint32_t port_kind;
    uint32_t reserved;
} ms_vtable_header;

typedef struct ms_bytes {
    uint8_t *ptr;
    size_t len;
} ms_bytes;

typedef int32_t (*ms_llm_complete_fn)(
    void *ctx,
    const uint8_t *request, size_t request_len,
    ms_bytes *response,
    uint8_t *err, size_t err_cap, size_t *err_len);

typedef void (*ms_llm_delta_fn)(void *sink_ctx, const uint8_t *ptr, size_t len);
typedef int32_t (*ms_llm_stream_fn)(
    void *ctx,
    const uint8_t *request, size_t request_len,
    ms_llm_delta_fn sink, void *sink_ctx,
    ms_bytes *response,
    uint8_t *err, size_t err_cap, size_t *err_len);

struct ms_llm_vtable {
    ms_vtable_header header;
    ms_llm_complete_fn complete;
    ms_llm_stream_fn stream;
    const void *reserved[6];
};

const ms_component *ms_component_entry(void);

#endif
