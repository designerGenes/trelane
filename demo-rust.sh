#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SANDBOX_ROOT="${1:-/tmp/trelane-demo}"
REPEAT="${TRELANE_DEMO_REPEAT:-1}"
REPORT_PATH="${TRELANE_DEMO_REPORT:-$SANDBOX_ROOT/report.jsonl}"

cargo build --release >/dev/null
TRELANE="$HERE/target/release/trelane"
STUB="$TRELANE --root {root} stub {agent}"

wait_idle() {
  for _ in $(seq 1 40); do
    if ! "$TRELANE" --root "$1" status | grep -q RUNNING; then
      return 0
    fi
    sleep 0.25
  done
  return 1
}

run_once() {
  local run_id="$1"
  local sandbox="$SANDBOX_ROOT/run-$run_id"
  local result="ok"
  local started_at ended_at duration_ms
  local scenarios="question-answer,claim-grant,deadlock-break"

  started_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  local started_epoch
  started_epoch="$(python3 -c 'import time; print(int(time.time()*1000))')"

  rm -rf "$sandbox"
  mkdir -p "$sandbox/src/ui" "$sandbox/src/api"
  git -C "$sandbox" init -q
  printf 'export const x = 1;\n' > "$sandbox/src/ui/app.ts"
  printf 'def handler(): pass\n' > "$sandbox/src/api/routes.py"

  {
    "$TRELANE" init --project "$sandbox"
    "$TRELANE" --root "$sandbox" attach --no-inject "$sandbox"
    "$TRELANE" --root "$sandbox" add-agent alpha --writable 'src/ui/**' --desc 'owns the UI layer' --launcher-agent stub-low-cost
    "$TRELANE" --root "$sandbox" add-agent beta --writable 'src/api/**' --desc 'owns the API layer' --launcher-agent stub-low-cost

    local msg req
    msg=$("$TRELANE" --root "$sandbox" send --from alpha --to beta --type question --subject "what shape is the /users payload?" --body "Need the response schema before wiring the UI table.")
    "$TRELANE" --root "$sandbox" park alpha --task task-ui-table --wait-reply "$msg" --waiting-on beta --resume-hint "wire UI table using beta's schema"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"

    "$TRELANE" --root "$sandbox" claim beta "$sandbox/src/ui/app.ts" || true
    req=$("$TRELANE" --root "$sandbox" send --from beta --to alpha --type claim-request --subject "need src/ui/app.ts to update the API client import" --path "$sandbox/src/ui/app.ts")
    "$TRELANE" --root "$sandbox" park beta --task task-fix-import --wait-reply "$req" --waiting-on alpha --resume-hint "claim app.ts with the grant, fix import"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"

    "$TRELANE" --root "$sandbox" park alpha --task task-a --wait-reply msg-never-a --waiting-on beta --resume-hint "blocked on beta forever"
    "$TRELANE" --root "$sandbox" park beta --task task-b --wait-reply msg-never-b --waiting-on alpha --resume-hint "blocked on alpha forever"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"
    "$TRELANE" --root "$sandbox" pump --once --launcher "$STUB"
    wait_idle "$sandbox"
  } >/dev/null || result="failed"

  ended_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  local ended_epoch
  ended_epoch="$(python3 -c 'import time; print(int(time.time()*1000))')"
  duration_ms="$((ended_epoch - started_epoch))"

  printf '{"run":%s,"started_at":"%s","ended_at":"%s","duration_ms":%s,"result":"%s","launcher":"stub-low-cost","scenarios":"%s"}\n' \
    "$run_id" "$started_at" "$ended_at" "$duration_ms" "$result" "$scenarios" >> "$REPORT_PATH"

  if [[ "$result" != "ok" ]]; then
    return 1
  fi
}

rm -f "$REPORT_PATH"
mkdir -p "$SANDBOX_ROOT"

for run_id in $(seq 1 "$REPEAT"); do
  run_once "$run_id"
done

printf 'report: %s\n' "$REPORT_PATH"
