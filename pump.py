#!/usr/bin/env python3
"""pump.py -- the dumb pump. The ONLY component that restarts agents.

It has zero intelligence by design: read durable state, decide who needs to
wake, exec the launcher. Run it as `--watch`, from cron with `--once`, or
from a fs-watcher. If the pump dies, nothing is lost -- state is on disk and
the next `--once` picks up exactly where things stand.

Wake reasons, in priority order:
  inbox    -- agent has unprocessed messages
  resume   -- a parked task's dependency is now satisfied
  deadlock -- a wait-cycle exists and no agent in it is otherwise wakeable;
              wake the lexicographically-first agent with a break directive
"""

import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import swarmctl as sc  # noqa: E402


def log(root, msg):
    line = f"{sc.now_iso()} {msg}"
    print(line)
    with open(sc.sdir(root, "pump.log"), "a", encoding="utf-8") as f:
        f.write(line + "\n")


def reap_leases(root):
    """Expired leases are removed; the ex-holder gets a system message."""
    for lease in sc.all_leases(root):
        if lease["expires_at"] < sc.now():
            try:
                os.remove(sc.lease_path(root, lease["path"]))
            except OSError:
                continue
            log(root, f"reaped expired lease {lease['path']} (was {lease['holder']})")
            if lease["holder"] in sc.list_agents(root):
                msg = {
                    "id": sc.new_id("msg"), "from": "system",
                    "to": lease["holder"], "type": "system",
                    "subject": f"lease expired: {lease['path']}",
                    "body": ("Your lease expired and was reaped. If you still "
                             "hold uncommitted work on this file, re-claim "
                             "before touching it again."),
                    "created_at": sc.now_iso(), "schema": 1,
                }
                sc.sign(root, msg)
                sc.write_json(os.path.join(sc.inbox_dir(root, lease["holder"]),
                                           msg["id"] + ".json"), msg)


def wake_candidates(root):
    """Return {agent: reason-string}, priority already applied per agent."""
    cands = {}
    for agent in sc.list_agents(root):
        if sc.is_running(root, agent):
            continue
        n = len([m for m in sc.inbox_messages(root, agent) if not m["_processed"]])
        if n:
            cands[agent] = f"inbox: {n} unprocessed message(s)"
            continue
        ready = [e["task"] for e in sc.all_parked(root)
                 if e["agent"] == agent and sc.park_satisfied(root, e)]
        if ready:
            cands[agent] = f"resume: parked task(s) ready: {', '.join(ready)}"

    if not cands:  # only consider deadlock-breaking when nothing else moves
        _, cycle = sc.wait_graph(root)
        if cycle and not any(sc.is_running(root, ag) for ag in cycle):
            victim = sorted(cycle)[0]
            cands[victim] = ("deadlock: wait-cycle "
                             f"{' -> '.join(cycle + [cycle[0]])}. You are the "
                             "designated breaker: proceed with a documented "
                             "assumption, message your counterpart stating it, "
                             "and unpark your task.")
    return cands


def tick(root, cfg, launcher_override=None):
    reap_leases(root)
    cands = wake_candidates(root)
    if not cands:
        return 0
    budget = cfg["pump"]["max_concurrent"] - sum(
        1 for ag in sc.list_agents(root) if sc.is_running(root, ag))
    launched = 0
    for agent, reason in sorted(cands.items()):
        if launched >= max(budget, 0):
            log(root, f"deferred wake of {agent} (concurrency budget reached)")
            continue
        ns = argparse.Namespace(agent=agent, why=reason,
                                launcher=launcher_override)
        log(root, f"waking {agent}: {reason}")
        sc.cmd_wake(root, ns)
        launched += 1
    return launched


def main():
    ap = argparse.ArgumentParser(prog="pump", description=__doc__)
    ap.add_argument("--root")
    ap.add_argument("--once", action="store_true", help="single tick (cron-friendly)")
    ap.add_argument("--watch", action="store_true", help="loop forever")
    ap.add_argument("--interval", type=int, help="override poll interval seconds")
    ap.add_argument("--launcher", help="override launcher template for this run")
    a = ap.parse_args()
    root = sc.find_root(a.root)
    cfg = sc.load_config(root)
    if a.once or not a.watch:
        tick(root, cfg, a.launcher)
        return
    interval = a.interval or cfg["pump"]["interval_s"]
    log(root, f"pump watching every {interval}s (ctrl-c to stop)")
    while True:
        try:
            tick(root, cfg, a.launcher)
        except Exception as e:  # a bad tick must never kill the pump
            log(root, f"tick error: {e!r}")
        time.sleep(interval)


if __name__ == "__main__":
    main()
