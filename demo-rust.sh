#!/usr/bin/env bash
# demo.sh -- exercises the full trelane lifecycle in a sandbox using the
# stub agent (no AI, no tokens). Three scenarios:
#   A. park-on-reply: alpha asks beta a question, parks, pump resurrects both
#   B. claim negotiation: beta wants a file in alpha's domain
#   C. total deadlock: alpha and beta park on each other; pump breaks it
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SANDBOX="${1:-/tmp/trelane-demo}"

# Build the binary if needed
cargo build --release 2>/dev/null
TRELANE="$HERE/target/release/trelane"
STUB="$TRELANE --root {root} stub {agent}"

wait_idle() {
  for _ in $(seq 1 40); do
    "$TRELANE" --root "$SANDBOX" status | grep -q RUNNING || return 0
    sleep 0.25
  done
}

step() { echo; echo "==================== $* ===================="; }

rm -rf "$SANDBOX"
mkdir -p "$SANDBOX/src/ui" "$SANDBOX/src/api"
git -C "$SANDBOX" init -q
echo "export const x = 1;" > "$SANDBOX/src/ui/app.ts"
echo "def handler(): pass"  > "$SANDBOX/src/api/routes.py"

step "init trelane + two agents with disjoint domains"
"$TRELANE" init --project "$SANDBOX"
"$TRELANE" --root "$SANDBOX" add-agent alpha --writable 'src/ui/**'  --desc 'owns the UI layer'
"$TRELANE" --root "$SANDBOX" add-agent beta  --writable 'src/api/**' --desc 'owns the API layer'

step "A1: alpha asks beta a question and parks on the reply"
MSG=$("$TRELANE" --root "$SANDBOX" send --from alpha --to beta --type question \
      --subject "what shape is the /users payload?" \
      --body "Need the response schema before wiring the UI table.")
"$TRELANE" --root "$SANDBOX" park alpha --task task-ui-table --wait-reply "$MSG" \
      --waiting-on beta --resume-hint "wire UI table using beta's schema"
"$TRELANE" --root "$SANDBOX" status

step "A2: pump tick -> beta wakes (inbox), answers, exits"
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle

step "A3: pump tick -> alpha wakes (reply arrived), resumes, unparks"
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle
"$TRELANE" --root "$SANDBOX" status

step "B1: beta tries to claim a file in ALPHA's domain -> denied"
"$TRELANE" --root "$SANDBOX" claim beta "$SANDBOX/src/ui/app.ts" || true

step "B2: beta sends claim-request, parks; pump negotiates the grant"
REQ=$("$TRELANE" --root "$SANDBOX" send --from beta --to alpha --type claim-request \
      --subject "need src/ui/app.ts to update the API client import" \
      --path "$SANDBOX/src/ui/app.ts")
"$TRELANE" --root "$SANDBOX" park beta --task task-fix-import --wait-reply "$REQ" \
      --waiting-on alpha --resume-hint "claim app.ts with the grant, fix import"
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle
"$TRELANE" --root "$SANDBOX" status

step "C1: manufacture TOTAL DEADLOCK: alpha waits on beta, beta on alpha"
"$TRELANE" --root "$SANDBOX" park alpha --task task-a --wait-reply msg-never-a --waiting-on beta \
      --resume-hint "blocked on beta forever"
"$TRELANE" --root "$SANDBOX" park beta  --task task-b --wait-reply msg-never-b --waiting-on alpha \
      --resume-hint "blocked on alpha forever"
"$TRELANE" --root "$SANDBOX" status

step "C2: pump tick -> cycle detected, designated breaker woken"
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle

step "C3: pump tick -> counterpart notified, unparks; swarm is clean"
"$TRELANE" --root "$SANDBOX" pump --once --launcher "$STUB"; wait_idle
"$TRELANE" --root "$SANDBOX" status

step "demo complete -- point config.json launcher.template at 'claude -p ...' for real agents"
