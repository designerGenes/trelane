#!/usr/bin/env bash
# demo.sh -- exercises the full swarm lifecycle in a sandbox using the
# stub agent (no AI, no tokens). Three scenarios:
#   A. park-on-reply: alpha asks beta a question, parks, pump resurrects both
#   B. claim negotiation: beta wants a file in alpha's domain
#   C. total deadlock: alpha and beta park on each other; pump breaks it
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SANDBOX="${1:-/tmp/swarm-demo}"
STUB='python3 {root}/stub_agent.py {agent}'
SWARMCTL="python3 $SANDBOX/swarmctl.py --root $SANDBOX"
PUMP="python3 $SANDBOX/pump.py --root $SANDBOX --once --launcher"

wait_idle() {  # stub runs are fast; poll until no .running locks remain live
  for _ in $(seq 1 40); do
    $SWARMCTL status | grep -q RUNNING || return 0
    sleep 0.25
  done
}

step() { echo; echo "==================== $* ===================="; }

rm -rf "$SANDBOX"
mkdir -p "$SANDBOX/src/ui" "$SANDBOX/src/api"
git -C "$SANDBOX" init -q
echo "export const x = 1;" > "$SANDBOX/src/ui/app.ts"
echo "def handler(): pass"  > "$SANDBOX/src/api/routes.py"

step "init swarm + two agents with disjoint domains"
python3 "$HERE/swarmctl.py" init --project "$SANDBOX" >/dev/null 2>&1 || true
cp "$HERE/swarmctl.py" "$SANDBOX/swarmctl.py"
cp "$HERE/pump.py" "$SANDBOX/pump.py"
cp "$HERE/stub_agent.py" "$SANDBOX/stub_agent.py"
cp -r "$HERE/prompts" "$SANDBOX/.swarm/"
$SWARMCTL add-agent alpha --writable 'src/ui/**'  --desc 'owns the UI layer'
$SWARMCTL add-agent beta  --writable 'src/api/**' --desc 'owns the API layer'

step "A1: alpha asks beta a question and parks on the reply"
MSG=$($SWARMCTL send --from alpha --to beta --type question \
      --subject "what shape is the /users payload?" \
      --body "Need the response schema before wiring the UI table.")
$SWARMCTL park alpha --task task-ui-table --wait-reply "$MSG" \
      --waiting-on beta --resume-hint "wire UI table using beta's schema"
$SWARMCTL status

step "A2: pump tick -> beta wakes (inbox), answers, exits"
$PUMP "$STUB"; wait_idle

step "A3: pump tick -> alpha wakes (reply arrived), resumes, unparks"
$PUMP "$STUB"; wait_idle
$SWARMCTL status

step "B1: beta tries to claim a file in ALPHA's domain -> denied"
$SWARMCTL claim beta "$SANDBOX/src/ui/app.ts" || true

step "B2: beta sends claim-request, parks; pump negotiates the grant"
REQ=$($SWARMCTL send --from beta --to alpha --type claim-request \
      --subject "need src/ui/app.ts to update the API client import" \
      --path "$SANDBOX/src/ui/app.ts")
$SWARMCTL park beta --task task-fix-import --wait-reply "$REQ" \
      --waiting-on alpha --resume-hint "claim app.ts with the grant, fix import"
$PUMP "$STUB"; wait_idle   # alpha wakes, grants
$PUMP "$STUB"; wait_idle   # beta wakes, claims with grant, edits, releases
$SWARMCTL status

step "C1: manufacture TOTAL DEADLOCK: alpha waits on beta, beta on alpha"
$SWARMCTL park alpha --task task-a --wait-reply msg-never-a --waiting-on beta \
      --resume-hint "blocked on beta forever"
$SWARMCTL park beta  --task task-b --wait-reply msg-never-b --waiting-on alpha \
      --resume-hint "blocked on alpha forever"
$SWARMCTL status

step "C2: pump tick -> cycle detected, designated breaker woken"
$PUMP "$STUB"; wait_idle

step "C3: pump tick -> counterpart notified, unparks; swarm is clean"
$PUMP "$STUB"; wait_idle
$SWARMCTL status

step "demo complete -- point launcher.template at 'claude -p ...' for real agents"
