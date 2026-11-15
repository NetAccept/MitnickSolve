# envd (0.2)

Execution environment. Two transports are implemented from
[`docs/envd-protocol.md`](../../docs/envd-protocol.md):

- `--embedded`: MSDP frames on stdin/stdout, MCP JSON-RPC in CONTROL frames.
  Used by the kernel for local/loopback execution.
- `--service`: MCP JSON-RPC over HTTP(S) plus an MSDP data plane over TCP/TLS.
  Both require a bearer token; TLS 1.3 is enabled when `--tls-cert` and
  `--tls-key` are supplied.

Tools: `agent_info`, `env_create`, `exec`, `read_file`, `write_file`,
`list_dir`, `stat_file`, `session_create/exec/write/read/resize/list/close`.
PTY sessions use raw `session_write`/`session_read`/`session_resize`;
`session_exec` is pipe-only.

## Build

```sh
go build -o ../../target/debug/envd ./cmd/envd
```

## Test

```sh
go test ./...
```

## Run embedded

Usually started by the kernel:

```sh
cargo run -p msk-kernel -- --envd target/debug/envd --root .msk
```

Standalone:

```sh
./envd --embedded --root .msk-envd
```

The process speaks binary MSDP frames, not text; running it in a terminal
without a client will wait for a HELLO frame.

## Run service mode

```sh
export ENVD_TOKEN=change-me
./envd --service --root .msk-envd \
  --listen 127.0.0.1:7800 --data-listen 127.0.0.1:7801
```

```sh
curl http://127.0.0.1:7800/healthz
curl -X POST http://127.0.0.1:7800/mcp \
  -H "Authorization: Bearer $ENVD_TOKEN" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize"}'
```

TLS:

```sh
./envd --service --token "$ENVD_TOKEN" \
  --tls-cert cert.pem --tls-key key.pem
```

The data plane accepts one MSDP connection per session view:
`HELLO` (token) -> `HELLO_ACK` -> `OPEN {"session_id": "ses_...",
"since_seq": N}` -> `OPEN_ACK` -> `DATA` both ways; `RESIZE`, `PING`/`PONG`,
`CLOSE`, and `EXIT` are supported. Output is byte-addressed by absolute `seq`
and kept in a bounded replay log (`session_create.output_buffer_bytes`, default
4 MiB). The writer enforces output CREDIT (initial 64 KiB, 64 KiB DATA frames,
4 MiB cap) and sends `GAP` (code 13) or `STALE` (code 14) when `since_seq` is
outside the retained window. A data connection owns the session's output while
attached; after detach, new output also feeds control-plane `session_read`, and
the replay log remains available for a reconnect. Input is also credit-limited
(64 KiB window, replenished as it is consumed) and `SEEK` repositions the view
within the retained log. WebSocket transport is not implemented yet. The Rust
`EnvDataClient` and mskd `--envd-data-addr` use this path for remote terminals.

## Notes

- Go standard library plus `github.com/aymanbagabas/go-pty` for PTY sessions.
- Service mode requires an explicit token; embedded mode has no in-band auth
  because the pipe is established by the parent process.
- `secureJoin` handles traversal and existing symlinks. Windows reserved names,
  ADS rejection, quotas, port forwarding, and audit are still TODO.
- Sessions live in the envd process; restarting envd drops them.
