use crate::error::Result;
use rusqlite::Connection;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS agents (
    id              TEXT PRIMARY KEY,
    description     TEXT NOT NULL DEFAULT '',
    writable_json   TEXT NOT NULL DEFAULT '[]',
    launcher_agent  TEXT,
    forbidden_json  TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    from_agent      TEXT NOT NULL,
    to_agent        TEXT NOT NULL,
    msg_type        TEXT NOT NULL,
    urgency         TEXT NOT NULL DEFAULT 'normal',
    subject         TEXT NOT NULL,
    body            TEXT NOT NULL DEFAULT '',
    re              TEXT,
    task            TEXT,
    paths_json      TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL,
    schema_version  INTEGER NOT NULL DEFAULT 1,
    sig             TEXT NOT NULL,
    processed_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_messages_to_unprocessed
    ON messages(to_agent, processed_at);
CREATE INDEX IF NOT EXISTS idx_messages_re ON messages(re);

CREATE TABLE IF NOT EXISTS parked_tasks (
    task            TEXT PRIMARY KEY,
    agent           TEXT NOT NULL,
    wait_type       TEXT NOT NULL,
    wait_re         TEXT,
    wait_path       TEXT,
    waiting_on      TEXT NOT NULL,
    resume_hint     TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_parked_agent ON parked_tasks(agent);
CREATE INDEX IF NOT EXISTS idx_parked_waiting_on ON parked_tasks(waiting_on);

CREATE TABLE IF NOT EXISTS claims (
    path            TEXT PRIMARY KEY,
    holder          TEXT NOT NULL,
    task            TEXT,
    grant           TEXT,
    acquired_at     TEXT NOT NULL,
    expires_at      REAL NOT NULL,
    expires_human   TEXT NOT NULL,
    contested       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS violations (
    id              TEXT PRIMARY KEY,
    agent           TEXT NOT NULL,
    paths_json      TEXT NOT NULL,
    at              TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_baselines (
    agent           TEXT PRIMARY KEY,
    hashes_json     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS running_locks (
    agent           TEXT PRIMARY KEY,
    pid             INTEGER NOT NULL,
    started_at      TEXT NOT NULL,
    reason          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_agents (
    name            TEXT PRIMARY KEY,
    enabled         INTEGER NOT NULL,
    source          TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS launch_targets (
    agent           TEXT PRIMARY KEY,
    adapter         TEXT NOT NULL,
    target          TEXT NOT NULL,
    command         TEXT NOT NULL,
    tmux_target     TEXT,
    updated_at      TEXT NOT NULL
);
"#;

/// C1 durable work ledger: tasks and the assignment / delegation / review
/// tables that record cross-domain assistance. Additive and idempotent
/// (CREATE ... IF NOT EXISTS), so it is safe to run on both fresh installs
/// and upgrades.
const SCHEMA_C1: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id                   TEXT PRIMARY KEY,
    owner_agent          TEXT NOT NULL,
    domain               TEXT NOT NULL DEFAULT '',
    parent_task          TEXT,
    subject              TEXT NOT NULL,
    body                 TEXT NOT NULL DEFAULT '',
    state                TEXT NOT NULL DEFAULT 'draft',
    priority             TEXT NOT NULL DEFAULT 'normal',
    assist_policy        TEXT NOT NULL DEFAULT 'open',
    desired_parallelism  INTEGER NOT NULL DEFAULT 1,
    path_scope_json      TEXT NOT NULL DEFAULT '[]',
    acceptance_json      TEXT NOT NULL DEFAULT '[]',
    blocked_by_json      TEXT NOT NULL DEFAULT '[]',
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_owner_state ON tasks(owner_agent, state);
CREATE INDEX IF NOT EXISTS idx_tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_task);

CREATE TABLE IF NOT EXISTS task_assignments (
    task_id       TEXT NOT NULL,
    agent         TEXT NOT NULL,
    role          TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'active',
    offer_id      TEXT,
    delegation_id TEXT,
    started_at    TEXT,
    completed_at  TEXT,
    PRIMARY KEY (task_id, agent, role)
);
CREATE INDEX IF NOT EXISTS idx_assignments_task ON task_assignments(task_id);
CREATE INDEX IF NOT EXISTS idx_assignments_agent ON task_assignments(agent);

CREATE TABLE IF NOT EXISTS delegations (
    id              TEXT PRIMARY KEY,
    task_id         TEXT NOT NULL,
    owner_agent     TEXT NOT NULL,
    helper_agent    TEXT NOT NULL,
    scope_json      TEXT NOT NULL DEFAULT '[]',
    allowed_ops_json TEXT NOT NULL DEFAULT '[]',
    constraints_json TEXT NOT NULL DEFAULT '{}',
    base_revision   TEXT,
    grant_message   TEXT NOT NULL DEFAULT '',
    issued_at       TEXT NOT NULL,
    expires_at      TEXT,
    status          TEXT NOT NULL DEFAULT 'offered'
);
CREATE INDEX IF NOT EXISTS idx_delegations_task ON delegations(task_id);
CREATE INDEX IF NOT EXISTS idx_delegations_helper_status ON delegations(helper_agent, status);

CREATE TABLE IF NOT EXISTS task_reviews (
    id             TEXT PRIMARY KEY,
    task_id        TEXT NOT NULL,
    delegation_id  TEXT,
    reviewer_agent TEXT NOT NULL,
    submission_ref TEXT NOT NULL DEFAULT '',
    decision       TEXT NOT NULL,
    notes          TEXT NOT NULL DEFAULT '',
    created_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reviews_task ON task_reviews(task_id);
"#;

/// C2 assistance protocol. The links are additive so existing C1 ledgers are
/// safe to upgrade in place.
const SCHEMA_C2: &str = r#"
ALTER TABLE claims ADD COLUMN delegation_id TEXT;
ALTER TABLE delegations ADD COLUMN offer_message TEXT NOT NULL DEFAULT '';
CREATE INDEX IF NOT EXISTS idx_claims_delegation ON claims(delegation_id);
CREATE INDEX IF NOT EXISTS idx_delegations_owner_status ON delegations(owner_agent, status);

CREATE TABLE IF NOT EXISTS task_submissions (
    id                TEXT PRIMARY KEY,
    task_id           TEXT NOT NULL,
    delegation_id     TEXT NOT NULL,
    helper_agent      TEXT NOT NULL,
    commit_ref        TEXT NOT NULL,
    base_revision     TEXT NOT NULL,
    summary           TEXT NOT NULL DEFAULT '',
    tests             TEXT NOT NULL DEFAULT '',
    changed_paths_json TEXT NOT NULL DEFAULT '[]',
    message_id        TEXT NOT NULL DEFAULT '',
    status            TEXT NOT NULL DEFAULT 'pending',
    created_at        TEXT NOT NULL,
    reviewed_at       TEXT
);
CREATE INDEX IF NOT EXISTS idx_submissions_task ON task_submissions(task_id, created_at);
CREATE INDEX IF NOT EXISTS idx_submissions_delegation_status
    ON task_submissions(delegation_id, status);
"#;

/// C3 capacity-aware scheduling: durable anti-churn state for bounded assist
/// discovery. Discovery is mechanical scheduling state, not semantic planning.
const SCHEMA_C3: &str = r#"
CREATE TABLE IF NOT EXISTS assist_discovery_state (
    helper_agent             TEXT PRIMARY KEY,
    last_discovery_at        TEXT,
    cooldown_until           TEXT,
    last_offered_fingerprint TEXT NOT NULL DEFAULT '',
    last_offer_id            TEXT,
    updated_at               TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS assist_rejection_backoff (
    helper_agent     TEXT NOT NULL,
    owner_agent      TEXT NOT NULL,
    rejection_count  INTEGER NOT NULL DEFAULT 0,
    last_rejected_at TEXT,
    retry_after      TEXT,
    PRIMARY KEY (helper_agent, owner_agent)
);

CREATE INDEX IF NOT EXISTS idx_assignments_agent_role_state
    ON task_assignments(agent, role, state);
CREATE INDEX IF NOT EXISTS idx_discovery_cooldown
    ON assist_discovery_state(cooldown_until);
CREATE INDEX IF NOT EXISTS idx_rejection_retry
    ON assist_rejection_backoff(helper_agent, owner_agent, retry_after);
"#;

/// C5 workspace isolation: per-delegation workspace records.
const SCHEMA_C5: &str = r#"
CREATE TABLE IF NOT EXISTS delegation_workspaces (
    delegation_id TEXT PRIMARY KEY,
    mode          TEXT NOT NULL DEFAULT 'shared',
    path          TEXT NOT NULL DEFAULT '',
    branch        TEXT NOT NULL DEFAULT '',
    created_at    TEXT NOT NULL
);
"#;

/// C7 observability and completion: project roles, validation checks, and
/// completion attestations so project completion is derived from durable
/// work state, not silence.
const SCHEMA_C7: &str = r#"
CREATE TABLE IF NOT EXISTS project_roles (
    agent         TEXT NOT NULL,
    role          TEXT NOT NULL,
    designated_by TEXT NOT NULL,
    designated_at TEXT NOT NULL,
    PRIMARY KEY(agent, role)
);

CREATE TABLE IF NOT EXISTS validation_checks (
    name       TEXT PRIMARY KEY,
    required   INTEGER NOT NULL DEFAULT 1,
    status     TEXT NOT NULL DEFAULT 'pending',
    revision   TEXT,
    details    TEXT NOT NULL DEFAULT '',
    checked_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS completion_attestations (
    id                   TEXT PRIMARY KEY,
    recorded_by          TEXT NOT NULL,
    role                 TEXT NOT NULL,
    snapshot_fingerprint TEXT NOT NULL,
    note                 TEXT NOT NULL DEFAULT '',
    recorded_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_completion_recorded_at
    ON completion_attestations(recorded_at);
"#;

/// Slice 4 bundle (GAP-03/04/05/07): TMP envelope columns on messages, the
/// four-surface message model, retention timestamps, domain-intrusion
/// tables, and the singleton project-state record.
const SCHEMA_V11: &str = r#"
-- 4B/4D: TMP envelope + retention columns on messages.
ALTER TABLE messages ADD COLUMN channel TEXT NOT NULL DEFAULT 'direct';
ALTER TABLE messages ADD COLUMN scope TEXT;
ALTER TABLE messages ADD COLUMN supersedes TEXT;
ALTER TABLE messages ADD COLUMN tmp_version TEXT NOT NULL DEFAULT '1.0';
ALTER TABLE messages ADD COLUMN last_touched_at TEXT;
ALTER TABLE messages ADD COLUMN archived_at TEXT;
UPDATE messages SET last_touched_at = created_at WHERE last_touched_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_messages_channel_scope ON messages(channel, scope);
CREATE INDEX IF NOT EXISTS idx_messages_supersedes ON messages(supersedes);
CREATE INDEX IF NOT EXISTS idx_messages_last_touched ON messages(last_touched_at);

-- 4A: domain intrusion requests (R9/R25). Approvals live in a child table
-- since more than one may arrive before resolution.
CREATE TABLE IF NOT EXISTS domain_intrusion_requests (
    id                 TEXT PRIMARY KEY,
    requester_agent    TEXT NOT NULL,
    target_domain      TEXT NOT NULL,
    path_glob          TEXT NOT NULL,
    purpose            TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'pending',
    created_at         TEXT NOT NULL,
    objection_deadline TEXT NOT NULL,
    resolved_at        TEXT,
    veto_agent         TEXT,
    veto_reason        TEXT
);
CREATE INDEX IF NOT EXISTS idx_di_requests_status
    ON domain_intrusion_requests(status);

CREATE TABLE IF NOT EXISTS domain_intrusion_approvals (
    request_id TEXT NOT NULL,
    agent      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (request_id, agent)
);

-- 4D: singleton project-level record (retention sweep marker, dormancy flag).
CREATE TABLE IF NOT EXISTS project_state (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    last_swept_at TEXT,
    dormant       INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO project_state (id) VALUES (1);
"#;

/// Slice 5 (GAP-10): Biplane progressive refinement and domain adjacency.
/// The `agents` table is the de-facto domain registry, so the refinement
/// lineage columns live there. NOTE: v12 was already taken by R23's
/// agent_starvation table, so Slice 5 lands as v13 (not v12 as the original
/// spec text proposed).
const SCHEMA_V13: &str = r#"
ALTER TABLE agents ADD COLUMN granularity_tier TEXT NOT NULL DEFAULT 'coarse';
ALTER TABLE agents ADD COLUMN parent_domain TEXT;
ALTER TABLE agents ADD COLUMN created_in_pass INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN owner_at_split_time TEXT;
ALTER TABLE agents ADD COLUMN tier_set_at TEXT;

CREATE TABLE IF NOT EXISTS domain_adjacency (
    from_domain TEXT NOT NULL,
    to_domain   TEXT NOT NULL,
    rank        INTEGER NOT NULL,
    rationale   TEXT NOT NULL DEFAULT '',
    source      TEXT NOT NULL DEFAULT 'sibling',
    created_at  TEXT NOT NULL,
    PRIMARY KEY (from_domain, to_domain)
);
CREATE INDEX IF NOT EXISTS idx_adjacency_from ON domain_adjacency(from_domain, rank);

CREATE TABLE IF NOT EXISTS split_proposals (
    id                  TEXT PRIMARY KEY,
    domain              TEXT NOT NULL,
    owner_at_split_time TEXT,
    proposal_json       TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending',
    created_at          TEXT NOT NULL,
    resolved_at         TEXT
);
CREATE INDEX IF NOT EXISTS idx_split_proposals_owner
    ON split_proposals(owner_at_split_time, status);

ALTER TABLE project_state ADD COLUMN refinement_pass INTEGER NOT NULL DEFAULT 0;
"#;

pub fn open(db_path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

/// Open an in-memory database with the full migrated schema. Intended for
/// tests and ephemeral tooling; avoids tempfile lifetime concerns.
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
    )?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let mut version: u32 = conn
        .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
        conn.execute_batch("PRAGMA user_version = 4;")?;
        // Fall through (instead of returning) so table-creating migrations
        // that are NOT part of SCHEMA_V1 -- cycle_break_attempts (v5) and the
        // C1 work ledger (v6) -- also run on a brand-new database. The v2..v4
        // steps are ALTER TABLEs already covered by SCHEMA_V1, and are skipped
        // because `version` is now 4.
        version = 4;
    }

    if version < 2 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_agents (
                name TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL,
                source TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        conn.execute_batch("PRAGMA user_version = 2;")?;
        version = 2;
    }
    if version < 3 {
        conn.execute_batch("ALTER TABLE agents ADD COLUMN launcher_agent TEXT;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS launch_targets (
                agent TEXT PRIMARY KEY,
                adapter TEXT NOT NULL,
                target TEXT NOT NULL,
                command TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );",
        )?;
        conn.execute_batch("PRAGMA user_version = 3;")?;
        version = 3;
    }
    if version < 4 {
        conn.execute_batch("ALTER TABLE launch_targets ADD COLUMN tmux_target TEXT;")?;
        conn.execute_batch("PRAGMA user_version = 4;")?;
    }
    if version < 5 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cycle_break_attempts (
                cycle_key       TEXT PRIMARY KEY,
                cycle_members   TEXT NOT NULL,
                designated      TEXT NOT NULL,
                attempts        INTEGER NOT NULL DEFAULT 0,
                last_attempt_at TEXT,
                escalated       INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        conn.execute_batch("PRAGMA user_version = 5;")?;
        version = 5;
    }
    if version < 6 {
        // C1: durable work ledger (tasks, assignments, delegations, reviews).
        conn.execute_batch(SCHEMA_C1)?;
        conn.execute_batch("PRAGMA user_version = 6;")?;
        version = 6;
    }
    if version < 7 {
        // C2: offer/grant/submission linkage and delegation-backed claims.
        conn.execute_batch(SCHEMA_C2)?;
        conn.execute_batch("PRAGMA user_version = 7;")?;
        version = 7;
    }
    if version < 8 {
        // C3: bounded assist-discovery anti-churn state.
        conn.execute_batch(SCHEMA_C3)?;
        conn.execute_batch("PRAGMA user_version = 8;")?;
        version = 8;
    }
    if version < 9 {
        // C5: workspace isolation records.
        conn.execute_batch(SCHEMA_C5)?;
        conn.execute_batch("PRAGMA user_version = 9;")?;
        version = 9;
    }
    if version < 10 {
        // C7: project completion and validation state.
        conn.execute_batch(SCHEMA_C7)?;
        conn.execute_batch("PRAGMA user_version = 10;")?;
        version = 10;
    }
    if version < 11 {
        // Slice 4: TMP envelope, retention, domain intrusion, project state.
        conn.execute_batch(SCHEMA_V11)?;
        conn.execute_batch("PRAGMA user_version = 11;")?;
    }
    if version < 12 {
        // R23: per-agent starvation counter. Tracks consecutive squire ticks
        // on which an agent was a valid wake candidate but got deferred past
        // the concurrency budget. Once the count reaches squire.starvation_ticks
        // the candidate is promoted ahead of ordinary ordering so it can't be
        // perpetually starved by lower sort priority.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_starvation (
                agent            TEXT PRIMARY KEY,
                deferred_ticks   INTEGER NOT NULL DEFAULT 0,
                last_deferred_at TEXT
            );",
        )?;
        conn.execute_batch("PRAGMA user_version = 12;")?;
        version = 12;
    }
    if version < 13 {
        // Slice 5 (R17-R22, R29): domain refinement lineage on agents (the
        // de-facto domain registry), the adjacency table, and split
        // proposals for owned-domain refinement (R20).
        conn.execute_batch(SCHEMA_V13)?;
        conn.execute_batch("PRAGMA user_version = 13;")?;
    }

    // Repair: task_submissions.message_id is referenced by the C2 code path
    // but was missing from the original C2 migration. Add it wherever absent,
    // regardless of how the database reached its current version.
    ensure_column(
        conn,
        "task_submissions",
        "message_id",
        "message_id TEXT NOT NULL DEFAULT ''",
    )?;
    Ok(())
}

/// Add a column to a table only if it is not already present. SQLite has no
/// `IF NOT EXISTS` for columns, so guard with `PRAGMA table_info`. No-op when
/// the table itself does not exist (e.g. a partially-migrated test fixture).
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|n| n.ok())
        .collect();
    if columns.is_empty() || columns.iter().any(|n| n == column) {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {decl};"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .any(|name| name.unwrap() == column)
    }

    #[test]
    fn fresh_database_reaches_c2_schema() {
        let conn = open_in_memory().unwrap();
        let version: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, 13);
        assert!(has_column(&conn, "claims", "delegation_id"));
        assert!(has_column(&conn, "delegations", "offer_message"));
        assert!(has_column(&conn, "task_submissions", "message_id"));
        assert!(has_column(
            &conn,
            "assist_discovery_state",
            "last_offered_fingerprint"
        ));
        assert!(has_column(
            &conn,
            "assist_rejection_backoff",
            "rejection_count"
        ));
        assert!(has_column(&conn, "delegation_workspaces", "mode"));
        assert!(has_column(
            &conn,
            "completion_attestations",
            "snapshot_fingerprint"
        ));
        assert!(has_column(&conn, "validation_checks", "status"));
        assert!(has_column(&conn, "project_roles", "role"));
    }

    #[test]
    fn version_six_database_upgrades_additively() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_C1).unwrap();
        conn.execute_batch("PRAGMA user_version = 6;").unwrap();
        migrate(&conn).unwrap();
        assert!(has_column(&conn, "claims", "delegation_id"));
        assert!(has_column(&conn, "delegations", "offer_message"));
        assert!(has_column(&conn, "task_submissions", "reviewed_at"));
        assert!(has_column(
            &conn,
            "assist_discovery_state",
            "cooldown_until"
        ));
        let version: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, 13);
    }

    #[test]
    fn version_seven_database_upgrades_to_c3_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_C1).unwrap();
        conn.execute_batch(SCHEMA_C2).unwrap();
        conn.execute_batch("PRAGMA user_version = 7;").unwrap();
        migrate(&conn).unwrap();
        assert!(has_column(
            &conn,
            "assist_discovery_state",
            "cooldown_until"
        ));
        assert!(has_column(&conn, "assist_rejection_backoff", "retry_after"));
        assert!(has_column(&conn, "delegation_workspaces", "mode"));
        assert!(has_column(&conn, "completion_attestations", "recorded_by"));
        let version: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, 13);
    }

    #[test]
    fn version_ten_database_gains_slice4_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch("PRAGMA user_version = 10;").unwrap();
        migrate(&conn).unwrap();
        for col in [
            "channel",
            "scope",
            "supersedes",
            "tmp_version",
            "last_touched_at",
            "archived_at",
        ] {
            assert!(has_column(&conn, "messages", col), "missing messages.{col}");
        }
        assert!(has_column(&conn, "domain_intrusion_requests", "purpose"));
        assert!(has_column(
            &conn,
            "domain_intrusion_requests",
            "objection_deadline"
        ));
        assert!(has_column(&conn, "domain_intrusion_approvals", "agent"));
        assert!(has_column(&conn, "project_state", "last_swept_at"));
        assert!(has_column(&conn, "project_state", "dormant"));
        let version: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, 13);
    }

    #[test]
    fn migration_is_idempotent_on_reopen() {
        let conn = open_in_memory().unwrap();
        let v1: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        migrate(&conn).unwrap();
        let v2: u32 = conn
            .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(v1, v2);
    }
}
