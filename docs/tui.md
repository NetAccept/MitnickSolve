# Terminal app

`msk` with no arguments opens the conversational app. It is a semi-graphical
terminal UI: a pixel wordmark, rounded composer, status chips, a session rail,
Mitnick's markdown-lite replies, readable tool summaries, a scrollback bar, modal
approvals and a command palette. It starts a local `mskd` daemon if one is
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

                                                      ~   scripted

                ╭──────────────────────────────────────────────────────────────────────────────────────╮
                │ Ask anything…  "inspect ./challenge and find the flag"                               │
                │                                                                                      │
                │  自 动  · scripted                                                                     │
                ╰──────────────────────────────────────────────────────────────────────────────────────╯
                                                 tab sessions   ctrl+n new   ctrl+p commands   F1 help

                            ╭ try ─────────────────────────────────────────────────────────╮
                            │   › inspect ./challenge and find the flag                    │
                            │   › map the services on 10.10.10.5                           │
                            │   › triage ./evidence and summarize the findings             │
                            │   /demo runs the offline tool demo                           │
                            ╰──────────────────────────────────────────────────────────────╯






  ~  ·  权 限  自 动                                                                                    MitnickSolve 0.1.6
```

- The large wordmark is drawn from Unicode blocks with a left-to-right gradient.
- The chips under it show the workspace, provider, model and effort.
- The rounded composer is always the focused input. Just start typing; Enter
  sends. `Shift+Enter` inserts a newline, bracketed paste inserts whole scripts.
- The leading slot of the composer shows the mode, then provider, model and
  effort. The `try` card below it suggests starter prompts; `/demo` runs the
  offline tool demo.
- `Tab` opens the session switcher, `Ctrl+N` starts a new session, `Ctrl+P`
  opens the command palette, `F1` shows every shortcut.
- The footer shows the workspace, permission posture, context tokens, tool
  count, a scrollback indicator and the version.

With no configured provider, a full-screen setup card opens first. Choose
`scripted-demo` to try the app offline with no key, or `openai` /
`openai-responses` for a real model.

## Setup: provider, model, effort and key

`Ctrl+P` → **Providers** for saved profiles, or **Provider and model** (`/setup`,
`msk-tui --setup`) for the step-by-step wizard.

```text
                  ╭ setup ───────────────────────────────────────────────────────────────────────────╮
                  │ MitnickSolve   connect a model                                                   │
                  │ You can change all of this later with ctrl+p.                                    │
                  │                                                                                  │
                  │ ● provider  ›  ○ service  ›  ○ model  ›  ○ effort  ›  ○ endpoint  ›  ○ key      │
                  │                                                                                  │
                  │ Choose how the agent reaches a model                                             │
                  │ scripted-demo needs no key and works offline.                                    │
                  │                                                                                  │
                  │   scripted-demo       offline canned tool session, no model or key               │
                  │ ❯ openai              OpenAI-compatible Chat Completions                         │
                  │   openai-responses    OpenAI Responses API function calls                        │
                  │   component           signed llm.provider component configured on mskd          │
                  │                                                                                  │
                  │ no API key yet (type one here or export MSK_LLM_API_KEY in mskd)                 │
                  │ config  C:\Users\cy\MitnickSolve\.msk\ui-home2\tui-config.json                   │
                  │ ↑/↓ choose   Enter next   Esc back/close   Ctrl+U clear input                    │
                  ╰──────────────────────────────────────────────────────────────────────────────────╯
```

1. **Provider** — `scripted-demo` (offline), `openai` (Chat Completions),
   `openai-responses`, or a signed `component`.
2. **Service** — endpoint presets: OpenAI, DeepSeek, OpenRouter, Groq, Together,
   local Ollama, local LM Studio, or a custom endpoint. A preset fills the base
   URL and suggests a model; it never overwrites an existing profile's model.
3. **Model** — presets (`gpt-5`, `gpt-5-mini`, `o3`, `o4-mini`) or any custom
   model id.
4. **Effort** — `default`, `minimal`, `low`, `medium`, `high`. It is sent as
   `reasoning_effort` (Chat Completions) or `reasoning.effort` (Responses).
   Press `F2` to cycle it, or `Shift+F2` to go back, without opening a dialog.
5. **Endpoint** — optional OpenAI-compatible base URL. Empty uses
   `MSK_LLM_BASE_URL` or the provider default.
6. **API key** — masked while you type or paste. Empty keeps using the daemon's
   `MSK_LLM_API_KEY`. Local presets (Ollama, LM Studio) insert a `local`
   placeholder because the provider client requires a non-empty value; replace
   it if your server checks keys. The wizard saves the result as a provider
   profile, so the key does not need to be exported again.

`scripted-demo` and `component` skip the service, model, effort, endpoint and
key steps and save immediately.

## Providers and API keys

`Ctrl+P` → **Providers** (or `/providers`) lists every saved endpoint. Each row
shows the provider, model, effort, base URL and whether a key is set.

```text
                ╭ providers ───────────────────────────────────────────────────────────────────────────╮
                │ Saved provider profiles. Switch with Enter; keys stay on this machine.               │
                │                                                                                      │
                │ ❯ ● openai · gpt-5                                                                   │
                │     openai · gpt-5 · effort high · https://api.openai.com/v1 · key set               │
                │   ○ local · qwen3                                                                    │
                │     openai · qwen3 · effort default · http://127.0.0.1:8000/v1 · key set             │
                │                                                                                      │
                │ ↑/↓ choose · Enter use · n new · e edit · d delete · s setup · Esc close             │
                ╰──────────────────────────────────────────────────────────────────────────────────────╯
```

| Key | Action |
| --- | --- |
| `Enter` | make the selected profile active for the next session |
| `n` | add a profile (opens the wizard with empty model, endpoint and key) |
| `e` | edit the selected profile, including replacing or clearing its key |
| `s` | open the full setup wizard from the current settings |
| `d` | delete; press `d` again on the same row to confirm |
| `↑` / `↓`, `j` / `k` | move; `g` / `G` jump to the first or last profile |

Switching profiles loads its provider, model, effort, endpoint and key for the
next session; the conversation you are reading keeps the provider it started
with. Press `Ctrl+N` to start a session with the selected profile. Active
settings are also written to `<root>/tui-config.json`; the profiles list lives
under the same file's `profiles` array. Keys are stored locally in that file
(mode `0600` on Unix, the usual user-profile ACL on Windows) and are sent to
`mskd` over the local IPC socket only when creating a task; the daemon passes the
key to the agent subprocess through its environment. Nothing is sent anywhere
else. If you do not want keys on disk, leave the key empty and export
`MSK_LLM_API_KEY` where `mskd` starts. `msk-tui --doctor` checks daemon, envd,
tools and key presence.

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
                      │   Providers                 saved provider profiles and API keys        │
                      │   Provider and model        choose provider, model, effort, key           │
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

`Tab` opens the session switcher. The first row starts a fresh session; saved
sessions follow, titled by their goal (internal task ids stay out of the way).
Up/down only moves the highlight, Enter opens the selected row, `n` starts a new
session, Esc closes. `Ctrl+N` starts a new session from anywhere without
replacing the conversation you are reading.

```text
╭ sessions ────────────────────────────────────────────────────────────────╮
│ Open a saved conversation or start a fresh one.                          │
│   ＋  New session   start a fresh conversation                            │
│     ✓ done        Find the flag in ./challenge                           │
│ › ● ✓ done        Map the target service                                 │
│                              ↑/↓ select · Enter open · n new · Esc close │
╰──────────────────────────────────────────────────────────────────────────╯
```

## Conversation

A real frame after a turn, with the session rail, status bar, role gutters,
markdown rendering and the composer at the bottom:

```text
 ╭ sessions ──────────╮   ▌ Find the flag in ./challenge  ● ready                        scripted · - · effort default
 │▌ ● Find the flag i…│   ────────────────────────────────────────────────────────────────────────────────────────────
 │                    │     Mitnick
 │＋  New session      │     ▸ env_create managed session
 │                    │     ✓ env_create
 │                    │
 │                    │     you
 │                    │   ❯ # Recon
 │                    │     - found **TLS** on `443`
 │                    │
 │                    │     Mitnick
 │                    │   │   echo: # Recon
 │                    │   │   • found TLS on 443
 │                    │
 │                    │   ─────────────── ✓ turn complete · 10:28:07 · scripted · - · effort default ────────────────
 │                    │   ╭──────────────────────────────────────────────────────────────────────────────────────────╮
 │                    │   │ Reply…   Shift+Enter for a new line                                                      │
 │                    │   │                                                                                          │
 │                    │   │  攻 击  · scripted                                                                         │
 │tab switch · ctrl+n │   ╰──────────────────────────────────────────────────────────────────────────────────────────╯
 ╰────────────────────╯                                        tab sessions   ctrl+n new   ctrl+p commands   F1 help
  ~  ·  权 限  手 动   ·  ↑ scrollback · End follows                                                    MitnickSolve 0.1.6
```

- On wide terminals the left rail lists sessions; `▌` marks the open one and the
  glyph shows its state. `＋ New session` sits below it.
- Each turn starts with a small `you` or `Mitnick` label. Your messages sit on a
  tinted row with `❯`; Mitnick uses a `│` gutter and a small Markdown
  renderer: headings, bullets, **bold**, `inline code` and fenced code blocks
  are styled.
- Tool calls appear as `▸ tool summary` (the shell command, target, path or URL),
  with the matching `✓ tool` (green) or `✗ tool` (red) result immediately
  below. Streaming text ends with a blinking block.
- Every finished turn closes with a `✓ turn complete` rule showing the time,
  provider, model, effort and elapsed time. It is recorded in the event log, so
  reopening the session shows the same marker.
- Typing `/` above the composer opens a filtered command popup; keep typing to
  narrow it, then press Enter to run the command.
- The header is a status bar: goal, a live state chip (`⠋ running`, `● ready`,
  `✗ failed`, `✓ done`), and provider/model/effort.
- `↑` / `↓` scroll one line (even while the composer is focused), `PgUp` /
  `PgDn` scroll ten, and the mouse wheel scrolls three. `Ctrl+Home` jumps to the
  top; `Ctrl+End` or `End` follows new output when the composer is idle.
  `Ctrl+↑` / `Ctrl+↓` (or `Alt+↑` / `Alt+↓`) walk the input history.
  A scrollback bar appears on the right when there is older output.
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
| `Ctrl+N` | start a new session |
| `Ctrl+P` | command palette |
| `F1` | keyboard shortcuts |
| `F2` / `Shift+F2` | cycle reasoning effort forward / back |
| `↑` / `↓` | scroll the conversation one line |
| `PgUp` / `PgDn` | scroll ten lines |
| `Ctrl/Alt+↑` / `Ctrl/Alt+↓` | older / newer input history |
| `Ctrl+Home` / `Ctrl+End` | transcript top / follow output |
| mouse wheel | scroll three lines |

Mouse capture is enabled for wheel scrolling; use `Shift`+drag for the
terminal's native text selection, or detach the PTY with `Ctrl+Q` first.
| `End` | follow output when the composer is idle (`Ctrl+End` while typing) |
| `y` / `n` | approve / deny a pending approval |
| `Ctrl+Q` | detach from the PTY terminal |
| `Ctrl+U` | clear the composer |

Slash commands still work if you prefer typing them:

```text
/new [goal]   /demo        /providers      /provider NAME   /models      /model NAME
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
`Build` — for example `自动 · openai · gpt-5 · effort high`. Enter saves the
selected mode for the **next** session; it never replaces the session you are
reading. The composer keeps showing the current session's mode. Press `Ctrl+N`
to start a session with the new mode. The list comes from the daemon's
`modes.json`, so custom modes appear automatically.

The mode is a tool-level boundary, not a label: `plan`/`investigate` cannot call
`exec`, `write_file`, `session_*`, `net_dial` or `archive_extract`, and subagents
cannot escalate. See [`docs/modes.md`](docs/modes.md) for the configurable
definitions, the full matrix and auto-routing rules.

## Permission modes

`Ctrl+P` → **Permission mode** (or `/permissions`) opens `auto`, `manual`, and
`bypass`. This is separate from the agent mode: the agent mode decides which
tools exist, the permission mode decides which calls pause for approval. The
footer shows the current session's posture (`权限 自动`, `手动`, or `放行`).
Changing it saves the posture for the next session; press `Ctrl+N` to start one.
See [`docs/permissions.md`](docs/permissions.md).

## Findings and reports

The agent records assessment findings with the `finding_add` tool. They live in
the daemon blackboard, survive restarts, and are visible to every attached
client. `Ctrl+P` → **Assessment findings** (or `/findings`) opens a
severity-ranked overlay: `j`/`k` scroll, `r` reloads, Esc closes. Each row shows
severity, title, asset, status, tags and a short description.

Use `Ctrl+P` → **Terminal** for the PTY, and ask Mitnick for `report_generate`
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

The easiest path is the wizard: `Ctrl+P` → **Provider and model**, enter the
model, base URL and API key, and it becomes a saved profile.

If you prefer environment variables, set them before launching:

```sh
export MSK_LLM_BASE_URL=https://api.openai.com/v1
export MSK_LLM_MODEL=gpt-5
export MSK_LLM_API_KEY=...
target/debug/msk
```

The setup card auto-selects `openai` when the daemon already has a key and model.
`MSK_PROVIDER` overrides the selection for one shell. If neither a saved profile
key nor `MSK_LLM_API_KEY` is present, the composer shows
`missing API key · ctrl+p providers` in amber instead of failing silently.

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
target/debug/msk-tui --root .msk --snapshot --snapshot-providers
```

`--doctor` reports the daemon, envd, tools directory, provider, model and key
presence. `--snapshot` renders one real frame to stdout (no alternate screen);
it is useful for CI and bug reports. The hidden snapshot flags render the
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
- **OpenAI task fails immediately** — run `--doctor`; check the selected
  profile's model, base URL and key in **Providers**, or the `MSK_LLM_MODEL`,
  `MSK_LLM_API_KEY` and `MSK_LLM_BASE_URL` environment. See `.msk/mskd.log` and
  the task's `agent.stderr.log`.
- **`missing API key` in the composer** — open `Ctrl+P` → **Providers**, edit
  the profile and paste a key, or export `MSK_LLM_API_KEY` before `mskd` starts.
  A saved key is sent per task, so an already running daemon picks it up without
  a restart. `Ctrl+P` still works while typing; choose `scripted-demo` to work
  offline.
- **`model_not_found` / HTTP 4xx or 5xx from the provider** — the turn now shows
  a red error block and the header reads `last turn failed`. The model id must
  match the provider's catalog exactly (some gateways suffix a release date,
  e.g. `deepseek-v4.1-flash-1017`, not `deepseek-v4.1-flash`). Open `Ctrl+P` →
  **Providers**, press `e`, choose `custom…`, and enter the exact id. A wrong
  model name does not break the session; correct it and send again.
- **A saved key is not used** — check that the active row in **Providers** has
  `●` and `key set`; `Enter` activates it, then send a message to start the new
  session. `--doctor` reports whether it found an environment key or a profile
  key (`key=set (profile)`).
- **Keys seem ignored** — `Ctrl+P`, `Tab` and `F1` are handled even while the
  composer has focus. If a terminal focus lost its PTY, press `Ctrl+Q`; the TUI
  also clears that state automatically on the next key.
- **Approval modal does not appear** — approvals are hydrated every second. If
  it still does not, run `mskctl --root .msk approvals`.
- **Stale daemon** — stop `mskd`, or use `--no-autostart` and start it manually
  with `mskd --root ... --envd ... --tools-dir ...`.
