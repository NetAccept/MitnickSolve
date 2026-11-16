# Permission modes

Permission modes control approval prompts. They are independent of agent modes:
the agent mode decides **which tools exist**, while the permission mode decides
**which tool calls pause for a human**.

| Mode | Chinese | Behavior |
| --- | --- | --- |
| `auto` | 自动 | Workspace writes, `net_scan`, and `web_request` run automatically. Commands, PTY writes, raw dials, archive extraction, HTTP-host calls and subagents still ask. Default. |
| `manual` | 手动 | Every gated tool asks: commands, writes, scans, HTTP, raw dials, extraction, delegation. |
| `bypass` | 放行 | No approval prompts. The policy table's hard `Deny` decisions still apply, and envd deployment policy (`--net-allow`, sandbox, profiles) is unaffected. |

A hard denial is not an approval prompt: a manifest cannot self-grant
capabilities, a mode does not load disallowed tools, and envd can still reject a
target outside its deployment policy. `bypass` removes prompts, not those
boundaries.

## Where it applies

- `msk --permission auto|manual|bypass`
- `mskctl create --permission ...`
- The daemon's `task.create` accepts `permission`; the task snapshot includes it.
- Web API `POST /api/tasks` accepts `permission`.
- Subagents inherit their parent's permission posture.

`--approvals ask|allow` remains as a compatibility alias (`ask` → `manual`,
`allow` → `bypass`). A new task with no explicit permission defaults to `auto`.

## TUI

- `Ctrl+P` → **Permission mode**, or `/permissions`, opens the picker.
- `/permissions auto|manual|bypass` sets it directly.
- The footer shows `权限 自动` / `手动` / `放行`.
- Changing it while a session is open starts a fresh session on the next
  message, because the approval posture is fixed when the agent process starts.

The agent mode is shown in the composer's leading slot; permission is shown in
the footer, so the two controls do not compete for the same space.

## Relationship to agent modes

| Agent mode | Typical permission |
| --- | --- |
| `plan` / `investigate` | `auto` or `manual`; the mode already excludes command and write tools |
| `exploit` | `auto` for assisted testing, `manual` for high-friction review, `bypass` only in disposable labs |
| `harden` | `auto` for approved maintenance windows; `manual` for production |

For a CTF where the environment is disposable, `exploit` + `auto` is a practical
default. For a production network, use `investigate` + `manual`, or `harden` +
`manual`, and keep the envd `--net-allow` deployment allowlist.
