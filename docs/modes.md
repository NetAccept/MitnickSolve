# Agent modes

MitnickSolve runs every task in one of four concrete modes, or in `auto` mode,
which routes to one of them before the agent starts. A mode is a security
boundary: the kernel only loads the tools the mode allows, appends mode-specific
operating rules to the system prompt, and records the selected mode in the task
snapshot (plus `mode.select`/`mode.route` events when the agent resolves it).

| Mode | Chinese | Intent | Writes / executes? | Typical use |
| --- | --- | --- | --- | --- |
| `plan` | 计划 | Read-only. Produce an actionable plan, execute nothing. | No | Engagement planning, attack-path review, change planning |
| `investigate` | 调查 | Read-only evidence collection and triage. | No workspace/target changes; findings only | Recon, incident triage, Windows/Linux host review |
| `exploit` | 攻击 | Authorized active testing and exploitation. | Yes, approval-gated | CTF, pentest actions, payload testing |
| `harden` | 加固 | Defensive remediation and verification. | Yes, approval-gated | Patching, firewall/SSH/HTTP hardening, rollback |
| `auto` | 自动（路由） | Route to the least-invasive concrete mode that fits the goal. | Inherits the resolved mode | Default for new tasks |

## What each mode can call

The matrix is enforced by loading only the listed tools. A mode cannot call a
tool that is not loaded, even if it asks for it by name.

| Capability | plan | investigate | exploit | harden |
| --- | --- | --- | --- | --- |
| Read files, list/stat, artifacts | yes | yes | yes | yes |
| `file_*`, `binary_info`, `secret_scan`, `archive_list`, `transform` | yes | yes | yes | yes |
| `net_info`, `net_dns`, `net_tls` | yes | yes | yes | yes |
| `net_scan`, `web_request` | no | yes (approval) | yes (approval) | yes (approval) |
| `net_dial`, `http_request` | no | no | yes (approval) | yes (approval) |
| `exec`, `session_*`, `write_file`, `archive_extract` | no | no | yes (approval) | yes (approval) |
| `finding_add` / `finding_list` / `report_generate` | list/report only | yes | yes | yes |
| `blackboard_set`, `flag_add` | no | flags/findings only | yes | yes |
| `spawn_agent` | plan child only | plan/investigate child | any child | plan/investigate/harden child |

`plan` and `investigate` are therefore safe defaults for an unfamiliar target.
Approvals remain in force for `exploit` and `harden`; disabling approvals is a
separate, explicit operator decision.

## Automatic routing

`auto` uses a deterministic, auditable keyword router. The decision is emitted as
a `mode.route` event with the matched keywords and per-mode scores, and the
resolved mode is stored in the task snapshot.

- Strong planning language ("plan", "design", "方案", "计划") routes to `plan`,
  even if the goal also mentions exploitation.
- Remediation language ("harden", "patch", "fix", "加固", "修复") routes to
  `harden`.
- Active language ("exploit", "get a shell", "攻击", "渗透") routes to `exploit`.
- Recon/triage language ("scan", "investigate", "调查", "取证") and the default
  fallback route to `investigate`.
- Ties prefer the least-invasive mode.

The router is intentionally conservative: ambiguous goals start in
`investigate`, where the agent can still read evidence and propose next steps.
Switch to `exploit` explicitly when the engagement authorizes active work.

## Subagents

`spawn_agent` takes an optional `mode`. A child can never be more powerful than
the parent:

- `plan` → `plan`.
- `investigate` → `plan` or `investigate`.
- `harden` → `plan`, `investigate`, or `harden`.
- `exploit` → any concrete mode.

Subagents inherit the parent's approval posture: when the parent runs with
approvals, the child asks for approval on gated tools instead of running
unattended.

## Configuration

Built-in modes are defaults. Put `modes.json` under the daemon root (`.msk/modes.json`
by default) or pass `--modes-file <path>` to `msk`/`mskd`. `MSK_MODES` is also
honored. The TUI reads the active mode list from the daemon (`kernel.hello`), so
new modes appear in the picker without rebuilding the TUI. The daemon reads the
file at startup; restart `mskd` after editing it.

```json
{
  "auto": {"default": "investigate"},
  "modes": [
    {
      "id": "plan",
      "label": "规划",
      "extends": "plan"
    },
    {
      "id": "osint",
      "label": "OSINT",
      "description": "public-source reconnaissance",
      "prompt": "MODE: OSINT. Collect only public, read-only evidence.",
      "extends": "investigate",
      "tools": ["net_dns", "net_tls", "web_request", "file_type", "file_hash", "transform"],
      "spawn": ["osint", "investigate", "plan"],
      "keywords": ["osint", "passive", "公开情报"],
      "priority": 2,
      "order": 4
    }
  ]
}
```

Fields:

| Field | Meaning |
| --- | --- |
| `id` | Lowercase ASCII id. Reserved: `auto`. |
| `label` | Name shown in the composer slot and picker (Chinese or English). |
| `description` | One-line picker description. |
| `prompt` | Operating rules appended to the system prompt. |
| `tools` | Tools loaded for this mode. `"*"` = all; `"@investigate"` = reuse another mode's tools. |
| `spawn` | Child mode ids this mode may delegate to. `"*"` = any. |
| `keywords` / `priority` | Deterministic `auto` scoring: a matching keyword adds `priority` points. |
| `order` | Picker order; lower comes first. |
| `write_capable` | Allow HTTP request bodies and write tools. Inferred from the tool set when omitted. |
| `extends` | Copy another mode first, then apply the non-empty fields in this entry. |
| `disabled` | Remove a built-in mode. |

Overriding a built-in by `id` replaces it; `extends` is the ergonomic way to
change only a label, prompt or tool subset. `@mode` references are validated
for unknown names and cycles. A complete example is
[`modes.example.json`](../modes.example.json).

The mode appears in exactly one place in the TUI: the leading slot of the
composer (`计划 · openai · gpt-5 · effort high`). There is no extra mode chip or
header suffix.

## Usage

CLI:

```sh
mskctl --root .msk create --mode plan --goal "plan the assessment"
mskctl --root .msk create --mode investigate --goal "inventory the exposed services"
mskctl --root .msk create --mode exploit --goal "test the upload endpoint"
mskctl --root .msk create --mode harden --goal "harden ssh and verify"
mskctl --root .msk create --mode auto --goal "find out what is exposed"
```

TUI:

- `Ctrl+P` → **Agent mode** (or `/mode`) opens the mode picker.
- `/mode <id|label>` sets the mode for the next session and saves it.
- Selecting a different mode while a session is open starts a fresh session on
  the next message; the old session remains in the session switcher.
- The composer's leading slot shows the active/pending mode label; there is no
  separate mode chip or header suffix.
- A scripted offline demo or a resumed checkpoint with `auto` defaults to the
  active `exploit` mode; pass an explicit mode to override it.

Direct agent:

```sh
target/debug/msk --mode plan --session ...
target/debug/msk --mode auto --goal "triage the host" --headless ...
```

`mskd` resolves `auto` from the task goal before spawning the agent, so the TUI
and `mskctl` see the concrete mode immediately. A scripted offline demo is
forced into `exploit` because that demo exists to exercise shell/session tools,
and resuming a checkpoint with `auto` also defaults to `exploit` because the
restored session may expect shell history. Pass an explicit mode to override
either default.

## Evidence and reporting

Modes do not discard evidence: `investigate` records findings and flags;
`exploit` records confirmed findings; `harden` records before/after evidence.
`report_generate` works in every mode that can read the ledger, and the reports
are written with `write_file` only in write-capable modes.
