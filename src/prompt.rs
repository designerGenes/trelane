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
   `claim` — and outside your domain also a `claim-grant` from the owner.

## Your domain

```json
[[DOMAIN_JSON]]
```

## Unprocessed inbox

[[INBOX_SUMMARY]]

## Your parked tasks

[[PARKED_SUMMARY]]

Any task marked `DEPENDENCY SATISFIED` should be resumed now: do the work,
then `unpark <task>`.

## Command crib sheet

    trelane inbox [[AGENT_ID]] --json          # full message bodies
    trelane ack [[AGENT_ID]] <msg-id>          # after handling, not before
    trelane send --from [[AGENT_ID]] --to <agent> --type question \
        --subject "..." --body "..."            # prints the msg id
    trelane park [[AGENT_ID]] --wait-reply <msg-id> --waiting-on <agent> \
        --resume-hint "what to do when the answer arrives"
    trelane claim [[AGENT_ID]] <path> [--grant <claim-grant-msg-id>]
    trelane release [[AGENT_ID]] <path>
    trelane unpark <task-id>
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

pub fn compose_prompt(conn: &Connection, root: &Path, agent: &str, reason: &str) -> Result<String> {
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

    let mut tpl = BOOTSTRAP_TEMPLATE.to_string();
    let subs: HashMap<&str, String> = [
        ("[[AGENT_ID]]", agent.to_string()),
        ("[[PROJECT_ROOT]]", root.display().to_string()),
        ("[[WAKE_REASON]]", reason.to_string()),
        ("[[DOMAIN_JSON]]", domain_json),
        ("[[INBOX_SUMMARY]]", inbox_summary),
        ("[[PARKED_SUMMARY]]", parked_summary),
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
            Ok(msgs.iter().any(|m| m.re.as_deref() == Some(re)))
        }
        "claim" => {
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
}
