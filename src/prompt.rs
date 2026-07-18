use crate::error::Result;
use crate::store;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

const BOOTSTRAP_TEMPLATE: &str = r#"# Trelane agent bootstrap

You are agent `[[AGENT_ID]]` in a multi-agent swarm working on the project at
`[[PROJECT_ROOT]]`. You were woken by the squire for this reason:

> [[WAKE_REASON]]

You cannot restart yourself. Your run is one bounded work slice: wake, act,
exit cleanly. The squire will wake you again when there is a reason to.
All coordination goes through the control tool (run from the project root):

    trelane <command> ...

## The three laws

1. **Never wait while running.** If you need something from another agent,
   send a message, `park` the blocked task, and either switch to other
   in-domain work or exit cleanly. A parked task is data, not a stuck process.
2. **Inbox first.** Before touching your own work, handle every message
   below. Answer questions (`send --type answer --re <id>`), respond to
   claim-requests (`claim-grant` or `claim-deny`), then `ack` each message.
3. **Stay in your domain.** You may read anything, but only write files
   matching your `writable` globs. Any file that is contested (overlaps
   another domain) or outside your domain requires a lease via
   `claim` — and outside your domain also an active, accepted delegation
   from the owner. A `claim-grant` alone is not sufficient; you need both
   the active delegation and a normal path lease.

## Your domain

```json
[[DOMAIN_JSON]]
```

## Unprocessed inbox

[[INBOX_SUMMARY]]

## Bulletin: your domain's working-set board (R12)

[[BULLETIN_SUMMARY]]

## Your parked tasks

[[PARKED_SUMMARY]]

Any task marked `DEPENDENCY SATISFIED` should be resumed now: do the work,
then `unpark <task>`.

## Domain-change check (R14)

[[DOMAIN_CHANGE]]

## Out of ready work? Adjacent domains (R21/R22)

[[ADJACENCY_SUMMARY]]

## Split proposals against your domain (R29)

[[SPLIT_PROPOSAL_SUMMARY]]

## Cross-domain assistance

Inbox, accepted assignments, and ready owned work come first.

If you have no actionable owned work, run:

    trelane work list --assistable --agent [[AGENT_ID]]

Inspect candidate work read-only. A discovery run may produce at most ONE
concrete scoped offer:

    trelane help offer --from [[AGENT_ID]] --to <owner> --task <task-id> \
        --path <path> --plan "..." --deliverable "..."

Reconnaissance and offers do not grant write authority. Do not edit, claim,
or otherwise mutate files in another domain until the owner accepts the offer
and Trelane records an active delegation.

Delegated writes require both the active delegation and a normal path lease.
Either one alone is insufficient.

Submit delegated implementation work through:

    trelane work submit <task-id> --by [[AGENT_ID]] \
        --delegation <delegation-id> --commit <sha> \
        --summary "..." --tests "..."

Do not mark a helper submission done yourself. The owner or designated
reviewer must run:

    trelane work review <task-id> --by <owner-or-reviewer> \
        --delegation <delegation-id> --accept|--request-changes|--reject

## Command crib sheet

    trelane inbox [[AGENT_ID]] --json          # full message bodies
    trelane ack [[AGENT_ID]] <msg-id>          # after handling, not before
    trelane send --from [[AGENT_ID]] --to <agent> --type question \
        --subject "..." --body "..."            # prints the msg id
    trelane park [[AGENT_ID]] --wait-reply <msg-id> --waiting-on <agent> \
        --resume-hint "what to do when the answer arrives"
    trelane claim [[AGENT_ID]] <path> [--delegation <delegation-id>]
    trelane release [[AGENT_ID]] <path>
    trelane unpark <task-id>
    trelane work list [--assistable] [--agent <helper>]
    trelane work show <task-id>
    trelane help offer --from [[AGENT_ID]] --to <owner> --task <id> ...
    trelane work submit <task-id> --by [[AGENT_ID]] --delegation <id> ...
    trelane work review <task-id> --by <reviewer> --delegation <id> ...
    trelane audit [[AGENT_ID]]                 # run before you exit
    trelane done [[AGENT_ID]]                  # your very last command

## Exit checklist (mandatory)

1. `release` every lease you hold, unless a parked task explicitly needs it.
2. `park` anything blocked, with a resume hint your future self will thank
   you for — you will wake with no memory of this run beyond what is on disk.
3. Write durable notes to `.trelane/agents/[[AGENT_ID]]/state.json` if needed
   (this file is yours; everything else under .trelane is trelane-only).
4. `audit [[AGENT_ID]]` — if it fails, revert the out-of-domain edits or
   hand them off before exiting.
5. `done [[AGENT_ID]]`, then stop. Do not linger, poll, sleep, or wait.

If your wake reason says **deadlock**, you are the designated breaker:
unpark the cycled task, proceed with a clearly documented assumption, and
send your counterpart an `info` message whose subject starts with
`deadlock` stating the assumption you made.
"#;

pub fn bootstrap_template() -> &'static str {
    BOOTSTRAP_TEMPLATE
}

/// The full protocol ruleset, embedded at compile time from the canonical
/// sources in `src/rules/for_agents/`. Embedded (not copied at runtime) so
/// the prompt is self-contained and the rules agents receive can never drift
/// out of sync with the binary they are running (GAP-02).
const PROTOCOL_DOC: &str = include_str!("rules/for_agents/TRELANE-PROTOCOL.md");
const TMP_DOC: &str = include_str!("rules/for_agents/TRELANE-MESSAGE-PROTOCOL.md");

/// The complete agent-facing document: the bootstrap template followed by the
/// full Trelane Protocol rules and the TMP v1.0 reference. This is what
/// agents actually receive at wake (via [`compose_prompt`]) and what
/// `cmd_init` writes to `.trelane/prompts/bootstrap.md` as a readable
/// reference copy.
pub fn full_bootstrap() -> String {
    format!("{BOOTSTRAP_TEMPLATE}\n\n---\n\n{PROTOCOL_DOC}\n\n---\n\n{TMP_DOC}")
}

pub fn compose_prompt(
    conn: &Connection,
    root: &Path,
    agent: &str,
    reason: &str,
    domain_change_paths: &[String],
) -> Result<String> {
    let domain = store::get_domain(conn, agent)?
        .ok_or_else(|| crate::error::TrelaneError::msg(format!("agent '{agent}' not found")))?;
    let domain_json = serde_json::to_string_pretty(&domain)?;

    let inbox = store::get_unprocessed_messages(conn, agent)?;
    let inbox_summary = if inbox.is_empty() {
        "(empty)".to_string()
    } else {
        inbox
            .iter()
            .map(|m| format!("- {} [{}] from {}: {}", m.id, m.msg_type, m.from, m.subject))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let parked = store::list_parked_tasks_for_agent(conn, agent)?;
    let parked_summary = if parked.is_empty() {
        "(none)".to_string()
    } else {
        parked
            .iter()
            .map(|e| {
                let satisfied = park_satisfied(conn, e).unwrap_or(false);
                let tag = if satisfied {
                    "  [DEPENDENCY SATISFIED -- resume this]"
                } else {
                    ""
                };
                format!(
                    "- {}: waiting on {} | resume hint: {}{}",
                    e.task,
                    wait_display(e),
                    if e.resume_hint.is_empty() {
                        "(none)"
                    } else {
                        &e.resume_hint
                    },
                    tag
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // 4B: the wake-time inbox drain widens to also scan bulletin entries
    // scoped to the agent's own domain -- what others have announced they are
    // (or were recently) working in. Pulled, never pushed (R13).
    let bulletin = store::get_bulletin(conn, agent, false).unwrap_or_default();
    let bulletin_summary = if bulletin.is_empty() {
        "(empty)".to_string()
    } else {
        bulletin
            .iter()
            .take(10)
            .map(|m| {
                let files = if m.paths.is_empty() {
                    "(whole domain)".to_string()
                } else {
                    m.paths.join(", ")
                };
                format!("- {} [{}] {}: {}", m.id, m.from, files, m.body.lines().next().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    // 4E: a non-empty domain-change notice gets checked, not assumed (R14).
    // The full reconciliation procedure is §5 of the Protocol below.
    let domain_change = if domain_change_paths.is_empty() {
        "(none -- your domain matches the snapshot from your last run.)".to_string()
    } else {
        format!(
            "These files in YOUR domain changed while you were away, and not by you:\n{}\n\n\
             Per §5 of the Trelane Protocol (below): check `trelane history --include-archived` \
             and `trelane bulletin list --domain <your-domain> --include-archived` for a resolved \
             intrusion or an announced working-file overlap that explains them.\n\
             Explained -> proceed as normal.\n\
             Unexplained -> do NOT silently revert and do NOT silently continue: post a message \
             naming the specific unexplained paths, then keep working. Never park on this -- it \
             is a flag, not a blocking wait.",
            domain_change_paths
                .iter()
                .map(|p| format!("- {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    // 5B/R21: when the agent's domain is exhausted, its ranked adjacency
    // list is attached -- detection is mechanical, acting is the agent's
    // call. R29: unreviewed split proposals surface the same way.
    let adjacency_summary = crate::refine::exhaustion_adjacency_summary(conn, agent)
        .unwrap_or_else(|| "(no adjacency list yet -- none computed, or you still have ready work.)".to_string());
    let split_proposal_summary = crate::refine::pending_split_summary(conn, agent)
        .unwrap_or_else(|| "(none)".to_string());

    let mut tpl = full_bootstrap();
    let subs: HashMap<&str, String> = [
        ("[[AGENT_ID]]", agent.to_string()),
        ("[[PROJECT_ROOT]]", root.display().to_string()),
        ("[[WAKE_REASON]]", reason.to_string()),
        ("[[DOMAIN_JSON]]", domain_json),
        ("[[INBOX_SUMMARY]]", inbox_summary),
        ("[[BULLETIN_SUMMARY]]", bulletin_summary),
        ("[[PARKED_SUMMARY]]", parked_summary),
        ("[[DOMAIN_CHANGE]]", domain_change),
        ("[[ADJACENCY_SUMMARY]]", adjacency_summary),
        ("[[SPLIT_PROPOSAL_SUMMARY]]", split_proposal_summary),
    ]
    .into_iter()
    .collect();
    for (k, v) in &subs {
        tpl = tpl.replace(k, v);
    }
    Ok(tpl)
}

fn wait_display(e: &crate::models::ParkedTask) -> String {
    match e.wait_type.as_str() {
        "reply" => format!("reply to {}", e.wait_re.as_deref().unwrap_or("?")),
        "claim" => format!("claim on {}", e.wait_path.as_deref().unwrap_or("?")),
        "claim-contested" => format!(
            "contested claim on {} (R26)",
            e.wait_path.as_deref().unwrap_or("?")
        ),
        "di_request" => format!("DI request {}", e.wait_re.as_deref().unwrap_or("?")),
        _ => e.wait_type.clone(),
    }
}

pub fn park_satisfied(conn: &Connection, entry: &crate::models::ParkedTask) -> Result<bool> {
    match entry.wait_type.as_str() {
        "reply" => {
            let re = match &entry.wait_re {
                Some(r) => r,
                None => return Ok(false),
            };
            let msgs = store::get_unprocessed_messages(conn, &entry.agent)?;
            if msgs.iter().any(|m| m.re.as_deref() == Some(re)) {
                return Ok(true);
            }
            // R27: archived or already-processed replies still satisfy the
            // park -- retention must never outpace resolution.
            store::any_reply_exists(conn, &entry.agent, re)
        }
        "claim" | "claim-contested" => {
            // claim-contested (R26) resolves exactly like any other lease
            // wait: the agent is woken when the lease frees.
            let path = match &entry.wait_path {
                Some(p) => p,
                None => return Ok(false),
            };
            match store::get_claim(conn, path)? {
                None => Ok(true),
                Some(lease) => {
                    let now = chrono::Utc::now().timestamp() as f64;
                    Ok(lease.expires_at < now || lease.holder == entry.agent)
                }
            }
        }
        "di_request" => {
            // Satisfied once the request is resolved in any direction --
            // the wake reason (approved/vetoed/expired) carries the outcome.
            let id = match &entry.wait_re {
                Some(r) => r,
                None => return Ok(false),
            };
            match crate::di::get_request(conn, id)? {
                Some(req) => Ok(req.status != crate::di::STATUS_PENDING),
                // A vanished request can never resolve; treat as satisfied so
                // the abandon path can wake the agent rather than strand it.
                None => Ok(true),
            }
        }
        _ => Ok(false),
    }
}

/// Returns true if the park's `waiting_on` agent is provably gone:
/// either never registered, or registered but disabled in session_agents.
pub fn park_target_gone(conn: &Connection, entry: &crate::models::ParkedTask) -> Result<bool> {
    if !store::agent_exists(conn, &entry.waiting_on)? {
        return Ok(true);
    }
    match store::session_agent_enabled(conn, &entry.waiting_on)? {
        None => Ok(false),
        Some(enabled) => Ok(!enabled),
    }
}

/// Pure function: returns true if the park's age exceeds the timeout.
/// No DB or clock access -- takes `now_iso` as a parameter for testability.
pub fn park_age_exceeds(entry: &crate::models::ParkedTask, timeout_s: u64, now_iso: &str) -> bool {
    let created = chrono::DateTime::parse_from_rfc3339(&entry.created_at);
    let now = chrono::DateTime::parse_from_rfc3339(now_iso);
    match (created, now) {
        (Ok(c), Ok(n)) => {
            let age = n
                .with_timezone(&chrono::Utc)
                .signed_duration_since(c.with_timezone(&chrono::Utc));
            age.num_seconds() > timeout_s as i64
        }
        _ => false,
    }
}

/// Returns true if the park should be treated as abandoned (not merely
/// unsatisfied).  An abandoned park means the squire should wake the
/// waiting agent with an abandonment reason rather than leaving it
/// parked forever.
pub fn park_abandoned(
    conn: &Connection,
    entry: &crate::models::ParkedTask,
    reply_timeout_s: Option<u64>,
) -> Result<bool> {
    if park_target_gone(conn, entry)? {
        return Ok(true);
    }
    if let Some(timeout) = reply_timeout_s {
        let now = crate::crypto::now_iso();
        if park_age_exceeds(entry, timeout, &now) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn write_prompt_file(
    trelane_dir: &Path,
    agent: &str,
    prompt: &str,
) -> Result<std::path::PathBuf> {
    let dir = trelane_dir.join("agents").join(agent);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(".prompt.md");
    std::fs::write(&path, prompt)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ParkedTask;

    fn make_parked(created_at: &str, waiting_on: &str) -> ParkedTask {
        ParkedTask {
            task: "task-1".to_string(),
            agent: "alpha".to_string(),
            wait_type: "reply".to_string(),
            wait_re: Some("msg-1".to_string()),
            wait_path: None,
            waiting_on: waiting_on.to_string(),
            resume_hint: String::new(),
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn park_age_exceeds_true_when_old() {
        let entry = make_parked("2026-01-01T00:00:00Z", "beta");
        assert!(park_age_exceeds(&entry, 60, "2026-01-01T00:05:00Z"));
    }

    #[test]
    fn park_age_exceeds_false_when_recent() {
        let entry = make_parked("2026-01-01T00:00:00Z", "beta");
        assert!(!park_age_exceeds(&entry, 600, "2026-01-01T00:05:00Z"));
    }

    #[test]
    fn park_age_exceeds_false_on_bad_timestamp() {
        let entry = make_parked("garbage", "beta");
        assert!(!park_age_exceeds(&entry, 1, "2026-01-01T00:00:00Z"));
    }

    // ------------------------------------------------------------- C4 tests

    #[test]
    fn prompt_contains_assistable_listing_command() {
        assert!(bootstrap_template().contains("trelane work list --assistable --agent"));
    }

    #[test]
    fn prompt_contains_one_offer_limit() {
        assert!(bootstrap_template().contains("at most ONE"));
    }

    #[test]
    fn prompt_contains_help_offer_command() {
        assert!(bootstrap_template().contains("trelane help offer"));
    }

    #[test]
    fn prompt_prohibits_writes_before_delegation() {
        let tpl = bootstrap_template();
        assert!(tpl.contains("Do not edit") && tpl.contains("until the owner accepts the offer"));
    }

    #[test]
    fn prompt_requires_both_delegation_and_lease() {
        let tpl = bootstrap_template();
        assert!(tpl.contains("both the active delegation and a normal path lease"));
    }

    #[test]
    fn prompt_contains_work_submit_command() {
        assert!(bootstrap_template().contains("trelane work submit"));
    }

    #[test]
    fn prompt_contains_work_review_command() {
        assert!(bootstrap_template().contains("trelane work review"));
    }

    #[test]
    fn prompt_mentions_designated_reviewer() {
        assert!(bootstrap_template().contains("designated"));
    }

    #[test]
    fn prompt_no_longer_claims_grant_alone_suffices() {
        let tpl = bootstrap_template();
        assert!(tpl.contains("A `claim-grant` alone is not sufficient"));
    }

    #[test]
    fn prompt_uses_squire_not_prop() {
        let tpl = bootstrap_template();
        assert!(tpl.contains("squire"));
        assert!(!tpl.contains("the prop"));
    }

    // ------------------------------------------------------- GAP-02 tests

    #[test]
    fn full_bootstrap_embeds_protocol_doc() {
        let full = full_bootstrap();
        // TRELANE-PROTOCOL.md headline sections must reach the agent.
        assert!(full.contains("# The Trelane Protocol"));
        assert!(full.contains("## 4. Domain Intrusion"));
        assert!(full.contains("## 1. Inbox before anything else"));
    }

    #[test]
    fn full_bootstrap_embeds_tmp_reference() {
        let full = full_bootstrap();
        assert!(full.contains("Trelane Message Protocol (TMP) v1.0"));
        assert!(full.contains("di_request"));
        assert!(full.contains("quiescence_notice"));
    }

    #[test]
    fn full_bootstrap_starts_with_bootstrap_template() {
        assert!(full_bootstrap().starts_with(&BOOTSTRAP_TEMPLATE[..100]));
    }
}
