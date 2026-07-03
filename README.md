# trelane -- park-and-pump multi-agent coordination

A coordination protocol for running multiple single-shot AI agents (Claude
Code, opencode, or any headless CLI agent) on one project without deadlock.

**Core inversion:** agents that cannot self-restart should not try to stay
alive. **Stopping is the normal unit of work.** An agent is not a process;
it is a durable identity plus state on disk. Each run is a bounded slice:
wake, drain inbox, work, park anything blocked, exit.

## The three invariants

1. **Never wait while running.** Blocking on another agent is forbidden.
   Blocked work is *parked* — turned into a ledger entry with a resume
   hint — and the agent moves on or exits. A parked task is data, not a
   stuck process, so no running agent can deadlock, by construction.
2. **Inbox first.** Every run begins by draining the inbox. Responsiveness
   happens at run boundaries; run boundaries are frequent because runs are
   deliberately short.
3. **The pump is the only restarter.** A dumb watcher (`trelane pump`) with
   zero intelligence: if an agent has unread mail, a ready parked task, or
   sits in a wait-cycle nobody else will break, relaunch it. Cron-friendly
   (`--once`) or looping (`--watch`).

Deadlock changes character: a wait-cycle can still form, but only in the
ledger, where it is inspectable data. The pump runs cycle detection on the
wait-for graph and, when a cycle has no other way to move, wakes the
lexicographically-first member as designated breaker (documented assumption
+ notify counterpart). Total silent deadlock is impossible.

## Architecture

All state lives in SQLite (`.trelane/trelane.db`) with WAL mode for
concurrent reads. No daemons, no external services, no message queues —
just one file.

Global configuration (launcher template, pump settings, claim TTL) lives
at `~/.config/trelane/config.json` (respects `XDG_CONFIG_HOME`). This is
shared across all projects — each project session only needs its own
database, secret, and prompts.

    ~/.config/trelane/
      config.json             global: default agents, launcher, pump settings, claim TTL

    <project>/.trelane/
      trelane.db              SQLite: agents, messages, claims, parked tasks, running locks
      secret                  HMAC key for message signing (gitignored)
      prompts/bootstrap.md    wake-up prompt template ([[TOKENS]] substituted)
      agents/<id>/
        state.json            agent-owned scratch state (optional)
        logs/                 stdout of each run
        .prompt.md            generated prompt for the current run (gitignored)

## Install

    cargo install --path .

Requires Rust 1.85+ (edition 2024). SQLite is compiled in via
`rusqlite`'s `bundled` feature — no system SQLite needed.

## Quickstart

    trelane init --project /path/to/repo
    cd /path/to/repo
    trelane --agents "claude,gpt-4" --no-agents "expensive-model" .
    trelane add-agent frontend --writable 'src/ui/**'  --desc 'owns the UI layer'
    trelane add-agent backend  --writable 'src/api/**' --desc 'owns the API layer'
    trelane send --from user --to frontend --type question \
        --subject "build the login page" --body "..."
    trelane pump --watch          # or: --once from cron

Dry-run the full lifecycle with zero tokens first:

    bash demo-rust.sh

This exercises message flow, claim negotiation, and a manufactured total
deadlock, all driven by `trelane stub` (a scripted no-AI agent). Set
`TRELANE_DEMO_REPEAT=N` to run it repeatedly and `TRELANE_DEMO_REPORT=/path/report.jsonl`
to capture a per-run report.

## Commands

| Command | Description |
|---------|-------------|
| `trelane [--agents A,B] [--no-agents C,D] PROJECT` | Attach/init Trelane for an existing project and inject `AGENTS.md` instructions |
| `trelane init [--project DIR]` | Initialize a new trelane session |
| `trelane attach [PROJECT] [--no-inject]` | Attach/init a project and optionally skip `AGENTS.md` injection |
| `trelane add-agent NAME --writable GLOB [--desc TEXT] [--launcher-agent MODEL]` | Register an agent with a domain and optional session-model binding |
| `trelane redomain AGENT --writable GLOB [--desc TEXT]` | Update an agent's domain and notify peers |
| `trelane send --from A --to B --type TYPE --subject TEXT [--body ...]` | Send a signed message |
| `trelane inbox AGENT [--json]` | List unprocessed messages |
| `trelane ack AGENT MSG_ID` | Mark a message as processed |
| `trelane claim AGENT PATH [--grant MSG_ID] [--ttl SECS]` | Acquire a file lease |
| `trelane release AGENT PATH [--force]` | Release a file lease |
| `trelane park AGENT --wait-reply MSG_ID \| --wait-claim PATH --waiting-on AGENT [--resume-hint TEXT]` | Park a blocked task |
| `trelane unpark TASK_ID` | Remove a parked task |
| `trelane status` | Show full swarm state |
| `trelane wake AGENT [--why TEXT] [--launcher CMD]` | Launch an agent process |
| `trelane set-launch-target AGENT --adapter tmux --target pane-or-session [--command TEXT]` | Store a GUI/terminal relaunch target |
| `trelane relaunch AGENT [--adapter ... --target ... --command ...]` | Inject a wake command into an attached terminal session |
| `trelane done AGENT` | Mark an agent as done (release running lock) |
| `trelane audit AGENT` | Check for out-of-domain file changes |
| `trelane pump --once \| --watch [--interval SECS]` | The dumb pump |
| `trelane stub AGENT` | Token-free scripted agent for demos |

## Launcher

`~/.config/trelane/config.json > launcher.template` is any shell command
with placeholders `{prompt_file}`, `{agent}`, `{root}`. Default targets
Claude Code headless mode:

    claude -p "$(cat {prompt_file})" --permission-mode acceptEdits \
      --allowedTools "Bash(trelane *)" --max-turns 50

Swap in any agent CLI that accepts a prompt non-interactively; the
protocol only assumes "reads prompt, can run trelane, exits."

## Attach Mode

Trelane is designed to be attachable to existing projects and already-open
agent sessions. The shortest attach form is:

    trelane --agents "claude,gpt-4,gpt-4-32k" --no-agents "gpt-3.5" .

This does three things:

1. Initializes `.trelane/` for the project if needed.
2. Records enabled/disabled session agents in `.trelane/trelane.db`.
3. Inserts a managed Trelane block into the project's `AGENTS.md`, giving
   already-running agents the protocol, commands, and exit checklist.

Session agent selection is now operational, not just informational: if a
domain agent is registered with `--launcher-agent <model>`, Trelane will
refuse to wake or relaunch it when that session model is disabled.

Default enabled/disabled agents can also be configured globally:

```json
{
  "agents": {
    "default": ["claude", "gpt-4"],
    "disabled": ["expensive-experimental-model"]
  },
  "launcher": {
    "template": "claude -p \"$(cat {prompt_file})\" --permission-mode acceptEdits --allowedTools \"Bash(trelane *)\" --max-turns 50"
  },
  "pump": {
    "interval_s": 20,
    "max_concurrent": 2
  },
  "claims": {
    "default_ttl_s": 900
  }
}
```

Use `trelane attach --no-inject .` when you want to initialize and record
agent selection without modifying `AGENTS.md`.

## GUI Relaunch

Headless relaunch is implemented through `trelane pump` and the launcher
template. Attached-session relaunch is now available through stored launch
targets:

    trelane set-launch-target alpha --adapter tmux --target trelane:alpha
    trelane relaunch alpha

`trelane pump` will prefer a stored launch target over the headless launcher.
Supported adapters are `tmux`, `ghostty`, `iterm2`, `wezterm`, `kitty`, and `terminal.app`.
The recommended approach for every supported terminal app is to run the agent
inside `tmux` and have Trelane target the tmux pane/session. That is the only
deterministic cross-terminal targeting layer Trelane currently supports well.

Examples:

    # inside Ghostty, iTerm2, WezTerm, kitty, or Terminal.app
    tmux new-session -s trelane-alpha
    trelane set-launch-target alpha --adapter tmux --target trelane-alpha

Native terminal adapters should be treated as best-effort fallbacks when tmux
is not available. They are useful for convenience, but they are less precise
than targeting tmux directly.

For Ghostty on macOS, `--target frontmost` sends to the active window, and any
other `--target` value is treated as a window-title substring. This still does
not choose a specific split; it sends to the currently focused Ghostty split.

## Message format

Messages are stored as rows in SQLite, HMAC-SHA256 signed over the
canonical JSON (sorted keys, `sig` excluded) with the session secret.

| field      | req | notes                                          |
|------------|-----|------------------------------------------------|
| id         | yes | `msg-<utc-stamp>-<hex>`                        |
| from / to  | yes | agent ids; `user` is a valid sender, not recipient |
| type       | yes | question, answer, info, claim-request, claim-grant, claim-deny, handoff, system |
| urgency    | yes | low, normal, high, critical                    |
| subject    | yes | one line                                       |
| body       | yes | free text (markdown)                           |
| re         | answer/deny | id of the message being replied to         |
| paths      | claim-grant | project-relative paths being granted        |
| task       | no  | related task id                                |
| created_at | yes | ISO8601 UTC                                    |
| schema     | yes | integer, currently 1                           |
| sig        | yes | HMAC-SHA256 hex                                 |

Lifecycle: unprocessed → (handled) → processed. An unacked message keeps
its recipient on the pump's wake list — messages cannot sit unnoticed.

## Domains and claims

`domain.json` globs (`**` spans directories) define what an agent may
write. Enforcement is three layers:

1. **Prompt**: the bootstrap states the domain and the rules.
2. **Claim gate**: `trelane claim` refuses paths in another agent's domain
   unless a `claim-grant` message id is presented (`--grant`). Leases are
   acquired via SQLite `INSERT OR IGNORE` — one winner even under a true
   race — and expire on TTL (the pump reaps and notifies).
3. **Audit**: at wake, trelane snapshots content hashes of all dirty files;
   `trelane audit <agent>` flags out-of-domain files changed *during that
   run*. Violations are recorded in the database and the run fails its exit
   checklist.

## Security model (honest edition)

Signing makes messages tamper-*evident* and blocks accidental or
prompt-injected forgery by anything that lacks the session secret. It is
not inter-agent authentication: all agents run as the same OS user and
could read the secret. Same for domains: the claim gate and audit are
guardrails against confused or prompt-injected agents, not sandboxes
against adversarial code. If you need hard isolation, run each agent as
its own OS user or container and mount only its domain read-write; the
protocol above is unchanged.

## Development

    cargo build          # compile
    cargo clippy -- -D warnings   # lint
    cargo test           # run unit tests
    TRELANE_DEMO_REPEAT=3 bash demo-rust.sh    # repeatable end-to-end protocol demo (no tokens)

## License

MIT
