# Execution targets

An **execution target** is the envd backend a session uses. It is a property of
the session, not of the daemon: one session can run on the local envd, another
on a remote lab, and a third with no execution environment at all.

Targets answer three separate questions:

| Question | Field |
| --- | --- |
| Where does this session execute? | `target` (session attribute) |
| Which container/workspace did envd create for it? | `env_id` (checkpoint + snapshot) |
| Which tools are available? | agent mode ∩ target kind |

## Configuration

Targets live in `<root>/envd.json` (usually `.msk/envd.json`). Pass
`--targets-file <path>` to `msk`/`mskd` to use another file, or
`--default-target <id>` to override the default.

```json
{
  "default": "local",
  "targets": [
    {
      "id": "local",
      "label": "Local envd",
      "kind": "embedded",
      "binary": "envd.exe",
      "profile": "dev",
      "workspace": "."
    },
    {
      "id": "lab",
      "label": "Lab envd",
      "kind": "service",
      "url": "https://envd.lab:7800",
      "token_env": "ENVD_LAB_TOKEN",
      "ca": "ca.pem",
      "data_addr": "envd.lab:7801",
      "data_tls": true,
      "profile": "isolated"
    }
  ]
}
```

- `id` is used by `/target`, `@target:...`, `task.create` and `mskctl target`.
  `none` is reserved for the detached target.
- `embedded` spawns the envd binary for the session. Only the `dev` profile is
  allowed, matching the existing embedded security boundary.
- `service` connects to a running `envd --service`. `token_env` names the
  environment variable holding the bearer token; the token is never written to
  `task.json` or TUI config.
- `ca` is a PEM bundle for the HTTPS control plane and TLS data plane.
- Relative embedded `binary` paths are resolved against the targets file, the
  current directory and the daemon binary directory.
- `none` is always available and does not need to be listed.

Without an `envd.json`, the legacy flags still work:

- `mskd --envd <path>` synthesizes the `local` target.
- `mskd --envd-url <url> --envd-token <token>` synthesizes the `remote` target.
- Both can be combined with `--targets-file`; a legacy target is added only if
  its id is not already defined.

## Selecting a target

At task creation:

```sh
mskctl --root .msk create --goal "triage ./evidence" --target lab --interactive
msk --envd-config .msk/envd.json --target lab --session
```

In the TUI:

- `Ctrl+P` → **Execution target**, or `/target`, opens the picker.
- `Enter` applies the choice. For an open session, the switch happens between
  turns; for a new or saved session it becomes the next session's target.
- `/target lab`, `/target none` and `/envd` are shortcuts.
- The footer shows `⌂ Local envd`, `☁ Lab envd`, `◇ Lab envd` (saved/detached)
  or `⊘ No execution environment`.

In a message:

```text
@lab scan 10.0.0.5 and show the TLS certificate
@target:none summarize the evidence you already collected
```

`@target:NAME` is accepted only at the start of a message and only for a known
target. The rest of the message is sent normally; an email address or prose
containing `@` is not interpreted.

`@target` is a session-level bind, not a per-tool router. All tools in that
message run on the same target. Switching to a target creates a **fresh
environment**; shells, PTY sessions and file paths from the previous environment
are not carried over. The conversation and checkpoint are preserved.

## What happens on a switch

1. The daemon validates the target and that the task is waiting for input.
2. The agent connects to the new envd and creates an environment.
3. Only after the connection succeeds does the agent replace the live backend.
   A failed switch leaves the previous target in place and reports
   `target.error`.
4. The new target, label and `env_id` are written to the task snapshot and
   `task.json`; a `target.switch` event records the tool set in force.
5. The old embedded envd exits when its client is dropped. Remote envd
   environments are left on the service; envd currently has no `env_close`
   tool, so operators should clean them up at the service or restart it.

Switching is rejected while a turn is running. Wait for the turn to finish, or
interrupt it first.

## Tool availability

The agent exposes `mode.tools ∩ target.tools`. With a target attached, the
mode's full tool set is available. With `none`, only tools handled by the
kernel and control plane remain:

`artifact_read`, `wasm_eval`, `http_request`, `blackboard_*`, `finding_*`,
`report_generate`, `flag_*`, `spawn_agent`, `agent_result`, `echo`.

`exec`, `session_*`, `read_file`/`write_file`, `net_*`, `web_request`,
`archive_extract` and `transform` require an envd target. Calls are rejected in
both the provider tool list and the kernel dispatcher; the model is not shown a
tool it cannot use.

## Persistence and resumes

- `task.json` stores `target`, `target_label`, `target_kind` and the last
  `target_env_id`; the API key is never stored.
- Restored sessions show as `▣ saved` with their original target. Replying
  creates a new task from the checkpoint on that same target.
- If the target was removed from `envd.json`, the saved transcript remains
  readable but resuming fails with a clear error instead of silently falling
  back to `local`.
- Subagents inherit the parent's target, so delegated work stays on the same
  execution boundary.

## Security notes

- The target is part of the execution boundary. It must be chosen explicitly;
  there is no automatic fallback to the local envd when a remote target is
  unreachable.
- Remote tokens come from environment variables and are passed to the agent
  process without being written to task metadata. TLS verification uses the
  system roots unless `ca` replaces them.
- Deployment-level restrictions still apply: envd `--net-allow`, isolated
  launcher requirements, approval policy and the mode's tool boundary are
  unchanged by target selection.
- The daemon-managed PTY (`t`) currently uses the **default** target, not the
  selected session's target. Use the agent's `session_*` tools for per-session
  shells; a target-aware terminal is a follow-up.
