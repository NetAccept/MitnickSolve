# Security Toolset

MITNICKSOLVE is positioned as a workbench for security engineers, network operations engineers, and CTF players: a single agent loop that can resolve and probe networks, inspect web surfaces, triage files and binaries, extract forensic evidence, and transform captured data without leaving the environment boundary.

The wrappers in `tools/wasm/` are manifests, not implementations. Each `.json` declares the tool name, description, input schema, and the capabilities the manifest may request. Each `.wat` is the same small passthrough module that imports `msk.tool_call` and forwards its two arguments. The kernel grants capabilities from its policy table; a manifest can never self-grant. Execution happens in `envd`, which owns the workspace and the network egress boundary. Every tool except `transform` takes an `env_id` and operates inside that environment.

## Recon

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `net_dns` | `net.egress` | Resolve A/AAAA/CNAME/MX/NS/TXT/SRV/PTR records, optionally through an explicit `host:port` DNS server. | `name`, `type`, `server`, `duration_ms`, `values`, and per-record `records` objects (address, target, preference, port, priority, weight, text, name as applicable). |
| `net_dial` | `net.egress` | Open a TCP or TLS connection, optionally send base64 bytes, and read a bounded banner. | `connected`, `host`, `port`, `duration_ms`, `latency_ms`, `tls`, `tls_version`, `cipher_suite`, `alpn`, `received_base64`, `received_bytes`, `eof`, plus `received_text` for printable data and `error` on write/read failure. |
| `net_tls` | `net.egress` | Inspect a TLS endpoint's negotiated version, cipher, ALPN, and peer certificate chain. | `version`, `cipher_suite`, `alpn`, `verified`, `verify_error`, and `peer_certificates` with subject, issuer, serial, validity, SANs, CA flag, signature algorithm, SHA-256 fingerprint, and bounded PEM; `certificates_truncated`/`pem_truncated` when the chain is cut. |
| `net_scan` | `net.egress` | Bounded TCP connect scan over explicit hosts and ports, with optional banners and a 30 s wall-clock deadline. | `targets` (each with `host`, `address`, `open` entries of `port`, `latency_ms`, optional `banner_base64`/`banner_text`, and an optional `error`), `probes`, `open_count`, `duration_ms`, `truncated`, `timed_out`. |
| `net_info` | `net.egress` | Report the environment's hostname, OS/arch, interfaces, addresses, and configured DNS servers. | `hostname`, `os`, `arch`, `interfaces` (name, index, MTU, flags, MAC, addresses with family/CIDR/loopback/private), `dns_servers`. |

## Web

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `web_request` | `net.egress` | Environment-scoped HTTP(S) with methods, headers, JSON or form bodies, base64 bodies, a cookie jar, redirect tracking, proxy, and TLS options. | `status`, `status_text`, `headers` (16 KiB cap, `headers_truncated`), `body_base64` (and `body_text` when printable), `bytes`, `truncated`, `duration_ms`, `final_url`, `redirects`, `cookies`, and TLS summary when applicable. |

## Files & Forensics

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `file_hash` | `fs.workspace` | Hash a workspace file with MD5, SHA-1, SHA-256, and/or SHA-512. | `path`, `size`, `hashes` keyed by algorithm, `duration_ms`. |
| `file_type` | `fs.workspace` | Identify a file from its first 4 KiB using magic bytes plus MIME detection. | `path`, `size`, `mime`, `magic_hex`, `magic_ascii`, `kind` (for example `elf`, `pe`, `macho`, `zip`, `gzip`, `pdf`, `text`, `base64`). |
| `file_strings` | `fs.workspace` | Extract printable ASCII, UTF-16LE, or both from a byte range. | `strings` entries of `offset` and `text`, `count`, `truncated`. |
| `file_hexdump` | `fs.workspace` | Render a range as canonical hex and ASCII rows. | `offset`, `length`, `eof`, `lines` entries of `offset`, `hex`, `ascii`. |
| `file_entropy` | `fs.workspace` | Compute Shannon entropy overall and per chunk to flag packing, compression, or encryption. | `size`, `overall`, `min`, `max`, `chunks` entries of `offset` and `entropy`, `classification`. |
| `file_search` | `fs.workspace` | Walk a workspace root and match file names or text content, with glob, case, and regex controls. | `matches` entries of `path`, `line`, `column`, `preview`, `size`, plus `count`, `truncated`, `files_walked`. |
| `archive_list` | `fs.workspace` | List zip, tar, tar.gz, and single-entry gzip archives without extracting them. | `format`, `entries` (name, size, compressed size, mode, modified, is_dir), `count`, `total_uncompressed`, `truncated`. |
| `archive_extract` | `fs.workspace` | Extract zip, tar, tar.gz, or gzip members into a workspace destination with zip-slip and size guards. | `extracted` entries of `name` and `bytes`, plus `skipped`, `total_bytes`, `truncated`. |
| `secret_scan` | `fs.workspace` | Walk text files for credential-shaped strings and return redacted previews. | `findings` entries of `path`, `line`, `pattern`, `preview` (redacted), plus `files_scanned`, `truncated`. |

## Binary

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `binary_info` | `fs.workspace` | Inspect ELF, PE, or Mach-O metadata, sections, imports, and hardening flags. | `format`, `arch`, `bits`, `endian`, `entry`, `sections`, `imports` (capped), and a `security` object with format-specific flags such as NX, PIE, RELRO, stack canary, ASLR, and CFG. |

## Data

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `transform` | none | Local-only encode/decode/hash/HMAC/JWT/gzip/XOR/UUID helper for CTF data. | `op`, `result` when textual, `result_base64` for binary output, `bytes`; `hashes` for `hash`, `hmac` for `hmac`, and decoded header/claims/signature for `jwt_decode`. |
| `wasm_eval` | none | Compile and run a pure WAT module with no imports; useful for a model-written parser or calculation. | `output_base64`, `bytes`; rejects host imports, input >64 KiB and output >256 KiB. |

## Findings & reporting

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `finding_add` | `state.shared` | Record or update a structured finding: title, severity (`info`/`low`/`medium`/`high`/`critical`), asset, description, evidence, status (`open`/`confirmed`/`fixed`/`false_positive`), tags, source. Idempotent for the same title and asset and bounded to 32 KiB of evidence. | `added`, the ledger `key`, and the stored `finding` (including id, timestamps, severity and status). |
| `finding_list` | `state.shared` | List findings filtered by severity, status, asset or tag. | `findings` sorted by severity then recency, and `count`. |
| `report_generate` | `state.shared` | Build a Markdown assessment report from the finding ledger and captured flags. | `title`, `markdown`, the structured `findings` and `flags`, per-severity `counts`, and `generated_ms`. Save it with `write_file` or paste it into an issue. |

Findings are stored as `finding:<sha256>` keys in the daemon blackboard, so they
survive across sessions and are visible to every attached client. Flags remain
in the `flag:` ledger. `report_generate` does not write files by itself; use
`write_file` with the returned Markdown when a durable report is needed.

## Writing tools in WASM

Tools are WebAssembly modules with a tiny ABI: the kernel writes the JSON input
at linear-memory offset 0, calls `run(ptr, len) -> i64`, and reads the output at
`(return >> 32)` with length `(return & 0xffffffff)`. A tool that needs the
kernel bridge imports `msk.tool_call`.

MoonBit is wired in as a first-party frontend. `tools/moonbit/mitnick_tools`
contains a `foreign_library` package implementing `transform`; its built
artifact is `tools/wasm/transform.wasm` and the kernel loader prefers
`<tool>.wasm` when no `<tool>.wat` exists. Build with:

```sh
./tools/moonbit/build.sh
./tools/moonbit/build.ps1
```

The MoonBit source imports `msk.tool_call` with `extern "wasm"`, exports memory
as `memory`, and exports `run` with `#export_name("run")`. See
[`tools/moonbit/README.md`](../tools/moonbit/README.md) for a new-tool recipe.

For an ad hoc pure computation written by the model itself, use `wasm_eval`:

```json
{
  "wat": "(module (memory (export \"memory\") 1) (func (export \"run\") (param i32 i32) (result i64) ...))",
  "input_base64": "aGVsbG8="
}
```

`wasm_eval` rejects **all imports**, so the generated module cannot reach envd,
files, the network or the kernel host. It has fuel and memory limits, input is
at most 64 KiB, output at most 256 KiB, and WAT source at most 64 KiB. This is
the safe way for an agent to hand-write a parser or encoder on demand; persistent
tools still go through the manifest + signed distribution path.

## PTY and full-screen tools

`session_create` with `pty: true` allocates a ConPTY (Windows) or Unix PTY and is
the supported way to drive full-screen programs such as `vim`, `top`, `htop`,
`less`, `tmux`, `nano` and interactive `ssh`:

1. `session_create {"env_id": "...", "pty": true, "cols": 120, "rows": 40}`
2. `session_write {"session_id": "...", "data_base64": "<raw bytes>"}` — send
   keystrokes, arrow keys (`ESC [ A`), Ctrl-C (`0x03`), paste, etc.
3. `session_read {"session_id": "...", "max_bytes": 65536, "timeout_ms": 1000}`
   — returns base64 terminal bytes, including escape sequences.
4. `session_resize {"session_id": "...", "cols": 100, "rows": 30}`
5. `session_close {"session_id": "..."}`

`session_exec` is pipe-only and intentionally cannot be used for a PTY. The TUI
itself uses the same envd PTY path for its terminal pane. AI agents can therefore
operate TUI applications exactly as a human would, with approvals controlling
when `session_write` is allowed.

## Collaboration

These pre-existing envd wrappers complete the workbench and run alongside the tools above. They are not part of this manifest batch.

| Tool | Capability | Use | Returns |
| --- | --- | --- | --- |
| `session_create`, `session_exec`, `session_read`, `session_write`, `session_resize`, `session_close` | `exec.session` | Persistent pipe or PTY shell sessions inside the environment. | Session IDs, captured output, and terminal state. |
| `blackboard_get`, `blackboard_set`, `blackboard_list` | `state.shared` | Share JSON values by key; `expected_version` enables compare-and-swap. | Values, versions, and entry listings. |
| `flag_add`, `flag_list` | `state.shared` | Record and list captured flags in the shared ledger. | Deduplicated flag entries. |
| `spawn_agent`, `agent_result` | `agent.spawn` | Delegate a goal to a headless subagent and collect its state and transcript. | `task_id` and the subagent result. |

## Hard limits

Limits are enforced in `envd`; request values outside a range are either clamped or rejected.

| Area | Limit |
| --- | --- |
| `net_dns` | Records: A, AAAA, CNAME, MX, NS, TXT, SRV, PTR. Timeout 100-30000 ms (default 5000). Custom server must be `host:port`. |
| `net_dial` | Port 1-65535. `send_base64` at most 64 KiB. `read_bytes` 0-65536 (default 1024). Timeout 100-30000 ms. Read window is capped at 2 s. |
| `net_tls` | Port defaults to 443. Timeout 100-30000 ms. Certificate verification is reported, never assumed. At most 8 certificates and 32 KiB of PEM are returned. |
| `net_scan` | 1-64 targets after de-duplication. Ports 1-65535. At most 8192 probes per call. Timeout 50-5000 ms (default 500). Concurrency 1-128 (default 32), with 5 ms pacing per probe. The whole call has a 30 s wall-clock deadline. Banner read at most 256 bytes; results are capped by a ~160 KiB global budget and `truncated`/`timed_out` are set when the budget or deadline is hit. |
| `web_request` | Only `http` and `https`. Timeout 100-120000 ms (default 30000). Response body is capped at 64 KiB (default 64 KiB) so the base64 body, optional UTF-8 copy and 16 KiB of headers fit one 256 KiB control frame; `truncated`/`headers_truncated` report cuts. `body_base64`, JSON and form request bodies are capped at 64 KiB; use `exec`/`curl` for larger uploads. Redirects follow by default, 0-20 hops (default 10); caller headers are dropped on cross-origin redirects and HTTPS-to-HTTP downgrades. Proxy must be `http` or `https`. |
| `file_hash` | File must be at most 512 MiB; the copy is bounded by `max_bytes` even if the file grows. |
| `file_strings` | `min_len` 4-64 (default 4). `limit` 1-500 (default 200). Scan range 1 byte-16 MiB (default 4 MiB). Each string is cut at 256 characters. |
| `file_hexdump` | Dump length 1 byte-16 KiB (default 4096). Width 8-32 bytes (default 16). |
| `file_entropy` | Analyze 1 byte-4 MiB (default 256 KiB). Chunk size 4096-65536 (default 4096). |
| `file_search` | Pattern at most 512 characters (256 when regex). Results 1-500 (default 100). Per-file read 1-8 MiB (default 1 MiB). Walk at most 1-100000 files (default 10000). Hidden files, symlinks, and NUL-containing files are skipped unless the mode allows them. |
| `archive_list` | zip, tar, tar.gz, and gzip only. Entry listing capped at 1000; tar.gz decompression is capped at 128 MiB. |
| `archive_extract` | zip, tar, tar.gz, and gzip only. 1-1000 entries (default 1000). 512 MiB total by default and at most 512 MiB. Zip-slip, symlink/junction destinations, reserved device names, symlink/device members, and escaping parents are rejected or skipped. Tar.gz decompression is bounded. Existing files are skipped unless `overwrite` is true; successful members are written through a temporary file and renamed. |
| `secret_scan` | 1-5000 files (default 1000). Per-file read 1-4 MiB (default 512 KiB). Findings capped at 500. Previews are redacted (short values reveal at most two leading characters; connection strings hide the password); raw secrets are never returned. Binary, symlink, and hidden files are skipped unless hidden traversal is enabled. |
| `binary_info` | ELF, PE, or Mach-O only. Sections and imports are capped at 200. Metadata only: no disassembly, emulation, or execution. Parser panics are converted to `INTERNAL` errors instead of killing envd. |
| `transform` | Output is capped at 64 KiB (use a workspace file for larger data). `random_bytes` length 1-4096. `xor_key` is required for `xor`. `data_base64` takes precedence over `data`. |

## Approval behavior

The kernel's ask policy (`policy_with_approvals(true)`) gates risky calls before `envd` runs them. Approval is per tool call, not per probe or per redirect, and denying a call returns an error to the agent without side effects. The selected **permission mode** chooses the prompt set: `manual` asks for every gated tool, `auto` auto-approves workspace writes/scans/HTTP but still asks for commands, raw dials, archive extraction and delegation, and `bypass` removes prompts while keeping hard denials. See [`permissions.md`](permissions.md).

| Decision | Tools |
| --- | --- |
| Ask (approval-gated) | `net_scan`, `net_dial`, `web_request`, `archive_extract`; also the existing `exec`, `session_exec`, `session_write`, `write_file`, `spawn_agent`, and `http_request`. |
| Allow (automatic) | `net_dns`, `net_tls`, `net_info`, `file_hash`, `file_type`, `file_strings`, `file_hexdump`, `file_entropy`, `file_search`, `archive_list`, `binary_info`, `secret_scan`, `finding_add`, `finding_list`, `report_generate`, and `transform`. |

Operators who need a stricter or looser posture change the policy table at deployment time. The approval prompt is where an operator confirms that the target host, ports, URL, or archive destination is in scope.

## Example workflows

### 1. Web recon

1. `net_info` to record the environment's addresses and DNS servers.
2. `net_dns` with `type` set to `A`/`AAAA`, then `CNAME`, then `TXT` to map the target and discover ownership or verification records.
3. `net_tls` against the web host to capture certificate names, issuer, validity, and ALPN before sending traffic.
4. `web_request` with `method: "GET"`, custom `headers`, and `follow_redirects: true` to collect status, headers, cookies, and body; use `json` or `form` for authenticated or API probes.
5. `net_scan` only against the explicit host list and the small set of ports implied by the observed services. Both `web_request` and `net_scan` require approval under the ask policy.

### 2. Host triage

1. `net_info` for hostname, interfaces, and resolver state.
2. `net_scan` with explicit `targets` and `ports` (or a narrow `port_range`) to enumerate reachable services.
3. `net_dial` on selected open ports with `read_bytes` to capture service banners, or with `tls: true` when the service speaks TLS.
4. `net_tls` on TLS services to record the certificate chain and verification result.
5. `file_hash` and `file_type` on artifacts pulled back into the workspace, then `secret_scan` on configuration trees to find exposed credentials.
6. `archive_list` followed by `archive_extract` when the collected evidence arrives as an archive; extraction requires approval.

### 3. CTF forensics

1. `file_type` to identify the challenge artifact, then `file_hash` to record its digest.
2. `file_entropy` to spot packing, compression, or encrypted regions.
3. `file_strings` and `file_hexdump` to recover hints, offsets, and embedded data.
4. `binary_info` for ELF/PE/Mach-O section, import, and hardening context.
5. `archive_list` and `archive_extract` when the artifact is a container.
6. `transform` with `base64_decode`, `hex_decode`, `gunzip`, `xor`, `jwt_decode`, `hash`, or `hmac` to peel encodings and recover the flag.
7. `secret_scan` to catch incidental credentials, and `flag_add` to record the flag in the shared ledger.

### 4. Assessment with evidence and report

1. `net_info`, `net_dns` and `net_tls` to establish the environment and target identity.
2. `net_scan` and `net_dial` for the approved host/port set; `web_request` for the web surface.
3. `file_type`, `file_hash`, `file_entropy` and `file_strings` on collected artifacts; `binary_info` for executables.
4. `finding_add` for each confirmed issue with severity, asset, evidence and tags. Re-running the same title/asset updates the existing entry instead of duplicating it.
5. `finding_list` with `severity: "high"` or a tag to review coverage.
6. `report_generate` and `write_file` to save `report.md` next to the evidence; `flag_add` records any captured flags for the scoreboard.

## Safety and scope

`envd` is the execution boundary. Tools run in the selected environment, file paths resolve inside that environment's workspace, and network calls originate from the environment rather than the control plane. The kernel grants only the capabilities declared by each manifest; `net.egress` covers the recon and web tools, `fs.workspace` covers file, archive, binary, and secret tools, and `transform` requests no capability. Agent modes restrict the loaded tool set further: `plan` and `investigate` do not load command, write, session, raw-dial or extraction tools. See [`modes.md`](modes.md).

The operator controls deployment: which environments exist, where they run, what egress they can reach, whether a sandbox or network policy is applied, and which policy table (allow, ask, or deny) is in force. `envd --net-allow <CIDR|domain>...` adds a deployment egress allowlist: every DNS, TCP, TLS, scan, HTTP and custom-resolver target is checked before any packet is sent, and an empty list allows every target. The older kernel-side `http_request` tool is a controller-side egress path and is not covered by the envd allowlist; prefer `web_request` for target traffic, and deny `http_request` in the policy if all egress must go through envd. Authorization to test a host, URL, or network is the operator's responsibility, not something a tool can infer.

There is no built-in internet-wide scanning. `net_scan` accepts an explicit host list of at most 64 entries and a bounded port set, performs at most 8192 connect probes per call, applies per-probe timeouts and pacing, and never expands a target into a range on its own. `net_dial`, `net_tls`, and `web_request` contact one explicit endpoint per call. Prefer the smallest probe set that answers the question, keep approvals enabled for risky calls, and use `secret_scan` previews rather than extracting raw credentials.
