# Terminal app

`msk` with no arguments is the normal entry point. It opens the conversational
TUI and starts a local `mskd` daemon if one is not already listening. The TUI
is a client of the same daemon used by `mskctl`, the web UI, and the eval
harness, so nothing in the kernel changes when you use it.

## Build and launch

```sh
cargo build --bins
(cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd)
target/debug/msk
```

If `msk-tui` is not next to `msk`, run `cargo build --bins` first. The daemon
state lives in `.msk/`; use `msk-tui --root /path` for a different location
(`msk` with arguments keeps its headless/agent behavior).

The embedded envd opens the **current directory as the workspace**. The agent's
file tools and shell therefore see the project you launched from, like a normal
coding CLI. Use `msk --workspace /path` or `mskd --workspace /path` for a
different project root; remote envd keeps its operator-configured workspace.

`msk` with arguments keeps its existing headless/session/daemon behavior; only
the no-argument invocation is the interactive app. `msk-tui` can also be
launched directly:

```sh
target/debug/msk-tui                 # auto-start/connect .msk
target/debug/msk-tui --root .msk/demo
target/debug/msk-tui --connect .msk # do not auto-start
target/debug/msk-tui --events .msk/events.jsonl  # read-only event viewer
```

## First screen

With no tasks the composer is focused. Type a challenge and press Enter; the
first message creates an interactive task, starts it, and sends the same text
as the first turn.

```text
MitnickSolve  .msk  ·  provider scripted-demo  ·  0 task(s)
conversation
  Welcome. Describe the challenge below and press Enter to start a task.
  Examples: inspect ./challenge and find the flag
  Commands: /new /provider openai /terminal /approvals /help
  Offline demo provider: replies are canned. Use /provider openai for a real model.
new task — provider scripted-demo
  > Describe the challenge…
```

With no API key, the offline `scripted-demo` provider replays a complete tool
session (persistent shell, large output → artifact, artifact read, close). It is
for showing the UI; it does not reason about the goal. `/provider openai` (with
the environment below) switches to a real model.

## What a completed turn looks like

This is an actual `msk-tui --root .msk/showcase --snapshot` frame after a
mock-model task asked for `exec` and the approval was granted:

```text
┌session───────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│MitnickSolve  C:\Users\cy\MitnickSolve\.msk\ws-project  ·  openai / gpt-5 / effort minimal  ·  1 session(s)           │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌session — Inspect challenge.txt and explain it [done]─────────────────────────────────────────────────────────────────┐
│tool: ▸ env_create {"kind":"managed","name":"session","profile":"dev","root":"."}                                     │
│                                                                                                                      │
│tool: ✓ env_create                                                                                                    │
│                                                                                                                      │
│you: Inspect challenge.txt and explain it                                                                             │
│                                                                                                                      │
│agent: mock complete                                                                                                  │
│                                                                                                                      │
│turn end: awaiting_input                                                                                              │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
│                                                                                                                      │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
┌message · Inspect challenge.txt and explain it [done]─────────────────────────────────────────────────────────────────┐
│> press Enter or i to write  ·  / commands                                                                            │
│                                                                                                                      │
│                                                                                                                      │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
Enter/i message  n new  / commands  Tab sessions  PgUp/PgDn scroll  v events  s interrupt  ? help  q quit
```

The approval step is a modal over the same conversation, with the tool, reason,
arguments, and `1/N` pending count. Press `y` to approve or `n` to deny.

## Provider, model and effort

Press `p` (or `/setup`) for the guided setup. It saves
`<root>/tui-config.json` and applies to new tasks:

1. **Provider** — `scripted-demo` (offline), `openai` (Chat Completions),
   `openai-responses`, or `component`.
2. **Model** — presets (`gpt-5`, `gpt-5-mini`, `o3`, `o4-mini`) or a custom
   model name.
3. **Effort** — `default`, `minimal`, `low`, `medium`, `high`. This is sent as
   `reasoning_effort` (Chat Completions) or `reasoning.effort` (Responses) to
   models that support it.
4. **Base URL** — optional; empty uses `MSK_LLM_BASE_URL` or the provider
   default.

The header always shows `provider / model / effort`. The API key is never
stored in the config or sent over IPC; export it where `mskd` starts. The setup
screen reports whether the daemon sees `MSK_LLM_API_KEY`. Run
`msk-tui --setup` to open the wizard directly, or `msk-tui --doctor` to check
paths and key presence.

## Keys and commands

| Key | Action |
| --- | --- |
| `Enter` / `i` | write a message, or describe a new task |
| `Enter` in the editor | send, or start the task |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | insert a newline |
| `Esc` | leave the editor; while running, interrupt the turn |
| `Ctrl+C` | interrupt a running turn; otherwise show the quit hint |
| `PgUp` / `PgDn` | scroll the conversation |
| `End` | jump to the newest message |
| `j` / `k`, `g` / `G` | previous/next task, first/last task |
| `n` | start a new task |
| `p` | provider / model / effort setup |
| `s` / `Esc` | interrupt the running turn |
| `x` | cancel the selected task |
| `a` | show the approval inbox |
| `y` / `n` | approve / deny when an approval modal is open |
| `t` | open/focus the daemon PTY; `Ctrl+Q` detaches |
| `v` | toggle the raw event inspector (off by default) |
| `?` | help |
| `q` | quit |

Slash commands work in the editor:

```text
/new [goal]      /demo           /provider NAME
/terminal        /approvals      /interrupt
/cancel          /clear          /status
/help            /quit
```

Providers: `scripted`, `scripted-demo`, `openai`, `openai-responses`
(`responses` is accepted as an alias), `component`.

Bracketed paste is enabled. Pasting a multi-line script inserts it rather than
executing each line, and pasted text never triggers `q`/`n`/`x` shortcuts.

## Tool activity and approvals

The conversation renders tool calls inline:

```text
tool: ▸ session_exec {"command":"echo session-one", ...}
tool: ✓ session_exec  {"stdout":"session-one"}
agent: session-one
```

When policy is `ask`, the turn pauses on the first risky tool and a modal shows
the tool, reason, arguments and the pending count (`1/N`). Press `y` to approve
or `n`/`Esc` to deny. Approvals are also loaded from `approval.list` on connect,
so a TUI attached after the request can still answer it. Enter is deliberately
not an approval shortcut.

## History and resume

Selecting a task hydrates its conversation from `task.history` (structured
events), not from a truncated transcript string. Switching tasks, attaching a
second TUI, or finishing a turn all reload the same timeline. `PgUp`/`PgDn`
scrolls it; `End` follows the newest output.

Tasks are in-memory in `mskd`, but their event logs and checkpoints remain under
`.msk/tasks/<task>/`. `msk --root ... --resume-checkpoint ...` can reopen a
checkpoint; in service mode it reopens the original envd environment when
possible, otherwise it creates a fresh one and marks old session IDs stale.
Forks always use a fresh environment.

## Real models

Set the OpenAI-compatible environment before launching:

```sh
export MSK_LLM_BASE_URL=https://api.openai.com/v1
export MSK_LLM_MODEL=gpt-5
export MSK_LLM_API_KEY=...
target/debug/msk
```

The TUI auto-selects `openai` when `MSK_LLM_MODEL` and `MSK_LLM_API_KEY` are
present. Otherwise it stays on the offline demo. `MSK_PROVIDER` overrides the
selection for one shell.

## One-command demo

The demo starts the mock OpenAI-compatible server and opens the TUI with a
clean daemon:

```powershell
# Windows
./scripts/demo.ps1
```

```sh
# Linux/macOS
./scripts/demo.sh
```

Then type `exec` and press Enter. The first model response asks the kernel to
run an `exec` tool; approve it with `y`. You should see the tool card, the
command output, and the final model text. Try `file`, `secret`, `flag`,
`board`, `spawn`, or `/help` for the other mock branches.

No Python/API key? Run the TUI with the offline provider and use `/demo`:

```sh
target/debug/msk-tui --root .msk/offline --provider scripted-demo
```

## Diagnostics

```sh
target/debug/msk-tui --doctor
target/debug/msk-tui --root .msk --snapshot
```

`--doctor` reports the daemon, envd, tools directory, provider, model and key
presence. `--snapshot` renders one real TUI frame to stdout (no alternate
screen); it is useful for CI or for pasting a screen into a bug report.

The raw event viewer remains available without a daemon:

```sh
target/debug/msk-tui --events .msk/events.jsonl
```

## Troubleshooting

- **`mskd is not next to msk-tui`** — run `cargo build --bins`; or set
  `MSK_MSKD`/`MSK_ENVD`.
- **`tools directory not found`** — run from the repository root, set
  `MSK_TOOLS_DIR`, or keep `tools/wasm` under an executable ancestor.
- **OpenAI task fails immediately** — run `--doctor`; check `MSK_LLM_MODEL`,
  `MSK_LLM_API_KEY`, and `MSK_LLM_BASE_URL`. See `.msk/mskd.log` and the task's
  `agent.stderr.log`.
- **Approval modal does not appear** — the daemon may have been started before
  the request; approvals are hydrated every second, so it should appear shortly.
  If it does not, run `mskctl --root .msk approvals`.
- **Stale daemon** — stop `mskd`, or use `--no-autostart` and start it manually
  with `mskd --root ... --envd ... --tools-dir ...`.
