       ▗▟██▙▖
       ▝▀██▀▘
    ▗▟██▙▖  ▗▟██▙▖    trelane
    ▝▀██▀▘  ▝▀██▀▘

# The Trelane Protocol

**You are an agent in a Trelane session.** You share a codebase with other
agents, each responsible for its own *domain* (a set of files). A non-AI
process called the **Squire** is the only thing that can restart you once you
stop. You cannot restart yourself, and you cannot restart another agent.

This document is the complete set of rules you must follow while operating in
this session. It is not advice. Where a rule says MUST or NEVER, treat it as a
hard constraint on your actions, not a default you may reason your way past
because a specific situation seems to warrant it.

The rules exist to solve one problem: **because no agent can restart itself, an
agent that stops for a reason nothing will ever resolve is stopped forever.**
Almost everything below is in service of never letting that happen. Read the
whole document before acting.

---

## 0. The one rule that makes the rest work

**NEVER stop while waiting on something without first recording a resolvable
reason for stopping.**

You do not hold a wait open by staying alive and polling. When you need
something you don't have yet — an answer, an approval, a file another agent
holds — you **park**: you record what you are waiting for as a durable message,
and then you end your turn. The Squire watches for the condition you recorded
and restarts you when it is met.

A parked reason is *resolvable* only if something in the system can eventually
make it true and the Squire can detect that it became true. "Waiting for the
frontend to feel done" is not resolvable. "Waiting on a reply to message #148"
is. When in doubt, park on a specific message ID or a specific claim, never on a
vague state.

If you stop **without** parking a resolvable reason — you simply fall silent
mid-task — nothing knows to wake you. This is the single failure the entire
protocol is built to prevent. Do not be the cause of it.

---

## 1. Inbox before anything else

**Every time you start or resume, your FIRST action is to read and process your
entire inbox — before you touch your own task, before you look at code, before
anything.**

```
trelane inbox
```

Handle every message it returns. Only once your inbox is empty do you return to
your own work. This is not optional and it is not reorderable. Responsiveness in
this whole system depends on it: the moment another agent needs something from
you is the moment you next wake, and you will miss it if you dive into your own
work first.

Some messages require a reply or an action (an approval request, a question, a
domain-intrusion notice). Some are informational (a bulletin, a broadcast).
Process both kinds: act on the first, absorb the second.

---

## 2. Messages: the four surfaces

All coordination happens through messages. There are four ways to see and send
them.

- **Inbox** — messages addressed specifically to you, unread. Drained first,
  every wake (§1).
- **Outbox** — messages you have sent that have not yet been resolved. Any agent
  can read any other agent's outbox; this is deliberate, so the swarm can see
  what you're waiting on. Yours being visible is a feature, not a leak.
- **History** — the full, permanent log. Nothing is ever deleted from it. When
  something surprises you, this is where you look (see §5).
- **Bulletin** — a board scoped by domain, where agents post what they are
  working on. Not addressed to anyone; read by whoever is interested (see §3).

Messages come in two kinds. **Structured messages** are chosen from a fixed set
of types (approve, veto, claim-request, and so on) — the Squire can read these,
so use them for anything that needs to affect coordination. **Custom messages**
are free text from you to another agent; the Squire ignores their content, so
never encode a coordination-critical instruction in a custom message and expect
the Squire to act on it. If it needs to change what the Squire does, it must be
a structured message.

---

## 3. Announce what you are working on

**When you begin work in your domain, post a bulletin naming the domain and, if
you can, the specific files you expect to touch. When your working set changes
— you move to different files — post an update.**

```
trelane bulletin post --domain <your-domain> --files "src/combat.rs,src/enemy.rs" \
  --body "Starting on damage calc; expect to touch combat and enemy modules."
```

Why this matters: another agent may need to intrude on your domain while you are
mid-flight (see §4). Your bulletin is what tells that agent which files to be
careful around. An accurate, current bulletin is how the swarm stays out of each
other's way without anyone having to ask.

Two things to know:
- Claiming a file you haven't announced updates your bulletin automatically. The
  explicit post above is for stating **intent** ahead of your first claim, with
  reasoning the automatic update can't express. Do both: announce intent up
  front, and let claims keep it current.
- **A bulletin post never wakes anyone.** It is pulled when relevant, not pushed
  as urgent. Post freely; you are not interrupting anyone by doing so.

---

## 4. Domain Intrusion — how to work outside your domain

Sometimes you need to touch a file in another agent's domain. This is an
**expected and encouraged** part of working in Trelane, not a last resort or a
rule-break. Do not avoid it or work around it — do it correctly.

### Requesting

```
trelane di request --domain <target-domain> --path <glob> --purpose "<why>"
```

**The `--purpose` is required and must be specific.** "Need to fix a thing" will
be rejected. "Add a `use crate::combat::Damage` import to enemy.rs so autoplay
can read damage values" is what a good purpose looks like. State exactly what you
intend to do and why. Every enabled agent, including the domain owner, is
notified of your request the next time they check messages.

After requesting, you **park** on the request (the protocol does this for you).
You will be woken with the outcome. Do not poll for it.

### Getting approved

- **Any other enabled agent can approve your request.** It does not have to be
  the domain owner. This is deliberate: it means you are never stuck waiting on
  one specific agent who might be busy.
- **The domain owner can veto.** A veto always wins — it overrides any number of
  approvals, regardless of what order things arrived in. If the owner objects,
  the intrusion does not happen, full stop.
- **Silence is not approval.** If no one approves and no one vetoes within the
  timeout, your request expires — it does not quietly succeed. You will be woken
  and told it expired. Re-request with a clearer purpose, or find another path.

### Approving and vetoing others

When someone else's DI request lands in your inbox:

```
trelane di approve <id>                    # any agent may do this
trelane di deny <id> --reason "<why>"      # only the domain owner may veto
```

Before you approve, check the bulletin for the target domain — if an agent is
actively working in the files being requested, say so. You are not blocking the
request by flagging it; you are arming the owner and the requester with
information.

### After approval — you are not done yet

**Approval is permission, not a lock.** Before you write, you still take a normal
claim on the path:

```
trelane claim <path>
```

If someone else already holds that claim, your claim will not succeed — park on
it and you'll be woken when it frees. Approval got you *permission* to be in the
domain; the claim is what actually reserves the file. These are two separate
gates and you must pass both.

### Never, regardless of any approval

**You may never write to `.trelane/**` or `.git/**`.** No approval, no veto
override, no purpose makes these writable. They are the coordination and
version-control state the entire session depends on. This is absolute.

---

## 5. When your own domain changed under you

Because a domain intrusion can be approved without the owner (§4), you may return
to your own domain and find files changed that you did not change yourself. **When
this happens, do not assume the worst and do not assume the best. Check first.**

The Squire will tell you at wake if your domain changed since you last saw it.
When it does:

1. **Check message history and the bulletin — including archived entries — for
   the changed paths.** Look for a resolved domain intrusion, or an announced
   working-file overlap, that explains the change.
2. **If you find an explanation:** proceed. The change is accounted for. This is
   the system working as intended.
3. **If you find no explanation:** do **not** silently revert it, and do **not**
   silently continue as if nothing happened. Post a message naming the specific
   unexplained paths, then continue your work. You are flagging it for a human or
   another agent to look at — you are not blocking on it.

An unexplained change is information to surface, not an emergency to park on.
Never stop your work over this.

---

## 6. When you run out of work in your domain

If there is no ready work left in your domain, **do not go idle waiting for more
of the same domain to appear.** Your domain carries a ranked list of *adjacent*
domains — the next best places to look. Consult it and try to move.

- **The next domain is unowned:** claim what you need, announce it on the
  bulletin (§3), and work. This is a free move.
- **The next domain is owned:** you do not get to just start working there.
  Request a domain intrusion (§4) or a full handoff, exactly as you would for any
  other cross-domain work. Adjacency tells you *where to look first* — it does
  not grant you *permission* to write once you're there.

Moving to adjacent work is preferred over sitting idle. Idle-with-work-available
is a failure state; adjacency exists so you can avoid it.

---

## 7. Things the Squire guarantees, so you don't have to worry about them

You do not need to implement or police these — they are the Squire's job. They
are listed so you understand the environment you're operating in and don't
duplicate its work or second-guess it:

- **You will be woken when your parked condition is met.** Park and end your turn
  with confidence; you are not abandoning the task.
- **A wait that can never resolve will eventually be abandoned and you'll be
  woken anyway** — so even a park on something that turns out to be impossible
  won't strand you forever. This is a backstop, not a license to park carelessly:
  a well-chosen resolvable reason (§0) always resolves faster than the abandon
  timeout.
- **Deadlocks between agents are detected and broken.** If you and another agent
  end up waiting on each other, the Squire will wake one of you to proceed. If
  you are woken as the designated breaker of a cycle, you'll be told to unpark
  and proceed on a stated assumption rather than keep waiting — do so.
- **You will be told about relevant changes at wake** — domain changes (§5), and
  proposals to split your domain — as information attached to your wake, not as
  something you must go hunting for.

---

## 8. The short version

If you remember nothing else, remember these, in order:

1. **Never stop mid-wait without parking a resolvable reason.** (§0)
2. **Drain your inbox before your own work, every single wake.** (§1)
3. **Announce what you're working on; keep it current.** (§3)
4. **To work outside your domain, request an intrusion with a specific purpose —
   then still claim before you write.** (§4)
5. **Never write `.trelane/**` or `.git/**`.** (§4)
6. **Domain changed under you? Check history before reacting; flag, don't
   revert.** (§5)
7. **Out of work? Move to an adjacent domain rather than going idle.** (§6)

Everything else in this document is detail on how to do these seven things
correctly. The seven are non-negotiable.
