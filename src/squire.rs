use crate::Context;
use crate::commands;
use crate::error::Result;
use crate::models::{AgentActivityState, AgentStatus, Message, StoryEvent, WakeCandidate, WakeKind};
use crate::prompt;
use crate::store;
use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

type WaitEdges = HashMap<String, HashSet<String>>;
type WaitResult = (WaitEdges, Option<Vec<String>>);

/// A plain, testable snapshot of the squire's concurrency situation on a
/// single tick: how many agents are registered, how many are currently
/// running, the configured simultaneous-execution ceiling, and how many
/// ready-to-wake candidates exist. Registered count and the execution limit
/// are deliberately tracked as *separate* numbers so a swarm with more
/// registered agents than the limit is never mistaken for "broken".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConcurrencyReport {
    /// Total agents registered in this project.
    pub registered: usize,
    /// Agents currently holding a running lock.
    pub running: usize,
    /// Configured simultaneous-execution ceiling (`squire.max_concurrent`).
    pub limit: usize,
    /// Ready-to-wake candidates the squire found this tick.
    pub ready: usize,
    /// Free slots under the limit right now (`limit - running`, floored at 0).
    pub budget: usize,
    /// Ready candidates that must be deferred this tick because the limit is
    /// already met (`ready - budget`, floored at 0).
    pub deferred: usize,
}

impl ConcurrencyReport {
    /// True when there is ready work the concurrency limit is preventing us
    /// from starting right now -- i.e. raising `squire.max_concurrent` (or a
    /// running agent finishing) would let more work proceed immediately.
    pub fn work_exceeds_budget(&self) -> bool {
        self.deferred > 0
    }
}

/// Build a [`ConcurrencyReport`] from raw counts. Pure and total (no I/O),
/// so it can be unit-tested directly and reused by `status`, `tick`, and
/// `config explain` without any of them re-deriving the arithmetic.
pub fn concurrency_report(
    registered: usize,
    running: usize,
    limit: usize,
    ready: usize,
) -> ConcurrencyReport {
    let budget = limit.saturating_sub(running);
    let deferred = ready.saturating_sub(budget);
    ConcurrencyReport {
        registered,
        running,
        limit,
        ready,
        budget,
        deferred,
    }
}

/// Build the wait-for graph from unsatisfied parked tasks and detect cycles.
/// Returns (edges, optional cycle path).
pub fn wait_graph(conn: &Connection) -> Result<WaitResult> {
    let parked = store::list_parked_tasks(conn)?;
    let mut edges: HashMap<String, HashSet<String>> = HashMap::new();

    for e in &parked {
        if !prompt::park_satisfied(conn, e)? {
            edges
                .entry(e.agent.clone())
                .or_default()
                .insert(e.waiting_on.clone());
        }
    }

    let mut visited = HashSet::new();
    let nodes: Vec<String> = edges.keys().cloned().collect();
    for node in &nodes {
        let mut on_stack = Vec::new();
        let mut on_stack_set = HashSet::new();
        if let Some(cycle) = dfs_cycle(node, &edges, &mut visited, &mut on_stack, &mut on_stack_set)
        {
            return Ok((edges, Some(cycle)));
        }
    }

    Ok((edges, None))
}

fn dfs_cycle(
    node: &str,
    edges: &HashMap<String, HashSet<String>>,
    visited: &mut HashSet<String>,
    on_stack: &mut Vec<String>,
    on_stack_set: &mut HashSet<String>,
) -> Option<Vec<String>> {
    if on_stack_set.contains(node) {
        let start = on_stack.iter().position(|n| n == node).unwrap();
        return Some(on_stack[start..].to_vec());
    }
    if visited.contains(node) {
        return None;
    }
    visited.insert(node.to_string());
    on_stack.push(node.to_string());
    on_stack_set.insert(node.to_string());

    if let Some(neighbors) = edges.get(node) {
        for n in neighbors {
            if let Some(cycle) = dfs_cycle(n, edges, visited, on_stack, on_stack_set) {
                return Some(cycle);
            }
        }
    }

    on_stack.pop();
    on_stack_set.remove(node);
    None
}

/// Reap expired leases and send system messages to ex-holders.
pub fn reap_leases(ctx: &Context) -> Result<()> {
    // Delegation expiry is an authority boundary, not merely bookkeeping:
    // the store transition also releases every linked claim atomically.
    store::expire_stale_delegations(&ctx.conn, &crate::crypto::now_iso())?;
    let leases = store::list_claims(&ctx.conn)?;
    let now_ts = chrono::Utc::now().timestamp() as f64;

    for lease in &leases {
        if lease.expires_at < now_ts {
            store::delete_claim(&ctx.conn, &lease.path)?;
            if store::agent_exists(&ctx.conn, &lease.holder)? {
                let mut msg = Message::new(
                    crate::crypto::new_id("msg"),
                    "system".to_string(),
                    lease.holder.clone(),
                    "system".to_string(),
                    "normal".to_string(),
                    format!("lease expired: {}", lease.path),
                    "Your lease expired and was reaped. If you still hold uncommitted work on this file, re-claim before touching it again.".to_string(),
                    None,
                    None,
                    vec![],
                    crate::crypto::now_iso(),
                );
                let secret = ctx.secret()?;
                crate::crypto::sign(&secret, &mut msg);
                store::insert_message(&ctx.conn, &msg)?;
            }
        }
    }

    Ok(())
}

fn urgency_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 3,
        "high" => 2,
        "low" => 0,
        _ => 1,
    }
}

/// A deterministic scheduler plan for one tick. Planning is side-effect-free:
/// abandoned parks are not deleted, cycle-break attempts are not incremented,
/// escalation messages are not sent, and discovery cooldowns are not recorded.
/// All of those are applied in `tick` only after a candidate successfully
/// launches.
pub struct WakePlan {
    pub candidates: Vec<WakeCandidate>,
    /// agent -> parked task IDs to delete after that agent launches.
    pub abandoned_parks: HashMap<String, Vec<String>>,
    /// Cycle-break plan to execute after the breaker launches.
    pub cycle: Option<CycleBreakPlan>,
}

pub struct CycleBreakPlan {
    pub cycle_key: String,
    pub cycle_members: Vec<String>,
    pub designated_breaker: String,
    pub current_attempt_count: i64,
    pub should_escalate: bool,
    pub alt_breaker: Option<String>,
}

/// Build a deterministic, side-effect-free wake plan.
pub fn wake_plan(ctx: &Context) -> Result<WakePlan> {
    let now = crate::crypto::now_iso();
    let mut cands: Vec<WakeCandidate> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut abandoned_parks: HashMap<String, Vec<String>> = HashMap::new();
    let agents = store::list_agents(&ctx.conn)?;
    let reply_timeout = ctx.config.squire.reply_timeout_s;

    // R23: per-agent consecutive-deferral counts, read once so the sort below
    // can promote any candidate that has been starved past the configured
    // threshold ahead of ordinary ordering. A promoted candidate claims one of
    // the concurrency budget's own slots (never an extra one — that stays R7's
    // job in `tick`); this only changes *which* candidates fill the budget, not
    // how many.
    let starvation_counts = store::starvation_counts(&ctx.conn)?;
    let starvation_threshold = ctx.config.squire.starvation_ticks;

    // Pass 1: inbox, abandoned parks, and satisfied parks per agent.
    for agent in &agents {
        if commands::is_running(&ctx.conn, agent)? {
            continue;
        }
        let inbox = store::get_unprocessed_messages(&ctx.conn, agent)?;
        if !inbox.is_empty() {
            let max_urgency = inbox
                .iter()
                .map(|m| urgency_rank(&m.urgency))
                .max()
                .unwrap_or(1);
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::Inbox,
                reason: format!("inbox: {} unprocessed message(s)", inbox.len()),
                urgency_rank: max_urgency,
                task_id: None,
                delegation_id: None,
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
            continue;
        }

        let parked = store::list_parked_tasks_for_agent(&ctx.conn, agent)?;

        let abandoned: Vec<&crate::models::ParkedTask> = parked
            .iter()
            .filter(|e| {
                if prompt::park_satisfied(&ctx.conn, e).unwrap_or(false) {
                    return false;
                }
                // R26: contested DI claims abandon on their own configured
                // timeout, not the generic reply timeout.
                let timeout = match e.wait_type.as_str() {
                    "claim-contested" => Some(ctx.config.di.claim_contested_timeout_s),
                    _ => reply_timeout,
                };
                prompt::park_abandoned(&ctx.conn, e, timeout).unwrap_or(false)
            })
            .collect();

        if !abandoned.is_empty() {
            let reasons: Vec<String> = abandoned
                .iter()
                .map(|e| {
                    let cause = if prompt::park_target_gone(&ctx.conn, e).unwrap_or(false) {
                        format!("agent '{}' is disabled or gone", e.waiting_on)
                    } else {
                        format!("timed out after park age exceeded {:?}", reply_timeout)
                    };
                    format!("task {} abandoned ({})", e.task, cause)
                })
                .collect();
            // Collect IDs for deferred deletion — do NOT delete here.
            abandoned_parks.insert(
                agent.clone(),
                abandoned.iter().map(|e| e.task.clone()).collect(),
            );
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::AbandonedPark,
                reason: format!(
                    "abandonment: your wait is abandoned ({}). Proceed with a documented assumption or escalate.",
                    reasons.join("; ")
                ),
                urgency_rank: 1,
                task_id: None,
                delegation_id: None,
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
            continue;
        }

        // 4A: resolved DI requests wake the parked requester with the
        // outcome as the wake reason (checked before generic ready-parks so
        // the reason is specific -- R4: one wake, one recorded reason).
        let di_resolved: Vec<crate::di::DiRequest> = parked
            .iter()
            .filter(|e| e.wait_type == "di_request")
            .filter_map(|e| {
                e.wait_re
                    .as_deref()
                    .and_then(|id| crate::di::get_request(&ctx.conn, id).ok().flatten())
            })
            .filter(|req| req.status != crate::di::STATUS_PENDING)
            .collect();
        if let Some(req) = di_resolved.into_iter().next() {
            let (kind, reason) = match req.status.as_str() {
                crate::di::STATUS_APPROVED => (
                    WakeKind::DIApproved,
                    format!(
                        "DI request {} APPROVED for {} in domain {}. Approval is permission, \
                         not a lock (R10): run `trelane claim {} {}` before writing. If the \
                         path is already leased, park on the contention instead of retrying: \
                         `trelane park {} --wait-contested-claim {} --waiting-on <holder>` (R26). \
                         Never write .trelane/** or .git/** regardless of approval (R11).",
                        req.id,
                        req.path_glob,
                        req.target_domain,
                        agent,
                        req.path_glob,
                        agent,
                        req.path_glob
                    ),
                ),
                crate::di::STATUS_VETOED => (
                    WakeKind::DIVetoed,
                    format!(
                        "DI request {} VETOED by {}: {}. A veto is final for this request -- \
                         do not retry the same request without addressing the objection.",
                        req.id,
                        req.veto_agent.as_deref().unwrap_or("?"),
                        req.veto_reason.as_deref().unwrap_or("(no reason given)")
                    ),
                ),
                _ => (
                    WakeKind::DIExpired,
                    format!(
                        "DI request {} EXPIRED: no approval and no veto within \
                         di.request_timeout_s (R25 -- silence is not permission). Re-request \
                         with a clearer purpose, or find another path.",
                        req.id
                    ),
                ),
            };
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind,
                reason,
                urgency_rank: 2,
                task_id: None,
                delegation_id: None,
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
            continue;
        }

        let ready: Vec<String> = parked
            .iter()
            .filter(|e| prompt::park_satisfied(&ctx.conn, e).unwrap_or(false))
            .map(|e| e.task.clone())
            .collect();
        if !ready.is_empty() {
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::ReadyPark,
                reason: format!("resume: parked task(s) ready: {}", ready.join(", ")),
                urgency_rank: 1,
                task_id: None,
                delegation_id: None,
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
        }
    }

    // Pass 2: cycle detection (side-effect-free — just plan the breaker).
    let mut cycle_plan: Option<CycleBreakPlan> = None;
    let (_, cycle) = wait_graph(&ctx.conn)?;
    if let Some(cycle) = cycle {
        let none_running = !cycle
            .iter()
            .any(|a| commands::is_running(&ctx.conn, a).unwrap_or(false));
        if none_running {
            let mut sorted = cycle.clone();
            sorted.sort();
            let cycle_key = sorted.join(",");
            let attempt_count = store::get_cycle_attempt_count(&ctx.conn, &cycle_key)?;
            // R24: the escalation threshold is operator-tunable, not a magic
            // constant (`squire.breaker_escalation_count`, default 3).
            let escalation_threshold = ctx.config.squire.breaker_escalation_count;
            let should_escalate = attempt_count > escalation_threshold
                && !store::is_cycle_escalated(&ctx.conn, &cycle_key)?;
            let (breaker, alt_breaker) = if should_escalate {
                let alt = sorted[(attempt_count as usize) % sorted.len()].clone();
                (alt.clone(), Some(alt))
            } else {
                (sorted[0].clone(), None)
            };
            cycle_plan = Some(CycleBreakPlan {
                cycle_key: cycle_key.clone(),
                cycle_members: cycle.clone(),
                designated_breaker: breaker.clone(),
                current_attempt_count: attempt_count,
                should_escalate,
                alt_breaker,
            });
            if !seen.contains(&breaker) {
                let mut display = cycle.clone();
                display.push(cycle[0].clone());
                let reason = if should_escalate {
                    format!(
                        "ESCALATED deadlock (attempt #{}): wait-cycle {}. \
                        Previous breaker(s) failed to resolve this. \
                        You are the new designated breaker: unpark your task, \
                        proceed with a clearly documented assumption, and message all cycle members.",
                        attempt_count,
                        display.join(" -> ")
                    )
                } else {
                    format!(
                        "deadlock: wait-cycle {}. You are the designated breaker: \
                        proceed with a documented assumption, message your counterpart \
                        stating it, and unpark your task.",
                        display.join(" -> ")
                    )
                };
                cands.push(WakeCandidate {
                    agent: breaker,
                    kind: WakeKind::CycleBreak,
                    reason,
                    urgency_rank: 2,
                    task_id: None,
                    delegation_id: None,
                    discovery_fingerprint: None,
                    discovery_task_id: None,
                });
            }
        }
    }

    // Pass 3: ready owned tasks (C3).
    for agent in &agents {
        if seen.contains(agent) || commands::is_running(&ctx.conn, agent)? {
            continue;
        }
        let ready_tasks = store::list_ready_owned_tasks(&ctx.conn, agent)?;
        if let Some(best) = ready_tasks.first() {
            let rank = urgency_rank(&best.priority);
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::OwnedTask,
                reason: format!("owned task ready: {} ({})", best.id, best.subject),
                urgency_rank: rank,
                task_id: Some(best.id.clone()),
                delegation_id: None,
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
        }
    }

    // Pass 4: active helper assignments (C3).
    for agent in &agents {
        if seen.contains(agent) || commands::is_running(&ctx.conn, agent)? {
            continue;
        }
        let assignments = store::list_runnable_helper_assignments(&ctx.conn, agent, &now)?;
        if let Some((_, task, delegation)) = assignments.first() {
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::HelperAssignment,
                reason: format!(
                    "helper assignment active: task {} (delegation {})",
                    task.id, delegation.id
                ),
                urgency_rank: urgency_rank(&task.priority),
                task_id: Some(task.id.clone()),
                delegation_id: Some(delegation.id.clone()),
                discovery_fingerprint: None,
                discovery_task_id: None,
            });
            seen.insert(agent.clone());
        }
    }

    // Pass 5: assist discovery during partial idleness (C3).
    let running_count = agents
        .iter()
        .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
        .count();
    if running_count < ctx.config.squire.max_concurrent {
        for agent in &agents {
            if seen.contains(agent) || commands::is_running(&ctx.conn, agent)? {
                continue;
            }
            // Check outstanding offers (default limit: 1).
            if store::count_outstanding_offers_for_helper(&ctx.conn, agent)? >= 1 {
                continue;
            }
            // Check cooldown.
            if let Some(state) = store::get_assist_discovery_state(&ctx.conn, agent)? {
                if let Some(ref cd) = state.cooldown_until {
                    if cd.as_str() > now.as_str() {
                        continue;
                    }
                }
            }
            // Get assistable tasks, filtering out owners with active backoff.
            let assistable = store::list_assistable_tasks(&ctx.conn, agent, &now)?;
            let eligible: Vec<_> = assistable
                .iter()
                .filter(|t| {
                    !store::rejection_backoff_active(&ctx.conn, agent, &t.owner_agent, &now)
                        .unwrap_or(false)
                })
                .collect();
            if eligible.is_empty() {
                continue;
            }
            let fingerprint = store::assist_backlog_fingerprint(&assistable);
            // Skip if fingerprint unchanged since last offer.
            if let Some(state) = store::get_assist_discovery_state(&ctx.conn, agent)? {
                if state.last_offered_fingerprint == fingerprint {
                    continue;
                }
            }
            let task_id = eligible.first().map(|t| t.id.clone());
            cands.push(WakeCandidate {
                agent: agent.clone(),
                kind: WakeKind::AssistDiscovery,
                reason: format!(
                    "available-to-help: {} assistable task(s) elsewhere; inspect read-only with `trelane work list --assistable --agent {}` and make at most one scoped help offer",
                    eligible.len(),
                    agent
                ),
                urgency_rank: 0,
                task_id: None,
                delegation_id: None,
                discovery_fingerprint: Some(fingerprint),
                discovery_task_id: task_id,
            });
            // Don't insert into `seen` — discovery is lowest priority and
            // we only add one candidate per agent anyway.
        }
    }

    // Deterministic sort. R23 starvation promotion is the PRIMARY key: any
    // candidate whose consecutive-deferral count has reached the threshold
    // sorts ahead of everything else, so it can't be perpetually starved by a
    // low kind-rank or a late-alphabetical name. Among promoted candidates,
    // and among ordinary ones, the original ordering (kind rank, urgency desc,
    // agent asc, ...) is preserved as the tie-break. The starvation flag is a
    // pure function of the pre-read `starvation_counts`, so the sort stays
    // deterministic within a tick.
    let is_starved = |c: &WakeCandidate| -> bool {
        starvation_threshold > 0
            && starvation_counts
                .get(&c.agent)
                .is_some_and(|&n| n >= starvation_threshold)
    };
    cands.sort_by(|a, b| {
        // starved-first: true (starved) must order before false, so compare
        // reversed (b vs a) on the boolean.
        is_starved(b)
            .cmp(&is_starved(a))
            .then_with(|| a.kind.rank().cmp(&b.kind.rank()))
            .then_with(|| b.urgency_rank.cmp(&a.urgency_rank))
            .then_with(|| a.agent.cmp(&b.agent))
            .then_with(|| a.task_id.cmp(&b.task_id))
            .then_with(|| a.delegation_id.cmp(&b.delegation_id))
    });

    // One candidate per agent (first wins after sort).
    let mut deduped: Vec<WakeCandidate> = Vec::new();
    let mut deduped_agents: HashSet<String> = HashSet::new();
    for c in cands {
        if deduped_agents.insert(c.agent.clone()) {
            deduped.push(c);
        }
    }

    Ok(WakePlan {
        candidates: deduped,
        abandoned_parks,
        cycle: cycle_plan,
    })
}

/// Return (agent, reason) pairs for agents that should be woken.
/// This is a backward-compatible wrapper around `wake_plan` that also clears
/// resolved cycle-break tracking (a safe side effect). For tick, use
/// `wake_plan` directly so side effects are deferred until after launch.
pub fn wake_candidates(ctx: &Context) -> Result<Vec<(String, String)>> {
    let plan = wake_plan(ctx)?;
    // Clear resolved cycle tracking when no cycle is detected.
    if plan.cycle.is_none() {
        let attempts = store::list_cycle_break_attempts(&ctx.conn).unwrap_or_default();
        for attempt in &attempts {
            let _ = store::clear_cycle_break_attempts(&ctx.conn, &attempt.cycle_key);
        }
    }
    Ok(plan
        .candidates
        .into_iter()
        .map(|c| (c.agent, c.reason))
        .collect())
}

/// One squire tick. Returns number of agents launched.
///
/// `verbose` controls chatter that is useful while debugging but noisy in a
/// live session frame (e.g. the concurrency-budget deferral notice).
pub fn tick(ctx: &Context, launcher_override: Option<&str>, verbose: bool) -> Result<usize> {
    // GAP-06 / C7: record a squire.tick span for every tick, including ones
    // that launch nothing (an idle tick is still signal). Best-effort per R16 —
    // the tracer is built lazily and every emit is ignored on failure, so
    // telemetry can never affect the tick's outcome. start_ns is captured before
    // any work so the span covers the whole tick.
    let start_ns = crate::telemetry::now_nanos();
    let emit_tick_span = |launched: usize, running: usize, cycle: bool| {
        if let Ok(tracer) =
            crate::telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string())
        {
            let _ = tracer.record_squire_tick(
                launched,
                running,
                cycle,
                start_ns,
                crate::telemetry::now_nanos(),
            );
        }
    };

    // Session pause (monitor-controlled, via the project_state.paused flag).
    // When paused, this tick does no planning and launches nothing -- but it
    // still records an idle span and returns Ok(0), so the squire keeps
    // ticking and resumes immediately once the flag is cleared. Checked first,
    // before the retention sweep and planning, so a paused session is fully
    // inert. Best-effort read: a failure defaults to not-paused (see
    // store::is_session_paused) so an error can never wedge the squire paused.
    if store::is_session_paused(&ctx.conn).unwrap_or(false) {
        let running_now = store::list_agents(&ctx.conn)
            .map(|ags| {
                ags.iter()
                    .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
                    .count()
            })
            .unwrap_or(0);
        if verbose {
            eprintln!(
                "{} session paused -- skipping launch ({} still running)",
                crate::crypto::now_iso(),
                running_now
            );
        }
        emit_tick_span(0, running_now, false);
        return Ok(0);
    }

    // 4D: retention sweep as the cheap first step of the tick (one restarter,
    // not a second daemon). Best-effort per R16: a sweep failure must never
    // fail the tick it ran inside.
    if let Err(e) = crate::retention::sweep(ctx, false) {
        eprintln!("retention sweep failed (non-fatal): {e}");
    }
    reap_leases(ctx)?;
    // 4A: resolve pending domain-intrusion requests against current durable
    // state (veto always wins; owner approval immediate; standing non-owner
    // approval past the objection window; silence expires per R25). Done
    // before planning so resolved requests surface as wake reasons this tick.
    let _di_resolved = crate::di::resolve_pending(ctx)?;
    let plan = wake_plan(ctx)?;
    let cycle_detected = plan.cycle.is_some();
    if plan.candidates.is_empty() {
        // Nothing to launch this tick — still record it, with the current
        // running count so idle spans carry the concurrency picture.
        let running_now = store::list_agents(&ctx.conn)
            .map(|ags| {
                ags.iter()
                    .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
                    .count()
            })
            .unwrap_or(0);
        // R28: total quiescence is recorded, never auto-acted on.
        maybe_post_quiescence_notice(ctx, running_now);
        emit_tick_span(0, running_now, cycle_detected);
        return Ok(0);
    }

    let agents = store::list_agents(&ctx.conn)?;
    let running_count = agents
        .iter()
        .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
        .count();

    let report = concurrency_report(
        agents.len(),
        running_count,
        ctx.config.squire.max_concurrent,
        plan.candidates.len(),
    );

    if report.work_exceeds_budget() {
        eprintln!(
            "{} WARNING: {} agent(s) ready but simultaneous-execution limit is {} \
             ({} running, {} slot(s) free) -- {} deferred to a later tick. Raise it with \
             `trelane config set squire.max_concurrent <N>` or `trelane squire --max-concurrent <N>`.",
            crate::crypto::now_iso(),
            report.ready,
            report.limit,
            report.running,
            report.budget,
            report.deferred,
        );
    }

    let budget = report.budget;
    let mut launched = 0;
    let now = crate::crypto::now_iso();
    // GAP-06: per-candidate launch outcomes, for the squire.wake_candidate
    // spans emitted at the end of the tick.
    let mut launched_agents: HashSet<&str> = HashSet::new();
    let mut launcher_skipped: HashSet<&str> = HashSet::new();

    for cand in &plan.candidates {
        if launched >= budget {
            if verbose {
                eprintln!(
                    "deferred wake of {} (concurrency budget reached)",
                    cand.agent
                );
            }
            // R23: this candidate was valid but deferred past the budget this
            // tick. Bump its consecutive-deferral count so that, once it crosses
            // squire.starvation_ticks, wake_plan's sort promotes it ahead of
            // ordinary ordering next time. Best-effort: a bookkeeping failure
            // must never abort the tick.
            let _ = store::increment_starvation(&ctx.conn, &cand.agent, &now);
            continue;
        }
        eprintln!(
            "{} waking {}: {}",
            crate::crypto::now_iso(),
            cand.agent,
            cand.reason
        );
        match commands::cmd_wake(
            ctx,
            &cand.agent,
            Some(cand.reason.as_str()),
            launcher_override,
        ) {
            Ok(()) => {
                launched += 1;
                launched_agents.insert(cand.agent.as_str());
                // Story ledger (best-effort, R16): record that the squire
                // issued this wake. The agent-side run_start event (emitted
                // by cmd_wake) carries the agent's view; this wake_issued
                // event carries the squire's view (it picked this agent
                // out of the queue with this reason). The two views JOIN
                // by ts_iso/agent -- the squire is the actor here, the
                // woken agent is in detail.woke.
                let _ = store::append_story_event(
                    &ctx.conn,
                    &StoryEvent::new("wake_issued", Some("squire".to_string()))
                        .trace(crate::telemetry::current_trace_id(&ctx.trelane_dir()))
                        .detail(serde_json::json!({
                            "woke": cand.agent,
                            "reason": cand.reason,
                        })),
                );
                // Apply deferred side effects now that the agent launched.

                // R23: the agent actually launched, so its starvation streak is
                // broken — clear the counter. This is the "clear only on real
                // launch, not on mere candidacy" discipline: the count must
                // survive from deferral through to selection, and only a
                // genuine wake resets it. Best-effort per the same rule.
                let _ = store::clear_starvation(&ctx.conn, &cand.agent);

                // Delete abandoned parks for this agent.
                if let Some(park_ids) = plan.abandoned_parks.get(&cand.agent) {
                    for pid in park_ids {
                        let _ = store::delete_parked_task(&ctx.conn, pid);
                    }
                }

                // Record cycle-break attempt and send escalation if needed.
                if cand.kind == WakeKind::CycleBreak {
                    if let Some(ref cp) = plan.cycle {
                        let attempt_count = store::record_cycle_break_attempt(
                            &ctx.conn,
                            &cp.cycle_key,
                            &cp.cycle_members,
                            &cp.designated_breaker,
                        )?;
                        if cp.should_escalate {
                            let secret = ctx.secret()?;
                            for member in &cp.cycle_members {
                                let mut msg = Message::new(
                                    crate::crypto::new_id("msg"),
                                    "system".to_string(),
                                    member.clone(),
                                    "system".to_string(),
                                    "critical".to_string(),
                                    format!(
                                        "cycle escalation: {} failed break attempts",
                                        attempt_count
                                    ),
                                    format!(
                                        "The wait-cycle has been broken {attempt_count} times without resolution. \
                                        Each member must reassess their parked tasks and either unpark with a \
                                        documented assumption or escalate to the user."
                                    ),
                                    None,
                                    None,
                                    vec![],
                                    crate::crypto::now_iso(),
                                );
                                crate::crypto::sign(&secret, &mut msg);
                                let _ = store::insert_message(&ctx.conn, &msg);
                            }
                            let alerts_dir = ctx.trelane_dir().join("alerts");
                            let _ = std::fs::create_dir_all(&alerts_dir);
                            let alert_path =
                                alerts_dir.join(format!("{}.txt", cp.cycle_key.replace(',', "-")));
                            let _ = std::fs::write(
                                &alert_path,
                                format!(
                                    "[{}] CYCLE ESCALATION\nCycle: {}\nAttempts: {}\nDesignated breaker: {}\n\n",
                                    crate::crypto::now_iso(),
                                    cp.cycle_members.join(" -> "),
                                    attempt_count,
                                    cp.designated_breaker,
                                ),
                            );
                            let _ = store::mark_cycle_escalated(&ctx.conn, &cp.cycle_key);
                        }
                    }
                }

                // Record discovery cooldown.
                if cand.kind == WakeKind::AssistDiscovery {
                    if let Some(ref fp) = cand.discovery_fingerprint {
                        let cooldown = chrono::DateTime::parse_from_rfc3339(&now)
                            .ok()
                            .and_then(|dt| dt.checked_add_signed(chrono::Duration::seconds(300)))
                            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                            .unwrap_or_else(|| now.clone());
                        let _ = store::record_discovery_wake(
                            &ctx.conn,
                            &cand.agent,
                            fp,
                            &now,
                            &cooldown,
                        );
                    }
                }
            }
            Err(e) if e.is_launcher_not_configured() => {
                launcher_skipped.insert(cand.agent.as_str());
                eprintln!("{} SKIPPED {}: {}", crate::crypto::now_iso(), cand.agent, e);
            }
            Err(e) => return Err(e),
        }
    }

    // GAP-06: one squire.wake_candidate span per agent *considered* this tick
    // -- not just the winners -- so "why didn't X wake" is answerable after
    // the fact. Best-effort per R16.
    emit_wake_candidate_spans(
        ctx,
        &plan,
        &agents,
        &launched_agents,
        &launcher_skipped,
        start_ns,
    );

    // Record the completed tick: how many launched, how many were running at
    // the start of the launch phase, and whether a wait-cycle was detected.
    emit_tick_span(launched, running_count, cycle_detected);
    Ok(launched)
}

/// GAP-06: emit a `squire.wake_candidate` span for every registered agent,
/// recording what was considered and why it was or wasn't chosen.
fn emit_wake_candidate_spans(
    ctx: &Context,
    plan: &WakePlan,
    agents: &[String],
    launched_agents: &HashSet<&str>,
    launcher_skipped: &HashSet<&str>,
    start_ns: u64,
) {
    let Ok(tracer) =
        crate::telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string())
    else {
        return;
    };
    let end_ns = crate::telemetry::now_nanos();
    for agent in agents {
        let cand = plan.candidates.iter().find(|c| &c.agent == agent);
        let (kind, chosen, reason_skipped) = match cand {
            Some(c) if launched_agents.contains(agent.as_str()) => {
                (format!("{:?}", c.kind), "true", "")
            }
            Some(c) if launcher_skipped.contains(agent.as_str()) => {
                (format!("{:?}", c.kind), "false", "launcher not configured")
            }
            Some(c) => (format!("{:?}", c.kind), "false", "concurrency budget"),
            None if commands::is_running(&ctx.conn, agent).unwrap_or(false) => {
                ("None".to_string(), "false", "already running")
            }
            None => ("None".to_string(), "false", "no wake reason"),
        };
        let _ = tracer.record_event(
            "squire.wake_candidate",
            &[
                ("agent.name", agent.as_str()),
                ("wake.kind_considered", kind.as_str()),
                ("wake.chosen", chosen),
                ("wake.reason_skipped", reason_skipped),
            ],
            start_ns,
            end_ns,
        );
    }
}

/// R28: when the whole swarm is quiescent -- no candidates, no running
/// agents, zero open tasks, zero open parked waits -- record that fact as a
/// system-level bulletin notice. Informational only: never a wake trigger,
/// never an automatic Biplane invocation (R19 holds even here). Posted once
/// per quiescent stretch (fresh activity re-arms it).
fn maybe_post_quiescence_notice(ctx: &Context, running_now: usize) {
    let _ = (|| -> Result<()> {
        if running_now > 0 {
            return Ok(());
        }
        // Zero open (non-terminal) tasks across every domain.
        let open_tasks: i64 = ctx.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE state NOT IN ('done', 'cancelled')",
            [],
            |r| r.get(0),
        )?;
        if open_tasks > 0 {
            return Ok(());
        }
        // Zero open parked waits -- a parked swarm is waiting, not finished.
        let open_parks: i64 = ctx
            .conn
            .query_row("SELECT COUNT(*) FROM parked_tasks", [], |r| r.get(0))?;
        if open_parks > 0 {
            return Ok(());
        }
        // Once per quiescent stretch: if the most recent activity on record
        // is already a quiescence notice, don't pile on. rowid orders by
        // insertion (created_at is only second-granularity).
        let latest: Option<(String, String)> = ctx
            .conn
            .query_row(
                "SELECT msg_type, id FROM messages ORDER BY rowid DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if matches!(latest, Some((ref t, _)) if t == "quiescence_notice") {
            return Ok(());
        }
        let now = crate::crypto::now_iso();
        let mut msg = Message::new(
            crate::crypto::new_id("msg"),
            "system".to_string(),
            String::new(), // broadcast: not addressed to one agent
            "quiescence_notice".to_string(),
            "low".to_string(),
            "quiescence: zero ready work across all domains".to_string(),
            "Total quiescence observed: no running agents, no open tasks, no parked waits. \
             Informational only (R28) -- this notice never triggers a wake and never invokes \
             Biplane automatically (R19)."
                .to_string(),
            None,
            None,
            vec![],
            now,
        );
        msg.channel = crate::models::CHANNEL_BULLETIN.to_string();
        msg.scope = Some("system".to_string());
        let secret = ctx.secret()?;
        crate::crypto::sign(&secret, &mut msg);
        store::insert_message(&ctx.conn, &msg)?;
        eprintln!(
            "{} quiescence: zero ready work across all domains (notice posted)",
            crate::crypto::now_iso()
        );
        Ok(())
    })();
}

/// Derived, read-only explanation of why an agent is in its current state.
/// Does NOT call `wake_plan` or `wake_candidates` — safe to call from status.
pub fn agent_activity_status(ctx: &Context, agent: &str) -> Result<AgentStatus> {
    let running = commands::is_running(&ctx.conn, agent)?;
    if running {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::Running,
            reason: "agent is running".to_string(),
            task_ids: vec![],
        });
    }

    let inbox = store::get_unprocessed_messages(&ctx.conn, agent)?;
    if !inbox.is_empty() {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::Running,
            reason: format!("{} unprocessed inbox message(s)", inbox.len()),
            task_ids: vec![],
        });
    }

    let now = crate::crypto::now_iso();
    let parked = store::list_parked_tasks_for_agent(&ctx.conn, agent)?;
    let unsatisfied: Vec<_> = parked
        .iter()
        .filter(|p| !prompt::park_satisfied(&ctx.conn, p).unwrap_or(false))
        .collect();
    if !unsatisfied.is_empty() {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::Blocked,
            reason: format!("waiting on {} parked task(s)", unsatisfied.len()),
            task_ids: unsatisfied.iter().map(|p| p.task.clone()).collect(),
        });
    }

    let ready_owned = store::list_ready_owned_tasks(&ctx.conn, agent)?;
    if !ready_owned.is_empty() {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::OwnedWorkReady,
            reason: format!("{} ready owned task(s)", ready_owned.len()),
            task_ids: ready_owned.iter().map(|t| t.id.clone()).collect(),
        });
    }

    let helper_assignments = store::list_runnable_helper_assignments(&ctx.conn, agent, &now)?;
    if !helper_assignments.is_empty() {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::HelpAssignmentReady,
            reason: format!("{} active helper assignment(s)", helper_assignments.len()),
            task_ids: helper_assignments
                .iter()
                .map(|(a, _, _)| a.task_id.clone())
                .collect(),
        });
    }

    let assistable = store::list_assistable_tasks(&ctx.conn, agent, &now)?;
    if !assistable.is_empty() {
        return Ok(AgentStatus {
            agent: agent.to_string(),
            state: AgentActivityState::AvailableToHelp,
            reason: format!("{} assistable task(s) elsewhere", assistable.len()),
            task_ids: assistable.iter().map(|t| t.id.clone()).collect(),
        });
    }

    Ok(AgentStatus {
        agent: agent.to_string(),
        state: AgentActivityState::Idle,
        reason: "no actionable work".to_string(),
        task_ids: vec![],
    })
}

pub fn agent_activity_statuses(ctx: &Context) -> Result<Vec<AgentStatus>> {
    let agents = store::list_agents(&ctx.conn)?;
    let mut out = Vec::new();
    for agent in &agents {
        out.push(agent_activity_status(ctx, agent)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::ParkedTask;

    fn in_memory_conn() -> Connection {
        // Use the real, fully-migrated schema rather than a hand-rolled
        // subset: fixtures drift silently when migrations add columns the
        // queries under test depend on (e.g. v11's messages.archived_at).
        db::open_in_memory().unwrap()
    }

    fn parked(agent: &str, waiting_on: &str) -> ParkedTask {
        ParkedTask {
            task: format!("task-{agent}"),
            agent: agent.to_string(),
            wait_type: "reply".to_string(),
            wait_re: Some(format!("msg-{agent}")),
            wait_path: None,
            waiting_on: waiting_on.to_string(),
            resume_hint: String::new(),
            created_at: "2026-07-03T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn wait_graph_detects_cycle() {
        let conn = in_memory_conn();
        store::insert_parked_task(&conn, &parked("alpha", "beta")).unwrap();
        store::insert_parked_task(&conn, &parked("beta", "gamma")).unwrap();
        store::insert_parked_task(&conn, &parked("gamma", "alpha")).unwrap();

        let (_, cycle) = wait_graph(&conn).unwrap();
        let cycle = cycle.unwrap();
        assert_eq!(cycle.len(), 3);
        assert!(cycle.contains(&"alpha".to_string()));
        assert!(cycle.contains(&"beta".to_string()));
        assert!(cycle.contains(&"gamma".to_string()));
    }

    #[test]
    fn wait_graph_returns_none_without_cycle() {
        let conn = in_memory_conn();
        store::insert_parked_task(&conn, &parked("alpha", "beta")).unwrap();
        store::insert_parked_task(&conn, &parked("beta", "user")).unwrap();

        let (_, cycle) = wait_graph(&conn).unwrap();
        assert!(cycle.is_none());
    }

    #[test]
    fn dfs_cycle_handles_self_loop() {
        let mut edges = HashMap::new();
        edges.insert("alpha".to_string(), HashSet::from(["alpha".to_string()]));
        let mut visited = HashSet::new();
        let mut stack = Vec::new();
        let mut stack_set = HashSet::new();

        let cycle = dfs_cycle("alpha", &edges, &mut visited, &mut stack, &mut stack_set).unwrap();
        assert_eq!(cycle, vec!["alpha".to_string()]);
    }

    #[test]
    fn wait_graph_detects_cycle_alongside_non_cycle() {
        // alpha -> beta -> alpha (cycle) + gamma -> user (no cycle)
        let conn = in_memory_conn();
        store::insert_parked_task(&conn, &parked("alpha", "beta")).unwrap();
        store::insert_parked_task(&conn, &parked("beta", "alpha")).unwrap();
        store::insert_parked_task(&conn, &parked("gamma", "user")).unwrap();

        let (edges, cycle) = wait_graph(&conn).unwrap();
        assert!(cycle.is_some(), "should detect the alpha-beta cycle");
        assert!(
            edges.contains_key("gamma"),
            "gamma should be in the wait graph even though it's not in a cycle"
        );
    }

    /// A fully-migrated (real schema) Context, needed for `tick` since it
    /// touches domains/agents/running-locks, not just the partial tables
    /// `in_memory_conn` provides.
    fn migrated_ctx(temp: &tempfile::TempDir) -> crate::Context {
        let root = temp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = db::open(&db_path).unwrap();
        crate::Context {
            root,
            conn,
            config: crate::models::Config::default(),
        }
    }

    #[test]
    fn tick_skips_an_unconfigured_agent_without_blocking_the_rest() {
        // This is the exact safety scenario: one agent has no launcher model
        // assigned (would otherwise silently hit the paid default) while a
        // sibling agent is properly configured. The unconfigured agent must
        // be skipped -- never launched, and never allowed to block or delay
        // the properly-configured one in the same tick.
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        crate::commands::cmd_add_agent(&ctx, "bad", &["src/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(
            &ctx,
            "good",
            &["src/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        crate::commands::cmd_send(
            &ctx,
            "user",
            "bad",
            "question",
            "normal",
            "task for bad",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();
        crate::commands::cmd_send(
            &ctx,
            "user",
            "good",
            "question",
            "normal",
            "task for good",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();

        let launched = tick(&ctx, None, false).expect("tick must not abort on one bad agent");
        assert_eq!(
            launched, 1,
            "only the properly-configured agent should launch"
        );

        assert!(
            !crate::commands::is_running(&ctx.conn, "bad").unwrap(),
            "the unconfigured agent must never actually launch"
        );
        // Its message stays unprocessed -- nothing corrupted, safe to retry
        // once the user assigns it a launcher.
        assert_eq!(
            store::get_unprocessed_messages(&ctx.conn, "bad")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn concurrency_report_four_agents_limit_two_reports_ceiling() {
        // Required scenario: four registered agents with max_concurrent=2 must
        // clearly report the two-agent ceiling (registered and limit tracked
        // separately), and flag that ready work is being deferred.
        let r = concurrency_report(4, 0, 2, 4);
        assert_eq!(r.registered, 4);
        assert_eq!(r.limit, 2);
        assert_eq!(
            r.budget, 2,
            "two free slots under a limit of 2 with none running"
        );
        assert_eq!(r.deferred, 2, "the other two ready agents are deferred");
        assert!(r.work_exceeds_budget());
    }

    #[test]
    fn concurrency_report_two_running_at_ceiling_defers_all_ready() {
        // The exact reported symptom: two already running at a limit of 2 means
        // zero budget, so every other ready agent sits idle until a slot frees.
        let r = concurrency_report(4, 2, 2, 2);
        assert_eq!(r.budget, 0);
        assert_eq!(r.deferred, 2);
        assert!(r.work_exceeds_budget());
    }

    #[test]
    fn concurrency_report_within_budget_does_not_warn() {
        // Ready work fits under the ceiling: nothing is deferred, no warning.
        let r = concurrency_report(4, 1, 4, 2);
        assert_eq!(r.budget, 3);
        assert_eq!(r.deferred, 0);
        assert!(!r.work_exceeds_budget());
    }

    #[test]
    fn concurrency_report_saturates_when_running_exceeds_limit() {
        // If somehow more agents are running than the limit (e.g. the limit was
        // lowered mid-session), budget must floor at 0, never underflow.
        let r = concurrency_report(4, 3, 2, 1);
        assert_eq!(r.budget, 0);
        assert_eq!(r.deferred, 1);
    }

    // ------------------------------------------------------------- C3 tests

    use crate::models::{
        AssistPolicy, Delegation, DelegationStatus, Task, TaskAssignment, TaskRole, TaskState,
    };

    fn make_task(id: &str, owner: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            owner_agent: owner.to_string(),
            domain: owner.to_string(),
            parent_task: None,
            subject: format!("task {id}"),
            body: String::new(),
            state,
            priority: "normal".to_string(),
            assist_policy: AssistPolicy::Open,
            desired_parallelism: 1,
            path_scope: vec![format!("src/{owner}/**")],
            acceptance: vec![],
            blocked_by: vec![],
            created_at: "2026-07-12T00:00:00Z".to_string(),
            updated_at: "2026-07-12T00:00:00Z".to_string(),
        }
    }

    fn make_delegation(
        id: &str,
        task_id: &str,
        owner: &str,
        helper: &str,
        status: DelegationStatus,
    ) -> Delegation {
        Delegation {
            id: id.to_string(),
            task_id: task_id.to_string(),
            owner_agent: owner.to_string(),
            helper_agent: helper.to_string(),
            scope: vec![format!("src/{owner}/**")],
            allowed_ops: vec!["write".to_string()],
            constraints_json: "{}".to_string(),
            base_revision: None,
            offer_message: format!("offer_{id}"),
            grant_message: if status == DelegationStatus::Offered {
                String::new()
            } else {
                format!("grant_{id}")
            },
            issued_at: "2026-07-12T00:00:00Z".to_string(),
            expires_at: Some("2099-12-31T00:00:00Z".to_string()),
            status,
        }
    }

    fn setup_two_agents(ctx: &crate::Context) {
        crate::commands::cmd_add_agent(
            ctx,
            "alpha",
            &["src/alpha/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        crate::commands::cmd_add_agent(ctx, "beta", &["src/beta/**".to_string()], &[], None, None)
            .unwrap();
    }

    #[test]
    fn ready_owned_task_requires_done_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let mut parent = make_task("parent", "alpha", TaskState::Ready);
        let mut child = make_task("child", "alpha", TaskState::Ready);
        child.blocked_by = vec!["parent".to_string()];
        store::insert_task(&ctx.conn, &parent).unwrap();
        store::insert_task(&ctx.conn, &child).unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // Child should NOT appear because its dependency is not done.
        assert!(
            plan.candidates
                .iter()
                .any(|c| c.agent == "alpha" && c.task_id == Some("parent".to_string()))
        );
        assert!(
            !plan
                .candidates
                .iter()
                .any(|c| c.task_id == Some("child".to_string()))
        );
    }

    #[test]
    fn active_helper_assignment_wakes_helper() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Active);
        store::insert_task(&ctx.conn, &task).unwrap();
        let del = make_delegation("del_1", "task_1", "alpha", "beta", DelegationStatus::Active);
        store::insert_delegation(&ctx.conn, &del).unwrap();
        store::upsert_assignment(
            &ctx.conn,
            &TaskAssignment {
                task_id: "task_1".to_string(),
                agent: "beta".to_string(),
                role: TaskRole::Helper,
                state: "active".to_string(),
                offer_id: Some("offer_del_1".to_string()),
                delegation_id: Some("del_1".to_string()),
                started_at: Some("2026-07-12T01:00:00Z".to_string()),
                completed_at: None,
            },
        )
        .unwrap();
        let plan = wake_plan(&ctx).unwrap();
        assert!(
            plan.candidates
                .iter()
                .any(|c| c.agent == "beta" && c.kind == WakeKind::HelperAssignment)
        );
    }

    #[test]
    fn offered_delegation_does_not_wake_helper() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        let del = make_delegation(
            "del_1",
            "task_1",
            "alpha",
            "beta",
            DelegationStatus::Offered,
        );
        store::insert_delegation(&ctx.conn, &del).unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // Beta should NOT have a HelperAssignment candidate (offered != active).
        assert!(
            !plan
                .candidates
                .iter()
                .any(|c| c.agent == "beta" && c.kind == WakeKind::HelperAssignment)
        );
    }

    #[test]
    fn pending_offer_suppresses_assist_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        // Beta has an outstanding offer.
        let del = make_delegation(
            "del_1",
            "task_1",
            "alpha",
            "beta",
            DelegationStatus::Offered,
        );
        store::insert_delegation(&ctx.conn, &del).unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // Beta should NOT get a discovery wake (outstanding offer = 1, limit = 1).
        assert!(
            !plan
                .candidates
                .iter()
                .any(|c| c.agent == "beta" && c.kind == WakeKind::AssistDiscovery)
        );
    }

    #[test]
    fn assist_discovery_occurs_during_partial_idleness() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        // Alpha has a ready task. Beta is idle.
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // Alpha gets OwnedTask wake.
        assert!(
            plan.candidates
                .iter()
                .any(|c| c.agent == "alpha" && c.kind == WakeKind::OwnedTask)
        );
        // Beta gets AssistDiscovery (running=0 < max_concurrent=2, assistable task exists).
        assert!(
            plan.candidates
                .iter()
                .any(|c| c.agent == "beta" && c.kind == WakeKind::AssistDiscovery),
            "beta should get a discovery wake; got: {:?}",
            plan.candidates
        );
    }

    #[test]
    fn unchanged_offered_fingerprint_suppresses_repeat_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        // Record that beta already offered for this backlog.
        let assistable =
            store::list_assistable_tasks(&ctx.conn, "beta", "2026-07-12T00:00:00Z").unwrap();
        let fp = store::assist_backlog_fingerprint(&assistable);
        store::record_offer_fingerprint(&ctx.conn, "beta", &fp, "del_old", "2026-07-12T00:00:00Z")
            .unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // Beta should NOT get a discovery wake (fingerprint unchanged).
        assert!(
            !plan
                .candidates
                .iter()
                .any(|c| c.agent == "beta" && c.kind == WakeKind::AssistDiscovery),
            "beta should not be rediscovered for the same backlog"
        );
    }

    #[test]
    fn candidate_order_is_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        let plan1 = wake_plan(&ctx).unwrap();
        let plan2 = wake_plan(&ctx).unwrap();
        let agents1: Vec<_> = plan1.candidates.iter().map(|c| c.agent.clone()).collect();
        let agents2: Vec<_> = plan2.candidates.iter().map(|c| c.agent.clone()).collect();
        assert_eq!(agents1, agents2, "candidate order must be deterministic");
    }

    #[test]
    fn deferred_abandoned_park_is_not_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        // Insert a parked task that is abandoned (target gone).
        store::insert_parked_task(
            &ctx.conn,
            &ParkedTask {
                task: "park_1".to_string(),
                agent: "alpha".to_string(),
                wait_type: "reply".to_string(),
                wait_re: Some("msg_1".to_string()),
                wait_path: None,
                waiting_on: "nonexistent".to_string(),
                resume_hint: String::new(),
                created_at: "2020-01-01T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let plan = wake_plan(&ctx).unwrap();
        // The park should still exist (deletion is deferred to tick).
        assert!(
            store::list_parked_tasks(&ctx.conn)
                .unwrap()
                .iter()
                .any(|p| p.task == "park_1"),
            "abandoned park must survive wake_plan"
        );
        // The plan should carry the deletion intent.
        assert!(plan.abandoned_parks.contains_key("alpha"));
    }

    #[test]
    fn deferred_cycle_break_does_not_increment_attempt_count() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        // Create a cycle: alpha waits on beta, beta waits on alpha.
        store::insert_parked_task(
            &ctx.conn,
            &ParkedTask {
                task: "park_a".to_string(),
                agent: "alpha".to_string(),
                wait_type: "reply".to_string(),
                wait_re: Some("msg_a".to_string()),
                wait_path: None,
                waiting_on: "beta".to_string(),
                resume_hint: String::new(),
                created_at: "2026-07-12T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        store::insert_parked_task(
            &ctx.conn,
            &ParkedTask {
                task: "park_b".to_string(),
                agent: "beta".to_string(),
                wait_type: "reply".to_string(),
                wait_re: Some("msg_b".to_string()),
                wait_path: None,
                waiting_on: "alpha".to_string(),
                resume_hint: String::new(),
                created_at: "2026-07-12T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let before = store::list_cycle_break_attempts(&ctx.conn).unwrap().len();
        let _plan = wake_plan(&ctx).unwrap();
        let after = store::list_cycle_break_attempts(&ctx.conn).unwrap().len();
        assert_eq!(
            before, after,
            "wake_plan must not record cycle break attempts"
        );
    }

    #[test]
    fn max_concurrent_two_preserves_c0_ceiling_report() {
        // C0 behavior: four registered agents with max_concurrent=2 must
        // clearly report the two-agent ceiling.
        let r = concurrency_report(4, 0, 2, 4);
        assert_eq!(r.registered, 4);
        assert_eq!(r.limit, 2);
        assert_eq!(r.budget, 2);
        assert_eq!(r.deferred, 2);
        assert!(r.work_exceeds_budget());
    }

    #[test]
    fn agent_activity_status_idle_when_no_work() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let status = agent_activity_status(&ctx, "beta").unwrap();
        assert_eq!(status.state, AgentActivityState::Idle);
    }

    #[test]
    fn agent_activity_status_available_to_help() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        let task = make_task("task_1", "alpha", TaskState::Ready);
        store::insert_task(&ctx.conn, &task).unwrap();
        let status = agent_activity_status(&ctx, "beta").unwrap();
        assert_eq!(status.state, AgentActivityState::AvailableToHelp);
    }

    // ------------------------------------------------------- R28 quiescence

    fn count_quiescence_notices(ctx: &crate::Context) -> i64 {
        ctx.conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE msg_type = 'quiescence_notice'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn quiescence_notice_posted_once_per_quiet_stretch() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        crate::commands::cmd_add_agent(&ctx, "alpha", &["src/**".to_string()], &[], None, None)
            .unwrap();

        // Total quiescence: no tasks, no parks, no running agents.
        tick(&ctx, None, false).unwrap();
        assert_eq!(count_quiescence_notices(&ctx), 1);
        // Second tick: same stretch, no duplicate.
        tick(&ctx, None, false).unwrap();
        assert_eq!(count_quiescence_notices(&ctx), 1);

        // Fresh activity (a bulletin post) re-arms the notice.
        crate::commands::post_bulletin(&ctx, "alpha", "alpha", &["src/a.rs".to_string()], "work")
            .unwrap();
        tick(&ctx, None, false).unwrap();
        assert_eq!(count_quiescence_notices(&ctx), 2);
    }

    #[test]
    fn open_tasks_prevent_quiescence_notice() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        setup_two_agents(&ctx);
        store::insert_task(&ctx.conn, &make_task("task_1", "alpha", TaskState::Ready)).unwrap();
        // The ready task itself is a wake reason, but even with candidates
        // suppressed the notice must not post while work is open.
        let _ = tick(&ctx, None, false);
        assert_eq!(count_quiescence_notices(&ctx), 0);
    }

    #[test]
    fn wake_candidate_spans_cover_considered_and_skipped() {
        // Two agents with inbox wakes, no launchers configured: both are
        // considered, neither chosen. Spans must record the skip reason.
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        crate::commands::cmd_add_agent(&ctx, "aa", &["src/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(&ctx, "bb", &["lib/**".to_string()], &[], None, None)
            .unwrap();
        for to in ["aa", "bb"] {
            crate::commands::cmd_send(
                &ctx,
                "user",
                to,
                "question",
                "normal",
                "q",
                "",
                &None,
                &None,
                &[],
            )
            .unwrap();
        }

        tick(&ctx, None, false).unwrap();

        let trace_file = ctx.trelane_dir().join("traces").join("ephemeral.jsonl");
        let spans = crate::telemetry::Tracer::read_spans(&trace_file).unwrap();
        let candidate_spans: Vec<_> = spans
            .iter()
            .filter(|s| s.name == "squire.wake_candidate")
            .collect();
        assert_eq!(candidate_spans.len(), 2, "one span per considered agent");
        for span in candidate_spans {
            let attrs = format!("{:?}", span.attributes);
            assert!(attrs.contains("Inbox"), "kind recorded: {attrs}");
            assert!(
                attrs.contains("wake.chosen"),
                "chosen attr present: {attrs}"
            );
            assert!(
                attrs.contains(r#"value: StringValue("false")"#),
                "not chosen: {attrs}"
            );
            assert!(
                attrs.contains("launcher not configured"),
                "skip reason: {attrs}"
            );
        }
        // And the tick span itself is present (GAP-06).
        assert!(spans.iter().any(|s| s.name == "squire.tick"));
    }

    // ------------------------------------------------------------- 4A DI

    #[test]
    fn di_resolution_produces_specific_wake_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = migrated_ctx(&temp);
        ctx.config.di.objection_window_s = 3600;
        crate::commands::cmd_add_agent(&ctx, "owner", &["src/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(&ctx, "helper", &["lib/**".to_string()], &[], None, None)
            .unwrap();
        let id = crate::di::create_request(
            &ctx,
            "helper",
            "owner",
            "src/enemy.rs",
            "add a Damage import for the autoplay decider",
        )
        .unwrap();
        crate::di::approve(&ctx, &id, "owner").unwrap();
        crate::di::resolve_pending(&ctx).unwrap();

        let plan = wake_plan(&ctx).unwrap();
        let cand = plan
            .candidates
            .iter()
            .find(|c| c.agent == "helper")
            .expect("helper should have a wake candidate");
        assert_eq!(cand.kind, crate::models::WakeKind::DIApproved);
        assert!(cand.reason.contains("APPROVED"));
        assert!(cand.reason.contains("R10"));
    }

    #[test]
    fn di_veto_produces_vetoed_wake_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        crate::commands::cmd_add_agent(&ctx, "owner", &["src/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(&ctx, "helper", &["lib/**".to_string()], &[], None, None)
            .unwrap();
        let id = crate::di::create_request(
            &ctx,
            "helper",
            "owner",
            "src/enemy.rs",
            "add a Damage import for the autoplay decider",
        )
        .unwrap();
        crate::di::deny(&ctx, &id, "owner", "mid-rewrite, hands off").unwrap();
        crate::di::resolve_pending(&ctx).unwrap();

        let plan = wake_plan(&ctx).unwrap();
        let cand = plan
            .candidates
            .iter()
            .find(|c| c.agent == "helper")
            .expect("helper should have a wake candidate");
        assert_eq!(cand.kind, crate::models::WakeKind::DIVetoed);
        assert!(cand.reason.contains("VETOED"));
        assert!(cand.reason.contains("mid-rewrite"));
    }

    #[test]
    fn claim_contested_park_abandons_on_own_timeout() {
        // R26: a contested DI claim waits out di.claim_contested_timeout_s,
        // not the (longer) generic reply timeout.
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = migrated_ctx(&temp);
        ctx.config.squire.reply_timeout_s = Some(86_400);
        ctx.config.di.claim_contested_timeout_s = 60;
        crate::commands::cmd_add_agent(&ctx, "owner", &["src/**".to_string()], &[], None, None)
            .unwrap();
        crate::commands::cmd_add_agent(&ctx, "helper", &["lib/**".to_string()], &[], None, None)
            .unwrap();
        // The contested lease is still held by the owner (unexpired), so the
        // park is genuinely unsatisfied.
        store::insert_claim(
            &ctx.conn,
            &crate::models::Lease {
                path: "src/enemy.rs".to_string(),
                holder: "owner".to_string(),
                task: None,
                grant: None,
                delegation_id: None,
                acquired_at: crate::crypto::now_iso(),
                expires_at: chrono::Utc::now().timestamp() as f64 + 3600.0,
                expires_human: "2099-01-01T00:00:00Z".to_string(),
                contested: true,
            },
        )
        .unwrap();
        store::insert_parked_task(
            &ctx.conn,
            &ParkedTask {
                task: "contested".to_string(),
                agent: "helper".to_string(),
                wait_type: "claim-contested".to_string(),
                wait_re: None,
                wait_path: Some("src/enemy.rs".to_string()),
                waiting_on: "owner".to_string(),
                resume_hint: String::new(),
                created_at: "2020-01-01T00:00:00Z".to_string(),
            },
        )
        .unwrap();

        let plan = wake_plan(&ctx).unwrap();
        let cand = plan
            .candidates
            .iter()
            .find(|c| c.agent == "helper")
            .expect("helper should have a wake candidate");
        assert_eq!(cand.kind, crate::models::WakeKind::AbandonedPark);
    }

    // ------------------------------------------------------- R23 starvation
    #[test]
    fn starvation_count_increments_and_clears() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        let now = crate::crypto::now_iso();
        assert_eq!(store::get_starvation_count(&ctx.conn, "alpha").unwrap(), 0);
        store::increment_starvation(&ctx.conn, "alpha", &now).unwrap();
        store::increment_starvation(&ctx.conn, "alpha", &now).unwrap();
        assert_eq!(store::get_starvation_count(&ctx.conn, "alpha").unwrap(), 2);
        // Launching clears it — the count tracks CONSECUTIVE deferrals only.
        store::clear_starvation(&ctx.conn, "alpha").unwrap();
        assert_eq!(store::get_starvation_count(&ctx.conn, "alpha").unwrap(), 0);
    }

    #[test]
    fn starvation_counts_map_omits_zeroed_agents() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        let now = crate::crypto::now_iso();
        store::increment_starvation(&ctx.conn, "starved", &now).unwrap();
        let map = store::starvation_counts(&ctx.conn).unwrap();
        assert_eq!(map.get("starved"), Some(&1));
        assert!(map.get("never-deferred").is_none());
    }

    #[test]
    fn starved_candidate_sorts_ahead_of_ordinary_one() {
        // Two agents with equally-valid inbox wakes. Without starvation, the
        // deterministic sort would order them alphabetically (alpha before
        // zeta). Give zeta a starvation count at the threshold and it must jump
        // to the front, so that under a budget of 1 it is the one that launches.
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = migrated_ctx(&temp);
        ctx.config.squire.max_concurrent = 1;
        ctx.config.squire.starvation_ticks = 3;
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/a/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        crate::commands::cmd_add_agent(
            &ctx,
            "zeta",
            &["src/z/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        // Both have unread inbox mail -> both are Inbox candidates.
        crate::commands::cmd_send(
            &ctx,
            "user",
            "alpha",
            "question",
            "normal",
            "for alpha",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();
        crate::commands::cmd_send(
            &ctx,
            "user",
            "zeta",
            "question",
            "normal",
            "for zeta",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();
        // zeta has been starved to the threshold.
        let now = crate::crypto::now_iso();
        for _ in 0..3 {
            store::increment_starvation(&ctx.conn, "zeta", &now).unwrap();
        }
        let plan = wake_plan(&ctx).unwrap();
        assert_eq!(
            plan.candidates.first().map(|c| c.agent.as_str()),
            Some("zeta"),
            "starved zeta must sort ahead of alpha; got {:?}",
            plan.candidates.iter().map(|c| &c.agent).collect::<Vec<_>>()
        );
    }

    #[test]
    fn below_threshold_does_not_promote() {
        // A candidate deferred only twice (threshold 3) is NOT promoted — the
        // normal alphabetical order still holds.
        let temp = tempfile::tempdir().unwrap();
        let mut ctx = migrated_ctx(&temp);
        ctx.config.squire.starvation_ticks = 3;
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/a/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        crate::commands::cmd_add_agent(
            &ctx,
            "zeta",
            &["src/z/**".to_string()],
            &[],
            None,
            Some("opencode"),
        )
        .unwrap();
        crate::commands::cmd_send(
            &ctx,
            "user",
            "alpha",
            "question",
            "normal",
            "for alpha",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();
        crate::commands::cmd_send(
            &ctx,
            "user",
            "zeta",
            "question",
            "normal",
            "for zeta",
            "",
            &None,
            &None,
            &[],
        )
        .unwrap();
        let now = crate::crypto::now_iso();
        store::increment_starvation(&ctx.conn, "zeta", &now).unwrap();
        store::increment_starvation(&ctx.conn, "zeta", &now).unwrap(); // only 2 < 3
        let plan = wake_plan(&ctx).unwrap();
        assert_eq!(
            plan.candidates.first().map(|c| c.agent.as_str()),
            Some("alpha"),
            "below-threshold zeta must NOT jump the queue"
        );
    }
}
