# Kernel components (M2)

A kernel component is a signed shared library that implements one open port.
The first and currently only activated port is `llm.provider`. Sealed ports
(`policy`, `capability`, `scheduler`, `audit`, `wasm.runtime`) can never be
implemented by a component.

## Install layout

```text
component/
  manifest.json     signed component manifest
  trust.json        publisher id -> base64 Ed25519 public key
  libmyllm.so       artifact named by manifest.artifact.file
```

`manifest.json`:

```json
{
  "component": {
    "id": "acme.llm",
    "version": "1.0.0",
    "abi": 1,
    "port": "llm.provider",
    "publisher": "acme",
    "thread_safety": "serial"
  },
  "requires": null,
  "artifact": { "file": "libmyllm.so", "sha256": "<64 hex chars>" },
  "signature": "<base64 Ed25519 signature>"
}
```

The signature covers `b"msk-manifest-v1\0"` followed by the compact JSON
serialization of the manifest with `signature` removed. `artifact.file` must be
a plain file name in the manifest directory. `trust.json`:

```json
{ "publishers": { "acme": "<base64 32-byte Ed25519 public key>" } }
```

## ABI v1 (64-bit only)

All structs use natural alignment and `struct_size` means the total size of the
containing struct. The Rust definitions live in `crates/msk-abi/src/lib.rs`; the
C header is `crates/msk-abi/include/msk_abi.h`. A build-script layout probe
compares every `sizeof`/`offsetof` with the Rust values in CI, so the two cannot
drift silently. Sizes on 64-bit:

| Struct | Size | Key offsets |
| --- | ---: | --- |
| `ms_abi_header` | 24 | id 8, version 16 |
| `ms_host` | 72 | ctx 24, alloc 32, free 40, log 48, config_get 56, now_unix_ms 64 |
| `ms_component` | 56 | port_kind 24, vtable 32, init 40, shutdown 48 |
| `ms_vtable_header` | 16 | abi 0, size 4, port 8, reserved 12 |
| `ms_bytes` | 16 | ptr 0, len 8 |
| `ms_llm_vtable` | 80 | complete 16, stream 24, reserved 32 |

Function pointers return `i32` status values, never a Rust enum. Component
entry symbol: `ms_component_entry`, returning a process-lifetime pointer to
`ms_component`.

`config_get(ctx, key, key_len, buf, len)`: `*len` is capacity on input and the
required length (including NUL) on output. A too-small or null buffer returns
`-1` (INVALID_ARGUMENT) and sets `*len`; a missing key returns `-3`.

## Ownership

- Request bytes: host-owned, valid only for the `complete` call; the component
  must not free or retain them.
- Response bytes: allocated by the component with `host.alloc(host.ctx, len)`;
  on status 0 ownership transfers to the host, which copies and frees them. A
  fresh allocation is required per call; static buffers are rejected.
- Error text: caller-provided buffer of 1024 bytes; the component sets
  `*err_len` and must not exceed the capacity.
- Instance context: owned by the component between `init` and `shutdown`, opaque
  to the host.
- `ms_component`, vtable, and id/version strings: static, process lifetime.
- `ms_host`: host-owned, stable address; valid until `shutdown` returns.
- The library is never unloaded, even on validation or init failure.

The host keeps an allocation ledger and frees only host-registered pointers,
using the recorded length. A registered buffer returned with an oversized
length is freed once and rejected; an unregistered pointer is leaked and
reported rather than guessed at.

## Lifecycle and failure

1. Verify manifest signature/trust first, then artifact SHA-256 and path.
2. Load the canonical artifact path; validate envelope, header, port, vtable
   header, required slots, and reserved slots before copying pointers.
3. Copy id/version/port/vtable; call `init` exactly once.
4. Calls are serialized under a mutex in v1 even for `thread_safety: safe`.
5. `shutdown` is called exactly once on drop, after a successful init.
6. Any load/validation/init failure is fail-closed: no scripted fallback, no
   `shutdown` after a failed `init`.

Status codes: `0` OK, `-1` invalid argument, `-2` permission denied, `-3` not
found, `-4` timeout, `-5` cancelled, `-6` unsupported, `-7` internal. Responses
are frozen JSON (`schema_version` 1, `text`, `tool_calls`, `usage_tokens`);
tool call ids must be non-empty and unique, with object arguments, at most 128.

## Limits

Manifest/trust 64 KiB; id/version 256 bytes; artifact 256 MiB; request 16 MiB;
response 8 MiB; single allocation 64 MiB; outstanding 128 MiB or 4096 entries;
log line 8 KiB. The current host rejects activation on non-64-bit targets.

## Usage

```sh
msk --provider component \
  --component-manifest .msk/component/manifest.json \
  [--component-trust .msk/component/trust.json] \
  [--component-host target/debug/msk-component-host] \
  --goal "solve the challenge" --envd target/debug/envd
```

Trust defaults to `trust.json` next to the manifest. The daemon accepts the same
flags and forwards them only to component-provider tasks; a daemon without them
rejects `provider=component` at `task.create`.

## Security boundary

Signatures, hashes, ABI, and vtable checks are install gates, not a sandbox.
With `--component-host`, the component runs in `msk-component-host`; a component
abort kills that child and the kernel receives a protocol error, as covered by
the abort test. The host is still trusted code with process access and no
restart policy, resource limits, or syscall sandbox; a hung call blocks the turn
thread. In-process mode (no `--component-host`) can read kernel memory and abort
the process. `thread_safety` does not grant concurrency. Component
configuration/secrets are not injected in this slice.

## Test fixture

`cargo run -p msk-loader --example sign_test_fixture -- <artifact> <out-dir>`
copies the workspace `msk-test-component` cdylib, hashes it, signs a manifest
with a fixed test key, and writes `manifest.json` + `trust.json`. The fixed key
is for tests only.

## Remaining M2 work

- Restart policy, resource limits, and syscall sandbox for the component host.
- Cross-language component conformance harness beyond layout parity.
- TOCTOU-hardened loading (memfd/`/proc/self/fd` on Linux, handle-based loading
  on Windows) and DLL search hardening.
- Streaming vtable slot (reserved in v1).
- Trust key rotation/revocation and conformance receipts.
