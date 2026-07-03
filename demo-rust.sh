#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SANDBOX_ROOT="${1:-/tmp/trelane-demo}"
REPEAT="${TRELANE_DEMO_REPEAT:-1}"
REPORT_PATH="${TRELANE_DEMO_REPORT:-$SANDBOX_ROOT/report.jsonl}"
SCENARIO_PATH="${TRELANE_DEMO_SCENARIO:-$HERE/tests/full-usage-scenario.json}"

cargo build --release >/dev/null
TRELANE="$HERE/target/release/trelane"
"$TRELANE" --testing "$SCENARIO_PATH" --testing-runs "$REPEAT" --testing-report "$REPORT_PATH" --testing-sandbox-root "$SANDBOX_ROOT" --testing-launcher "trelane --root {root} stub {agent}"
