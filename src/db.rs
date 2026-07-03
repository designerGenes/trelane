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

const SCHEMA_V2: &str = r#"
CREATE TABLE IF NOT EXISTS session_agents (
    name            TEXT PRIMARY KEY,
    enabled         INTEGER NOT NULL,
    source          TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
"#;

const SCHEMA_V3: &str = r#"
CREATE TABLE IF NOT EXISTS launch_targets (
    agent           TEXT PRIMARY KEY,
    adapter         TEXT NOT NULL,
    target          TEXT NOT NULL,
    command         TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

ALTER TABLE agents ADD COLUMN launcher_agent TEXT;
"#;

const SCHEMA_V4: &str = r#"
ALTER TABLE launch_targets ADD COLUMN tmux_target TEXT;
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
    let version: u32 = conn
        .query_row("SELECT user_version FROM pragma_user_version", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if version < 1 {
        conn.execute_batch(SCHEMA_V1)?;
        conn.execute_batch("PRAGMA user_version = 3;")?;
    } else if version < 2 {
        conn.execute_batch(SCHEMA_V2)?;
        conn.execute_batch("PRAGMA user_version = 2;")?;
    }
    if version < 3 {
        match conn.execute_batch(SCHEMA_V3) {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.extended_code == rusqlite::ffi::SQLITE_ERROR =>
            {
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS launch_targets (
                        agent TEXT PRIMARY KEY,
                        adapter TEXT NOT NULL,
                        target TEXT NOT NULL,
                        command TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );",
                )?;
            }
            Err(e) => return Err(e.into()),
        }
        conn.execute_batch("PRAGMA user_version = 3;")?;
    }
    if version < 4 {
        match conn.execute_batch(SCHEMA_V4) {
            Ok(()) => {}
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.extended_code == rusqlite::ffi::SQLITE_ERROR => {}
            Err(e) => return Err(e.into()),
        }
        conn.execute_batch("PRAGMA user_version = 4;")?;
    }
    Ok(())
}
