# envd (M0)

Embedded execution environment. M0 implements the `embedded-stdio` binding from
[`docs/envd-protocol.md`](../../docs/envd-protocol.md): frames on stdin/stdout,
MCP JSON-RPC in CONTROL frames, and the `agent_info`, `env_create`, `exec`,
`read_file`, `write_file`, `list_dir`, and `stat_file` tools.

Not implemented yet: service mode (MCP over HTTP), sessions/PTY, data-plane
file streams, port forwarding, quotas, and audit. Those land with M3.

## Build

```sh
go build -o ../../target/debug/envd ./cmd/envd
```

## Test

```sh
go test ./...
```

## Run

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

## Notes

- Standard library only.
- `secureJoin` currently handles traversal and existing symlinks. `os.Root`,
  Windows reserved names, and ADS rejection are TODO before service mode.
- `exec` is one-shot; sessions and process groups/Job Objects come next.
