//! Slice 4A: Domain Intrusion (R9, R10, R11, R25, R26).
//!
//! A narrow, ad hoc way for an agent to write inside another agent's domain:
//! `di request` opens a request and parks the requester; any *other* enabled
//! agent may approve (R9 -- permission never stalls on one specific agent);
//! the registered domain owner may veto, and a veto always wins regardless of
//! approvals or arrival order. Resolution is evaluated fresh every squire
//! tick against durable state, never against which message arrived first.
//!
//! Outcomes: Pending -> Approved (owner approval, or a standing non-owner
//! approval past the objection window), Pending -> Vetoed (owner veto),
//! Pending -> Expired (silence past `di.request_timeout_s` -- R25: silence is
//! not permission). Approval is *permission only*; the requester still takes
//! a normal claim before writing (R10), and `.trelane/**`/`.git/**` stay
//! forbidden no matter what was approved (R11). If that claim loses the lease
//! race, the requester parks on the contention (`claim-contested`, R26).

use crate::Context;
use crate::error::{Result, TrelaneError};
use crate::models::Message;
use crate::{crypto, store};
use rusqlite::{OptionalExtension, params};

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_APPROVED: &str = "approved";
pub const STATUS_VETOED: &str = "vetoed";
pub const STATUS_EXPIRED: &str = "expired";

#[derive(Debug, Clone)]
pub struct DiRequest {
    pub id: String,
    pub requester_agent: String,
    pub target_domain: String,
    pub path_glob: String,
    pub purpose: String,
    pub status: String,
    pub created_at: String,
    pub objection_deadline: String,
    pub resolved_at: Option<String>,
    pub veto_agent: Option<String>,
    pub veto_reason: Option<String>,
    pub approvals: Vec<String>,
}

fn row_to_request(
    conn: &rusqlite::Connection,
    row: &rusqlite::Row,
) -> rusqlite::Result<DiRequest> {
    let id: String = row.get("id")?;
    let approvals = approvals_of(conn, &id).unwrap_or_default();
    Ok(DiRequest {
        id,
        requester_agent: row.get("requester_agent")?,
        target_domain: row.get("target_domain")?,
        path_glob: row.get("path_glob")?,
        purpose: row.get("purpose")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        objection_deadline: row.get("objection_deadline")?,
        resolved_at: row.get("resolved_at")?,
        veto_agent: row.get("veto_agent")?,
        veto_reason: row.get("veto_reason")?,
        approvals,
    })
}

const REQUEST_COLS: &str =
    "id, requester_agent, target_domain, path_glob, purpose, status, created_at, \
     objection_deadline, resolved_at, veto_agent, veto_reason";

fn approvals_of(conn: &rusqlite::Connection, request_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT agent FROM domain_intrusion_approvals WHERE request_id = ?1 ORDER BY created_at",
    )?;
    let rows = stmt.query_map(params![request_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn get_request(conn: &rusqlite::Connection, id: &str) -> Result<Option<DiRequest>> {
    let result = conn
        .query_row(
            &format!("SELECT {REQUEST_COLS} FROM domain_intrusion_requests WHERE id = ?1"),
            params![id],
            |row| row_to_request(conn, row),
        )
        .optional()?;
    Ok(result)
}

pub fn list_requests(
    conn: &rusqlite::Connection,
    status: Option<&str>,
) -> Result<Vec<DiRequest>> {
    let (sql, with_status) = match status {
        Some(_) => (
            format!("SELECT {REQUEST_COLS} FROM domain_intrusion_requests WHERE status = ?1 ORDER BY created_at"),
            true,
        ),
        None => (
            format!("SELECT {REQUEST_COLS} FROM domain_intrusion_requests ORDER BY created_at"),
            false,
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<DiRequest> = if with_status {
        stmt.query_map(params![status.unwrap()], |row| row_to_request(conn, row))?
            .filter_map(|r| r.ok())
            .collect()
    } else {
        stmt.query_map([], |row| row_to_request(conn, row))?
            .filter_map(|r| r.ok())
            .collect()
    };
    Ok(rows)
}

fn agent_enabled(conn: &rusqlite::Connection, agent: &str) -> Result<bool> {
    Ok(store::agent_exists(conn, agent)?
        && store::session_agent_enabled(conn, agent)?.unwrap_or(true))
}

fn iso_plus(now_iso: &str, seconds: u64) -> String {
    let now = chrono::DateTime::parse_from_rfc3339(now_iso)
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|_| chrono::Utc::now());
    (now + chrono::Duration::seconds(seconds as i64))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// R11: reject requests whose glob targets permission-proof paths. The glob
/// is a pattern, not a concrete path, so this checks the literal prefix; the
/// claim-time hard-forbidden guard remains the absolute enforcement.
fn glob_targets_forbidden(glob: &str) -> bool {
    let g = glob.trim_start_matches('/');
    g == ".trelane"
        || g == ".git"
        || g.starts_with(".trelane/")
        || g.starts_with(".git/")
}

/// Open a DI request: validate, record, broadcast to all enabled agents, and
/// park the requester on the outcome. Returns the new request id.
pub fn create_request(
    ctx: &Context,
    requester: &str,
    target_domain: &str,
    path_glob: &str,
    purpose: &str,
) -> Result<String> {
    if !agent_enabled(&ctx.conn, requester)? {
        return Err(TrelaneError::msg(format!(
            "unknown or disabled agent '{requester}'"
        )));
    }
    if !store::agent_exists(&ctx.conn, target_domain)? {
        return Err(TrelaneError::msg(format!(
            "unknown target domain '{target_domain}' (domains are named by their owning agent)"
        )));
    }
    if requester == target_domain {
        return Err(TrelaneError::msg(
            "that is your own domain -- no intrusion request needed",
        ));
    }
    let glob = path_glob.trim();
    if glob.is_empty() {
        return Err(TrelaneError::msg("--path glob cannot be empty"));
    }
    if glob_targets_forbidden(glob) {
        return Err(TrelaneError::msg(format!(
            "'{glob}' targets permission-proof paths (.trelane/** or .git/**); \
             no approval can ever make those writable (R11)"
        )));
    }
    let purpose = purpose.trim();
    if purpose.is_empty() {
        return Err(TrelaneError::msg(
            "--purpose is required and must be specific: state exactly what you \
             intend to do and why",
        ));
    }
    // The requested glob must actually be inside the target's domain --
    // otherwise there is nothing to intrude upon.
    let owner_dom = store::get_domain(&ctx.conn, target_domain)?
        .ok_or_else(|| TrelaneError::msg("target domain not found"))?;
    if !crate::domain::domain_allows_scope(&owner_dom, glob)? {
        return Err(TrelaneError::msg(format!(
            "'{glob}' is not inside {target_domain}'s domain"
        )));
    }

    let now = crypto::now_iso();
    let id = crypto::new_id("di");
    let deadline = iso_plus(&now, ctx.config.di.objection_window_s);
    ctx.conn.execute(
        "INSERT INTO domain_intrusion_requests
            (id, requester_agent, target_domain, path_glob, purpose, status,
             created_at, objection_deadline)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
        params![id, requester, target_domain, glob, purpose, now, deadline],
    )?;

    // Broadcast to every other enabled agent (R9: any of them may approve).
    let secret = ctx.secret()?;
    for agent in store::list_agents(&ctx.conn)? {
        if agent == requester || !agent_enabled(&ctx.conn, &agent)? {
            continue;
        }
        let mut msg = Message::new(
            crypto::new_id("msg"),
            requester.to_string(),
            agent.clone(),
            "di_request".to_string(),
            "normal".to_string(),
            format!("DI request {id}: {glob} in {target_domain}"),
            format!(
                "domain-intrusion request {id}\nrequester: {requester}\ntarget domain: {target_domain}\n\
                 path: {glob}\npurpose: {purpose}\nobjection deadline: {deadline}\n\n\
                 Any enabled agent may approve:  trelane di approve {id} --from <you>\n\
                 Only the domain owner may veto:  trelane di deny {id} --from {target_domain} --reason \"...\""
            ),
            None,
            None,
            vec![glob.to_string()],
            now.clone(),
        );
        crypto::sign(&secret, &mut msg);
        store::insert_message(&ctx.conn, &msg)?;
    }

    // Park the requester on the outcome (the protocol does this for you).
    store::insert_parked_task(
        &ctx.conn,
        &crate::models::ParkedTask {
            task: id.clone(),
            agent: requester.to_string(),
            wait_type: "di_request".to_string(),
            wait_re: Some(id.clone()),
            wait_path: None,
            waiting_on: target_domain.to_string(),
            resume_hint: format!(
                "if approved: `trelane claim {requester} {glob}` before writing (R10); \
                 if the lease is contested, park with --wait-contested-claim (R26)"
            ),
            created_at: now,
        },
    )?;

    emit_di_span(ctx, "di.request", &id, requester, "pending");
    Ok(id)
}

/// Record an approval. Any enabled agent except the requester may approve
/// (R9). Resolution itself happens in the squire tick, against current state.
pub fn approve(ctx: &Context, id: &str, agent: &str) -> Result<()> {
    let req = get_request(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown DI request '{id}'")))?;
    if req.status != STATUS_PENDING {
        return Err(TrelaneError::msg(format!(
            "DI request '{id}' is already {}",
            req.status
        )));
    }
    if agent == req.requester_agent {
        return Err(TrelaneError::msg(
            "the requester cannot approve their own request",
        ));
    }
    if !agent_enabled(&ctx.conn, agent)? {
        return Err(TrelaneError::msg(format!(
            "unknown or disabled agent '{agent}'"
        )));
    }
    ctx.conn.execute(
        "INSERT OR IGNORE INTO domain_intrusion_approvals (request_id, agent, created_at)
         VALUES (?1, ?2, ?3)",
        params![id, agent, crypto::now_iso()],
    )?;
    emit_di_span(ctx, "di.approve", id, agent, "");
    Ok(())
}

/// Record the domain owner's veto. Only the registered owner of the target
/// domain may veto, and a reason is required (TMP).
pub fn deny(ctx: &Context, id: &str, agent: &str, reason: &str) -> Result<()> {
    let req = get_request(&ctx.conn, id)?
        .ok_or_else(|| TrelaneError::msg(format!("unknown DI request '{id}'")))?;
    if req.status != STATUS_PENDING {
        return Err(TrelaneError::msg(format!(
            "DI request '{id}' is already {}",
            req.status
        )));
    }
    if agent != req.target_domain {
        return Err(TrelaneError::msg(format!(
            "only the domain owner ({}) may veto DI request '{id}'",
            req.target_domain
        )));
    }
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(TrelaneError::msg("--reason is required for a veto"));
    }
    ctx.conn.execute(
        "UPDATE domain_intrusion_requests SET veto_agent = ?2, veto_reason = ?3
         WHERE id = ?1 AND status = 'pending'",
        params![id, agent, reason],
    )?;
    emit_di_span(ctx, "di.veto", id, agent, reason);
    Ok(())
}

/// Tick-driven resolution (R9/R25): evaluate every pending request against
/// current durable state. Veto present -> Vetoed, full stop, regardless of
/// approvals or arrival order. Owner approval -> Approved immediately (no
/// window needed). A standing non-owner approval past the objection window ->
/// Approved. Silence past `di.request_timeout_s` -> Expired, never Approved.
/// Returns the requests this call resolved.
pub fn resolve_pending(ctx: &Context) -> Result<Vec<DiRequest>> {
    let now = crypto::now_iso();
    let pending = list_requests(&ctx.conn, Some(STATUS_PENDING))?;
    let mut resolved = Vec::new();
    for mut req in pending {
        let outcome = if req.veto_agent.is_some() {
            Some(STATUS_VETOED)
        } else if req.approvals.iter().any(|a| a == &req.target_domain) {
            Some(STATUS_APPROVED)
        } else if !req.approvals.is_empty() && now >= req.objection_deadline {
            Some(STATUS_APPROVED)
        } else if now >= iso_plus(&req.created_at, ctx.config.di.request_timeout_s) {
            Some(STATUS_EXPIRED)
        } else {
            None
        };
        if let Some(status) = outcome {
            ctx.conn.execute(
                "UPDATE domain_intrusion_requests SET status = ?2, resolved_at = ?3
                 WHERE id = ?1 AND status = 'pending'",
                params![req.id, status, now],
            )?;
            req.status = status.to_string();
            req.resolved_at = Some(now.clone());
            emit_di_span(ctx, "di.resolve", &req.id, &req.requester_agent, status);
            resolved.push(req);
        }
    }
    Ok(resolved)
}

/// R10: does this agent hold an approved DI request whose glob covers `rel`?
/// Used by `cmd_claim` to admit DI-approved cross-domain claims without a
/// delegation -- the claim (lease) is still required and still taken.
pub fn has_approved_covering(ctx: &Context, agent: &str, rel: &str) -> Result<bool> {
    let mut stmt = ctx.conn.prepare(
        "SELECT path_glob FROM domain_intrusion_requests
         WHERE requester_agent = ?1 AND status = 'approved'",
    )?;
    let globs: Vec<String> = stmt
        .query_map(params![agent], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    for glob in globs {
        if glob_matches(&glob, rel) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Match a relative path against a DI path glob. Exact paths match
/// themselves; `**` globs behave as usual. A trailing `/` prefix match is
/// also accepted so `src/combat` covers `src/combat/enemy.rs`.
fn glob_matches(glob: &str, rel: &str) -> bool {
    if glob == rel {
        return true;
    }
    if let Some(prefix) = glob.strip_suffix("/**") {
        return rel.starts_with(&format!("{prefix}/"));
    }
    if !glob.contains('*') && rel.starts_with(&format!("{glob}/")) {
        return true;
    }
    globset::Glob::new(glob)
        .map(|g| g.compile_matcher().is_match(rel))
        .unwrap_or(false)
}

/// Best-effort di.* audit span (R16: telemetry never fails the operation).
fn emit_di_span(ctx: &Context, event: &str, request_id: &str, agent: &str, outcome: &str) {
    if let Ok(tracer) =
        crate::telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string())
    {
        let now = crate::telemetry::now_nanos();
        let _ = tracer.record_event(
            event,
            &[
                ("di.request_id", request_id),
                ("di.agent", agent),
                ("di.outcome", outcome),
            ],
            now,
            now,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_agents(temp: &tempfile::TempDir) -> Context {
        let root = temp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        let ctx = Context {
            root,
            conn,
            config: crate::models::Config::default(),
        };
        crate::commands::cmd_add_agent(
            &ctx,
            "owner",
            &["src/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        crate::commands::cmd_add_agent(&ctx, "helper", &["lib/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(&ctx, "third", &["docs/**".to_string()], &[], None, None)
            .unwrap();
        ctx
    }

    fn make_request(ctx: &Context) -> String {
        create_request(
            ctx,
            "helper",
            "owner",
            "src/enemy.rs",
            "add `use crate::combat::Damage` so autoplay can read damage values",
        )
        .unwrap()
    }

    #[test]
    fn request_creates_row_broadcasts_and_parks() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        let id = make_request(&ctx);

        let req = get_request(&ctx.conn, &id).unwrap().unwrap();
        assert_eq!(req.status, STATUS_PENDING);
        assert_eq!(req.requester_agent, "helper");
        assert_eq!(req.target_domain, "owner");

        // Broadcast: owner and third got it, requester did not.
        let owner_inbox = store::get_unprocessed_messages(&ctx.conn, "owner").unwrap();
        assert!(owner_inbox.iter().any(|m| m.msg_type == "di_request"));
        let third_inbox = store::get_unprocessed_messages(&ctx.conn, "third").unwrap();
        assert!(third_inbox.iter().any(|m| m.msg_type == "di_request"));
        let helper_inbox = store::get_unprocessed_messages(&ctx.conn, "helper").unwrap();
        assert!(helper_inbox.is_empty());

        // Requester is parked on the request.
        let parked = store::list_parked_tasks_for_agent(&ctx.conn, "helper").unwrap();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].wait_type, "di_request");
        assert_eq!(parked[0].wait_re.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn non_owner_approval_resolves_after_objection_window() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.objection_window_s = 0; // deadline already passed
        let id = make_request(&ctx);
        approve(&ctx, &id, "third").unwrap();

        let resolved = resolve_pending(&ctx).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].status, STATUS_APPROVED);
        assert_eq!(
            get_request(&ctx.conn, &id).unwrap().unwrap().status,
            STATUS_APPROVED
        );
    }

    #[test]
    fn non_owner_approval_waits_for_window() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.objection_window_s = 3600; // far future
        let id = make_request(&ctx);
        approve(&ctx, &id, "third").unwrap();

        assert!(resolve_pending(&ctx).unwrap().is_empty());
        assert_eq!(
            get_request(&ctx.conn, &id).unwrap().unwrap().status,
            STATUS_PENDING
        );
    }

    #[test]
    fn owner_approval_resolves_immediately_without_window() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.objection_window_s = 3600;
        let id = make_request(&ctx);
        approve(&ctx, &id, "owner").unwrap();

        let resolved = resolve_pending(&ctx).unwrap();
        assert_eq!(resolved[0].status, STATUS_APPROVED);
    }

    #[test]
    fn veto_always_wins_regardless_of_approvals() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.objection_window_s = 0;
        let id = make_request(&ctx);
        approve(&ctx, &id, "third").unwrap();
        deny(&ctx, &id, "owner", "enemy.rs is mid-rewrite").unwrap();

        let resolved = resolve_pending(&ctx).unwrap();
        assert_eq!(resolved[0].status, STATUS_VETOED);
        let req = get_request(&ctx.conn, &id).unwrap().unwrap();
        assert_eq!(req.veto_agent.as_deref(), Some("owner"));
        assert_eq!(req.veto_reason.as_deref(), Some("enemy.rs is mid-rewrite"));
    }

    #[test]
    fn silence_expires_never_approves() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.request_timeout_s = 0; // already too old
        let _id = make_request(&ctx);

        let resolved = resolve_pending(&ctx).unwrap();
        assert_eq!(resolved[0].status, STATUS_EXPIRED);
    }

    #[test]
    fn requester_cannot_self_approve() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        let id = make_request(&ctx);
        assert!(approve(&ctx, &id, "helper").is_err());
    }

    #[test]
    fn non_owner_cannot_veto() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        let id = make_request(&ctx);
        assert!(deny(&ctx, &id, "third", "not my call").is_err());
    }

    #[test]
    fn forbidden_paths_are_unrequestable() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        assert!(
            create_request(&ctx, "helper", "owner", ".trelane/**", "rewrite state").is_err(),
            "R11: .trelane/** must be rejected outright"
        );
        assert!(
            create_request(&ctx, "helper", "owner", ".git/config", "tamper").is_err(),
            "R11: .git/** must be rejected outright"
        );
    }

    #[test]
    fn vague_or_empty_purpose_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        assert!(create_request(&ctx, "helper", "owner", "src/enemy.rs", "  ").is_err());
    }

    #[test]
    fn path_outside_target_domain_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = ctx_with_agents(&temp);
        assert!(
            create_request(&ctx, "helper", "owner", "lib/foo.rs", "not their file").is_err()
        );
    }

    #[test]
    fn approved_request_covers_claim_path() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with_agents(&temp);
        ctx.config.di.objection_window_s = 0;
        let id = make_request(&ctx);
        approve(&ctx, &id, "third").unwrap();
        resolve_pending(&ctx).unwrap();

        assert!(has_approved_covering(&ctx, "helper", "src/enemy.rs").unwrap());
        assert!(!has_approved_covering(&ctx, "helper", "src/other.rs").unwrap());
        assert!(!has_approved_covering(&ctx, "third", "src/enemy.rs").unwrap());
    }

    // -------------------------------------------------- 4A config-inversion guard

    #[test]
    fn di_config_defaults_validate() {
        // The shipped defaults (300 / 3600 / 1800) are a sane configuration:
        // the objection window fits well inside the request lifetime, and the
        // claim-contested timeout does not outlive the request it contests.
        assert!(crate::models::DiConfig::default().validate().is_ok());
    }

    #[test]
    fn di_config_rejects_objection_window_longer_than_request_timeout() {
        let mut di = crate::models::DiConfig::default();
        di.objection_window_s = 7200;
        di.request_timeout_s = 3600;
        let err = di.validate().unwrap_err().to_string();
        assert!(
            err.contains("di.objection_window_s"),
            "names the offending key: {err}"
        );
        assert!(
            err.contains("di.request_timeout_s"),
            "names the compared key: {err}"
        );
        assert!(err.contains("7200"), "shows the offending value: {err}");
        assert!(err.contains("3600"), "shows the bound value: {err}");
    }

    #[test]
    fn di_config_rejects_claim_contested_longer_than_request_timeout() {
        let mut di = crate::models::DiConfig::default();
        di.claim_contested_timeout_s = 5400;
        di.request_timeout_s = 3600;
        let err = di.validate().unwrap_err().to_string();
        assert!(
            err.contains("di.claim_contested_timeout_s"),
            "names the offending key: {err}"
        );
        assert!(
            err.contains("di.request_timeout_s"),
            "names the compared key: {err}"
        );
        assert!(err.contains("5400"), "shows the offending value: {err}");
    }

    #[test]
    fn di_config_accepts_boundary_equality() {
        // Equality is not an inversion: an approval that clears the window at
        // the exact moment the request would expire still resolves (the
        // objection-window check is <=, and resolution is evaluated fresh each
        // tick, so the approval wins the tie).
        let mut di = crate::models::DiConfig::default();
        di.objection_window_s = 3600;
        di.request_timeout_s = 3600;
        di.claim_contested_timeout_s = 3600;
        assert!(di.validate().is_ok());
    }

    #[test]
    fn di_config_rejects_both_inversions_in_one_config() {
        // If both relationships are inverted, the objection-window check fires
        // first (it is checked first in validate()). The error names that
        // relationship; a second set+save would then surface the second one.
        let mut di = crate::models::DiConfig::default();
        di.objection_window_s = 10_000;
        di.claim_contested_timeout_s = 9_000;
        di.request_timeout_s = 5_000;
        let err = di.validate().unwrap_err().to_string();
        assert!(err.contains("di.objection_window_s"));
    }
}
