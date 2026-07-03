use crate::error::{Result, TrelaneError};
use crate::models::{Domain, LaunchTarget, Lease, Message, ParkedTask, RunningLock, Violation};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

// ------------------------------------------------------------------- agents

pub fn insert_agent(
    conn: &Connection,
    name: &str,
    description: &str,
    writable: &[String],
    launcher_agent: Option<&str>,
    forbidden: &[String],
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agents (id, description, writable_json, launcher_agent, forbidden_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            name,
            description,
            serde_json::to_string(writable)?,
            launcher_agent,
            serde_json::to_string(forbidden)?,
            created_at,
        ],
    )?;
    Ok(())
}

pub fn list_agents(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM agents ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn agent_exists(conn: &Connection, name: &str) -> Result<bool> {
    let n: i32 = conn.query_row(
        "SELECT COUNT(*) FROM agents WHERE id = ?1",
        params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn get_domain(conn: &Connection, agent: &str) -> Result<Option<Domain>> {
    let result = conn
        .query_row(
            "SELECT id, description, writable_json, launcher_agent, forbidden_json FROM agents WHERE id = ?1",
            params![agent],
            |r| {
                let writable: Vec<String> =
                    serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
                let forbidden: Vec<String> =
                    serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default();
                Ok(Domain {
                    agent: r.get(0)?,
                    description: r.get(1)?,
                    writable,
                    launcher_agent: r.get(3)?,
                    forbidden_write: forbidden,
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn upsert_agent(
    conn: &Connection,
    name: &str,
    description: &str,
    writable: &[String],
    launcher_agent: Option<&str>,
    forbidden: &[String],
    created_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agents (id, description, writable_json, launcher_agent, forbidden_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(id) DO UPDATE SET
            description = excluded.description,
            writable_json = excluded.writable_json,
            launcher_agent = excluded.launcher_agent,
            forbidden_json = excluded.forbidden_json",
        params![
            name,
            description,
            serde_json::to_string(writable)?,
            launcher_agent,
            serde_json::to_string(forbidden)?,
            created_at,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- messages

pub fn insert_message(conn: &Connection, msg: &Message) -> Result<()> {
    conn.execute(
        "INSERT INTO messages
            (id, from_agent, to_agent, msg_type, urgency, subject, body,
             re, task, paths_json, created_at, schema_version, sig, processed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, NULL)",
        params![
            msg.id,
            msg.from,
            msg.to,
            msg.msg_type,
            msg.urgency,
            msg.subject,
            msg.body,
            msg.re,
            msg.task,
            serde_json::to_string(&msg.paths)?,
            msg.created_at,
            msg.schema,
            msg.sig,
        ],
    )?;
    Ok(())
}

fn row_to_message(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    let paths_json: String = row.get("paths_json")?;
    let paths: Vec<String> = serde_json::from_str(&paths_json).unwrap_or_default();
    Ok(Message {
        id: row.get("id")?,
        from: row.get("from_agent")?,
        to: row.get("to_agent")?,
        msg_type: row.get("msg_type")?,
        urgency: row.get("urgency")?,
        subject: row.get("subject")?,
        body: row.get("body")?,
        re: row.get("re")?,
        task: row.get("task")?,
        paths,
        created_at: row.get("created_at")?,
        schema: row.get("schema_version")?,
        sig: row.get("sig")?,
        processed_at: row.get("processed_at")?,
    })
}

pub fn get_message(conn: &Connection, id: &str) -> Result<Option<Message>> {
    let result = conn
        .query_row(
            "SELECT * FROM messages WHERE id = ?1",
            params![id],
            row_to_message,
        )
        .optional()?;
    Ok(result)
}

pub fn get_unprocessed_messages(conn: &Connection, agent: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM messages WHERE to_agent = ?1 AND processed_at IS NULL ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![agent], row_to_message)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get_all_messages_for_agent(conn: &Connection, agent: &str) -> Result<Vec<Message>> {
    let mut stmt =
        conn.prepare("SELECT * FROM messages WHERE to_agent = ?1 ORDER BY created_at")?;
    let rows = stmt.query_map(params![agent], row_to_message)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn mark_processed(conn: &Connection, agent: &str, msg_id: &str, at: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE messages SET processed_at = ?3
         WHERE id = ?1 AND to_agent = ?2 AND processed_at IS NULL",
        params![msg_id, agent, at],
    )?;
    if n == 0 {
        return Err(TrelaneError::Msg(format!(
            "no unprocessed message {msg_id} in {agent}'s inbox"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------ parked tasks

pub fn insert_parked_task(conn: &Connection, task: &ParkedTask) -> Result<()> {
    conn.execute(
        "INSERT INTO parked_tasks
            (task, agent, wait_type, wait_re, wait_path, waiting_on, resume_hint, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            task.task,
            task.agent,
            task.wait_type,
            task.wait_re,
            task.wait_path,
            task.waiting_on,
            task.resume_hint,
            task.created_at,
        ],
    )?;
    Ok(())
}

pub fn delete_parked_task(conn: &Connection, task_id: &str) -> Result<()> {
    let n = conn.execute("DELETE FROM parked_tasks WHERE task = ?1", params![task_id])?;
    if n == 0 {
        return Err(TrelaneError::Msg(format!("no parked task {task_id}")));
    }
    Ok(())
}

pub fn list_parked_tasks(conn: &Connection) -> Result<Vec<ParkedTask>> {
    let mut stmt = conn.prepare("SELECT * FROM parked_tasks ORDER BY created_at")?;
    let rows = stmt.query_map([], |row| {
        Ok(ParkedTask {
            task: row.get("task")?,
            agent: row.get("agent")?,
            wait_type: row.get("wait_type")?,
            wait_re: row.get("wait_re")?,
            wait_path: row.get("wait_path")?,
            waiting_on: row.get("waiting_on")?,
            resume_hint: row.get("resume_hint")?,
            created_at: row.get("created_at")?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_parked_tasks_for_agent(conn: &Connection, agent: &str) -> Result<Vec<ParkedTask>> {
    let mut stmt =
        conn.prepare("SELECT * FROM parked_tasks WHERE agent = ?1 ORDER BY created_at")?;
    let rows = stmt.query_map(params![agent], |row| {
        Ok(ParkedTask {
            task: row.get("task")?,
            agent: row.get("agent")?,
            wait_type: row.get("wait_type")?,
            wait_re: row.get("wait_re")?,
            wait_path: row.get("wait_path")?,
            waiting_on: row.get("waiting_on")?,
            resume_hint: row.get("resume_hint")?,
            created_at: row.get("created_at")?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ----------------------------------------------------------------- claims

pub fn insert_claim(conn: &Connection, lease: &Lease) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO claims
            (path, holder, task, grant, acquired_at, expires_at, expires_human, contested)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            lease.path,
            lease.holder,
            lease.task,
            lease.grant,
            lease.acquired_at,
            lease.expires_at,
            lease.expires_human,
            lease.contested as i32,
        ],
    )?;
    Ok(n > 0)
}

pub fn get_claim(conn: &Connection, path: &str) -> Result<Option<Lease>> {
    let result = conn
        .query_row(
            "SELECT * FROM claims WHERE path = ?1",
            params![path],
            |row| {
                Ok(Lease {
                    path: row.get("path")?,
                    holder: row.get("holder")?,
                    task: row.get("task")?,
                    grant: row.get("grant")?,
                    acquired_at: row.get("acquired_at")?,
                    expires_at: row.get("expires_at")?,
                    expires_human: row.get("expires_human")?,
                    contested: row.get::<_, i32>("contested")? != 0,
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn delete_claim(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("DELETE FROM claims WHERE path = ?1", params![path])?;
    Ok(())
}

pub fn update_claim_expiry(
    conn: &Connection,
    path: &str,
    expires_at: f64,
    expires_human: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE claims SET expires_at = ?2, expires_human = ?3 WHERE path = ?1",
        params![path, expires_at, expires_human],
    )?;
    Ok(())
}

pub fn list_claims(conn: &Connection) -> Result<Vec<Lease>> {
    let mut stmt = conn.prepare("SELECT * FROM claims ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        Ok(Lease {
            path: row.get("path")?,
            holder: row.get("holder")?,
            task: row.get("task")?,
            grant: row.get("grant")?,
            acquired_at: row.get("acquired_at")?,
            expires_at: row.get("expires_at")?,
            expires_human: row.get("expires_human")?,
            contested: row.get::<_, i32>("contested")? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ----------------------------------------------------------- running locks

pub fn insert_running_lock(
    conn: &Connection,
    agent: &str,
    pid: i32,
    started_at: &str,
    reason: &str,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO running_locks (agent, pid, started_at, reason)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent, pid, started_at, reason],
    )?;
    Ok(n > 0)
}

pub fn delete_running_lock(conn: &Connection, agent: &str) -> Result<()> {
    conn.execute("DELETE FROM running_locks WHERE agent = ?1", params![agent])?;
    Ok(())
}

pub fn get_running_lock(conn: &Connection, agent: &str) -> Result<Option<RunningLock>> {
    let result = conn
        .query_row(
            "SELECT * FROM running_locks WHERE agent = ?1",
            params![agent],
            |row| {
                Ok(RunningLock {
                    agent: row.get("agent")?,
                    pid: row.get("pid")?,
                    started_at: row.get("started_at")?,
                    reason: row.get("reason")?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

// ------------------------------------------------------------- violations

pub fn insert_violation(conn: &Connection, v: &Violation) -> Result<()> {
    conn.execute(
        "INSERT INTO violations (id, agent, paths_json, at)
         VALUES (?1, ?2, ?3, ?4)",
        params![v.id, v.agent, serde_json::to_string(&v.paths)?, v.at],
    )?;
    Ok(())
}

// -------------------------------------------------------- audit baselines

pub fn save_audit_baseline(
    conn: &Connection,
    agent: &str,
    hashes: &HashMap<String, String>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO audit_baselines (agent, hashes_json)
         VALUES (?1, ?2)",
        params![agent, serde_json::to_string(hashes)?],
    )?;
    Ok(())
}

pub fn get_audit_baseline(
    conn: &Connection,
    agent: &str,
) -> Result<Option<HashMap<String, String>>> {
    let result = conn
        .query_row(
            "SELECT hashes_json FROM audit_baselines WHERE agent = ?1",
            params![agent],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match result {
        Some(json) => Ok(Some(serde_json::from_str(&json).unwrap_or_default())),
        None => Ok(None),
    }
}

// ----------------------------------------------------------- session agents

pub fn upsert_session_agent(
    conn: &Connection,
    name: &str,
    enabled: bool,
    source: &str,
    updated_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO session_agents (name, enabled, source, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(name) DO UPDATE SET
            enabled = excluded.enabled,
            source = excluded.source,
            updated_at = excluded.updated_at",
        params![name, enabled as i32, source, updated_at],
    )?;
    Ok(())
}

pub fn list_session_agents(conn: &Connection) -> Result<Vec<(String, bool, String)>> {
    let mut stmt = conn
        .prepare("SELECT name, enabled, source FROM session_agents ORDER BY enabled DESC, name")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i32>(1)? != 0,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn session_agent_enabled(conn: &Connection, name: &str) -> Result<Option<bool>> {
    let result = conn
        .query_row(
            "SELECT enabled FROM session_agents WHERE name = ?1",
            params![name],
            |row| Ok(row.get::<_, i32>(0)? != 0),
        )
        .optional()?;
    Ok(result)
}

// ------------------------------------------------------------ launch targets

pub fn upsert_launch_target(
    conn: &Connection,
    agent: &str,
    adapter: &str,
    target: &str,
    command: &str,
    updated_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO launch_targets (agent, adapter, target, command, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(agent) DO UPDATE SET
            adapter = excluded.adapter,
            target = excluded.target,
            command = excluded.command,
            updated_at = excluded.updated_at",
        params![agent, adapter, target, command, updated_at],
    )?;
    Ok(())
}

pub fn get_launch_target(conn: &Connection, agent: &str) -> Result<Option<LaunchTarget>> {
    let result = conn
        .query_row(
            "SELECT agent, adapter, target, command, updated_at FROM launch_targets WHERE agent = ?1",
            params![agent],
            |row| {
                Ok(LaunchTarget {
                    agent: row.get(0)?,
                    adapter: row.get(1)?,
                    target: row.get(2)?,
                    command: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn list_launch_targets(conn: &Connection) -> Result<Vec<LaunchTarget>> {
    let mut stmt = conn.prepare(
        "SELECT agent, adapter, target, command, updated_at FROM launch_targets ORDER BY agent",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(LaunchTarget {
            agent: row.get(0)?,
            adapter: row.get(1)?,
            target: row.get(2)?,
            command: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}
