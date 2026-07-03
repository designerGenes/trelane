use crate::Context;
use crate::commands;
use crate::error::Result;
use crate::models::Message;
use crate::prompt;
use crate::store;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

type WaitEdges = HashMap<String, HashSet<String>>;
type WaitResult = (WaitEdges, Option<Vec<String>>);

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

/// Return (agent, reason) pairs for agents that should be woken.
pub fn wake_candidates(ctx: &Context) -> Result<Vec<(String, String)>> {
    let mut cands = Vec::new();
    let agents = store::list_agents(&ctx.conn)?;

    for agent in &agents {
        if commands::is_running(&ctx.conn, agent)? {
            continue;
        }
        let inbox = store::get_unprocessed_messages(&ctx.conn, agent)?;
        if !inbox.is_empty() {
            cands.push((
                agent.clone(),
                format!("inbox: {} unprocessed message(s)", inbox.len()),
            ));
            continue;
        }
        let parked = store::list_parked_tasks_for_agent(&ctx.conn, agent)?;
        let ready: Vec<String> = parked
            .iter()
            .filter(|e| prompt::park_satisfied(&ctx.conn, e).unwrap_or(false))
            .map(|e| e.task.clone())
            .collect();
        if !ready.is_empty() {
            cands.push((
                agent.clone(),
                format!("resume: parked task(s) ready: {}", ready.join(", ")),
            ));
        }
    }

    // Deadlock breaking — only when nothing else moves
    if cands.is_empty() {
        let (_, cycle) = wait_graph(&ctx.conn)?;
        if let Some(cycle) = cycle {
            let none_running = !cycle
                .iter()
                .any(|a| commands::is_running(&ctx.conn, a).unwrap_or(false));
            if none_running {
                let victim = cycle.iter().min().unwrap().clone();
                let mut display = cycle.clone();
                display.push(cycle[0].clone());
                cands.push((
                    victim,
                    format!(
                        "deadlock: wait-cycle {}. You are the designated breaker: proceed with a documented assumption, message your counterpart stating it, and unpark your task.",
                        display.join(" -> ")
                    ),
                ));
            }
        }
    }

    Ok(cands)
}

/// One pump tick. Returns number of agents launched.
pub fn tick(ctx: &Context, launcher_override: Option<&str>) -> Result<usize> {
    reap_leases(ctx)?;
    let cands = wake_candidates(ctx)?;
    if cands.is_empty() {
        return Ok(0);
    }

    let running_count = store::list_agents(&ctx.conn)?
        .iter()
        .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
        .count();

    let budget = ctx.config.pump.max_concurrent.saturating_sub(running_count);
    let mut launched = 0;

    for (agent, reason) in &cands {
        if launched >= budget {
            eprintln!("deferred wake of {agent} (concurrency budget reached)");
            continue;
        }
        eprintln!("{} waking {agent}: {reason}", crate::crypto::now_iso());
        commands::cmd_wake(ctx, agent, Some(reason.as_str()), launcher_override)?;
        launched += 1;
    }

    Ok(launched)
}
