# .swarm -- park-and-pump multi-agent coordination

A drop-in coordination layer for running multiple single-shot AI agents
(Claude Code, or any headless CLI agent) on one project without deadlock.

Core inversion: agents that cannot self-restart should not try to stay
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
3. **The pump is the only restarter.** A dumb watcher (`pump.py`) with
   zero intelligence: if an agent has unread mail, a ready parked task, or
   sits in a wait-cycle nobody else will break, relaunch it. Cron-friendly
   (`--once`) or looping (`--watch`).

Deadlock changes character: a wait-cycle can still form, but only in the
*ledger*, where it is inspectable data. The pump runs cycle detection on
the wait-for graph and, when a cycle has no other way to move, wakes the
lexicographically-first member as designated breaker (documented
assumption + notify counterpart). Total silent deadlock is impossible.

## Layout (all state is files; no daemons, no databases)

    .swarm/
      swarm.json               launcher template, pump + claim settings
      secret                   HMAC key for message signing (gitignored)
      swarmctl.py              the agent-facing API (stdlib only)
      pump.py                  the dumb pump
      stub_agent.py            scripted no-AI agent for token-free demos
      prompts/bootstrap.md     wake-up prompt template ([[TOKENS]] substituted)
      agents/<id>/
        domain.json            writable/forbidden glob patterns
        inbox/*.json           one signed message per file
        inbox/processed/       acked messages
        state.json             agent-owned scratch state (optional)
        logs/                  stdout of each run
      claims/<sha1>.json       file leases (O_EXCL-acquired, TTL'd)
      ledger/parked/*.json     parked tasks = the continuation store
      ledger/violations/*.json audit failures

## Quickstart

    python3 swarmctl.py init --project /path/to/repo
    cd /path/to/repo
    python3 swarmctl.py add-agent frontend --writable 'src/ui/**'
    python3 swarmctl.py add-agent backend  --writable 'src/api/**'
    python3 swarmctl.py send --from user --to frontend \
        --type question --subject "build the login page" --body "..."
    python3 pump.py --watch          # or: --once from cron

Dry-run the whole lifecycle with zero tokens first: `bash demo.sh`
(message flow, claim negotiation, and a manufactured total deadlock,
all driven by the stub agent).

## Launcher

`swarm.json > launcher.template` is any shell command; placeholders
`{prompt_file}`, `{agent}`, `{root}`. Default targets Claude Code headless
mode (docs: https://code.claude.com/docs/en/headless):

    claude -p "$(cat {prompt_file})" --permission-mode acceptEdits \
      --allowedTools "Bash(python3 {root}/swarmctl.py *)" --max-turns 50

Swap in any agent CLI that accepts a prompt non-interactively; the
protocol only assumes "reads prompt, can run swarmctl, exits."

## Message format

One JSON file per message in the recipient's inbox, written atomically
(tmp + rename), HMAC-SHA256 signed over the canonical JSON (sorted keys,
`sig` excluded) with the swarm secret.

| field      | req | notes                                              |
|------------|-----|----------------------------------------------------|
| id         | yes | `msg-<utc-stamp>-<hex>`                            |
| from / to  | yes | agent ids; `user` is a valid sender, not recipient |
| type       | yes | question, answer, info, claim-request, claim-grant, claim-deny, handoff, system |
| subject    | yes | one line                                           |
| body       | yes | free text (markdown)                               |
| re         | answer/deny | id of the message being replied to         |
| paths      | claim-grant | project-relative paths being granted        |
| task       | no  | related task id                                    |
| created_at | yes | ISO8601 UTC                                        |
| schema     | yes | integer, currently 1                               |
| sig        | yes | HMAC-SHA256 hex                                    |

Lifecycle: `inbox/<id>.json` -> (handled) -> `inbox/processed/<id>.json`.
An unacked message keeps its recipient on the pump's wake list -- messages
cannot sit unnoticed.

## Domains and claims

`domain.json` globs (`**` spans directories) define what an agent may
write. Enforcement is three layers:

1. **Prompt**: the bootstrap states the domain and the rules.
2. **Claim gate**: `swarmctl claim` refuses paths in another agent's
   domain unless a `claim-grant` message id is presented (`--grant`).
   Leases are acquired with `O_CREAT|O_EXCL` -- one winner even under a
   true race -- and expire on TTL (the pump reaps and notifies).
3. **Audit**: at wake, swarmctl snapshots content hashes of all dirty
   files; `swarmctl audit <agent>` flags out-of-domain files changed
   *during that run*. Violations are recorded in the ledger and the run
   fails its exit checklist.

## Security model (honest edition)

Signing makes messages tamper-*evident* and blocks accidental or
prompt-injected forgery by anything that lacks the swarm secret. It is
not inter-agent authentication: all agents run as the same OS user and
could read the secret. Same for domains: the claim gate and audit are
guardrails against confused or prompt-injected agents, not sandboxes
against adversarial code. If you need hard isolation, run each agent as
its own OS user or container and mount only its domain read-write; the
protocol above is unchanged.

## Limitations / next steps

- Pump polls; an inotify/fswatch trigger is a drop-in upgrade.
- Audit requires git (skips gracefully without it).
- `waiting_on` is declared by the parking agent; a liar can only hurt
  cycle *detection*, and the TTL reaper still unsticks its claims.
- One pump per project; the running-lock protocol tolerates a second
  pump but wastes launches.
