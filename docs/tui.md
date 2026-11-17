# Terminal app

`msk` with no arguments opens the conversational app. It is a semi-graphical
terminal UI: a pixel wordmark, rounded composer, status chips, live tool cards,
modal approvals and a command palette. It starts a local `mskd` daemon if one is
not already listening, and it is a normal client of that daemon — the same one
used by `mskctl`, the browser UI and the eval harness.

## Build and launch

```sh
cargo build --bins
(cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd)
target/debug/msk
```

`msk-tui` lives next to `msk`. The daemon state is under `.msk/` by default. The
app uses the **current directory as the workspace**, so the agent's file tools
and shell see the project you launched from. Use `msk --workspace /path` or
`mskd --workspace /path` for another project root.

`msk` with arguments keeps its headless/agent/daemon behavior. You can also run
the app directly:

```sh
target/debug/msk-tui                 # auto-start/connect .msk
target/debug/msk-tui --root .msk/demo
target/debug/msk-tui --connect .msk  # do not auto-start
target/debug/msk-tui --events .msk/events.jsonl   # read-only event viewer
```

## First screen

This is an actual `--snapshot` frame (120×40):

```text
                         █   █ █████ █████ █   █ █████  ████ █   █  ████  ███  █     █   █ █████
                         ██ ██   █     █   ██  █   █   █     █  █  █     █   █ █     █   █ █
                         █ █ █   █     █   █ █ █   █   █     █ █   █     █   █ █     █   █ █
                         █ █ █   █     █   █ █ █   █   █     ██     ███  █   █ █     █   █ ████
                         █   █   █     █   █  ██   █   █     █ █       █ █   █ █     █   █ █
                         █   █   █     █   █   █   █   █     █  █      █ █   █ █      █ █  █
                         █   █ █████   █   █   █ █████  ████ █   █ ████   ███  █████   █   █████

                                                    ~   scripted-demo

                ╭──────────────────────────────────────────────────────────────────────────────────────╮
                │ Ask anything…  "inspect ./challenge and find the flag"                               │
                │                                                                                      │
                │  Build · scripted-demo                                                               │
                ╰──────────────────────────────────────────────────────────────────────────────────────╯
                                                               tab sessions   ctrl+p commands   F1 help

  ~                                                                                 MitnickSolve 0.1.1
```

- The large wordmark is drawn from Unicode blocks with a left-to-right gradient.
- The chips under it show the workspace, provider, model and effort.
- The rounded composer is always the focused input. Just start typing; Enter
  sends. `Shift+Enter` inserts a newline, bracketed paste inserts whole scripts.
- `Build · provider · model · effort` is always visible inside the composer.
- `Tab` opens the session switcher, `Ctrl+P` opens the command palette, `F1`
  shows every shortcut.
- The footer shows the workspace on the left and the version on the right.

With no configured provider, a full-screen setup card opens first. Choose
`scripted-demo` to try the app offline with no key, or `openai` /
`openai-responses` for a real model.

## Setup: provider, model and effort

`Ctrl+P` → **Provider and model**, or `/setup`, or `msk-tui --setup`.

```text
                  ╭ setup ───────────────────────────────────────────────────────────────────────────╮
                  │ MitnickSolve   connect a model                                                   │
                  │ You can change all of this later with ctrl+p.                                    │
                  │                                                                                  │
                  │ ● provider  ›  ○ model  ›  ○ effort  ›  ○ endpoint                               │
                  │                                                                                  │
                  │ Choose how the agent reaches a model                                             │
                  │ scripted-demo needs no key and works offline.                                    │
                  │                                                                                  │
                  │   scripted-demo       offline canned tool session, no model or key               │
                  │ ❯ openai              OpenAI-compatible Chat Completions                         │
                  │   openai-responses    OpenAI Responses API function calls                        │
                  │   component           signed llm.provider component configured on mskd          │
                  │                                                                                  │
                  │ mskd is missing MSK_LLM_API_KEY (export it before starting mskd)                 │
                  │ config  C:\Users\cy\MitnickSolve\.msk\ui-home2\tui-config.json                   │
                  │ ↑/↓ choose   Enter next   Esc back/close   Ctrl+U clear input                    │
                  ╰──────────────────────────────────────────────────────────────────────────────────╯
```

1. **Provider** — `scripted-demo` (offline), `openai` (Chat Completions),
   `openai-responses`, or a signed `component`.
2. **Model** — presets (`gpt-5`, `gpt-5-mini`, `o3`, `o4-mini`) or any custom
   model id.
3. **Effort** — `default`, `minimal`, `low`, `medium`, `high`. It is sent as
   `reasoning_effort` (Chat Completions) or `reasoning.effort` (Responses).
4. **Endpoint** — optional OpenAI-compatible base URL. Empty uses
   `MSK_LLM_BASE_URL` or the provider default.

The choice is saved to `<root>/tui-config.json` and applies to new sessions.
The API key is never stored in that file or sent over IPC; export
`MSK_LLM_API_KEY` where `mskd` starts. The setup card reports whether the daemon
sees it. `msk-tui --doctor` checks daemon, envd, tools and key presence.

## Command palette and sessions

`Ctrl+P` opens a fuzzy-filtered command palette. Type to filter, `↑/↓` to move,
Enter to run, Esc to close.

```text
                      ╭ commands ────────────────────────────────────────────────────────────────╮
                      │ ❯ ▌  type a command                                                      │
                      │                                                                          │
                      │ › New session               start a fresh conversation                    │
                      │   Sessions                  switch to another session (tab)               │
                      │   Resume latest             restore the newest checkpoint                 │
                      │   Provider and model        choose provider, model, effort                │
                      │   Model                     pick a model preset                           │
                      │   Reasoning effort          minimal, low, medium, high                    │
                      │   Agent mode                plan, investigate, exploit, harden, auto       │
                      │   Offline demo              run a canned tool session                     │
                      │   Terminal                  open the PTY workspace                        │
                      │   Event inspector           show the raw event stream                     │
                      │   Pending approvals         check queued tool approvals                   │
                      │   Assessment findings       review severity-ranked findings              │
                      │   Clear transcript          hide messages in this view                    │
                      │   Keyboard shortcuts        open the help overlay                         │
                      │   Quit                      leave MitnickSolve                            │
                      ╰──────────────────────────────────────────────────────────────────────────╯
```

`Tab` opens the session switcher. Sessions are titled by their goal; internal
task ids stay out of the way. Up/down selects, Enter switches, Esc closes.

```text
                      ╭ sessions ────────────────────────────────────────────────────────────────╮
                      │ › ● ready     Find the flag in ./challenge                                │
                      │   ✗ cancelled Interactive session                                         │
                      │                                    ↑/↓ select · Enter switch · Esc close │
                      ╰──────────────────────────────────────────────────────────────────────────╯
```

## Conversation

A real frame after a turn, with the header rule, role glyphs, tool cards and the
composer at the bottom:

```text
  ▌ Find the flag in ./challenge  ● ready                                                scripted · - · effort default
  ────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
    ▸ env_create {"kind":"managed","name":"session","profile":"dev","root":"."}

    ✓ env_create

  ❯ inspect ./challenge and find the flag

    echo: inspect ./challenge and find the flag

        ╭──────────────────────────────────────────────────────────────────────────────────────────────────────╮
        │ Reply…   Shift+Enter for a new line                                                                  │
        │                                                                                                      │
        │  Build · scripted                                                                                    │
        ╰──────────────────────────────────────────────────────────────────────────────────────────────────────╯
                                                                       tab sessions   ctrl+p commands   F1 help
  ~/.msk/ui-chat/ws                                                                                 MitnickSolve 0.1.1
```

- `▌` and `❯` mark the active session and your messages; agent text is plain.
- Tool calls appear as `▸ tool args`, results as `✓ tool` (green) or `✗ tool`
  (red). Streaming text ends with a blinking block.
- The header shows the session goal, a live state (`⠋ running`, `● ready`,
  `✗ failed`, `✓ done`), and provider/model/effort.
- `PgUp` / `PgDn` or `↑` / `↓` scroll; `End` follows new output.
- The event inspector (`Ctrl+P` → Event inspector, or `/events`) opens a dimmed
  sidebar with the raw kernel event stream.

## Approvals

When policy is `ask`, the turn pauses on the first risky tool and a modal opens
over the dimmed conversation:

```text
  ▌ run exec echo approved  ⠋ running                                                  openai · gpt-5 · effort minimal
  ────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
    ❯ please exec echo approved

    ▸ exec {"argv":["cmd","/C","echo approved"],"env_id":"env_..."}

            ╭ approval required · 1 of 1 · #1 ─────────────────────────────────────────────────────────────╮
            │tool  exec                                                                                    │
            │why   execute a command                                                                       │
            │                                                                                              │
            │arguments                                                                                     │
            │  {                                                                                           │
            │    "argv": [                                                                                 │
            │      "cmd",                                                                                  │
            │      "/C",                                                                                   │
            │      "echo approved"                                                                         │
            │    ],                                                                                        │
            │    "env_id": "env_..."                                                                       │
            │  }                                                                                           │
            │                                                                                              │
            │y allow     n deny                                                                            │
            ╰──────────────────────────────────────────────────────────────────────────────────────────────╯
```

Press `y` to approve or `n`/`Esc` to deny. Approvals are also hydrated from
`approval.list` every second, so a TUI attached after the request can answer it.

## Keys and commands

| Key | Action |
| --- | --- |
| any printable key | focus the composer and type |
| `Enter` | send, or start a new session |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | insert a newline |
| `Esc` | interrupt a running turn, or close the editor / overlay |
| `Ctrl+C` | interrupt a running turn; when idle, quit |
| `Tab` | session switcher |
| `Ctrl+P` | command palette |
| `F1` | keyboard shortcuts |
| `PgUp` / `PgDn`, `↑` / `↓` | scroll the conversation |
| `End` | jump to the newest output |
| `y` / `n` | approve / deny a pending approval |
| `Ctrl+Q` | detach from the PTY terminal |
| `Ctrl+U` | clear the composer |

Slash commands still work if you prefer typing them:

```text
/new [goal]   /demo        /provider NAME   /models      /model NAME
/effort NAME  /setup       /resume          /sessions    /terminal
/events       /approvals   /findings      /interrupt   /cancel      /clear
/mode         /mode NAME   /status        /help        /quit
```

Providers: `scripted`, `scripted-demo`, `openai`, `openai-responses`
(`responses` is an alias), `component`.

Bracketed paste is enabled. Pasting a multi-line script inserts it rather than
running each line, and pasted text never triggers a shortcut.

## Agent modes

`Ctrl+P` → **Agent mode** (or `/mode`) opens the mode picker. The mode is shown
in exactly one place: the leading slot of the composer, where opencode shows
`Build` — for example `自动 · openai · gpt-5 · effort high`. Enter applies the
selected mode (the configured label changes in that slot) and saves it. The list
comes from the daemon's `modes.json`, so custom modes appear automatically.

The mode is a tool-level boundary, not a label: `plan`/`investigate` cannot call
`exec`, `write_file`, `session_*`, `net_dial` or `archive_extract`, and subagents
cannot escalate. See [`docs/modes.md`](docs/modes.md) for the configurable
definitions, the full matrix and auto-routing rules.

## Permission modes

`Ctrl+P` → **Permission mode** (or `/permissions`) opens `auto`, `manual`, and
`bypass`. This is separate from the agent mode: the agent mode decides which
tools exist, the permission mode decides which calls pause for approval. The
footer shows `权限 自动`, `手动`, or `放行`. Changing it while a session is open
starts a fresh session on the next message. See
[`docs/permissions.md`](docs/permissions.md).

## Findings and reports

The agent records assessment findings with the `finding_add` tool. They live in
the daemon blackboard, survive restarts, and are visible to every attached
client. `Ctrl+P` → **Assessment findings** (or `/findings`) opens a
severity-ranked overlay: `j`/`k` scroll, `r` reloads, Esc closes. Each row shows
severity, title, asset, status, tags and a short description.

Use `Ctrl+P` → **Terminal** for the PTY, and ask the agent for `report_generate`
when you want a Markdown report assembled from the finding ledger and captured
flags; it saves with `write_file`.

## History and resume

Selecting a session hydrates its conversation from `task.history` (structured
events), not from a truncated transcript string. Switching sessions, attaching a
second TUI, or finishing a turn all reload the same timeline.

Tasks are in-memory in `mskd`, but event logs and checkpoints stay under
`.msk/tasks/<task>/`. `Ctrl+P` → **Resume latest** (`/resume`) restores the
newest checkpoint after a daemon restart. In service mode it reopens the
original envd environment when possible; otherwise it creates a fresh one and
marks old session ids stale. Forks always use a fresh environment.

## Real models

Set the OpenAI-compatible environment before launching:

```sh
export MSK_LLM_BASE_URL=https://api.openai.com/v1
export MSK_LLM_MODEL=gpt-5
export MSK_LLM_API_KEY=...
target/debug/msk
```

The setup card auto-selects `openai` when the daemon already has a key and model.
`MSK_PROVIDER` overrides the selection for one shell. If the key is missing, the
composer shows `missing MSK_LLM_API_KEY` in amber instead of failing silently.

## Demos

**Offline, no server or key:**

```sh
target/debug/msk --provider scripted-demo
```

Then press `Ctrl+P` → Offline demo, or type `/demo`. The daemon replays a full
tool session (persistent shell, large output → artifact, artifact read, close).

**Real model against a local mock:**

```powershell
./scripts/demo.ps1     # Windows
```

```sh
./scripts/demo.sh      # Linux/macOS
```

Type `exec` and press Enter, then approve with `y`. You should see the tool
card, command output and final model text. Try `file`, `secret`, `flag`,
`board`, `spawn` for other mock branches.

## Browser UI

If you want a real graphical window instead of a terminal, the daemon also
serves a browser UI:

```sh
target/debug/msk-web --root .msk --listen 127.0.0.1:8787 --token dev
# open http://127.0.0.1:8787/?token=dev
```

It uses the same tasks, approvals and blackboard over the same IPC protocol.

## Diagnostics

```sh
target/debug/msk-tui --doctor
target/debug/msk-tui --root .msk --snapshot
target/debug/msk-tui --root .msk --snapshot --snapshot-palette
target/debug/msk-tui --root .msk --snapshot --snapshot-sessions
```

`--doctor` reports the daemon, envd, tools directory, provider, model and key
presence. `--snapshot` renders one real frame to stdout (no alternate screen);
it is useful for CI and bug reports. The two hidden snapshot flags render the
overlays for documentation screenshots.

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
  `MSK_LLM_API_KEY` and `MSK_LLM_BASE_URL`. See `.msk/mskd.log` and the task's
  `agent.stderr.log`.
- **Approval modal does not appear** — approvals are hydrated every second. If
  it still does not, run `mskctl --root .msk approvals`.
- **Stale daemon** — stop `mskd`, or use `--no-autostart` and start it manually
  with `mskd --root ... --envd ... --tools-dir ...`.
