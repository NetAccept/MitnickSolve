# MitnickSolve

CTF agent with a lightweight Rust kernel, WASM-only tools, and a uniform execution
backend (`envd`). Design documents live in [`docs/`](docs/). Performance notes and benchmark
results are in [`docs/performance.md`](docs/performance.md).

## Status

M0 scaffold plus the P1 session/turn slice:

- Rust workspace with the kernel/TUI skeleton and core types.
- Remote envd client: `msk --envd-url http://host:7800 --envd-token ...`
  (and the same flags on `mskd`) speaks MCP over HTTP(S) instead of spawning
  envd; tasks, subagents, and the daemon terminal share the remote service.
  `mskd --envd-data-addr host:7801` uses the binary MSDP data plane for
  terminals, with CREDIT and replay-cursor reconnects. Embedded stdio remains
  the default.
- Embedded `envd` over stdio frames (Go; `go-pty` for cross-platform PTY) and
  service mode: MCP JSON-RPC over HTTP(S) plus an MSDP TCP/TLS data plane, both
  behind a bearer token. The data plane uses absolute byte offsets, a bounded
  replay log, input/output CREDIT backpressure, and `OPEN since_seq` reconnect with
  explicit GAP/STALE errors.
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
- Persistent session/turn engine: `msk --session` keeps one agent session alive
  and reads user turns from stdin. `--stdio-json` is the strict daemon protocol
  on stdout (human output goes to stderr). Tool failures and provider failures
  end only the current turn, not the process.
- Daemon message channel: `task.create {"interactive": true}` spawns a persistent
  agent; `task.message` appends a user turn and `turn.end`/`agent.text`
  notifications report progress. `mskctl create --interactive` and
  `mskctl send` expose it from the CLI.
- Connection authentication: every IPC connection starts unauthenticated; the
  only allowed call is `kernel.hello` with the per-root token (constant-time
  comparison), and all other methods are rejected until it succeeds.
- Event envelope: `schema_version` and a process-local monotonic `seq` are
  written with every event; older logs without these fields still load.
- Pair-aware context compaction: an assistant tool-call message is removed only
  together with every matching tool result, so long sessions stay valid for
  providers that enforce call/result pairing.
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
- Tri-state tool policy and approvals: `exec`, `session_exec`, and
  `session_write` are `allow` or `ask`; unknown tools are `deny`. Interactive
  tasks default to `ask`, so the turn blocks until a human approves or denies.
  Denials become paired tool-error observations and the model continues.
- Approval channels: the agent emits `approval.request` on stdout; mskd
  broadcasts it as `approval.request` and writes `approval.respond` back to the
  agent's stdin. `mskctl approvals/approve/deny` and the TUI modal (`y`/`n`)
  resolve it. `--approvals allow` restores unattended execution.
- Conversational daemon TUI: task list, transcript, message editor, streamed
  assistant text, cooperative interrupt, and a live event pane. `n` creates an
  interactive task; `Enter`/`i` writes, `s` interrupts, `t` opens the PTY.
- SQLite event index: `msk-store` indexes events by id/task/kind/seq. Start
  mskd with `--store <path>` and query via `mskctl events` or
  `msk-replay --db <path>`; duplicate ids from log replay are ignored.
- Native context checkpoints: every session turn writes
  `<root>/checkpoint.json` (or `--checkpoint`) with the full message list,
  including native tool call/result pairs. `--resume-checkpoint` restores it
  under a fresh env and marks old env/session IDs stale; turn numbering
  continues. Unix checkpoint files are written 0600.
- Event integrity and replay: `msk-replay` validates `schema_version`, seq
  order, duplicate IDs, and parent links, then prints a transcript or filtered
  JSONL. `msk --session --resume <events.jsonl>` pins a capped transcript into
  the new session so work can continue after an envd/session restart.
- Signed component manifests: Ed25519 signatures over canonical manifest
  bytes, an explicit publisher trust store, and SHA-256/path checks for
  component artifacts.
- Component host: `libloading` resolves `ms_component_entry`, validates the
  signed manifest, ABI envelope, LLM vtable, and artifact, then calls `init`
  exactly once. `msk --provider component --component-manifest ...` uses a
  signed cdylib as the model provider; the kernel keeps an allocation ledger,
  copies and frees responses, serializes calls, and never unloads the library.
  `--component-host <binary>` runs the same component in
  `msk-component-host`, so a component abort kills only that process. A C header
  and a build-time `sizeof`/`offsetof` parity probe keep the Rust and C ABI in
  sync. See [`docs/components.md`](docs/components.md).
- Subagents: `spawn_agent` creates a headless child task and `agent_result`
  waits for its state/transcript. Both require `agent.spawn`; spawning is
  approval-gated by default, the child runs with delegated `approvals=allow`,
  and depth is capped at 2. `max_steps` and `max_context_tokens` bound each
  child; `mskctl create --headless --goal ...` and `mskctl result task-...`
  expose the same path.
- Shared blackboard: `blackboard_get/set/list` (`state.shared`) persist a
  per-root JSON map (`<root>/blackboard.json`, 64 KiB/value, 1 MiB total) so
  parent and child agents can coordinate findings. `mskctl bb-get/bb-set/bb-list`
  expose it directly. `flag_add/flag_list` record captured flags in the same
  ledger, deduplicated by SHA-256 of the value and attributed with an optional
  source.
- Secrets: `EnvSecretStore` maps `$secret:name` to `MSK_SECRET_NAME` (names are
  uppercased, non-alphanumerics become `_`). `http_request` resolves secret
  header values on the host after the `secrets.read` capability check; the
  plaintext never enters the model-visible observation or event log.
- File and HTTP tools: `read_file`/`write_file`/`list_dir`/`stat_file` run
  through envd with `fs.workspace` (writes require approval); `http_request`
  performs host-side HTTP(S) under `net.egress` with a 30 s default timeout and
  4 MiB response cap. Only http/https URLs are accepted.
- Scripted LLM provider exercising LLM -> tool call -> WASM -> host bridge -> envd;
  `exec.session` is granted by the kernel and checked in `EnvHost`.
- Service-mode envd, persistence/replay, and multi-agent are next.

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

Run it as a standalone service (remote/host deployment):

```sh
export ENVD_TOKEN=change-me
target/debug/envd --service --root .msk/envd-service \
  --listen 127.0.0.1:7800 --data-listen 127.0.0.1:7801
curl -H "Authorization: Bearer $ENVD_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize"}' \
  http://127.0.0.1:7800/mcp
```

Add `--tls-cert` and `--tls-key` for TLS 1.3. The data plane accepts MSDP
`HELLO`/`OPEN`/`DATA`/`RESIZE`/`CLOSE` frames for an existing `session_id`; see
[`tools/envd/README.md`](tools/envd/README.md). The kernel defaults to embedded
stdio; point it at the service explicitly:

```sh
target/debug/msk --envd-url http://127.0.0.1:7800 --envd-token "$ENVD_TOKEN" \
  --root .msk/remote --session
```

The same flags work with `--headless` and `--goal`. mskd can also be pointed at
a service; tasks and the daemon-managed PTY then use it as well:

```sh
target/debug/mskd --root .msk --envd-url http://127.0.0.1:7800 \
  --envd-token "$ENVD_TOKEN" --envd-data-addr 127.0.0.1:7801 \
  --tools-dir tools/wasm
```

With `--envd-data-addr`, the daemon terminal uses the binary MSDP data plane
(`DATA`/`CREDIT`/`RESIZE`); without it, terminal I/O falls back to control-plane
`session_read`/`session_write`.

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

## Signed LLM component

Build the test cdylib, generate a signed fixture, and use it as the provider:

```sh
cargo build -p msk-test-component
cargo run -p msk-loader --example sign_test_fixture -- \
  target/debug/build/msk-test-component/*/out/msk_test_component.dll \
  .msk/component-test
target/debug/msk --envd target/debug/envd --root .msk/component-cli \
  --headless --provider component \
  --component-manifest .msk/component-test/manifest.json \
  --goal "hello component"
```

Add `--component-host target/debug/msk-component-host` to isolate the component
in a child process. The daemon accepts `--component-manifest`,
`--component-trust`, and `--component-host` and forwards them to tasks created
with `--provider component`.

On Linux/macOS the artifact is `libmsk_test_component.so` / `.dylib`. Run the
install-time conformance check (or `--verify-only` to hash without loading):

```sh
target/debug/msk-component-check --manifest .msk/component-test/manifest.json \
  --trust .msk/component-test/trust.json
target/debug/msk-component-check --verify-only --manifest ... --trust ...
```

See [`docs/components.md`](docs/components.md) for the ABI, ownership rules,
limits, and security boundary.

## Interactive session

`--session` starts one long-lived agent session, creates one envd environment,
and reads user messages as lines from stdin. `/quit` or EOF ends the session;
`/interrupt` cooperatively stops the current turn. The offline `scripted`
provider echoes the current user turn, so this path is usable without a model:

```sh
target/debug/msk --envd target/debug/envd --root .msk/session --session
target/debug/msk --envd target/debug/envd --root .msk/session --session \
  --resume .msk/tasks/task-.../events.jsonl
target/debug/msk --envd target/debug/envd --root .msk/session --session \
  --resume-checkpoint .msk/old-task/checkpoint.json
```

The resumed transcript is pinned as context; old envd environment and session
IDs are explicitly marked stale, so the model should create a new session.
`--resume-checkpoint` restores the native context (including tool call/result
pairs) and continues turn numbering; `--checkpoint` changes the write path.

Use `--provider openai` (or `openai-responses`) with the usual model environment
variables for a real conversation. `--goal` is only used by the one-shot demo;
in session mode the first turn is whatever the user sends. Sessions default to
`--approvals ask`: command-executing tools print an approval prompt and wait
for `y`/`n` (or an `approval.respond` frame in JSON mode). Use
`--approvals allow` for unattended runs.

The daemon uses the same engine with a strict protocol:

```sh
target/debug/msk --envd target/debug/envd --root .msk/session \
  --session --stdio-json --stream
```

In this mode stdout carries only JSON lines: `{"type":"user","text":"..."}`,
`{"type":"interrupt"}`, or `{"type":"approval.respond","id":N,"approved":true}`
on stdin, and `agent.text`, `approval.request`, `turn.end`, `pong`, or `error`
frames on stdout. All setup messages go to stderr. `turn.end` has `status`
`awaiting_input`, `interrupted`, `stopped`, or `error`; errors do not terminate
the session. Interrupts are cooperative: the current blocking model or tool call
finishes, the engine stops before the next step, and every assistant tool call
still gets a paired result so the next turn is valid.

## Real provider

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

Inspect and validate an event log without executing anything:

```sh
target/debug/msk-replay --events .msk/events.jsonl --check
target/debug/msk-replay --events .msk/events.jsonl --task task-... --max-chars 4000
target/debug/msk-replay --events .msk/events.jsonl --from-seq 100 --format jsonl
target/debug/msk-replay --db .msk/store.db --task task-... --check
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
target/debug/mskd --root .msk --store .msk/store.db \
  --envd target/debug/envd --tools-dir tools/wasm
target/debug/mskctl --root .msk list
target/debug/mskctl --root .msk create --goal "Create a shell, run echo hi, and close it."
target/debug/mskctl --root .msk watch --timeout-ms 5000
target/debug/msk-tui --connect .msk
```

`mskd` writes a per-root token to `.msk/ipc.token`; `mskctl` and the TUI read it
automatically. On Unix the socket and token are mode 0600; Windows named pipes
currently rely on local-user access rather than an explicit DACL.

Interactive tasks stay alive between messages:

```sh
target/debug/mskctl --root .msk create --interactive
target/debug/mskctl --root .msk create --headless --goal "run one task and exit"
target/debug/mskctl --root .msk create --headless --max-context-tokens 5000
target/debug/mskctl --root .msk result task-...
target/debug/mskctl --root .msk send --task task-... --text "list the workspace"
target/debug/mskctl --root .msk interrupt task-...
target/debug/mskctl --root .msk approvals
target/debug/mskctl --root .msk approve --task task-... --id 1
target/debug/mskctl --root .msk deny --task task-... --id 2
target/debug/mskctl --root .msk bb-set finding '{"port":8080}'
target/debug/mskctl --root .msk bb-get finding
target/debug/mskctl --root .msk bb-list
target/debug/mskctl --root .msk events --task task-... --limit 50
target/debug/mskctl --root .msk watch --task task-...
target/debug/mskctl --root .msk state task-...
```

`mskctl events` and `msk-replay --db .msk/store.db` query the SQLite index;
`--check` works on indexed events too.

`task.message` is accepted only when the task is `waiting_input`; while a turn
is running it returns an error instead of interleaving messages. `turn.end`
notifications carry the turn number and any error message, and the agent stays
available for the next turn. Interactive tasks default to `approvals=ask`;
pass `--approvals allow` to `create` for unattended execution. Use
`--provider openai` on `create` together with `MSK_LLM_*` in the daemon
environment for a real model.

The TUI in daemon mode is a conversational workbench: a task list, a transcript,
and a message editor. `Enter` or `i` starts writing; `Enter` sends, `Esc` leaves
the editor, and `←`/`→`, `Home`/`End`, `Backspace`, and `Ctrl+U` edit the line.
`s` interrupts the running turn, `n` creates an interactive scripted task,
`x` cancels, `a` shows pending approvals, and a pending approval opens a modal
where `y` approves and `n`/`Esc` denies. `t` opens/focuses the daemon-managed
PTY terminal (`Ctrl+Q` detaches), `j`/`k` select tasks, `r` reloads, and `?`
shows help. Assistant text streams into the transcript as `agent.text` deltas;
errors and interrupts appear as system lines. The right pane keeps the live
event stream for debugging.

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
| `crates/msk-store` | SQLite event index (open Store port) |
| `crates/msk-secret` | Secret store port, env and memory backends |
| `crates/msk-wasm` | WASM tool runtime boundary (stub) |
| `crates/msk-loader` | Signed core component manifest/loader boundary |
| `crates/msk-kernel` | Kernel binary and M0 demo |
| `crates/msk-tui` | Minimal event viewer |
| `tools/envd` | Go execution environment |
| `tools/wasm` | WAT modules and manifests |
| `docs/` | Design and protocol drafts |
