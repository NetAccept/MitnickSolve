# envd (0.2)

Execution environment. Two transports are implemented from
[`docs/envd-protocol.md`](../../docs/envd-protocol.md):

- `--embedded`: MSDP frames on stdin/stdout, MCP JSON-RPC in CONTROL frames.
  Used by the kernel for local/loopback execution.
- `--service`: MCP JSON-RPC over HTTP(S) plus an MSDP data plane over TCP/TLS.
  Both require a bearer token; TLS 1.3 is enabled when `--tls-cert` and
  `--tls-key` are supplied.

Tools: `agent_info`, `env_create`, `env_open`, `exec`, `read_file`, `write_file`,
`list_dir`, `stat_file`, `session_create/exec/write/read/resize/list/close`.

Security and network operations add: `net_info`, `net_dns`, `net_dial`,
`net_tls`, `net_scan`, `web_request`; `file_hash`, `file_type`, `file_strings`,
`file_hexdump`, `file_entropy`, `file_search`, `archive_list`,
`archive_extract`, `binary_info`, `secret_scan`; and `transform` for
base64/hex/URL/hash/HMAC/JWT/gzip/XOR operations. All are exposed over the same
MCP control plane and bounded by explicit host/port lists, byte limits and
timeouts. `net_scan`, `net_dial`, `web_request` and `archive_extract` are
approval-gated in the kernel's `ask` policy.

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
  --listen 127.0.0.1:7800 --data-listen 127.0.0.1:7801 \
  --net-allow 10.0.0.0/8 --net-allow example.test
```

`--net-allow` is an optional deployment egress allowlist: CIDR blocks and
domain suffixes (repeatable or comma-separated). When present, every DNS, TCP,
TLS, scan, HTTP and custom-resolver target is checked before any packet is
sent; an empty list allows every target.

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

## Profiles and isolation

`env_create` accepts three profiles. Their names describe deployment policy,
not built-in sandboxing:

| profile | Behavior |
| --- | --- |
| `dev` | Direct execution; the only profile available on `--embedded`. Unisolated. |
| `host` | Direct execution over `--service`; unisolated but explicit. |
| `isolated` | Every `exec`/pipe/PTY launch is prefixed with the deployment launcher; service mode only. Fails closed without a usable launcher. |

Configure a launcher once at startup. Arguments are not shell-evaluated:

```sh
envd --service --token "$ENVD_TOKEN" \
  --sandbox-command /usr/bin/bwrap \
  --sandbox-arg=--unshare-all --sandbox-arg=--die-with-parent \
  --sandbox-arg=--ro-bind --sandbox-arg=/ --sandbox-arg=/ \
  --sandbox-arg=--dev --sandbox-arg=/dev --sandbox-arg=--proc --sandbox-arg=/proc \
  --sandbox-arg=--tmpfs --sandbox-arg=/tmp \
  --sandbox-arg=--bind --sandbox-arg=/srv/ctf --sandbox-arg=/srv/ctf \
  --sandbox-arg=--
```

The launcher receives `argv = prefix + target argv`. It must:

- accept stdin/stdout/stderr and propagate the target exit code;
- terminate its sandbox when its root process dies (`--die-with-parent`,
  container init, or an equivalent wrapper);
- expose the environment workspace at the same host path (or `chdir` itself);
  `cmd.Dir` is the host workspace before entering the launcher namespace;
  bind only the workspace parent read-write; do **not** use `--dev-bind / /`,
  which exposes the whole host root writable;
- resolve the shell inside its own namespace if it provides a different
  filesystem. envd does not resolve the shell on the host for isolated sessions.

The launcher path is validated at startup and before every isolated launch:
absolute, executable (PE on Windows), not a `.bat`/`.cmd`/`.ps1`, and not under
the client-writable data root. If validation fails, `env_create` or the launch
returns `FAILED_PRECONDITION`; envd never falls back to direct execution.
The target `argv[0]` may not start with `-`. Prefer a `--` terminator in the
prefix so target arguments can never be parsed as launcher options.

File tools (`read_file`/`write_file`/...) stay in the envd process and are not
wrapped; the workspace-relative checks are best-effort, not a filesystem
sandbox. Child environments have all `ENVD_*` variables removed; the launcher
must sanitize anything else it considers secret. envd terminates only the
direct child, so process-tree teardown belongs to the launcher.

`agent_info` reports `profiles`, `transport`, and `isolation`; `sandbox.launcher`
is advertised only when isolated is actually available.

## Notes

- Go standard library plus `github.com/aymanbagabas/go-pty` for PTY sessions.
- `--sandbox-command/--sandbox-arg` require `--service`; embedded is dev-only.
- Service mode requires an explicit token; embedded mode has no in-band auth
  because the pipe is established by the parent process.
- `secureJoin` handles traversal and existing symlinks. Windows reserved names,
  ADS rejection, quotas, port forwarding, and audit are still TODO.
- Sessions live in the envd process; restarting envd drops them.
