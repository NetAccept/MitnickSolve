# MitnickSolve

CTF agent with a lightweight Rust kernel, WASM-only tools, and a uniform execution
backend (`envd`). Design documents live in [`docs/`](docs/). Performance notes and benchmark
results are in [`docs/performance.md`](docs/performance.md).

## Status

M0 scaffold:

- Rust workspace with the kernel/TUI skeleton and core types.
- Embedded `envd` over stdio frames (Go; `go-pty` for cross-platform PTY).
- One end-to-end demo: kernel -> embedded envd -> `env_create` -> `write_file` ->
  `read_file` -> `exec` -> JSONL events.
- wasmtime-backed WASM tool runtime: fresh store per call, fuel limit, 64 MiB
  memory cap, JSON-in-linear-memory ABI, and a capability-gated host bridge.
- Manifest-driven policy: `msk-core::PolicyTable` intersects the manifest's
  declared capabilities with kernel grants; a manifest can never self-grant.
- Persistent shell sessions in envd (`session_create` / `session_exec` /
  `session_close`): repeated commands reuse one shell instead of spawning a process.
- PTY sessions: `session_create` with `pty=true` uses ConPTY on Windows or a
  Unix PTY; raw interaction goes through `session_write`, `session_read`, and
  `session_resize`. `session_exec` remains pipe-only to keep marker parsing safe.
- Multi-turn scripted agent loop: each LLM turn can issue tool calls, observations
  feed the next turn. The default demo creates a session, runs commands, verifies
  shell state, and closes it through WASM + manifest policy.
- Context budgeting and artifacts: `msk-core::context` tracks tokens, pins key
  facts, and compacts old observations; outputs above 2 KiB are stored in
  `msk-data` by hash and replaced with a head/tail summary plus an ObjectRef.
- Artifact reads: the `artifact_read` tool (capability `artifact.read`) retrieves
  a slice of a stored output by ObjectRef.
- Persistent artifact store: `msk-data::FileObjectStore` writes objects and
  metadata under `.msk/artifacts`; IDs are SHA-256 and reads fall back to base64
  for binary chunks.
- OpenAI-compatible provider: `--provider openai` sends manifest-derived tool
  schemas to `/chat/completions`; `--provider openai-responses` uses the native
  Responses API function_call/function_call_output items. Scripted remains the
  offline default.
- Chat Completions SSE streaming: `--stream` prints text deltas as they arrive;
  Responses currently falls back to the provider's non-streaming `complete`.
- TUI event workbench: incremental JSONL tail, kind filters, search, detail pane,
  context/artifact/tool counters, and live follow mode.
- Daemon and local IPC: `mskd` supervises agent tasks over a Unix socket or
  Windows named pipe; `mskctl` and `msk-tui --connect` can list/create/cancel
  tasks and stream events. A per-root token file gates local clients.
- Daemon-managed PTY terminal: `t` in the daemon TUI opens an interactive
  ConPTY/Unix PTY proxied by the daemon over a binary MSKD data plane with
  credit-based backpressure; `Ctrl+Q` detaches. The TUI renders it through a
  `vt100` parser (colors, cursor, alternate screen), so full-screen programs are
  usable. `mskctl terminal-*` exposes the control-plane path for scripting.
- Scripted LLM provider exercising LLM -> tool call -> WASM -> host bridge -> envd;
  `exec.session` is granted by the kernel and checked in `EnvHost`.
- Real streaming providers, session/PTY, and service-mode envd are next.

The Go side is not compiled in this repository yet; see
[`tools/envd/README.md`](tools/envd/README.md).

## Build

```sh
cargo build --workspace
cargo test --workspace
```

Build the embedded execution environment (requires Go 1.26+; CI uses 1.27):

```sh
cd tools/envd
go build -o ../../target/debug/envd ./cmd/envd
```

## Run the M0 demo

```sh
cargo run -p msk-kernel --bin msk -- --envd target/debug/envd --root .msk
```

On Windows the envd binary is `target/debug/envd.exe`. The default
`--demo session` uses `tools/wasm/session_*.json` and falls back to
`tools/wasm/exec.wat` for the host bridge. Use `--demo echo` for the echo-only
tool boundary:

```sh
cargo run -p msk-kernel --bin msk -- --envd target/debug/envd --root .msk --demo echo
```

The context budget defaults to 100k tokens; pass `--max-context-tokens 32000`
to exercise compaction. Large session output is stored as a `data.put` artifact
and summarized in the transcript.

Real provider (OpenAI-compatible, non-streaming):

```sh
export MSK_LLM_API_KEY=...
export MSK_LLM_MODEL=...
cargo run -p msk-kernel --bin msk -- --envd target/debug/envd \
  --provider openai --stream --goal "Create a session, run echo hello, and close it."
```

Use `--provider openai-responses` for the Responses API, and
`--llm-base-url` or `MSK_LLM_BASE_URL` for compatible endpoints. A local mock is
included for offline testing:

```sh
python bench/mock_llm.py 8765
cargo run -p msk-kernel --bin msk -- --envd target/debug/envd \
  --provider openai --stream --llm-base-url http://127.0.0.1:8765/v1 \
  --llm-model mock --llm-api-key test --demo echo
cargo run -p msk-kernel --bin msk -- --envd target/debug/envd \
  --provider openai-responses --llm-base-url http://127.0.0.1:8765/v1 \
  --llm-model mock --llm-api-key test --demo echo
```

Benchmarks:

```sh
cargo build --release -p msk-kernel --bin msk-bench
target/release/msk-bench wasm-echo --iterations 5000
target/release/msk-bench envd-exec --envd target/debug/envd --iterations 10
target/release/msk-bench session-exec --envd target/debug/envd --iterations 50
```

The demo writes events to `.msk/events.jsonl`. Inspect them with the TUI:

```sh
cargo run -p msk-tui -- --events .msk/events.jsonl
```

TUI keys: `q` quit, `j`/`k` move, `g`/`G` top/bottom, `tab` cycle filters,
`1..5` all/llm/tools/artifacts/context, `/` search, `f` follow, `r` reload,
`?` help. The viewer reads the JSONL log incrementally and shows the selected
event's full payload, including `data.put` artifact metadata.

## Daemon mode

```sh
cargo build --bins -p msk-kernel -p msk-tui
target/debug/mskd --root .msk --envd target/debug/envd --tools-dir tools/wasm
target/debug/mskctl --root .msk list
target/debug/mskctl --root .msk create --goal "Create a shell, run echo hi, and close it."
target/debug/mskctl --root .msk watch --timeout-ms 5000
target/debug/msk-tui --connect .msk
```

`mskd` writes a per-root token to `.msk/ipc.token`; `mskctl` and the TUI read it
automatically. On Unix the socket and token are mode 0600; Windows named pipes
currently rely on local-user access rather than an explicit DACL.

The TUI in daemon mode supports `j`/`k` task selection, `n` to create a default
session task, `x` to cancel, `t` to open/focus the daemon-managed PTY terminal
(`Ctrl+Q` detaches), `r` to reload, `a` for the approvals placeholder, and `?`
for help.

Terminal scripting:

```sh
target/debug/mskctl --root .msk terminal-open
target/debug/mskctl --root .msk terminal-write --session ses_... --line "echo hi"
target/debug/mskctl --root .msk terminal-read --session ses_... --timeout-ms 300
target/debug/mskctl --root .msk terminal-resize --session ses_... --cols 120 --rows 40
target/debug/mskctl --root .msk terminal-close --session ses_...
target/debug/mskctl --root .msk terminal-stream --line "echo hi" --timeout-ms 2000
```

`terminal-stream` uses the binary data plane (MSKD frames, credit, resize,
close); the control-plane `terminal-*` commands are convenient for scripting.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/msk-core` | Domain types and ports |
| `crates/msk-abi` | C ABI definitions for kernel components |
| `crates/msk-events` | Append-only event log |
| `crates/msk-ipc` | Local-socket JSON-RPC types, transport, and client |
| `crates/msk-data` | Content-addressed artifact store |
| `crates/msk-llm` | LLM provider port and scripted provider |
| `crates/msk-mcp` | MCP client and embedded stdio frame transport |
| `crates/msk-wasm` | WASM tool runtime boundary (stub) |
| `crates/msk-loader` | Signed core component manifest/loader boundary |
| `crates/msk-kernel` | Kernel binary and M0 demo |
| `crates/msk-tui` | Minimal event viewer |
| `tools/envd` | Go execution environment |
| `tools/wasm` | WAT modules and manifests |
| `docs/` | Design and protocol drafts |
