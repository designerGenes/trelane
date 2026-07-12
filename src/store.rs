use crate::error::{Result, TrelaneError};
use crate::models::{
    AssistDiscoveryState, AssistPolicy, Delegation, DelegationStatus,
    Domain, LaunchTarget, Lease, Message, ParkedTask, ReviewDecision, RunningLock, Task,
    TaskAssignment, TaskReview, TaskRole, TaskState, TaskSubmission, Violation,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
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
            (path, holder, task, grant, delegation_id, acquired_at, expires_at, expires_human, contested)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            lease.path,
            lease.holder,
            lease.task,
            lease.grant,
            lease.delegation_id,
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
                    delegation_id: row.get("delegation_id")?,
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

pub fn update_claim_renewal(
    conn: &Connection,
    path: &str,
    task: Option<&str>,
    grant: Option<&str>,
    delegation_id: Option<&str>,
    expires_at: f64,
    expires_human: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE claims SET task = ?2, grant = ?3, delegation_id = ?4,
             expires_at = ?5, expires_human = ?6 WHERE path = ?1",
        params![path, task, grant, delegation_id, expires_at, expires_human],
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
            delegation_id: row.get("delegation_id")?,
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
    tmux_target: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO launch_targets (agent, adapter, target, command, tmux_target, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(agent) DO UPDATE SET
            adapter = excluded.adapter,
            target = excluded.target,
            command = excluded.command,
            tmux_target = excluded.tmux_target,
            updated_at = excluded.updated_at",
        params![agent, adapter, target, command, tmux_target, updated_at],
    )?;
    Ok(())
}

pub fn get_launch_target(conn: &Connection, agent: &str) -> Result<Option<LaunchTarget>> {
    let result = conn
        .query_row(
            "SELECT agent, adapter, target, command, tmux_target, updated_at FROM launch_targets WHERE agent = ?1",
            params![agent],
            |row| {
                Ok(LaunchTarget {
                    agent: row.get(0)?,
                    adapter: row.get(1)?,
                    target: row.get(2)?,
                    command: row.get(3)?,
                    tmux_target: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn list_launch_targets(conn: &Connection) -> Result<Vec<LaunchTarget>> {
    let mut stmt = conn.prepare(
        "SELECT agent, adapter, target, command, tmux_target, updated_at FROM launch_targets ORDER BY agent",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(LaunchTarget {
            agent: row.get(0)?,
            adapter: row.get(1)?,
            target: row.get(2)?,
            command: row.get(3)?,
            tmux_target: row.get(4)?,
            updated_at: row.get(5)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ------------------------------------------------- cycle break attempts (T3)

/// Record or increment a cycle-break attempt for a given cycle.
/// Returns the new attempt count.
pub fn record_cycle_break_attempt(
    conn: &Connection,
    cycle_key: &str,
    cycle_members: &[String],
    designated: &str,
) -> Result<i64> {
    let members_str = cycle_members.join(",");
    let now = crate::crypto::now_iso();

    conn.execute(
        "INSERT INTO cycle_break_attempts (cycle_key, cycle_members, designated, attempts, last_attempt_at, escalated)
         VALUES (?1, ?2, ?3, 1, ?4, 0)
         ON CONFLICT(cycle_key) DO UPDATE SET
            attempts = attempts + 1,
            designated = excluded.designated,
            last_attempt_at = excluded.last_attempt_at",
        params![cycle_key, members_str, designated, now],
    )?;

    let count: i64 = conn.query_row(
        "SELECT attempts FROM cycle_break_attempts WHERE cycle_key = ?1",
        params![cycle_key],
        |r| r.get(0),
    )?;
    Ok(count)
}

/// Reset the attempt counter for a cycle (call when the cycle is resolved).
pub fn clear_cycle_break_attempts(conn: &Connection, cycle_key: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM cycle_break_attempts WHERE cycle_key = ?1",
        params![cycle_key],
    )?;
    Ok(())
}

/// Mark a cycle as escalated (so we don't re-escalate every tick).
pub fn mark_cycle_escalated(conn: &Connection, cycle_key: &str) -> Result<()> {
    conn.execute(
        "UPDATE cycle_break_attempts SET escalated = 1 WHERE cycle_key = ?1",
        params![cycle_key],
    )?;
    Ok(())
}

/// Check if a cycle has already been escalated.
pub fn is_cycle_escalated(conn: &Connection, cycle_key: &str) -> Result<bool> {
    let result: Option<i64> = conn
        .query_row(
            "SELECT escalated FROM cycle_break_attempts WHERE cycle_key = ?1",
            params![cycle_key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(result == Some(1))
}

/// Get the current attempt count for a cycle (0 if never recorded).
pub fn get_cycle_attempt_count(conn: &Connection, cycle_key: &str) -> Result<i64> {
    let result: Option<i64> = conn
        .query_row(
            "SELECT attempts FROM cycle_break_attempts WHERE cycle_key = ?1",
            params![cycle_key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(result.unwrap_or(0))
}

/// A cycle break attempt record.
pub struct CycleBreakAttempt {
    pub cycle_key: String,
    pub cycle_members: String,
    pub designated: String,
    pub attempts: i64,
    pub last_attempt_at: Option<String>,
    pub escalated: bool,
}

/// List all cycle break attempt records (for diagnostics and cleanup).
pub fn list_cycle_break_attempts(conn: &Connection) -> Result<Vec<CycleBreakAttempt>> {
    let mut stmt = conn.prepare(
        "SELECT cycle_key, cycle_members, designated, attempts, last_attempt_at, escalated
         FROM cycle_break_attempts ORDER BY attempts DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(CycleBreakAttempt {
            cycle_key: r.get(0)?,
            cycle_members: r.get(1)?,
            designated: r.get(2)?,
            attempts: r.get(3)?,
            last_attempt_at: r.get(4)?,
            escalated: r.get::<_, i64>(5)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// --------------------------------------------------------------------- tasks
//
// C1 work-ledger persistence. list/get helpers reconstruct typed models from
// the JSON-encoded columns; enum columns are validated on read and fall back
// to a safe default rather than erroring, so a hand-edited or
// forward-versioned DB can never panic the scheduler.

const TASK_COLUMNS: &str = "id, owner_agent, domain, parent_task, subject, body, state, priority, \
     assist_policy, desired_parallelism, path_scope_json, acceptance_json, \
     blocked_by_json, created_at, updated_at";

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<Task> {
    let state_s: String = row.get("state")?;
    let policy_s: String = row.get("assist_policy")?;
    let path_scope: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("path_scope_json")?).unwrap_or_default();
    let acceptance: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("acceptance_json")?).unwrap_or_default();
    let blocked_by: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("blocked_by_json")?).unwrap_or_default();
    Ok(Task {
        id: row.get("id")?,
        owner_agent: row.get("owner_agent")?,
        domain: row.get("domain")?,
        parent_task: row.get("parent_task")?,
        subject: row.get("subject")?,
        body: row.get("body")?,
        state: TaskState::parse(&state_s).unwrap_or(TaskState::Draft),
        priority: row.get("priority")?,
        assist_policy: AssistPolicy::parse(&policy_s).unwrap_or(AssistPolicy::Open),
        desired_parallelism: row.get::<_, i64>("desired_parallelism")?.max(1) as u32,
        path_scope,
        acceptance,
        blocked_by,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub fn insert_task(conn: &Connection, task: &Task) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks
            (id, owner_agent, domain, parent_task, subject, body, state, priority,
             assist_policy, desired_parallelism, path_scope_json, acceptance_json,
             blocked_by_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            task.id,
            task.owner_agent,
            task.domain,
            task.parent_task,
            task.subject,
            task.body,
            task.state.as_str(),
            task.priority,
            task.assist_policy.as_str(),
            task.desired_parallelism as i64,
            serde_json::to_string(&task.path_scope)?,
            serde_json::to_string(&task.acceptance)?,
            serde_json::to_string(&task.blocked_by)?,
            task.created_at,
            task.updated_at,
        ],
    )?;
    Ok(())
}

pub fn get_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1");
    let result = conn.query_row(&sql, params![id], row_to_task).optional()?;
    Ok(result)
}

pub fn list_tasks(conn: &Connection) -> Result<Vec<Task>> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_to_task)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_tasks_for_owner(conn: &Connection, owner: &str) -> Result<Vec<Task>> {
    let sql =
        format!("SELECT {TASK_COLUMNS} FROM tasks WHERE owner_agent = ?1 ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![owner], row_to_task)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_tasks_in_state(conn: &Connection, state: TaskState) -> Result<Vec<Task>> {
    let sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE state = ?1 ORDER BY created_at, id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![state.as_str()], row_to_task)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Set of task ids that have reached the `done` state -- used to evaluate
/// dependency satisfaction for readiness.
pub fn done_task_ids(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM tasks WHERE state = 'done'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Update a task's state and bump `updated_at`. Returns true if a row changed.
pub fn set_task_state(
    conn: &Connection,
    id: &str,
    state: TaskState,
    updated_at: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE tasks SET state = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, state.as_str(), updated_at],
    )?;
    Ok(n > 0)
}

/// Compare-and-swap a task state so concurrent protocol commands cannot
/// silently overwrite one another.
pub fn set_task_state_if(
    conn: &Connection,
    id: &str,
    expected: TaskState,
    state: TaskState,
    updated_at: &str,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE tasks SET state = ?3, updated_at = ?4 WHERE id = ?1 AND state = ?2",
        params![id, expected.as_str(), state.as_str(), updated_at],
    )?;
    Ok(n > 0)
}

/// Replace the full mutable body of a task (everything except id/created_at).
pub fn update_task(conn: &Connection, task: &Task) -> Result<bool> {
    let n = conn.execute(
        "UPDATE tasks SET
            owner_agent = ?2, domain = ?3, parent_task = ?4, subject = ?5, body = ?6,
            state = ?7, priority = ?8, assist_policy = ?9, desired_parallelism = ?10,
            path_scope_json = ?11, acceptance_json = ?12, blocked_by_json = ?13,
            updated_at = ?14
         WHERE id = ?1",
        params![
            task.id,
            task.owner_agent,
            task.domain,
            task.parent_task,
            task.subject,
            task.body,
            task.state.as_str(),
            task.priority,
            task.assist_policy.as_str(),
            task.desired_parallelism as i64,
            serde_json::to_string(&task.path_scope)?,
            serde_json::to_string(&task.acceptance)?,
            serde_json::to_string(&task.blocked_by)?,
            task.updated_at,
        ],
    )?;
    Ok(n > 0)
}

// --------------------------------------------------------------- assignments

pub fn upsert_assignment(conn: &Connection, a: &TaskAssignment) -> Result<()> {
    conn.execute(
        "INSERT INTO task_assignments
            (task_id, agent, role, state, offer_id, delegation_id, started_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(task_id, agent, role) DO UPDATE SET
            state = excluded.state,
            offer_id = excluded.offer_id,
            delegation_id = excluded.delegation_id,
            started_at = excluded.started_at,
            completed_at = excluded.completed_at",
        params![
            a.task_id,
            a.agent,
            a.role.as_str(),
            a.state,
            a.offer_id,
            a.delegation_id,
            a.started_at,
            a.completed_at,
        ],
    )?;
    Ok(())
}

fn row_to_assignment(row: &rusqlite::Row) -> rusqlite::Result<TaskAssignment> {
    let role_s: String = row.get("role")?;
    Ok(TaskAssignment {
        task_id: row.get("task_id")?,
        agent: row.get("agent")?,
        role: TaskRole::parse(&role_s).unwrap_or(TaskRole::Helper),
        state: row.get("state")?,
        offer_id: row.get("offer_id")?,
        delegation_id: row.get("delegation_id")?,
        started_at: row.get("started_at")?,
        completed_at: row.get("completed_at")?,
    })
}

const ASSIGNMENT_COLUMNS: &str =
    "task_id, agent, role, state, offer_id, delegation_id, started_at, completed_at";

pub fn list_assignments_for_task(conn: &Connection, task_id: &str) -> Result<Vec<TaskAssignment>> {
    let sql = format!(
        "SELECT {ASSIGNMENT_COLUMNS} FROM task_assignments WHERE task_id = ?1 ORDER BY role, agent"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![task_id], row_to_assignment)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_assignments_for_agent(conn: &Connection, agent: &str) -> Result<Vec<TaskAssignment>> {
    let sql = format!(
        "SELECT {ASSIGNMENT_COLUMNS} FROM task_assignments WHERE agent = ?1 ORDER BY task_id, role"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![agent], row_to_assignment)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------- delegations

const DELEGATION_COLUMNS: &str = "id, task_id, owner_agent, helper_agent, scope_json, allowed_ops_json, \
     constraints_json, base_revision, offer_message, grant_message, issued_at, expires_at, status";

pub fn insert_delegation(conn: &Connection, d: &Delegation) -> Result<()> {
    conn.execute(
        "INSERT INTO delegations
            (id, task_id, owner_agent, helper_agent, scope_json, allowed_ops_json,
              constraints_json, base_revision, offer_message, grant_message, issued_at, expires_at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            d.id,
            d.task_id,
            d.owner_agent,
            d.helper_agent,
            serde_json::to_string(&d.scope)?,
            serde_json::to_string(&d.allowed_ops)?,
            d.constraints_json,
            d.base_revision,
            d.offer_message,
            d.grant_message,
            d.issued_at,
            d.expires_at,
            d.status.as_str(),
        ],
    )?;
    Ok(())
}

fn row_to_delegation(row: &rusqlite::Row) -> rusqlite::Result<Delegation> {
    let status_s: String = row.get("status")?;
    let scope: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("scope_json")?).unwrap_or_default();
    let allowed_ops: Vec<String> =
        serde_json::from_str(&row.get::<_, String>("allowed_ops_json")?).unwrap_or_default();
    Ok(Delegation {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        owner_agent: row.get("owner_agent")?,
        helper_agent: row.get("helper_agent")?,
        scope,
        allowed_ops,
        constraints_json: row.get("constraints_json")?,
        base_revision: row.get("base_revision")?,
        offer_message: row.get("offer_message")?,
        grant_message: row.get("grant_message")?,
        issued_at: row.get("issued_at")?,
        expires_at: row.get("expires_at")?,
        status: DelegationStatus::parse(&status_s).unwrap_or(DelegationStatus::Offered),
    })
}

pub fn get_delegation(conn: &Connection, id: &str) -> Result<Option<Delegation>> {
    let sql = format!("SELECT {DELEGATION_COLUMNS} FROM delegations WHERE id = ?1");
    let result = conn
        .query_row(&sql, params![id], row_to_delegation)
        .optional()?;
    Ok(result)
}

pub fn get_delegation_by_grant_message(
    conn: &Connection,
    grant_message: &str,
) -> Result<Option<Delegation>> {
    let sql =
        format!("SELECT {DELEGATION_COLUMNS} FROM delegations WHERE grant_message = ?1 LIMIT 1");
    Ok(conn
        .query_row(&sql, params![grant_message], row_to_delegation)
        .optional()?)
}

pub fn list_delegations_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Delegation>> {
    let sql = format!(
        "SELECT {DELEGATION_COLUMNS} FROM delegations WHERE task_id = ?1 ORDER BY issued_at, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![task_id], row_to_delegation)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn list_delegations_for_helper(
    conn: &Connection,
    helper: &str,
    status: Option<DelegationStatus>,
) -> Result<Vec<Delegation>> {
    let mut out = Vec::new();
    if let Some(status) = status {
        let sql = format!(
            "SELECT {DELEGATION_COLUMNS} FROM delegations \
             WHERE helper_agent = ?1 AND status = ?2 ORDER BY issued_at, id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![helper, status.as_str()], row_to_delegation)?;
        for row in rows {
            out.push(row?);
        }
    } else {
        let sql = format!(
            "SELECT {DELEGATION_COLUMNS} FROM delegations \
             WHERE helper_agent = ?1 ORDER BY issued_at, id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![helper], row_to_delegation)?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

pub fn list_open_offers_for_owner(conn: &Connection, owner: &str) -> Result<Vec<Delegation>> {
    let sql = format!(
        "SELECT {DELEGATION_COLUMNS} FROM delegations \
         WHERE owner_agent = ?1 AND status = 'offered' ORDER BY issued_at, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![owner], row_to_delegation)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn set_delegation_status(
    conn: &Connection,
    id: &str,
    status: DelegationStatus,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE delegations SET status = ?2 WHERE id = ?1",
        params![id, status.as_str()],
    )?;
    Ok(n > 0)
}

pub fn set_delegation_status_if(
    conn: &Connection,
    id: &str,
    expected: DelegationStatus,
    status: DelegationStatus,
) -> Result<bool> {
    let n = conn.execute(
        "UPDATE delegations SET status = ?3 WHERE id = ?1 AND status = ?2",
        params![id, expected.as_str(), status.as_str()],
    )?;
    Ok(n > 0)
}

/// Persist an offered delegation and its signed notification atomically.
pub fn insert_offer(conn: &Connection, d: &Delegation, msg: &Message) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    insert_message(&tx, msg)?;
    insert_delegation(&tx, d)?;
    tx.commit()?;
    Ok(())
}

pub fn reject_offer_with_message(
    conn: &Connection,
    id: &str,
    msg: &Message,
    now: &str,
) -> Result<bool> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let changed = set_delegation_status_if(
        &tx,
        id,
        DelegationStatus::Offered,
        DelegationStatus::Rejected,
    )?;
    if changed {
        insert_message(&tx, msg)?;
        tx.execute(
            "UPDATE task_assignments SET state = 'rejected', completed_at = ?2
             WHERE delegation_id = ?1",
            params![id, now],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

pub fn revoke_delegation_with_message(
    conn: &Connection,
    id: &str,
    msg: &Message,
    now: &str,
) -> Result<bool> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE delegations SET status = 'revoked'
         WHERE id = ?1 AND status IN ('offered', 'active', 'submitted')",
        params![id],
    )?;
    if changed == 1 {
        insert_message(&tx, msg)?;
        tx.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?;
        tx.execute(
            "UPDATE task_assignments SET state = 'revoked', completed_at = ?2
             WHERE delegation_id = ?1",
            params![id, now],
        )?;
    }
    tx.commit()?;
    Ok(changed == 1)
}

/// Atomically consume one offer, enforce task helper capacity, insert the
/// signed grant, and record the active helper assignment.
#[allow(clippy::too_many_arguments)]
pub fn activate_delegation_and_assign(
    conn: &Connection,
    id: &str,
    scope: &[String],
    allowed_ops: &[String],
    expires_at: &str,
    grant: &Message,
    now: &str,
) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let delegation = get_delegation(&tx, id)?
        .ok_or_else(|| TrelaneError::msg(format!("no help offer '{id}'")))?;
    if delegation.status != DelegationStatus::Offered {
        return Err(TrelaneError::msg(format!(
            "help offer '{id}' is {}, not offered",
            delegation.status.as_str()
        )));
    }
    let task = get_task(&tx, &delegation.task_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{}'", delegation.task_id)))?;
    if task.state.is_terminal() {
        return Err(TrelaneError::msg("cannot accept help for a terminal task"));
    }
    let active_helpers: i64 = tx.query_row(
        "SELECT COUNT(*) FROM task_assignments
         WHERE task_id = ?1 AND role = 'helper' AND state IN ('active', 'submitted')",
        params![task.id],
        |r| r.get(0),
    )?;
    if active_helpers >= task.desired_parallelism as i64 {
        return Err(TrelaneError::msg(format!(
            "task '{}' helper capacity is full ({}/{})",
            task.id, active_helpers, task.desired_parallelism
        )));
    }
    let changed = tx.execute(
        "UPDATE delegations SET scope_json = ?2, allowed_ops_json = ?3,
             expires_at = ?4, grant_message = ?5, status = 'active'
         WHERE id = ?1 AND status = 'offered'",
        params![
            id,
            serde_json::to_string(scope)?,
            serde_json::to_string(allowed_ops)?,
            expires_at,
            grant.id,
        ],
    )?;
    if changed != 1 {
        return Err(TrelaneError::msg(format!(
            "help offer '{id}' changed concurrently"
        )));
    }
    insert_message(&tx, grant)?;
    upsert_assignment(
        &tx,
        &TaskAssignment {
            task_id: task.id.clone(),
            agent: delegation.helper_agent,
            role: TaskRole::Helper,
            state: "active".to_string(),
            offer_id: Some(delegation.offer_message),
            delegation_id: Some(id.to_string()),
            started_at: Some(now.to_string()),
            completed_at: None,
        },
    )?;
    if task.state == TaskState::Ready {
        set_task_state_if(&tx, &task.id, TaskState::Ready, TaskState::Active, now)?;
    }
    tx.commit()?;
    Ok(())
}

/// End one capability with a compare-and-swap transition and synchronously
/// release every linked lease.
pub fn end_delegation_authority(
    conn: &Connection,
    id: &str,
    expected: DelegationStatus,
    terminal: DelegationStatus,
    assignment_state: &str,
    now: &str,
) -> Result<bool> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE delegations SET status = ?3 WHERE id = ?1 AND status = ?2",
        params![id, expected.as_str(), terminal.as_str()],
    )?;
    if changed == 1 {
        tx.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?;
        tx.execute(
            "UPDATE task_assignments SET state = ?2, completed_at = ?3
             WHERE delegation_id = ?1",
            params![id, assignment_state, now],
        )?;
    }
    tx.commit()?;
    Ok(changed == 1)
}

pub fn release_claims_for_delegation(conn: &Connection, id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?)
}

/// Expire offered/active capabilities and synchronously release authority.
pub fn expire_stale_delegations(conn: &Connection, now: &str) -> Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let ids = {
        let mut stmt = tx.prepare(
            "SELECT id FROM delegations
             WHERE status IN ('offered', 'active') AND expires_at IS NOT NULL AND expires_at <= ?1",
        )?;
        let rows = stmt.query_map(params![now], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };
    for id in &ids {
        tx.execute(
            "UPDATE delegations SET status = 'expired'
             WHERE id = ?1 AND status IN ('offered', 'active')",
            params![id],
        )?;
        tx.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?;
        tx.execute(
            "UPDATE task_assignments SET state = 'expired', completed_at = ?2
             WHERE delegation_id = ?1 AND state IN ('active', 'submitted')",
            params![id, now],
        )?;
    }
    tx.commit()?;
    Ok(ids.len())
}

/// Revoke all residual capability rows for a terminal/rejected task. Selected
/// delegations already moved to accepted/rejected retain that audit state.
pub fn revoke_delegations_for_task(conn: &Connection, task_id: &str, now: &str) -> Result<usize> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let ids = {
        let mut stmt = tx.prepare(
            "SELECT id FROM delegations
             WHERE task_id = ?1 AND status IN ('offered', 'active', 'submitted')",
        )?;
        let rows = stmt.query_map(params![task_id], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        ids
    };
    for id in &ids {
        tx.execute(
            "UPDATE delegations SET status = 'revoked'
             WHERE id = ?1 AND status IN ('offered', 'active', 'submitted')",
            params![id],
        )?;
        tx.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?;
        tx.execute(
            "UPDATE task_assignments SET state = 'revoked', completed_at = ?2
             WHERE delegation_id = ?1 AND state IN ('active', 'submitted')",
            params![id, now],
        )?;
    }
    tx.commit()?;
    Ok(ids.len())
}

// ----------------------------------------------------------- task submissions

fn row_to_submission(row: &rusqlite::Row) -> rusqlite::Result<TaskSubmission> {
    Ok(TaskSubmission {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        delegation_id: row.get("delegation_id")?,
        helper_agent: row.get("helper_agent")?,
        commit_ref: row.get("commit_ref")?,
        base_revision: row.get("base_revision")?,
        summary: row.get("summary")?,
        tests: row.get("tests")?,
        changed_paths: serde_json::from_str(&row.get::<_, String>("changed_paths_json")?)
            .unwrap_or_default(),
        message_id: row.get("message_id")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        reviewed_at: row.get("reviewed_at")?,
    })
}

const SUBMISSION_COLUMNS: &str = "id, task_id, delegation_id, helper_agent, commit_ref, \
    base_revision, summary, tests, changed_paths_json, message_id, status, created_at, reviewed_at";

pub fn get_submission(conn: &Connection, id: &str) -> Result<Option<TaskSubmission>> {
    let sql = format!("SELECT {SUBMISSION_COLUMNS} FROM task_submissions WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![id], row_to_submission)
        .optional()?)
}

pub fn latest_submission_for_delegation(
    conn: &Connection,
    task_id: &str,
    delegation_id: &str,
) -> Result<Option<TaskSubmission>> {
    let sql = format!(
        "SELECT {SUBMISSION_COLUMNS} FROM task_submissions
         WHERE task_id = ?1 AND delegation_id = ?2 ORDER BY created_at DESC, id DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![task_id, delegation_id], row_to_submission)
        .optional()?)
}

pub fn list_submissions_for_task(conn: &Connection, task_id: &str) -> Result<Vec<TaskSubmission>> {
    let sql = format!(
        "SELECT {SUBMISSION_COLUMNS} FROM task_submissions WHERE task_id = ?1 ORDER BY created_at, id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![task_id], row_to_submission)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Record a validated submission, move task/delegation to review/submitted,
/// and release execution leases atomically.
pub fn record_submission(
    conn: &Connection,
    submission: &TaskSubmission,
    msg: &Message,
) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let delegation = get_delegation(&tx, &submission.delegation_id)?
        .ok_or_else(|| TrelaneError::msg("delegation disappeared before submission"))?;
    if delegation.status != DelegationStatus::Active {
        return Err(TrelaneError::msg(format!(
            "delegation '{}' is {}, not active",
            delegation.id,
            delegation.status.as_str()
        )));
    }
    let task = get_task(&tx, &submission.task_id)?
        .ok_or_else(|| TrelaneError::msg("task disappeared before submission"))?;
    if task.state != TaskState::Active {
        return Err(TrelaneError::msg(format!(
            "task '{}' is {}, not active",
            task.id,
            task.state.as_str()
        )));
    }
    if !set_delegation_status_if(
        &tx,
        &delegation.id,
        DelegationStatus::Active,
        DelegationStatus::Submitted,
    )? || !set_task_state_if(
        &tx,
        &task.id,
        TaskState::Active,
        TaskState::Review,
        &submission.created_at,
    )? {
        return Err(TrelaneError::msg(
            "submission lost a concurrent state transition",
        ));
    }
    tx.execute(
        "INSERT INTO task_submissions
          (id, task_id, delegation_id, helper_agent, commit_ref, base_revision,
           summary, tests, changed_paths_json, message_id, status, created_at, reviewed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            submission.id,
            submission.task_id,
            submission.delegation_id,
            submission.helper_agent,
            submission.commit_ref,
            submission.base_revision,
            submission.summary,
            submission.tests,
            serde_json::to_string(&submission.changed_paths)?,
            submission.message_id,
            submission.status,
            submission.created_at,
            submission.reviewed_at,
        ],
    )?;
    insert_message(&tx, msg)?;
    tx.execute(
        "UPDATE task_assignments SET state = 'submitted' WHERE delegation_id = ?1",
        params![submission.delegation_id],
    )?;
    tx.execute(
        "DELETE FROM claims WHERE delegation_id = ?1",
        params![submission.delegation_id],
    )?;
    tx.commit()?;
    Ok(())
}

// --------------------------------------------------------------- task reviews

pub fn insert_review(conn: &Connection, rv: &TaskReview) -> Result<()> {
    conn.execute(
        "INSERT INTO task_reviews
            (id, task_id, delegation_id, reviewer_agent, submission_ref, decision, notes, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            rv.id,
            rv.task_id,
            rv.delegation_id,
            rv.reviewer_agent,
            rv.submission_ref,
            rv.decision.as_str(),
            rv.notes,
            rv.created_at,
        ],
    )?;
    Ok(())
}

fn row_to_review(row: &rusqlite::Row) -> rusqlite::Result<TaskReview> {
    let decision_s: String = row.get("decision")?;
    Ok(TaskReview {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        delegation_id: row.get("delegation_id")?,
        reviewer_agent: row.get("reviewer_agent")?,
        submission_ref: row.get("submission_ref")?,
        decision: ReviewDecision::parse(&decision_s).unwrap_or(ReviewDecision::RequestChanges),
        notes: row.get("notes")?,
        created_at: row.get("created_at")?,
    })
}

pub fn list_reviews_for_task(conn: &Connection, task_id: &str) -> Result<Vec<TaskReview>> {
    let mut stmt = conn.prepare(
        "SELECT id, task_id, delegation_id, reviewer_agent, submission_ref, decision, notes, created_at
         FROM task_reviews WHERE task_id = ?1 ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![task_id], row_to_review)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Persist a review and its signed result, then apply all linked lifecycle
/// transitions in one immediate transaction.
pub fn record_review_result(
    conn: &Connection,
    review: &TaskReview,
    result_message: &Message,
) -> Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let delegation_id = review
        .delegation_id
        .as_deref()
        .ok_or_else(|| TrelaneError::msg("delegated review requires a delegation id"))?;
    let delegation = get_delegation(&tx, delegation_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no delegation '{delegation_id}'")))?;
    let task = get_task(&tx, &review.task_id)?
        .ok_or_else(|| TrelaneError::msg(format!("no task '{}'", review.task_id)))?;
    let submission = get_submission(&tx, &review.submission_ref)?
        .ok_or_else(|| TrelaneError::msg("review submission disappeared"))?;
    if task.state != TaskState::Review
        || delegation.status != DelegationStatus::Submitted
        || submission.status != "pending"
        || submission.task_id != task.id
        || submission.delegation_id != delegation.id
    {
        return Err(TrelaneError::msg(
            "task, delegation, or submission is no longer awaiting review",
        ));
    }

    insert_review(&tx, review)?;
    insert_message(&tx, result_message)?;
    let (task_state, delegation_status, submission_status, assignment_state) = match review.decision
    {
        ReviewDecision::Accept => (
            TaskState::Done,
            DelegationStatus::Accepted,
            "accepted",
            "completed",
        ),
        ReviewDecision::RequestChanges => (
            TaskState::Active,
            DelegationStatus::Active,
            "changes-requested",
            "active",
        ),
        ReviewDecision::Reject => (
            TaskState::Ready,
            DelegationStatus::Rejected,
            "rejected",
            "rejected",
        ),
    };
    if !set_task_state_if(
        &tx,
        &task.id,
        TaskState::Review,
        task_state,
        &review.created_at,
    )? || !set_delegation_status_if(
        &tx,
        &delegation.id,
        DelegationStatus::Submitted,
        delegation_status,
    )? {
        return Err(TrelaneError::msg(
            "review lost a concurrent state transition",
        ));
    }
    tx.execute(
        "UPDATE task_submissions SET status = ?2, reviewed_at = ?3 WHERE id = ?1 AND status = 'pending'",
        params![submission.id, submission_status, review.created_at],
    )?;
    tx.execute(
        "UPDATE task_assignments SET state = ?2, completed_at = ?3 WHERE delegation_id = ?1",
        params![
            delegation.id,
            assignment_state,
            if review.decision == ReviewDecision::RequestChanges {
                None::<&str>
            } else {
                Some(review.created_at.as_str())
            }
        ],
    )?;
    tx.execute(
        "DELETE FROM claims WHERE delegation_id = ?1",
        params![delegation.id],
    )?;

    if review.decision != ReviewDecision::RequestChanges {
        let mut stmt = tx.prepare(
            "SELECT id FROM delegations
             WHERE task_id = ?1 AND id != ?2 AND status IN ('offered', 'active', 'submitted')",
        )?;
        let rows = stmt.query_map(params![task.id, delegation.id], |r| r.get::<_, String>(0))?;
        let mut residual = Vec::new();
        for row in rows {
            residual.push(row?);
        }
        drop(stmt);
        for id in residual {
            tx.execute(
                "UPDATE delegations SET status = 'revoked' WHERE id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM claims WHERE delegation_id = ?1", params![id])?;
            tx.execute(
                "UPDATE task_assignments SET state = 'revoked', completed_at = ?2
                 WHERE delegation_id = ?1",
                params![id, review.created_at],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

// --------------------------------------------------- C3 assist-discovery state
//
// Durable anti-churn state for bounded assist discovery. These are pure
// persistence helpers; the scheduler decides when to call them.

/// Stable backlog fingerprint: sorted, deduplicated, NUL-separated task IDs
/// hashed with SHA-256 and hex-encoded. Same task set always yields the same
/// fingerprint regardless of insertion/query order.
pub fn assist_backlog_fingerprint(tasks: &[Task]) -> String {
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    for id in &ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\0");
    }
    crate::crypto::hex_encode(&hasher.finalize())
}

/// Ready tasks owned by `agent` with all dependencies satisfied.
pub fn list_ready_owned_tasks(conn: &Connection, agent: &str) -> Result<Vec<Task>> {
    let done = done_task_ids(conn)?;
    let all = list_tasks_for_owner(conn, agent)?;
    Ok(all
        .into_iter()
        .filter(|t| t.state == TaskState::Ready && t.deps_satisfied(&done))
        .collect())
}

/// Active helper assignments with their task and delegation, where the
/// delegation is active and unexpired.
pub fn list_runnable_helper_assignments(
    conn: &Connection,
    agent: &str,
    now: &str,
) -> Result<Vec<(TaskAssignment, Task, Delegation)>> {
    let assignments = list_assignments_for_agent(conn, agent)?;
    let mut out = Vec::new();
    for a in &assignments {
        if a.role != TaskRole::Helper || a.state != "active" {
            continue;
        }
        let Some(del_id) = &a.delegation_id else {
            continue;
        };
        let Some(delegation) = get_delegation(conn, del_id)? else {
            continue;
        };
        if delegation.status != DelegationStatus::Active {
            continue;
        }
        if let Some(ref exp) = delegation.expires_at {
            if exp.as_str() <= now {
                continue;
            }
        }
        let Some(task) = get_task(conn, &a.task_id)? else {
            continue;
        };
        if task.state.is_terminal() || task.state == TaskState::Review {
            continue;
        }
        out.push((a.clone(), task, delegation));
    }
    Ok(out)
}

/// Tasks open to assistance from `helper`: not owned by helper, open policy,
/// non-terminal, not in review, dependencies satisfied if ready.
pub fn list_assistable_tasks(conn: &Connection, helper: &str, _now: &str) -> Result<Vec<Task>> {
    let done = done_task_ids(conn)?;
    let all = list_tasks(conn)?;
    Ok(all
        .into_iter()
        .filter(|t| {
            t.owner_agent != helper
                && t.assist_policy == AssistPolicy::Open
                && !t.state.is_terminal()
                && t.state != TaskState::Review
                && t.state != TaskState::Draft
                && t.deps_satisfied(&done)
        })
        .collect())
}

/// Count of `offered` delegations for this helper (outstanding offers).
pub fn count_outstanding_offers_for_helper(conn: &Connection, helper: &str) -> Result<usize> {
    let sql = "SELECT COUNT(*) FROM delegations WHERE helper_agent = ?1 AND status = 'offered'";
    let n: i64 = conn.query_row(sql, params![helper], |r| r.get(0))?;
    Ok(n as usize)
}

pub fn get_assist_discovery_state(
    conn: &Connection,
    helper: &str,
) -> Result<Option<AssistDiscoveryState>> {
    let result = conn
        .query_row(
            "SELECT helper_agent, last_discovery_at, cooldown_until,
                    last_offered_fingerprint, last_offer_id, updated_at
             FROM assist_discovery_state WHERE helper_agent = ?1",
            params![helper],
            |r| {
                Ok(AssistDiscoveryState {
                    helper_agent: r.get(0)?,
                    last_discovery_at: r.get(1)?,
                    cooldown_until: r.get(2)?,
                    last_offered_fingerprint: r.get(3)?,
                    last_offer_id: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

pub fn record_discovery_wake(
    conn: &Connection,
    helper: &str,
    fingerprint: &str,
    now: &str,
    cooldown_until: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO assist_discovery_state
            (helper_agent, last_discovery_at, cooldown_until,
             last_offered_fingerprint, last_offer_id, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5)
         ON CONFLICT(helper_agent) DO UPDATE SET
            last_discovery_at = excluded.last_discovery_at,
            cooldown_until = excluded.cooldown_until,
            updated_at = excluded.updated_at",
        params![helper, now, cooldown_until, fingerprint, now],
    )?;
    Ok(())
}

pub fn record_offer_fingerprint(
    conn: &Connection,
    helper: &str,
    fingerprint: &str,
    offer_id: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO assist_discovery_state
            (helper_agent, last_discovery_at, cooldown_until,
             last_offered_fingerprint, last_offer_id, updated_at)
         VALUES (?1, NULL, NULL, ?2, ?3, ?4)
         ON CONFLICT(helper_agent) DO UPDATE SET
            last_offered_fingerprint = excluded.last_offered_fingerprint,
            last_offer_id = excluded.last_offer_id,
            updated_at = excluded.updated_at",
        params![helper, fingerprint, offer_id, now],
    )?;
    Ok(())
}

pub fn record_rejection_backoff(
    conn: &Connection,
    helper: &str,
    owner: &str,
    now: &str,
) -> Result<()> {
    // Exponential backoff: 5m, 10m, 20m, 40m, ... capped at 24h.
    let existing: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT rejection_count, retry_after FROM assist_rejection_backoff
             WHERE helper_agent = ?1 AND owner_agent = ?2",
            params![helper, owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let count = existing.map(|(c, _)| c + 1).unwrap_or(1);
    let base_secs: u64 = 300;
    let max_secs: u64 = 86400;
    let multiplier = 1u64 << (count as u32 - 1).min(20);
    let delay = (base_secs * multiplier).min(max_secs);
    let retry_after = chrono::DateTime::parse_from_rfc3339(now)
        .ok()
        .and_then(|dt| {
            dt.checked_add_signed(chrono::Duration::seconds(delay as i64))
        })
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    conn.execute(
        "INSERT INTO assist_rejection_backoff
            (helper_agent, owner_agent, rejection_count, last_rejected_at, retry_after)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(helper_agent, owner_agent) DO UPDATE SET
            rejection_count = excluded.rejection_count,
            last_rejected_at = excluded.last_rejected_at,
            retry_after = excluded.retry_after",
        params![helper, owner, count, now, retry_after],
    )?;
    Ok(())
}

pub fn clear_rejection_backoff(conn: &Connection, helper: &str, owner: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM assist_rejection_backoff WHERE helper_agent = ?1 AND owner_agent = ?2",
        params![helper, owner],
    )?;
    Ok(())
}

pub fn rejection_backoff_active(
    conn: &Connection,
    helper: &str,
    owner: &str,
    now: &str,
) -> Result<bool> {
    let result = conn
        .query_row(
            "SELECT retry_after FROM assist_rejection_backoff
             WHERE helper_agent = ?1 AND owner_agent = ?2",
            params![helper, owner],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    match result {
        Some(Some(retry)) => Ok(retry.as_str() > now),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::*;

    fn conn() -> Connection {
        crate::db::open_in_memory().unwrap()
    }

    fn sample_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            owner_agent: "alpha".to_string(),
            domain: "alpha".to_string(),
            parent_task: None,
            subject: "do the thing".to_string(),
            body: "details".to_string(),
            state: TaskState::Ready,
            priority: "high".to_string(),
            assist_policy: AssistPolicy::Open,
            desired_parallelism: 2,
            path_scope: vec!["src/alpha/**".to_string()],
            acceptance: vec!["tests pass".to_string()],
            blocked_by: vec![],
            created_at: "2026-07-11T00:00:00Z".to_string(),
            updated_at: "2026-07-11T00:00:00Z".to_string(),
        }
    }

    fn sample_delegation(id: &str, helper: &str, status: DelegationStatus) -> Delegation {
        Delegation {
            id: id.to_string(),
            task_id: "task_1".to_string(),
            owner_agent: "alpha".to_string(),
            helper_agent: helper.to_string(),
            scope: vec!["src/alpha/**".to_string()],
            allowed_ops: vec!["write".to_string()],
            constraints_json: "{}".to_string(),
            base_revision: Some("base".to_string()),
            offer_message: format!("offer_{id}"),
            grant_message: if status == DelegationStatus::Offered {
                String::new()
            } else {
                format!("grant_{id}")
            },
            issued_at: "2026-07-11T00:00:00Z".to_string(),
            expires_at: Some("2099-07-11T00:00:00Z".to_string()),
            status,
        }
    }

    fn message(id: &str, kind: &str) -> Message {
        Message::new(
            id.to_string(),
            "alpha".to_string(),
            "beta".to_string(),
            kind.to_string(),
            "normal".to_string(),
            kind.to_string(),
            "{}".to_string(),
            None,
            Some("task_1".to_string()),
            vec![],
            "2026-07-11T00:00:00Z".to_string(),
        )
    }

    fn submitted_conn() -> Connection {
        let c = conn();
        let mut task = sample_task("task_1");
        task.state = TaskState::Active;
        insert_task(&c, &task).unwrap();
        insert_delegation(
            &c,
            &sample_delegation("del_1", "beta", DelegationStatus::Active),
        )
        .unwrap();
        upsert_assignment(
            &c,
            &TaskAssignment {
                task_id: "task_1".to_string(),
                agent: "beta".to_string(),
                role: TaskRole::Helper,
                state: "active".to_string(),
                offer_id: Some("offer_del_1".to_string()),
                delegation_id: Some("del_1".to_string()),
                started_at: Some("t0".to_string()),
                completed_at: None,
            },
        )
        .unwrap();
        record_submission(
            &c,
            &TaskSubmission {
                id: "sub_1".to_string(),
                task_id: "task_1".to_string(),
                delegation_id: "del_1".to_string(),
                helper_agent: "beta".to_string(),
                commit_ref: "commit".to_string(),
                base_revision: "base".to_string(),
                summary: "done".to_string(),
                tests: "cargo test".to_string(),
                changed_paths: vec!["src/alpha/a.rs".to_string()],
                message_id: "submission_msg".to_string(),
                status: "pending".to_string(),
                created_at: "2026-07-11T02:00:00Z".to_string(),
                reviewed_at: None,
            },
            &message("submission_msg", "submission"),
        )
        .unwrap();
        c
    }

    #[test]
    fn task_round_trips_all_fields() {
        let c = conn();
        insert_task(&c, &sample_task("task_1")).unwrap();
        let got = get_task(&c, "task_1").unwrap().expect("task should exist");
        assert_eq!(got.owner_agent, "alpha");
        assert_eq!(got.state, TaskState::Ready);
        assert_eq!(got.assist_policy, AssistPolicy::Open);
        assert_eq!(got.desired_parallelism, 2);
        assert_eq!(got.path_scope, vec!["src/alpha/**".to_string()]);
        assert_eq!(got.acceptance, vec!["tests pass".to_string()]);
        assert_eq!(got.priority, "high");
    }

    #[test]
    fn set_task_state_transitions_and_lists() {
        let c = conn();
        insert_task(&c, &sample_task("task_1")).unwrap();
        assert_eq!(list_tasks_in_state(&c, TaskState::Ready).unwrap().len(), 1);
        assert!(set_task_state(&c, "task_1", TaskState::Done, "2026-07-11T01:00:00Z").unwrap());
        assert_eq!(list_tasks_in_state(&c, TaskState::Ready).unwrap().len(), 0);
        assert_eq!(list_tasks_in_state(&c, TaskState::Done).unwrap().len(), 1);
        assert!(done_task_ids(&c).unwrap().contains("task_1"));
    }

    #[test]
    fn dependencies_gate_readiness() {
        let c = conn();
        let mut child = sample_task("child");
        child.blocked_by = vec!["parent".to_string()];
        insert_task(&c, &sample_task("parent")).unwrap();
        insert_task(&c, &child).unwrap();
        let done = done_task_ids(&c).unwrap();
        assert!(
            !get_task(&c, "child")
                .unwrap()
                .unwrap()
                .deps_satisfied(&done)
        );
        set_task_state(&c, "parent", TaskState::Done, "x").unwrap();
        let done = done_task_ids(&c).unwrap();
        assert!(
            get_task(&c, "child")
                .unwrap()
                .unwrap()
                .deps_satisfied(&done)
        );
    }

    #[test]
    fn assignment_upsert_is_idempotent_per_role() {
        let c = conn();
        insert_task(&c, &sample_task("task_1")).unwrap();
        let mut a = TaskAssignment {
            task_id: "task_1".to_string(),
            agent: "beta".to_string(),
            role: TaskRole::Helper,
            state: "active".to_string(),
            offer_id: None,
            delegation_id: None,
            started_at: Some("t0".to_string()),
            completed_at: None,
        };
        upsert_assignment(&c, &a).unwrap();
        a.completed_at = Some("t1".to_string());
        a.state = "completed".to_string();
        upsert_assignment(&c, &a).unwrap();
        let list = list_assignments_for_task(&c, "task_1").unwrap();
        assert_eq!(list.len(), 1, "same (task,agent,role) upserts in place");
        assert_eq!(list[0].state, "completed");
        assert_eq!(list[0].completed_at.as_deref(), Some("t1"));
    }

    #[test]
    fn delegation_and_review_round_trip() {
        let c = conn();
        insert_task(&c, &sample_task("task_1")).unwrap();
        let d = Delegation {
            id: "del_1".to_string(),
            task_id: "task_1".to_string(),
            owner_agent: "alpha".to_string(),
            helper_agent: "beta".to_string(),
            scope: vec!["src/alpha/util.rs".to_string()],
            allowed_ops: vec!["edit".to_string()],
            constraints_json: "{\"tests_only\":true}".to_string(),
            base_revision: Some("abc123".to_string()),
            offer_message: "offer_1".to_string(),
            grant_message: "help with util".to_string(),
            issued_at: "t0".to_string(),
            expires_at: Some("t9".to_string()),
            status: DelegationStatus::Active,
        };
        insert_delegation(&c, &d).unwrap();
        let got = get_delegation(&c, "del_1").unwrap().unwrap();
        assert_eq!(got.helper_agent, "beta");
        assert_eq!(got.scope, vec!["src/alpha/util.rs".to_string()]);
        assert!(got.status.is_live());
        assert!(set_delegation_status(&c, "del_1", DelegationStatus::Revoked).unwrap());
        assert!(
            !get_delegation(&c, "del_1")
                .unwrap()
                .unwrap()
                .status
                .is_live()
        );

        let rv = TaskReview {
            id: "rev_1".to_string(),
            task_id: "task_1".to_string(),
            delegation_id: Some("del_1".to_string()),
            reviewer_agent: "alpha".to_string(),
            submission_ref: "patch:1".to_string(),
            decision: ReviewDecision::Accept,
            notes: "lgtm".to_string(),
            created_at: "t2".to_string(),
        };
        insert_review(&c, &rv).unwrap();
        let reviews = list_reviews_for_task(&c, "task_1").unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].decision, ReviewDecision::Accept);
    }

    #[test]
    fn atomic_assignment_enforces_desired_parallelism_capacity() {
        let c = conn();
        let mut task = sample_task("task_1");
        task.desired_parallelism = 1;
        insert_task(&c, &task).unwrap();
        insert_delegation(
            &c,
            &sample_delegation("del_1", "beta", DelegationStatus::Offered),
        )
        .unwrap();
        insert_delegation(
            &c,
            &sample_delegation("del_2", "gamma", DelegationStatus::Offered),
        )
        .unwrap();

        activate_delegation_and_assign(
            &c,
            "del_1",
            &["src/alpha/tests/**".to_string()],
            &["write".to_string()],
            "2099-07-11T00:00:00Z",
            &message("grant_1", "help-accept"),
            "2026-07-11T01:00:00Z",
        )
        .unwrap();
        let error = activate_delegation_and_assign(
            &c,
            "del_2",
            &["src/alpha/tests/**".to_string()],
            &["write".to_string()],
            "2099-07-11T00:00:00Z",
            &message("grant_2", "help-accept"),
            "2026-07-11T01:00:00Z",
        )
        .unwrap_err();
        assert!(error.to_string().contains("capacity is full"));
        assert_eq!(
            get_delegation(&c, "del_2").unwrap().unwrap().status,
            DelegationStatus::Offered
        );
        assert_eq!(list_assignments_for_task(&c, "task_1").unwrap().len(), 1);
    }

    #[test]
    fn expiration_revokes_authority_and_releases_linked_claims() {
        let c = conn();
        insert_task(&c, &sample_task("task_1")).unwrap();
        let mut delegation = sample_delegation("del_1", "beta", DelegationStatus::Active);
        delegation.expires_at = Some("2020-01-01T00:00:00Z".to_string());
        insert_delegation(&c, &delegation).unwrap();
        insert_claim(
            &c,
            &Lease {
                path: "src/alpha/tests/a.rs".to_string(),
                holder: "beta".to_string(),
                task: Some("task_1".to_string()),
                grant: Some("grant_del_1".to_string()),
                delegation_id: Some("del_1".to_string()),
                acquired_at: "2019-01-01T00:00:00Z".to_string(),
                expires_at: 4_102_444_800.0,
                expires_human: "2099-12-31T00:00:00Z".to_string(),
                contested: true,
            },
        )
        .unwrap();
        assert_eq!(
            expire_stale_delegations(&c, "2026-07-11T00:00:00Z").unwrap(),
            1
        );
        assert_eq!(
            get_delegation(&c, "del_1").unwrap().unwrap().status,
            DelegationStatus::Expired
        );
        assert!(get_claim(&c, "src/alpha/tests/a.rs").unwrap().is_none());
    }

    #[test]
    fn submission_and_accept_review_lifecycle_is_atomic() {
        let c = conn();
        let mut task = sample_task("task_1");
        task.state = TaskState::Active;
        insert_task(&c, &task).unwrap();
        insert_delegation(
            &c,
            &sample_delegation("del_1", "beta", DelegationStatus::Active),
        )
        .unwrap();
        upsert_assignment(
            &c,
            &TaskAssignment {
                task_id: "task_1".to_string(),
                agent: "beta".to_string(),
                role: TaskRole::Helper,
                state: "active".to_string(),
                offer_id: Some("offer_del_1".to_string()),
                delegation_id: Some("del_1".to_string()),
                started_at: Some("t0".to_string()),
                completed_at: None,
            },
        )
        .unwrap();
        let submission = TaskSubmission {
            id: "sub_1".to_string(),
            task_id: "task_1".to_string(),
            delegation_id: "del_1".to_string(),
            helper_agent: "beta".to_string(),
            commit_ref: "commit".to_string(),
            base_revision: "base".to_string(),
            summary: "done".to_string(),
            tests: "cargo test".to_string(),
            changed_paths: vec!["src/alpha/a.rs".to_string()],
            message_id: "submission_msg".to_string(),
            status: "pending".to_string(),
            created_at: "2026-07-11T02:00:00Z".to_string(),
            reviewed_at: None,
        };
        record_submission(&c, &submission, &message("submission_msg", "submission")).unwrap();
        assert_eq!(
            get_task(&c, "task_1").unwrap().unwrap().state,
            TaskState::Review
        );
        assert_eq!(
            get_delegation(&c, "del_1").unwrap().unwrap().status,
            DelegationStatus::Submitted
        );

        let review = TaskReview {
            id: "review_1".to_string(),
            task_id: "task_1".to_string(),
            delegation_id: Some("del_1".to_string()),
            reviewer_agent: "alpha".to_string(),
            submission_ref: "sub_1".to_string(),
            decision: ReviewDecision::Accept,
            notes: "lgtm".to_string(),
            created_at: "2026-07-11T03:00:00Z".to_string(),
        };
        record_review_result(&c, &review, &message("review_msg", "review-result")).unwrap();
        assert_eq!(
            get_task(&c, "task_1").unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(
            get_delegation(&c, "del_1").unwrap().unwrap().status,
            DelegationStatus::Accepted
        );
        assert_eq!(
            get_submission(&c, "sub_1").unwrap().unwrap().status,
            "accepted"
        );
    }

    #[test]
    fn request_changes_reactivates_and_reject_returns_task_to_ready() {
        let changes = submitted_conn();
        record_review_result(
            &changes,
            &TaskReview {
                id: "review_changes".to_string(),
                task_id: "task_1".to_string(),
                delegation_id: Some("del_1".to_string()),
                reviewer_agent: "alpha".to_string(),
                submission_ref: "sub_1".to_string(),
                decision: ReviewDecision::RequestChanges,
                notes: "add an edge case".to_string(),
                created_at: "2026-07-11T03:00:00Z".to_string(),
            },
            &message("review_changes_msg", "review-result"),
        )
        .unwrap();
        assert_eq!(
            get_task(&changes, "task_1").unwrap().unwrap().state,
            TaskState::Active
        );
        assert_eq!(
            get_delegation(&changes, "del_1").unwrap().unwrap().status,
            DelegationStatus::Active
        );
        assert_eq!(
            get_submission(&changes, "sub_1").unwrap().unwrap().status,
            "changes-requested"
        );

        let rejected = submitted_conn();
        record_review_result(
            &rejected,
            &TaskReview {
                id: "review_reject".to_string(),
                task_id: "task_1".to_string(),
                delegation_id: Some("del_1".to_string()),
                reviewer_agent: "alpha".to_string(),
                submission_ref: "sub_1".to_string(),
                decision: ReviewDecision::Reject,
                notes: "wrong approach".to_string(),
                created_at: "2026-07-11T03:00:00Z".to_string(),
            },
            &message("review_reject_msg", "review-result"),
        )
        .unwrap();
        assert_eq!(
            get_task(&rejected, "task_1").unwrap().unwrap().state,
            TaskState::Ready
        );
        assert_eq!(
            get_delegation(&rejected, "del_1").unwrap().unwrap().status,
            DelegationStatus::Rejected
        );
    }
}
