//! Slice 4D: retention (R15, R27).
//!
//! Staleness demotes, it doesn't destroy. One timestamp, `last_touched_at`,
//! drives everything: messages untouched longer than `retention.hot_days`
//! move to the archived tier (excluded from default queries, fully readable
//! under `--include-archived`). Real deletion only ever happens when
//! `retention.purge_days` is explicitly configured -- never by default.
//!
//! The sweep runs at most once per real day, called as a cheap first step of
//! `squire::tick` (one restarter, R3 -- not a second daemon), and is
//! best-effort: a sweep failure never fails the tick it ran inside (R16).
//!
//! R27: a message that is the live target of a still-open park is exempt
//! from archival until the park is consumed -- retention never outpaces
//! resolution.

use crate::Context;
use crate::error::Result;
use rusqlite::params;

/// What one sweep did. `ran == false` means the once-a-day gate short-
/// circuited (nothing was touched).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub ran: bool,
    pub archived: usize,
    pub purged: usize,
    pub dormant: bool,
}

fn today(iso: &str) -> &str {
    // RFC3339 timestamps start with YYYY-MM-DD.
    iso.get(..10).unwrap_or(iso)
}

fn days_ago_iso(now_iso: &str, days: u64) -> String {
    let now = chrono::DateTime::parse_from_rfc3339(now_iso)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    (now - chrono::Duration::days(days as i64))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Run the retention sweep. Gated to once per UTC day unless `force` is set
/// (the `trelane retention sweep --now` path).
pub fn sweep(ctx: &Context, force: bool) -> Result<RetentionOutcome> {
    let now = crate::crypto::now_iso();

    let last_swept: Option<String> = ctx
        .conn
        .query_row(
            "SELECT last_swept_at FROM project_state WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap_or(None);
    if !force && last_swept.as_deref().map(today) == Some(today(&now)) {
        return Ok(RetentionOutcome {
            ran: false,
            archived: 0,
            purged: 0,
            dormant: false,
        });
    }

    // Archive stale hot messages. R27: exclude any message still referenced
    // by an open parked task -- an answer someone is waiting on is not stale.
    let hot_cutoff = days_ago_iso(&now, ctx.config.retention.hot_days);
    let archived = ctx.conn.execute(
        "UPDATE messages SET archived_at = ?1
         WHERE archived_at IS NULL AND last_touched_at < ?2
           AND id NOT IN (SELECT wait_re FROM parked_tasks WHERE wait_re IS NOT NULL)",
        params![now, hot_cutoff],
    )?;

    // 4B: an active bulletin entry archives when the posting agent has gone
    // idle (its working-set announcement is no longer current).
    let mut bulletin_archived = 0;
    {
        let mut stmt = ctx.conn.prepare(
            "SELECT DISTINCT from_agent FROM messages
             WHERE channel = 'bulletin' AND archived_at IS NULL",
        )?;
        let posters: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for poster in posters {
            let idle = crate::squire::agent_activity_status(ctx, &poster)
                .map(|s| s.state == crate::models::AgentActivityState::Idle)
                .unwrap_or(false);
            if idle {
                bulletin_archived += ctx.conn.execute(
                    "UPDATE messages SET archived_at = ?1
                     WHERE channel = 'bulletin' AND from_agent = ?2 AND archived_at IS NULL",
                    params![now, poster],
                )?;
            }
        }
    }

    // Real deletion: only when explicitly configured (R15).
    let mut purged = 0;
    if let Some(purge_days) = ctx.config.retention.purge_days {
        let purge_cutoff = days_ago_iso(&now, purge_days);
        purged = ctx.conn.execute(
            "DELETE FROM messages WHERE archived_at IS NOT NULL AND last_touched_at < ?1",
            params![purge_cutoff],
        )?;
    }

    // Dormancy: the whole project has had zero agent activity for
    // `retention.dormant_days`. A marker only; no data is touched.
    let latest_activity: Option<String> = ctx
        .conn
        .query_row("SELECT MAX(created_at) FROM messages", [], |r| r.get(0))
        .unwrap_or(None);
    let dormant = match latest_activity {
        Some(latest) => latest < days_ago_iso(&now, ctx.config.retention.dormant_days),
        // No messages at all: too cheap to flag a brand-new project dormant.
        None => false,
    };

    ctx.conn.execute(
        "UPDATE project_state SET last_swept_at = ?1, dormant = ?2 WHERE id = 1",
        params![now, dormant as i64],
    )?;

    Ok(RetentionOutcome {
        ran: true,
        archived: archived + bulletin_archived,
        purged,
        dormant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Config, Message};

    fn ctx() -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".trelane")).unwrap();
        let conn = crate::db::open_in_memory().unwrap();
        (
            dir,
            Context {
                root,
                conn,
                config: Config::default(),
            },
        )
    }

    fn msg(id: &str, created_at: &str) -> Message {
        Message::new(
            id.to_string(),
            "alpha".to_string(),
            "beta".to_string(),
            "question".to_string(),
            "normal".to_string(),
            format!("subj {id}"),
            String::new(),
            None,
            None,
            vec![],
            created_at.to_string(),
        )
    }

    fn is_archived(ctx: &Context, id: &str) -> bool {
        ctx.conn
            .query_row(
                "SELECT archived_at IS NOT NULL FROM messages WHERE id = ?1",
                params![id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
    }

    #[test]
    fn sweep_archives_old_and_keeps_fresh() {
        let (_d, mut c) = ctx();
        c.config.retention.hot_days = 30;
        let now = crate::crypto::now_iso();
        crate::store::insert_message(&c.conn, &msg("old", "2026-01-01T00:00:00Z")).unwrap();
        crate::store::insert_message(&c.conn, &msg("fresh", &now)).unwrap();
        let out = sweep(&c, true).unwrap();
        assert!(out.ran);
        assert_eq!(out.archived, 1);
        assert!(is_archived(&c, "old"));
        assert!(!is_archived(&c, "fresh"));
    }

    #[test]
    fn second_sweep_same_day_is_noop() {
        let (_d, c) = ctx();
        crate::store::insert_message(&c.conn, &msg("m1", "2026-01-01T00:00:00Z")).unwrap();
        assert!(sweep(&c, false).unwrap().ran);
        assert!(!sweep(&c, false).unwrap().ran);
    }

    #[test]
    fn open_park_exempts_message_from_archival() {
        let (_d, c) = ctx();
        crate::store::insert_message(&c.conn, &msg("waiting-on", "2026-01-01T00:00:00Z")).unwrap();
        crate::store::insert_parked_task(
            &c.conn,
            &crate::models::ParkedTask {
                task: "t1".to_string(),
                agent: "beta".to_string(),
                wait_type: "reply".to_string(),
                wait_re: Some("waiting-on".to_string()),
                wait_path: None,
                waiting_on: "alpha".to_string(),
                resume_hint: String::new(),
                created_at: "2026-01-02T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let out = sweep(&c, true).unwrap();
        assert_eq!(out.archived, 0, "R27: live park target must stay hot");
        assert!(!is_archived(&c, "waiting-on"));
    }

    #[test]
    fn project_with_fresh_message_is_never_dormant() {
        let (_d, c) = ctx();
        let now = crate::crypto::now_iso();
        crate::store::insert_message(&c.conn, &msg("m1", &now)).unwrap();
        assert!(!sweep(&c, true).unwrap().dormant);
    }

    #[test]
    fn stale_project_is_flagged_dormant() {
        let (_d, mut c) = ctx();
        c.config.retention.dormant_days = 90;
        crate::store::insert_message(&c.conn, &msg("m1", "2026-01-01T00:00:00Z")).unwrap();
        assert!(sweep(&c, true).unwrap().dormant);
        let dormant: bool = c
            .conn
            .query_row("SELECT dormant FROM project_state WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(dormant);
    }

    #[test]
    fn archived_message_round_trips_through_history() {
        let (_d, c) = ctx();
        crate::store::insert_message(&c.conn, &msg("m1", "2026-01-01T00:00:00Z")).unwrap();
        sweep(&c, true).unwrap();
        assert!(crate::store::get_history(&c.conn, None, false).unwrap().is_empty());
        let with_archived = crate::store::get_history(&c.conn, None, true).unwrap();
        assert_eq!(with_archived.len(), 1);
        assert_eq!(with_archived[0].id, "m1");
    }

    #[test]
    fn purge_only_when_configured() {
        let (_d, mut c) = ctx();
        crate::store::insert_message(&c.conn, &msg("m1", "2026-01-01T00:00:00Z")).unwrap();
        sweep(&c, true).unwrap();
        assert!(crate::store::get_message(&c.conn, "m1").unwrap().is_some());

        c.config.retention.purge_days = Some(1);
        // Force a second sweep on the "next day" by clearing the gate.
        c.conn
            .execute("UPDATE project_state SET last_swept_at = NULL WHERE id = 1", [])
            .unwrap();
        let out = sweep(&c, true).unwrap();
        assert_eq!(out.purged, 1);
        assert!(crate::store::get_message(&c.conn, "m1").unwrap().is_none());
    }
}
