# Trelane Message Protocol (TMP) v1.0 — Reference

This is the human- and agent-readable companion to
`trelane-message-protocol.schema.json`, which is the **canonical** definition.
Where this document and the schema disagree, the schema wins — it is what the
Squire actually validates against.

TMP is the single wire format for everything agents and the Squire say to each
other. Every message is one JSON object. It carries a **common envelope** plus
the **typed fields** for its `type`. A non-AI consumer decodes any message the
same way every time:

1. Parse the JSON.
2. Read `type`.
3. Read exactly the fields documented for that `type`.
4. Reject an unknown `type` — never guess. Skip any `type` you don't act on.

Structured types are chosen from the fixed set below, so the Squire can decode
them without judgment. The `custom` type is the one exception: free text between
agents, which the Squire never interprets. If something must change what the
Squire *does*, it has to be a structured type — a coordination instruction
buried in a `custom` body will be ignored.

---

## The common envelope

Every message, regardless of type, has these fields. The first six are required.

| Field | Required | Meaning |
|---|---|---|
| `tmp_version` | yes | Always `"1.0"`. A consumer rejects a version it doesn't implement. |
| `id` | yes | Unique message id, shaped `msg-...`. |
| `type` | yes | The discriminator. Determines which typed fields follow. |
| `from` | yes | Sender: an agent id, or `"squire"`. |
| `channel` | yes | `direct` (inbox-addressed) or `bulletin` (domain board, never wakes anyone). |
| `created_tick` | yes | Tick the message was created. All ages/timeouts are in ticks, not clock time. |
| `to` | no | Recipient agent id. `null` for bulletins, broadcasts, and system notices. |
| `re` | no | Id of the message this replies to. |
| `supersedes` | no | Id of a message this replaces (e.g. a bulletin update). |
| `body` | no* | Human-readable text. The Squire ignores it for structured types. *Required for `custom`. |

---

## The message types

Authored-by tells you who legitimately sends each type. The Squire will not act
on a structured message sent by the wrong party (e.g. a `di_deny` from someone
who isn't the domain owner).

### Liveness

**`park`** — *authored by: agent.* Records a wait and ends the turn. This is how
an agent stops without stopping forever.
- `condition_kind` — one of `reply`, `di_request`, `cycle_break`,
  `domain_exhausted`, `claim_contested`.
- `condition_ref` — the specific id/path being awaited. Must be specific enough
  for the Squire to detect satisfaction; never a vague state.
- `parked_since_tick` — when the park was recorded.

**`wake`** — *authored by: squire.* The one recorded reason an agent is being
restarted.
- `wake_kind` — one of `inbox`, `reply_satisfied`, `abandoned`, `cycle_break`,
  `di_approved`, `di_vetoed`, `claim_freed`, `domain_exhausted`. Exactly one.
- `reason_ref` — the id the reason refers to, or `null`.
- `domain_change_paths` — optional. Paths in the agent's own domain that changed
  since it last looked, attached so it can reconcile before reacting.

### Domain intrusion

**`di_request`** — *authored by: agent.* Opens a request to write outside your
domain. Broadcast to all enabled agents.
- `request_id`, `target_domain`, `path_glob`.
- `purpose` — **required, non-empty, specific.** A vague purpose is grounds for
  rejection.
- `objection_deadline_tick` — after this tick, a standing non-owner approval
  resolves to Approved absent a veto.

**`di_approve`** — *authored by: any enabled agent except the requester.*
- `request_id`.

**`di_deny`** — *authored by: the target domain's owner only.* A veto; overrides
any number of approvals regardless of arrival order.
- `request_id`.
- `reason` — **required.**

### Claims

**`claim`** — *authored by: agent.* Leases a path. Separate gate from DI
permission — you need both to write outside your domain.
- `path`, `ttl_s`.
- The Squire enforces that `.trelane/**` and `.git/**` are never claimable. The
  schema does **not** enforce this (see "Division of labor" below).

### Visibility

**`bulletin`** — *authored by: agent. `channel` must be `bulletin`.* Announces
what you're working on. Pulled on demand; never wakes anyone.
- `scope` — the domain.
- `paths` — optional list of files you're in or intend to touch.

**`domain_change_notice`** — *authored by: squire.* Tells an agent its own
domain changed without its action, so it reconciles before reacting.
- `changed_paths` — non-empty.

**`split_proposal_notice`** — *authored by: squire.* Informs a domain owner a
Biplane pass proposed splitting its domain. Informational; doesn't block.
- `domain`, `proposal_ref`.

**`quiescence_notice`** — *authored by: squire, broadcast.* Records that the
whole swarm has zero ready work. Informational only — never triggers a wake or
an automatic Biplane pass.
- `tick`.

### Free-form

**`custom`** — *authored by: agent.* Free text to another agent. The Squire
never decodes the content.
- `body` — **required.**

---

## Division of labor: what the schema does and doesn't guarantee

The schema guarantees **shape**: that a message has the right fields of the right
types for its `type`, that required fields are present and non-empty, that
enums hold only known values. A consumer that validates against the schema can
trust the shape without further checking.

The schema deliberately does **not** enforce **policy**, because JSON Schema
can't express these cleanly enough for the Squire to depend on:

- **Forbidden paths (R11).** A `claim` on `.git/config` is *structurally valid
  TMP*. Refusing it is the Squire's job. This is proven and locked in by a test
  in the validation harness, so the boundary can't be accidentally "fixed" into
  a false sense of safety.
- **Authorization.** The schema can't know that only a domain owner may
  `di_deny`, or that a `di_approve` mustn't come from the requester. The Squire
  checks sender identity against the request.
- **Referential integrity.** The schema can't confirm a `condition_ref` points
  at a real message, or that a `re` threads to one that exists.

Shape is the schema's contract. Policy is the Squire's. Keeping them separate is
what lets the schema stay simple enough to be a reliable decoder and the Squire
stay the single place coordination rules live.

---

## A worked message

A frontend agent requesting to add an import in the combat domain:

```json
{
  "tmp_version": "1.0",
  "id": "msg-7f3a",
  "type": "di_request",
  "from": "agent-frontend",
  "to": null,
  "channel": "direct",
  "created_tick": 47,
  "body": "autoplay needs to read damage values",
  "request_id": "di-9",
  "target_domain": "combat",
  "path_glob": "src/enemy.rs",
  "purpose": "add `use crate::combat::Damage` to enemy.rs so the autoplay decider can read damage values",
  "objection_deadline_tick": 60
}
```

The Squire reads `type: di_request`, decodes the five typed fields, broadcasts
it, and parks the requester. The combat owner, on its next inbox drain, sees it
and may `di_deny` with a reason; anyone else may `di_approve`. The `body` is for
the humans and agents reading along; the Squire never needs it.
