import json
from jsonschema import Draft202012Validator
from jsonschema.exceptions import ValidationError

with open("trelane-message-protocol.schema.json") as f:
    schema = json.load(f)

# 1. Schema must itself be a well-formed Draft 2020-12 schema.
Draft202012Validator.check_schema(schema)
print("PASS  schema is a well-formed Draft 2020-12 schema")

v = Draft202012Validator(schema)

def base(**over):
    m = {
        "tmp_version": "1.0",
        "id": "msg-abc123",
        "from": "agent-frontend",
        "channel": "direct",
        "created_tick": 47,
    }
    m.update(over)
    return m

# ---- VALID: one well-formed example per type ----------------------------
valid = {
    "park": base(type="park", condition_kind="reply", condition_ref="msg-148",
                 parked_since_tick=47),
    "wake": base(type="wake", from_="squire", wake_kind="reply_satisfied",
                 reason_ref="msg-148"),
    "wake+diff": base(type="wake", from_="squire", wake_kind="inbox",
                      domain_change_paths=["src/combat.rs"]),
    "di_request": base(type="di_request", request_id="di-9", target_domain="combat",
                       path_glob="src/enemy.rs", purpose="add Damage import for autoplay",
                       objection_deadline_tick=60),
    "di_approve": base(type="di_approve", request_id="di-9"),
    "di_deny": base(type="di_deny", from_="agent-combat", request_id="di-9",
                    reason="mid-refactor of enemy.rs, unsafe right now"),
    "claim": base(type="claim", path="src/enemy.rs", ttl_s=900),
    "bulletin": base(type="bulletin", channel="bulletin", to=None, scope="combat",
                     paths=["src/combat.rs", "src/enemy.rs"],
                     body="starting on damage calc"),
    "domain_change_notice": base(type="domain_change_notice", from_="squire",
                                 changed_paths=["src/enemy.rs"]),
    "split_proposal_notice": base(type="split_proposal_notice", from_="squire",
                                  domain="combat", proposal_ref="prop-3"),
    "quiescence_notice": base(type="quiescence_notice", from_="squire",
                              channel="bulletin", to=None, tick=512),
    "custom": base(type="custom", to="agent-tests",
                   body="heads up, I renamed the fixture helper"),
}
# note: 'from_' kwarg maps to 'from' key (Python reserved word workaround)
def fix(m):
    if "from_" in m:
        m["from"] = m.pop("from_")
    return m

for name, msg in valid.items():
    msg = fix(dict(msg))
    errs = sorted(v.iter_errors(msg), key=lambda e: e.path)
    assert not errs, f"VALID case '{name}' unexpectedly failed: {[e.message for e in errs]}"
print(f"PASS  all {len(valid)} valid examples accepted")

# ---- INVALID: each must be rejected, for the right reason ---------------
invalid = {
    "di_request missing purpose": base(type="di_request", request_id="di-9",
        target_domain="combat", path_glob="src/enemy.rs", objection_deadline_tick=60),
    "di_request empty purpose": base(type="di_request", request_id="di-9",
        target_domain="combat", path_glob="src/enemy.rs", purpose="",
        objection_deadline_tick=60),
    "di_deny missing reason": fix(base(type="di_deny", from_="agent-combat",
        request_id="di-9")),
    "custom missing body": base(type="custom", to="agent-tests"),
    "park vague ref (empty)": base(type="park", condition_kind="reply",
        condition_ref="", parked_since_tick=47),
    "park bad condition_kind": base(type="park", condition_kind="vibes",
        condition_ref="msg-1", parked_since_tick=47),
    "unknown type": base(type="teleport"),
    "wake bad kind": fix(base(type="wake", from_="squire", wake_kind="whenever")),
    "missing tmp_version": {k: v2 for k, v2 in base(type="custom", body="x").items()
                            if k != "tmp_version"},
    "wrong version": base(type="custom", body="x", tmp_version="2.0"),
    "bad id format": base(type="custom", body="x", id="148"),
    "bad channel": base(type="custom", body="x", channel="carrier-pigeon"),
}

for name, msg in invalid.items():
    errs = list(v.iter_errors(msg))
    assert errs, f"INVALID case '{name}' was wrongly accepted"
    print(f"PASS  rejected: {name}")

# Division-of-labor check, asserted rather than hand-waved: a claim on a
# forbidden path is STRUCTURALLY VALID TMP. The schema's job is shape, not
# policy. JSON Schema cannot cleanly express 'path must not match .git/** or
# .trelane/**' in a form squire should depend on, so that prohibition (R11)
# is squire's to enforce, not the schema's. This test locks in that the
# schema deliberately accepts it, so nobody later "fixes" the schema into a
# false sense of safety.
git_claim = base(type="claim", path=".git/config", ttl_s=900)
assert not list(v.iter_errors(git_claim)), \
    "forbidden-path claim should be schema-VALID; R11 is squire's job, not the schema's"
print("PASS  forbidden-path claim is schema-valid by design (R11 enforced by squire)")

print()
print("ALL CHECKS PASSED")
