# trelane -- park-and-squire multi-agent coordination

A coordination protocol for running multiple single-shot AI agents (Claude
Code, opencode, or any headless CLI agent) on one project without deadlock.

**Core inversion:** agents that cannot self-restart should not try to stay
alive. **Stopping is the normal unit of work.** An agent is not a process;
it is a durable identity plus state on disk. Each run is a bounded slice:
wake, drain inbox, work, park anything blocked, exit.

## The three invariants

1. **Never wait while running.** Blocking on another agent is forbidden.
   Blocked work is *parked* -- turned into a ledger entry with a resume
   hint -- and the agent moves on or exits. A parked task is data, not a
   stuck process, so no running agent can deadlock, by construction.
2. **Inbox first.** Every run begins by draining the inbox. Responsiveness
   happens at run boundaries; run boundaries are frequent because runs are
   deliberately short.
3. **The squire is the only restarter.** A dumb watcher (`trelane squire`, formerly
   `trelane squire`) with zero intelligence: if an agent has unread mail, a
   ready parked task, or sits in a wait-cycle nobody else will break,
   relaunch it. Cron-friendly (`--once`) or looping (`--watch`).

Deadlock changes character: a wait-cycle can still form, but only in the
ledger, where it is inspectable data. The squire runs cycle detection on the
wait-for graph and, when a cycle has no other way to move, wakes the
lexicographically-first member as designated breaker (documented assumption
+ notify counterpart). Total silent deadlock is impossible.

## Architecture

All state lives in SQLite (`.trelane/trelane.db`) with WAL mode for
concurrent reads. No daemons, no external services, no message queues --
just one file.

Global configuration (launcher template, squire settings, claim TTL, UI,
biplane) lives at `~/.config/trelane/config.json` (respects
`XDG_CONFIG_HOME`). This is shared across all projects -- each project
session only needs its own database, secret, and prompts.

    ~/.config/trelane/
      config.json             global: launcher profiles, squire settings, claim TTL, UI, biplane

    <project>/.trelane/
      trelane.db              SQLite: agents, messages, claims, parked tasks, running locks
      secret                  HMAC key for message signing (gitignored)
      prompts/bootstrap.md    wake-up prompt template ([[TOKENS]] substituted)
      biplane-description.json  structured project description (optional, from --describe)
      biplane-plan.json       derived agent plan (optional, from --emit-plan)
      agents/<id>/
        state.json            agent-owned scratch state (optional)
        logs/                 stdout of each run
        .prompt.md            generated prompt for the current run (gitignored)
        launch.sh             per-agent tmux launch script (gitignored)

## Install

    cargo install --path .

Requires Rust 1.85+ (edition 2024). SQLite is compiled in via
`rusqlite`'s `bundled` feature -- no system SQLite needed.

## Quickstart

    trelane init --project /path/to/repo
    cd /path/to/repo
    trelane add-agent frontend --writable 'src/ui/**'  --desc 'owns the UI layer'
    trelane add-agent backend  --writable 'src/api/**' --desc 'owns the API layer'
    trelane send --from user --to frontend --type question \
        --subject "build the login page" --body "..."
    trelane squire --watch          # or: --once from cron

Or launch everything at once with Biplane:

    trelane /path/to/repo --models glm-5.2 --max-agents 3 --with-biplane

Dry-run the full lifecycle with zero tokens:

    trelane --testing tests/small.json

## Commands

| Command | Description |
|---------|-------------|
| `trelane PROJECT --models M --max-agents N [--with-biplane]` | Launch interactive tmux session with agents |
| `trelane init [--project DIR]` | Initialize a new trelane session |
| `trelane add-agent NAME --writable GLOB [--desc TEXT] [--launcher-agent MODEL]` | Register an agent with a domain |
| `trelane redomain AGENT --writable GLOB [--desc TEXT]` | Update an agent's domain and notify peers |
| `trelane send --from A --to B --type TYPE --subject TEXT [--body ...]` | Send a signed message |
| `trelane inbox AGENT [--json]` | List unprocessed messages |
| `trelane ack AGENT MSG_ID` | Mark a message as processed |
| `trelane claim AGENT PATH [--grant MSG_ID] [--ttl SECS]` | Acquire a file lease |
| `trelane release AGENT PATH [--force]` | Release a file lease |
| `trelane park AGENT --wait-reply MSG_ID \| --wait-claim PATH --waiting-on AGENT` | Park a blocked task |
| `trelane unpark TASK_ID` | Remove a parked task |
| `trelane status` | Show full swarm state |
| `trelane biplane [--json] [--safe-pocket DIR] [--describe FILE] [--next-steps] [--emit-plan] [--interactive] [--accept-defaults]` | Analyze project, generate reports, plan domains |
| `trelane wake AGENT [--why TEXT] [--launcher CMD]` | Launch an agent process |
| `trelane set-launch-target AGENT --adapter tmux --target PANE [--command TEXT]` | Store a tmux relaunch target |
| `trelane relaunch AGENT` | Inject a wake command into a tmux target |
| `trelane done AGENT` | Mark an agent as done (release running lock) |
| `trelane audit AGENT` | Check for out-of-domain file changes |
| `trelane squire --once \| --watch [--interval SECS] [--launcher L] [--verbose\|-v]` | The dumb prop (`squire` still works as an alias) |
| `trelane stub AGENT` | Token-free scripted agent for demos |
| `trelane --testing tests/scenario.json [--testing-runs N]` | Run a scenario harness |

## Launcher

`~/.config/trelane/config.json > launcher.template` is any shell command
with placeholders `{prompt_file}`, `{agent}`, `{root}`. Three built-in
profiles ship in the default config:

| Profile | Template | Notes |
|---------|----------|-------|
| `claude-code` | `claude -p "$(cat {prompt_file})" --permission-mode acceptEdits --allowedTools "Bash(trelane *)" --max-turns 50` | Default; targets Claude Code headless |
| `opencode` | `opencode run "$(cat {prompt_file})"` | Targets opencode CLI |
| `copilot` | `copilot -p "$(cat {prompt_file})" --allow-all-tools` | Targets GitHub Copilot CLI |

Antigravity has no headless CLI; it must be driven via a custom tmux/adapter
command.

Select a profile per agent with `--launcher-agent <profile>` at registration,
or override any template in config.json.

## Session UI

When running inside a tmux session, Trelane provides a live status bar and
configurable keybindings.

**Status bar states** (displayed at the top of the tmux window):

| State | Color | Meaning |
|-------|-------|---------|
| ACTIVE | green | At least one agent is running |
| IDLE | grey (colour 240) | No agent is running, but work may still be pending |
| DEADLOCK | red | A wait-cycle has been detected in the ledger |

Note: green-for-active is a change from pre-0.3, where red meant active.

**Keybindings** (configurable via `config.json > ui.keys`):

| Key | Default | Action |
|-----|---------|--------|
| `diagnostics` | `F2` | Pop a split showing `trelane status` |
| `inbox` | `F3` | Pop a split showing the focused pane's agent inbox |
| `verbose_toggle` | `F4` | Toggle verbose squire output (also settable via `TRELANE_VERBOSE=1` env) |

**Pane navigation** (`config.json > ui.pane_navigation`, `ui.match_host_terminal`):

When `match_host_terminal` is true, Trelane reads `~/.config/ghostty/config`
and mirrors any `goto_split` bindings whose modifiers tmux can actually
receive (Alt/Ctrl/Shift). Cmd/Super-based bindings can't be forwarded to
tmux on macOS, so those fall back to Alt+arrows with a printed note. Other
terminals use Alt+arrows directly.

## Biplane

Biplane is Trelane's analysis and planning tool.

### Plain report

    trelane biplane
    trelane biplane --json | jq .
    trelane biplane --safe-pocket ~/.safe_pocket

Shows all agents, domains, running state, inbox counts, parked tasks,
claims, deadlock detection, safe_pocket features, and recommendations.

### Structured project description (offline, no model call)

    trelane biplane --describe tests/space_rogue.describe.json

Analyzes a structured JSON file defining domains, their writable globs,
dependencies, and planned work. Validates the description, detects
dependency cycles, and topologically orders the domains.

### Next-steps phased scheduling

    trelane biplane --describe tests/space_rogue.describe.json --next-steps

Produces a phased schedule that respects domain dependencies: domains in
the same phase can start in parallel; domains in later phases wait for
earlier ones to be underway.

### Emit plan

    trelane biplane --describe tests/space_rogue.describe.json --emit-plan

Writes the derived agent plan to `.trelane/biplane-plan.json`.

### Interactive biplane

    trelane biplane --interactive
    trelane biplane --interactive --describe tests/space_rogue.describe.json
    trelane biplane --interactive --accept-defaults

Walks through proposed domains, lets you select which to register, and
optionally applies them to the live session. `--json` is analysis-only
(never applies to a live session) to keep stdout parseable.

### Biplane re-analysis on all-stop

When `biplane.reanalyze_on_all_stop` is set to `true` in config.json, the
prop watch loop checks for uncovered domains each time the swarm becomes
fully quiescent (no running agents, empty inboxes, no parked tasks) and
auto-registers agents for any new domains found. This is additive-only:
existing agents are never removed or re-assigned.

## Testing Harness

Full usage scenarios live under `tests/` as JSON files.

| Scenario | Purpose |
|----------|---------|
| `tests/small.json` | Small project, fast verification |
| `tests/medium.json` | Medium complexity, multiple domains |
| `tests/large.json` | Large project; engineers a genuine wait-cycle to exercise the real squire cycle detector (not a stub side-effect) |
| `tests/full-usage-scenario.json` | Full lifecycle: messaging, claims, deadlock, redomaining |

Run one directly:

    trelane --testing tests/small.json --testing-runs 3

The runner emits regular debug output and appends one JSON object per run
to a JSONL report file. Report fields include `messages_sent`,
`squire_ticks` (renamed from `pumps` in pre-0.3), `redomains`, and
`deadlocks_detected`.

## Configuration

```json
{
  "agents": {
    "default": [],
    "disabled": []
  },
  "launcher": {
    "template": "claude -p \"$(cat {prompt_file})\" --permission-mode acceptEdits --allowedTools \"Bash(trelane *)\" --max-turns 50",
    "profiles": {
      "claude-code": "claude -p \"$(cat {prompt_file})\" --permission-mode acceptEdits --allowedTools \"Bash(trelane *)\" --max-turns 50",
      "opencode": "opencode run \"$(cat {prompt_file})\"",
      "copilot": "copilot -p \"$(cat {prompt_file})\" --allow-all-tools"
    }
  },
  "squire": {
    "interval_s": 20,
    "max_concurrent": 4
  },
  "claims": {
    "default_ttl_s": 900
  },
  "ui": {
    "keys": {
      "diagnostics": "F2",
      "inbox": "F3",
      "verbose_toggle": "F4"
    },
    "pane_navigation": true,
    "match_host_terminal": true
  },
  "biplane": {
    "reanalyze_on_all_stop": false
  }
}
```

The `squire` key accepts `squire` as a serde alias for pre-0.3 config
compatibility. The CLI command `squire` remains as an alias for `squire`.

## Command Sequence Examples

### Launch a new project with Biplane

    trelane /path/to/project --models glm-5.2 --max-agents 3 --with-biplane

Biplane analyzes the project, proposes domains, registers agents, sends
initial work, opens a Terminal.app window with a tmux session, and starts
the squire. All in one command.

### Resume an existing session

    trelane /path/to/project --models glm-5.2 --max-agents 4

Finds existing agents, clears stale locks, reports pending work, and
relaunches the tmux session. All previous context (messages, claims,
parked tasks) is preserved.

### Cross-domain claim negotiation

    trelane send --from frontend --to backend --type claim-request \
        --subject "need src/api/auth.py" --path src/api/auth.py
    trelane park frontend --wait-reply msg-XXXX --waiting-on backend
    trelane squire --once

### Deadlock resolution

    trelane park alpha --wait-reply msg-never-a --waiting-on beta
    trelane park beta --wait-reply msg-never-b --waiting-on alpha
    trelane squire --once
    trelane status

### Domain shifting mid-session

    trelane redomain research --writable 'research/**' 'src/ui/**'
    trelane squire --once

### Interactive testing with real AI

    trelane --testing tests/full-usage-scenario-interactive.json

## Message format

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

## Domains and claims

`domain.json` globs (`**` spans directories) define what an agent may
write. Enforcement is three layers:

1. **Prompt**: the bootstrap states the domain and the rules.
2. **Claim gate**: `trelane claim` refuses paths in another agent's domain
   unless a `claim-grant` message id is presented (`--grant`). Leases are
   acquired via SQLite `INSERT OR IGNORE` -- one winner even under a true
   race -- and expire on TTL (the squire reaps and notifies).
3. **Audit**: at wake, trelane snapshots content hashes of all dirty files;
   `trelane audit <agent>` flags out-of-domain files changed *during that
   run*.

## Security model (honest edition)

Signing makes messages tamper-*evident* and blocks accidental or
prompt-injected forgery by anything that lacks the session secret. It is
not inter-agent authentication: all agents run as the same OS user and
could read the secret. Same for domains: the claim gate and audit are
guardrails against confused or prompt-injected agents, not sandboxes
against adversarial code.

## Development

    cargo build                              # compile
    cargo clippy -- -D warnings              # lint
    cargo test                               # run unit tests (46 passing)
    trelane --testing tests/small.json       # quick scenario test
    trelane --testing tests/medium.json      # medium scenario
    trelane --testing tests/large.json       # large scenario with real deadlock

## License

MIT
