# MitnickSolve

CTF agent with a lightweight Rust kernel, WASM-only tools, and a uniform execution
backend (`envd`). Design documents live in [`docs/`](docs/). Performance notes and benchmark
results are in [`docs/performance.md`](docs/performance.md).

## Status

M0 scaffold:

- Rust workspace with the kernel/TUI skeleton and core types.
- Embedded `envd` over stdio frames (Go, standard library only).
- One end-to-end demo: kernel -> embedded envd -> `env_create` -> `write_file` ->
  `read_file` -> `exec` -> JSONL events.
- wasmtime-backed WASM tool runtime: fresh store per call, fuel limit, 64 MiB
  memory cap, JSON-in-linear-memory ABI, and a capability-gated host bridge.
- Manifest-driven policy: `msk-core::PolicyTable` intersects the manifest's
  declared capabilities with kernel grants; a manifest can never self-grant.
- Persistent shell sessions in envd (`session_create` / `session_exec` /
  `session_close`): repeated commands reuse one shell instead of spawning a process.
- Multi-turn scripted agent loop: each LLM turn can issue tool calls, observations
  feed the next turn. The default demo creates a session, runs commands, verifies
  shell state, and closes it through WASM + manifest policy.
- Context budgeting and artifacts: `msk-core::context` tracks tokens, pins key
  facts, and compacts old observations; outputs above 2 KiB are stored in
  `msk-data` by hash and replaced with a head/tail summary plus an ObjectRef.
- Artifact reads: the `artifact_read` tool (capability `artifact.read`) retrieves
  a slice of a stored output by ObjectRef.
- OpenAI-compatible provider: `--provider openai` sends manifest-derived tool
  schemas to `/chat/completions`; `--provider openai-responses` uses the native
  Responses API function_call/function_call_output items. Scripted remains the
  offline default.
- Chat Completions SSE streaming: `--stream` prints text deltas as they arrive;
  Responses currently falls back to the provider's non-streaming `complete`.
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

Build the embedded execution environment (requires Go 1.25+):

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

## Layout

| Path | Purpose |
| --- | --- |
| `crates/msk-core` | Domain types and ports |
| `crates/msk-abi` | C ABI definitions for kernel components |
| `crates/msk-events` | Append-only event log |
| `crates/msk-ipc` | Frontend JSON-RPC types |
| `crates/msk-data` | Shared objects and views |
| `crates/msk-llm` | LLM provider port and scripted provider |
| `crates/msk-mcp` | MCP client and embedded stdio frame transport |
| `crates/msk-wasm` | WASM tool runtime boundary (stub) |
| `crates/msk-loader` | Signed core component manifest/loader boundary |
| `crates/msk-kernel` | Kernel binary and M0 demo |
| `crates/msk-tui` | Minimal event viewer |
| `tools/envd` | Go execution environment |
| `tools/wasm` | WAT modules and manifests |
| `docs/` | Design and protocol drafts |
