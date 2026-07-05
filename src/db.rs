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

pub fn open(db_path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
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
        return Ok(());
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
    }
    Ok(())
}
