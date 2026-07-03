#!/usr/bin/env python3
"""swarmctl -- agent-facing control tool for the .swarm coordination protocol.

Design invariants (see .swarm/README.md):
  1. A running agent NEVER waits on another agent. It parks and moves on / exits.
  2. Every agent run is inbox-first.
  3. Only the pump restarts agents. swarmctl only mutates durable state.

Stdlib only. No daemons. All state is files under <project>/.swarm/.
All multi-writer files are written atomically (tmp + rename); leases are
acquired with O_EXCL so two agents can never hold the same file.
"""

import argparse
import hashlib
import hmac
import json
import os
import re
import secrets
import subprocess
import sys
import time

SWARM = ".swarm"
MSG_TYPES = (
    "question",      # needs an answer; sender should park on the reply
    "answer",        # reply to a question ('re' required)
    "info",          # FYI, no reply expected
    "claim-request", # ask the domain owner to grant a file lease
    "claim-grant",   # owner grants; must include 'paths'
    "claim-deny",    # owner refuses; must include 're'
    "handoff",       # transfer a task to another agent
    "system",        # emitted by the pump/swarmctl itself
)

# ---------------------------------------------------------------- utilities

def die(msg, code=1):
    print(f"swarmctl: error: {msg}", file=sys.stderr)
    sys.exit(code)

def now():
    return time.time()

def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

def new_id(prefix):
    return f"{prefix}-{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{secrets.token_hex(3)}"

def find_root(explicit=None):
    """Walk up from cwd (or SWARM_ROOT) until a .swarm dir is found."""
    p = os.path.abspath(explicit or os.environ.get("SWARM_ROOT") or os.getcwd())
    while True:
        if os.path.isdir(os.path.join(p, SWARM)):
            return p
        parent = os.path.dirname(p)
        if parent == p:
            die("no .swarm directory found here or above; run 'swarmctl init' first")
        p = parent

def sdir(root, *parts):
    return os.path.join(root, SWARM, *parts)

def agent_dir(root, agent):
    return sdir(root, "agents", agent)

def atomic_write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = f"{path}.tmp.{os.getpid()}.{secrets.token_hex(2)}"
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)

def read_json(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)

def write_json(path, obj):
    atomic_write(path, json.dumps(obj, indent=2, sort_keys=True) + "\n")

def load_config(root):
    return read_json(sdir(root, "swarm.json"))

def list_agents(root):
    base = sdir(root, "agents")
    if not os.path.isdir(base):
        return []
    return sorted(d for d in os.listdir(base)
                  if os.path.isdir(os.path.join(base, d)))

def require_agent(root, agent):
    if agent != "user" and agent not in list_agents(root):
        die(f"unknown agent '{agent}' (known: {', '.join(list_agents(root)) or 'none'})")

# ------------------------------------------------------------ signing (HMAC)

def load_secret(root):
    with open(sdir(root, "secret"), "rb") as f:
        return f.read().strip()

def canonical(msg):
    return json.dumps({k: v for k, v in msg.items() if k != "sig"},
                      sort_keys=True, separators=(",", ":")).encode()

def sign(root, msg):
    msg["sig"] = hmac.new(load_secret(root), canonical(msg), hashlib.sha256).hexdigest()
    return msg

def verify(root, msg):
    expect = hmac.new(load_secret(root), canonical(msg), hashlib.sha256).hexdigest()
    return hmac.compare_digest(expect, msg.get("sig", ""))

# ----------------------------------------------------------- glob / domains

def glob_to_re(pat):
    """Translate a domain glob to a regex. '**' spans '/', '*' does not."""
    out, i = [], 0
    while i < len(pat):
        c = pat[i]
        if c == "*":
            if pat[i:i + 3] == "**/":
                out.append(r"(?:.*/)?"); i += 3
            elif pat[i:i + 2] == "**":
                out.append(r".*"); i += 2
            else:
                out.append(r"[^/]*"); i += 1
        elif c == "?":
            out.append(r"[^/]"); i += 1
        else:
            out.append(re.escape(c)); i += 1
    return re.compile("^" + "".join(out) + "$")

def norm_rel(root, path):
    rel = os.path.relpath(os.path.abspath(path), root)
    rel = rel.replace(os.sep, "/")
    if rel.startswith(".."):
        die(f"path escapes project root: {path}")
    return rel

def load_domain(root, agent):
    p = os.path.join(agent_dir(root, agent), "domain.json")
    if not os.path.isfile(p):
        return {"agent": agent, "writable": [], "forbidden_write": [SWARM + "/**", ".git/**"]}
    return read_json(p)

def domain_writable(dom, rel):
    if any(glob_to_re(g).match(rel) for g in dom.get("forbidden_write", [])):
        return False
    return any(glob_to_re(g).match(rel) for g in dom.get("writable", []))

def owners_of(root, rel, exclude=None):
    """Which agents' writable globs cover this path?"""
    return [a for a in list_agents(root)
            if a != exclude and domain_writable(load_domain(root, a), rel)]

# ------------------------------------------------------------------ messages

def inbox_dir(root, agent):
    return os.path.join(agent_dir(root, agent), "inbox")

def processed_dir(root, agent):
    return os.path.join(inbox_dir(root, agent), "processed")

def inbox_messages(root, agent, include_processed=False):
    msgs = []
    for base in ([inbox_dir(root, agent)] +
                 ([processed_dir(root, agent)] if include_processed else [])):
        if not os.path.isdir(base):
            continue
        for fn in sorted(os.listdir(base)):
            if fn.endswith(".json"):
                try:
                    m = read_json(os.path.join(base, fn))
                    m["_file"] = os.path.join(base, fn)
                    m["_processed"] = base.endswith("processed")
                    msgs.append(m)
                except (json.JSONDecodeError, OSError) as e:
                    print(f"warning: unreadable message {fn}: {e}", file=sys.stderr)
    return msgs

def cmd_send(root, a):
    require_agent(root, a.frm)
    require_agent(root, a.to)
    if a.to == "user":
        die("'user' has no inbox; write your findings to your run output instead")
    if a.type not in MSG_TYPES:
        die(f"invalid type '{a.type}' (valid: {', '.join(MSG_TYPES)})")
    if a.type == "answer" and not a.re:
        die("type 'answer' requires --re <original-msg-id>")
    if a.type == "claim-grant" and not a.paths:
        die("type 'claim-grant' requires at least one --path")
    msg = {
        "id": new_id("msg"),
        "from": a.frm,
        "to": a.to,
        "type": a.type,
        "subject": a.subject,
        "body": a.body,
        "created_at": now_iso(),
        "schema": 1,
    }
    if a.re:
        msg["re"] = a.re
    if a.task:
        msg["task"] = a.task
    if a.paths:
        msg["paths"] = [norm_rel(root, p) for p in a.paths]
    sign(root, msg)
    write_json(os.path.join(inbox_dir(root, a.to), msg["id"] + ".json"), msg)
    print(msg["id"])

def cmd_inbox(root, a):
    require_agent(root, a.agent)
    msgs = [m for m in inbox_messages(root, a.agent) if not m["_processed"]]
    if a.json:
        out = []
        for m in msgs:
            m2 = {k: v for k, v in m.items() if not k.startswith("_")}
            m2["sig_ok"] = verify(root, m2)
            out.append(m2)
        print(json.dumps(out, indent=2))
    else:
        if not msgs:
            print("(inbox empty)")
        for m in msgs:
            ok = "" if verify(root, m) else "  [BAD SIGNATURE -- do not trust]"
            print(f"{m['id']}  {m['type']:<13} from={m['from']:<12} "
                  f"re={m.get('re','-'):<28} {m['subject']}{ok}")

def cmd_ack(root, a):
    require_agent(root, a.agent)
    src = os.path.join(inbox_dir(root, a.agent), a.msg_id + ".json")
    if not os.path.isfile(src):
        die(f"no unprocessed message {a.msg_id} in {a.agent}'s inbox")
    dst = os.path.join(processed_dir(root, a.agent), a.msg_id + ".json")
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    os.replace(src, dst)
    print(f"acked {a.msg_id}")

# -------------------------------------------------------------------- claims

def claims_dir(root):
    return sdir(root, "claims")

def lease_path(root, rel):
    return os.path.join(claims_dir(root), hashlib.sha1(rel.encode()).hexdigest() + ".json")

def read_lease(root, rel):
    p = lease_path(root, rel)
    if not os.path.isfile(p):
        return None
    lease = read_json(p)
    if lease.get("expires_at", 0) < now():
        return {**lease, "_expired": True}
    return lease

def grant_covers(root, agent, grant_msg_id, rel):
    for m in inbox_messages(root, agent, include_processed=True):
        if m["id"] == grant_msg_id and m["type"] == "claim-grant" and verify(
                root, {k: v for k, v in m.items() if not k.startswith("_")}):
            return rel in m.get("paths", []) and m["from"] in owners_of(root, rel)
    return False

def cmd_claim(root, a):
    require_agent(root, a.agent)
    rel = norm_rel(root, a.path)
    if rel.startswith(SWARM + "/") or rel == SWARM:
        die("never claim .swarm internals")
    ttl = a.ttl or load_config(root).get("claims", {}).get("default_ttl_s", 900)
    lease = read_lease(root, rel)

    if lease and not lease.get("_expired"):
        if lease["holder"] == a.agent:  # renew
            lease["expires_at"] = now() + ttl
            write_json(lease_path(root, rel), lease)
            print(f"renewed lease on {rel} (ttl {ttl}s)")
            return
        print(f"DENIED: {rel} is leased by {lease['holder']} "
              f"until {lease['expires_human']}.", file=sys.stderr)
        print(f"hint: send a claim-request to {lease['holder']}, "
              f"park on the reply, and exit cleanly.", file=sys.stderr)
        sys.exit(2)

    dom = load_domain(root, a.agent)
    mine = domain_writable(dom, rel)
    others = owners_of(root, rel, exclude=a.agent)

    if not mine and others and not (a.grant and grant_covers(root, a.agent, a.grant, rel)):
        print(f"DENIED: {rel} is in the domain of {', '.join(others)} and not yours.",
              file=sys.stderr)
        print(f"hint: send a claim-request to the owner; claim again with "
              f"--grant <claim-grant-msg-id> once granted.", file=sys.stderr)
        sys.exit(3)
    if not mine and not others and any(
            glob_to_re(g).match(rel) for g in dom.get("forbidden_write", [])):
        die(f"{rel} matches your forbidden_write patterns")

    if lease and lease.get("_expired"):
        os.remove(lease_path(root, rel))  # reap, then race for it below

    new_lease = {
        "path": rel,
        "holder": a.agent,
        "task": a.task,
        "grant": a.grant,
        "acquired_at": now_iso(),
        "expires_at": now() + ttl,
        "expires_human": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now() + ttl)),
        "contested": bool(others),
    }
    os.makedirs(claims_dir(root), exist_ok=True)
    try:  # O_EXCL: exactly one winner even under a true race
        fd = os.open(lease_path(root, rel), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError:
        print(f"DENIED: lost race for {rel}; re-check with 'swarmctl claim' later "
              f"or park on it.", file=sys.stderr)
        sys.exit(2)
    with os.fdopen(fd, "w") as f:
        json.dump(new_lease, f, indent=2, sort_keys=True)
    tag = " (contested -- overlaps another domain; lease is mandatory)" if others else ""
    print(f"claimed {rel} for {a.agent}, ttl {ttl}s{tag}")

def cmd_release(root, a):
    require_agent(root, a.agent)
    rel = norm_rel(root, a.path)
    lease = read_lease(root, rel)
    if not lease:
        print(f"(no lease on {rel})")
        return
    if lease["holder"] != a.agent and not a.force:
        die(f"{rel} is held by {lease['holder']}, not you (use --force only if reaping)")
    os.remove(lease_path(root, rel))
    print(f"released {rel}")

def all_leases(root):
    out = []
    if os.path.isdir(claims_dir(root)):
        for fn in sorted(os.listdir(claims_dir(root))):
            if fn.endswith(".json"):
                out.append(read_json(os.path.join(claims_dir(root), fn)))
    return out

# ----------------------------------------------------------- parking / ledger

def parked_dir(root):
    return sdir(root, "ledger", "parked")

def park_path(root, task):
    return os.path.join(parked_dir(root), task + ".json")

def cmd_park(root, a):
    require_agent(root, a.agent)
    if bool(a.wait_reply) == bool(a.wait_claim):
        die("specify exactly one of --wait-reply MSG_ID or --wait-claim PATH")
    wait = ({"type": "reply", "re": a.wait_reply} if a.wait_reply
            else {"type": "claim", "path": norm_rel(root, a.wait_claim)})
    entry = {
        "task": a.task or new_id("task"),
        "agent": a.agent,
        "wait": wait,
        "waiting_on": a.waiting_on,
        "resume_hint": a.resume_hint,
        "created_at": now_iso(),
    }
    write_json(park_path(root, entry["task"]), entry)
    print(entry["task"])

def cmd_unpark(root, a):
    p = park_path(root, a.task)
    if not os.path.isfile(p):
        die(f"no parked task {a.task}")
    os.remove(p)
    print(f"unparked {a.task}")

def all_parked(root):
    out = []
    if os.path.isdir(parked_dir(root)):
        for fn in sorted(os.listdir(parked_dir(root))):
            if fn.endswith(".json"):
                out.append(read_json(os.path.join(parked_dir(root), fn)))
    return out

def park_satisfied(root, entry):
    """A parked task is resumable when its dependency is met."""
    w = entry["wait"]
    if w["type"] == "reply":
        return any(m.get("re") == w["re"] and not m["_processed"]
                   for m in inbox_messages(root, entry["agent"]))
    if w["type"] == "claim":
        lease = read_lease(root, w["path"])
        return lease is None or lease.get("_expired") or lease["holder"] == entry["agent"]
    return False

# --------------------------------------------------------------------- audit

def _hash_file(root, rel):
    p = os.path.join(root, rel)
    if not os.path.isfile(p):
        return "absent"
    h = hashlib.sha1()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()

def git_dirty(root):
    """{relpath: content-sha1} of dirty files, or None if git unavailable."""
    try:
        out = subprocess.run(["git", "-C", root, "status", "--porcelain=v1"],
                             capture_output=True, text=True, timeout=30)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if out.returncode != 0:
        return None
    dirty = {}
    for line in out.stdout.splitlines():
        rel = line[3:].split(" -> ")[-1].strip().strip('"').replace(os.sep, "/")
        if not (rel.startswith(SWARM + "/") or rel == SWARM):
            dirty[rel] = _hash_file(root, rel)
    return dirty

def audit_base_path(root, agent):
    return os.path.join(agent_dir(root, agent), ".audit_base.json")

def snapshot_audit_base(root, agent):
    dirty = git_dirty(root)
    if dirty is not None:
        write_json(audit_base_path(root, agent), dirty)

def cmd_audit(root, a):
    """Compare current dirty files to the snapshot taken at wake time, so
    only files changed DURING THIS RUN are attributed to this agent. Another
    agent's pre-existing uncommitted work never fails your audit."""
    require_agent(root, a.agent)
    dirty = git_dirty(root)
    if dirty is None:
        print("audit skipped: git unavailable or not a repository")
        return
    base = {}
    bp = audit_base_path(root, a.agent)
    if os.path.isfile(bp):
        base = read_json(bp)
    else:
        print("warning: no wake-time snapshot; auditing entire worktree")
    dom = load_domain(root, a.agent)
    violations = sorted(rel for rel, h in dirty.items()
                        if base.get(rel) != h and not domain_writable(dom, rel))
    if violations:
        rec = {"id": new_id("viol"), "agent": a.agent, "paths": violations,
               "at": now_iso()}
        write_json(sdir(root, "ledger", "violations", rec["id"] + ".json"), rec)
        print(f"AUDIT FAIL: {a.agent} dirtied files outside its domain this run:")
        for v in violations:
            print(f"  - {v}")
        print("Revert these changes or message the owning agent with a handoff.")
        sys.exit(1)
    print(f"audit ok: no out-of-domain files dirtied during this run")

# ------------------------------------------------------------------- status

def running_lock(root, agent):
    return os.path.join(agent_dir(root, agent), ".running")

def is_running(root, agent):
    p = running_lock(root, agent)
    if not os.path.isfile(p):
        return False
    try:
        info = read_json(p)
        os.kill(int(info["pid"]), 0)
        return True
    except (OSError, ValueError, KeyError):
        try:
            os.remove(p)  # stale
        except OSError:
            pass
        return False

def wait_graph(root):
    """agent -> agent edges from unsatisfied parks; returns (edges, cycle|None)."""
    edges = {}
    for e in all_parked(root):
        if e.get("waiting_on") and not park_satisfied(root, e):
            edges.setdefault(e["agent"], set()).add(e["waiting_on"])
    seen, stack = set(), []
    def dfs(n):
        if n in stack:
            return stack[stack.index(n):]
        if n in seen:
            return None
        seen.add(n); stack.append(n)
        for m in edges.get(n, ()):
            c = dfs(m)
            if c:
                return c
        stack.pop()
        return None
    for n in list(edges):
        c = dfs(n)
        if c:
            return edges, c
    return edges, None

def cmd_status(root, a):
    agents = list_agents(root)
    print(f"project root : {root}")
    print(f"agents       : {', '.join(agents) or '(none)'}")
    for ag in agents:
        n = len([m for m in inbox_messages(root, ag) if not m["_processed"]])
        run = "RUNNING" if is_running(root, ag) else "stopped"
        print(f"  {ag:<16} {run:<8} inbox={n}")
    parked = all_parked(root)
    print(f"parked tasks : {len(parked)}")
    for e in parked:
        sat = "READY" if park_satisfied(root, e) else "waiting"
        print(f"  {e['task']}  {e['agent']} -> {e.get('waiting_on','?')} "
              f"[{sat}] {e['wait']}")
    leases = all_leases(root)
    print(f"claims       : {len(leases)}")
    for l in leases:
        exp = "EXPIRED" if l["expires_at"] < now() else l["expires_human"]
        print(f"  {l['path']}  held by {l['holder']} until {exp}")
    _, cycle = wait_graph(root)
    if cycle:
        print(f"DEADLOCK     : cycle detected: {' -> '.join(cycle + [cycle[0]])}")
    else:
        print("deadlock     : none")

# ----------------------------------------------------------- wake / launcher

def compose_prompt(root, agent, reason):
    tpl_path = sdir(root, "prompts", "bootstrap.md")
    with open(tpl_path, "r", encoding="utf-8") as f:
        tpl = f.read()
    inbox = [f"- {m['id']} [{m['type']}] from {m['from']}: {m['subject']}"
             for m in inbox_messages(root, agent) if not m["_processed"]]
    parked = [f"- {e['task']}: waiting on {e['wait']} | resume hint: "
              f"{e.get('resume_hint') or '(none)'}"
              + ("  [DEPENDENCY SATISFIED -- resume this]"
                 if park_satisfied(root, e) else "")
              for e in all_parked(root) if e["agent"] == agent]
    subs = {
        "[[AGENT_ID]]": agent,
        "[[PROJECT_ROOT]]": root,
        "[[WAKE_REASON]]": reason,
        "[[DOMAIN_JSON]]": json.dumps(load_domain(root, agent), indent=2),
        "[[INBOX_SUMMARY]]": "\n".join(inbox) or "(empty)",
        "[[PARKED_SUMMARY]]": "\n".join(parked) or "(none)",
    }
    for k, v in subs.items():
        tpl = tpl.replace(k, v)
    return tpl

def cmd_wake(root, a):
    require_agent(root, a.agent)
    if is_running(root, a.agent):
        die(f"{a.agent} is already running")
    prompt = compose_prompt(root, a.agent, a.why or "manual wake")
    pfile = os.path.join(agent_dir(root, a.agent), ".prompt.md")
    atomic_write(pfile, prompt)
    snapshot_audit_base(root, a.agent)  # baseline for this run's audit
    tpl = (a.launcher or load_config(root)["launcher"]["template"])
    cmd = (tpl.replace("{prompt_file}", pfile)
              .replace("{agent}", a.agent)
              .replace("{root}", root))
    logdir = os.path.join(agent_dir(root, a.agent), "logs")
    os.makedirs(logdir, exist_ok=True)
    logf = open(os.path.join(logdir, f"run-{new_id('r')[2:]}.log"), "w")
    proc = subprocess.Popen(cmd, shell=True, cwd=root, stdout=logf,
                            stderr=subprocess.STDOUT, start_new_session=True)
    logf.close()  # child holds its own copy of the fd
    write_json(running_lock(root, a.agent),
               {"pid": proc.pid, "started_at": now_iso(), "reason": a.why or "manual"})
    print(f"launched {a.agent} pid={proc.pid} reason={a.why or 'manual'}")
    return proc

def cmd_done(root, a):
    """Agent calls this as its last act: releases the running lock cleanly."""
    p = running_lock(root, a.agent)
    if os.path.isfile(p):
        os.remove(p)
    print(f"{a.agent} marked done")

# ------------------------------------------------------------- init/add-agent

DEFAULT_CONFIG = {
    "launcher": {
        "template": ('claude -p "$(cat {prompt_file})" '
                     '--permission-mode acceptEdits '
                      '--allowedTools "Bash(python3 {root}/swarmctl.py *)" '
                     '--max-turns 50'),
        "_note": ("Any headless CLI agent works. Placeholders: {prompt_file} "
                  "{agent} {root}. For a no-AI dry run use: "
                  "python3 {root}/stub_agent.py {agent}"),
    },
    "pump": {"interval_s": 20, "max_concurrent": 2},
    "claims": {"default_ttl_s": 900},
}

def cmd_init(root_arg, a):
    root = os.path.abspath(a.project or os.getcwd())
    base = os.path.join(root, SWARM)
    if os.path.isdir(base) and os.path.isfile(os.path.join(base, "swarm.json")):
        die(f"{base} already initialized")
    for d in ("agents", "claims", "ledger/parked", "ledger/violations",
              "bin", "prompts"):
        os.makedirs(os.path.join(base, *d.split("/")), exist_ok=True)
    write_json(os.path.join(base, "swarm.json"), DEFAULT_CONFIG)
    atomic_write(os.path.join(base, "secret"), secrets.token_hex(32) + "\n")
    atomic_write(os.path.join(base, ".gitignore"),
                 "secret\nagents/*/.prompt.md\nagents/*/.running\n"
                 "agents/*/logs/\npump.log\n")
    print(f"initialized swarm at {base}")
    print("next: swarmctl add-agent <name> --writable '<glob>' ...")

def cmd_add_agent(root, a):
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,31}", a.name):
        die("agent names: lowercase alnum, '-', '_' (max 32 chars)")
    ad = agent_dir(root, a.name)
    os.makedirs(os.path.join(ad, "inbox", "processed"), exist_ok=True)
    os.makedirs(os.path.join(ad, "logs"), exist_ok=True)
    write_json(os.path.join(ad, "domain.json"), {
        "agent": a.name,
        "description": a.desc or "",
        "writable": a.writable or [],
        "forbidden_write": [SWARM + "/**", ".git/**"],
    })
    print(f"added agent '{a.name}' writable={a.writable or []}")

# ----------------------------------------------------------------------- CLI

def main(argv=None):
    ap = argparse.ArgumentParser(prog="swarmctl", description=__doc__)
    ap.add_argument("--root", default=None,
                    help="project root (default: walk up from cwd)")
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--root", default=argparse.SUPPRESS)
    sub = ap.add_subparsers(dest="cmd", required=True)

    p = sub.add_parser("init", parents=[common]); p.add_argument("--project")
    p = sub.add_parser("add-agent", parents=[common])
    p.add_argument("name"); p.add_argument("--writable", action="append")
    p.add_argument("--desc")
    p = sub.add_parser("send", parents=[common])
    p.add_argument("--from", dest="frm", required=True)
    p.add_argument("--to", required=True); p.add_argument("--type", required=True)
    p.add_argument("--subject", required=True); p.add_argument("--body", default="")
    p.add_argument("--re"); p.add_argument("--task")
    p.add_argument("--path", dest="paths", action="append")
    p = sub.add_parser("inbox", parents=[common])
    p.add_argument("agent"); p.add_argument("--json", action="store_true")
    p = sub.add_parser("ack", parents=[common])
    p.add_argument("agent"); p.add_argument("msg_id")
    p = sub.add_parser("claim", parents=[common])
    p.add_argument("agent"); p.add_argument("path")
    p.add_argument("--ttl", type=int); p.add_argument("--task")
    p.add_argument("--grant", help="claim-grant message id authorizing this claim")
    p = sub.add_parser("release", parents=[common])
    p.add_argument("agent"); p.add_argument("path")
    p.add_argument("--force", action="store_true")
    p = sub.add_parser("park", parents=[common])
    p.add_argument("agent"); p.add_argument("--task")
    p.add_argument("--wait-reply"); p.add_argument("--wait-claim")
    p.add_argument("--waiting-on", required=True)
    p.add_argument("--resume-hint", default="")
    p = sub.add_parser("unpark", parents=[common]); p.add_argument("task")
    p = sub.add_parser("audit", parents=[common]); p.add_argument("agent")
    p = sub.add_parser("status", parents=[common])
    p = sub.add_parser("wake", parents=[common])
    p.add_argument("agent"); p.add_argument("--why"); p.add_argument("--launcher")
    p = sub.add_parser("done", parents=[common]); p.add_argument("agent")

    a = ap.parse_args(argv)
    if a.cmd == "init":
        return cmd_init(None, a)
    root = find_root(a.root)
    return {
        "add-agent": cmd_add_agent, "send": cmd_send, "inbox": cmd_inbox,
        "ack": cmd_ack, "claim": cmd_claim, "release": cmd_release,
        "park": cmd_park, "unpark": cmd_unpark, "audit": cmd_audit,
        "status": cmd_status, "wake": cmd_wake, "done": cmd_done,
    }[a.cmd](root, a)

if __name__ == "__main__":
    main()
