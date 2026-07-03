#!/usr/bin/env python3
"""stub_agent.py -- a scripted, no-AI stand-in for a real agent.

Lets you demo the full swarm lifecycle (pump wakes, inbox-first, answering,
claim grants, park/unpark, deadlock breaking) without spending a single
model token. It follows the exact protocol a real agent is instructed to
follow in prompts/bootstrap.md:

  1. Resume any parked task whose dependency is satisfied.
  2. Drain the inbox: answer questions, grant claim-requests, honor grants.
  3. If woken with an empty inbox and only UNsatisfied parks, assume it is
     the designated deadlock breaker: unpark, notify counterparts.
  4. Exit cleanly (a stub's whole run is one 'slice').

Usage: python3 stub_agent.py <agent-id>
"""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import swarmctl as sc  # noqa: E402


def ns(**kw):
    return argparse.Namespace(**kw)


def send(root, frm, to, typ, subject, body="", re=None, paths=None, task=None):
    sc.cmd_send(root, ns(frm=frm, to=to, type=typ, subject=subject, body=body,
                         re=re, paths=paths, task=task))


def main():
    agent = sys.argv[1]
    root = sc.find_root(None)
    print(f"[stub:{agent}] awake")

    # 1. Resume satisfied parks first.
    for e in sc.all_parked(root):
        if e["agent"] == agent and sc.park_satisfied(root, e):
            print(f"[stub:{agent}] resuming parked task {e['task']} "
                  f"(hint: {e.get('resume_hint') or 'none'})")
            sc.cmd_unpark(root, ns(task=e["task"]))

    # 2. Inbox-first: drain and handle every unprocessed message.
    msgs = [m for m in sc.inbox_messages(root, agent) if not m["_processed"]]
    for m in msgs:
        clean = {k: v for k, v in m.items() if not k.startswith("_")}
        if not sc.verify(root, clean):
            print(f"[stub:{agent}] REJECTING unsigned/tampered {m['id']}")
            sc.cmd_ack(root, ns(agent=agent, msg_id=m["id"]))
            continue
        t = m["type"]
        if t == "question" and m["from"] != "user":
            send(root, agent, m["from"], "answer",
                 f"re: {m['subject']}",
                 "Stub answer: yes, proceed with the default approach.",
                 re=m["id"])
            print(f"[stub:{agent}] answered {m['id']} from {m['from']}")
        elif t == "claim-request":
            send(root, agent, m["from"], "claim-grant",
                 f"granted: {m['subject']}",
                 "Stub grants this claim. Release when finished.",
                 re=m["id"], paths=m.get("paths") or [])
            print(f"[stub:{agent}] granted claim to {m['from']} "
                  f"for {m.get('paths')}")
        elif t == "claim-grant":
            for rel in m.get("paths", []):
                sc.cmd_claim(root, ns(agent=agent, path=os.path.join(root, rel),
                                      ttl=60, task=None, grant=m["id"]))
                print(f"[stub:{agent}] claimed {rel} using grant {m['id']}; "
                      f"pretending to edit; releasing")
                sc.cmd_release(root, ns(agent=agent,
                                        path=os.path.join(root, rel),
                                        force=False))
        elif t == "info" and m["subject"].startswith("deadlock"):
            for e in sc.all_parked(root):
                if e["agent"] == agent and e.get("waiting_on") == m["from"]:
                    print(f"[stub:{agent}] counterpart broke deadlock; "
                          f"unparking {e['task']}")
                    sc.cmd_unpark(root, ns(task=e["task"]))
        sc.cmd_ack(root, ns(agent=agent, msg_id=m["id"]))

    # 3. Deadlock breaker: woken, nothing in inbox, only unsatisfied parks.
    if not msgs:
        stuck = [e for e in sc.all_parked(root)
                 if e["agent"] == agent and not sc.park_satisfied(root, e)]
        for e in stuck:
            other = e.get("waiting_on")
            print(f"[stub:{agent}] deadlock breaker: unparking {e['task']}, "
                  f"proceeding on documented assumption, notifying {other}")
            sc.cmd_unpark(root, ns(task=e["task"]))
            if other and other != "user":
                send(root, agent, other, "info",
                     "deadlock broken by counterpart",
                     f"I was designated deadlock breaker for the cycle "
                     f"involving us. I unparked '{e['task']}' and proceeded "
                     f"assuming: default interface, no breaking changes. "
                     f"Object via a new question if wrong.")

    # 4. Exit cleanly.
    sc.cmd_done(root, ns(agent=agent))
    print(f"[stub:{agent}] slice complete, exiting")


if __name__ == "__main__":
    main()
