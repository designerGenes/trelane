use crate::error::{Result, TrelaneError};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct BiplaneReport {
    pub project_root: String,
    pub analysis_at: String,
    pub agents: Vec<BiplaneAgentSummary>,
    pub parked_tasks: Vec<BiplaneParkedTask>,
    pub claims: Vec<BiplaneClaim>,
    pub deadlock: Option<Vec<String>>,
    pub safe_pocket_features: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BiplaneAgentSummary {
    pub name: String,
    pub description: String,
    pub writable: Vec<String>,
    pub forbidden_write: Vec<String>,
    pub launcher_agent: Option<String>,
    pub running: bool,
    pub inbox_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BiplaneParkedTask {
    pub task: String,
    pub agent: String,
    pub waiting_on: String,
    pub satisfied: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BiplaneClaim {
    pub path: String,
    pub holder: String,
    pub expires_human: String,
    pub contested: bool,
}

pub fn cmd_biplane(ctx: &crate::Context, safe_pocket_dir: Option<&Path>, json: bool) -> Result<()> {
    let agents = crate::store::list_agents(&ctx.conn)?;
    let mut agent_summaries = Vec::new();
    for name in &agents {
        let dom = crate::store::get_domain(&ctx.conn, name)?
            .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{name}'")))?;
        let running = crate::commands::is_running(&ctx.conn, name)?;
        let inbox_count = crate::store::get_unprocessed_messages(&ctx.conn, name)?.len();
        agent_summaries.push(BiplaneAgentSummary {
            name: name.clone(),
            description: dom.description.clone(),
            writable: dom.writable.clone(),
            forbidden_write: dom.forbidden_write.clone(),
            launcher_agent: dom.launcher_agent.clone(),
            running,
            inbox_count,
        });
    }

    let parked = crate::store::list_parked_tasks(&ctx.conn)?;
    let mut parked_summaries = Vec::new();
    for e in &parked {
        let satisfied = crate::prompt::park_satisfied(&ctx.conn, e).unwrap_or(false);
        parked_summaries.push(BiplaneParkedTask {
            task: e.task.clone(),
            agent: e.agent.clone(),
            waiting_on: e.waiting_on.clone(),
            satisfied,
        });
    }

    let claims = crate::store::list_claims(&ctx.conn)?;
    let mut claim_summaries = Vec::new();
    for c in &claims {
        claim_summaries.push(BiplaneClaim {
            path: c.path.clone(),
            holder: c.holder.clone(),
            expires_human: c.expires_human.clone(),
            contested: c.contested,
        });
    }

    let (_, cycle) = crate::pump::wait_graph(&ctx.conn)?;
    let deadlock = cycle.clone();

    let safe_pocket_features = scan_safe_pocket_features(safe_pocket_dir);

    let mut recommendations = Vec::new();
    if agents.is_empty() {
        recommendations.push(
            "No agents registered. Use 'trelane add-agent' to create agents with domains."
                .to_string(),
        );
    }
    for a in &agent_summaries {
        if a.inbox_count > 0 && !a.running {
            recommendations.push(format!(
                "Agent '{}' has {} unprocessed message(s) but is not running. Consider 'trelane wake {}' or 'trelane pump --once'.",
                a.name, a.inbox_count, a.name
            ));
        }
    }
    for p in &parked_summaries {
        if !p.satisfied {
            recommendations.push(format!(
                "Parked task '{}' for agent '{}' is still waiting on '{}'. Check if that agent can respond.",
                p.task, p.agent, p.waiting_on
            ));
        }
    }
    if deadlock.is_some() {
        recommendations.push("Deadlock detected in the wait-for graph. Run 'trelane pump --once' to trigger the designated breaker.".to_string());
    }
    if !safe_pocket_features.is_empty() {
        recommendations.push(format!(
            "Found {} safe_pocket feature file(s). Consider using 'trelane biplane' with --safe-pocket to generate a project plan from these features.",
            safe_pocket_features.len()
        ));
    }

    let report = BiplaneReport {
        project_root: ctx.root.display().to_string(),
        analysis_at: crate::crypto::now_iso(),
        agents: agent_summaries,
        parked_tasks: parked_summaries,
        claims: claim_summaries,
        deadlock,
        safe_pocket_features,
        recommendations,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_biplane_report(&report);
    }

    Ok(())
}

fn print_biplane_report(report: &BiplaneReport) {
    println!();
    crate::logo::print_logo();
    println!("  Biplane Project Analysis");
    println!("  ========================");
    println!("  Project   : {}", report.project_root);
    println!("  Analyzed  : {}", report.analysis_at);
    println!();

    println!("  Agents ({}):", report.agents.len());
    if report.agents.is_empty() {
        println!("    (none)");
    }
    for a in &report.agents {
        let status = if a.running { "RUNNING" } else { "stopped" };
        println!("    {:<16} {:<8} inbox={}", a.name, status, a.inbox_count);
        println!("      writable  : {}", a.writable.join(", "));
        if !a.forbidden_write.is_empty() {
            println!("      forbidden : {}", a.forbidden_write.join(", "));
        }
        if let Some(la) = &a.launcher_agent {
            println!("      model     : {}", la);
        }
    }
    println!();

    println!("  Parked tasks ({}):", report.parked_tasks.len());
    if report.parked_tasks.is_empty() {
        println!("    (none)");
    }
    for p in &report.parked_tasks {
        let sat = if p.satisfied { "READY" } else { "waiting" };
        println!("    {}  {} -> {} [{}]", p.task, p.agent, p.waiting_on, sat);
    }
    println!();

    println!("  Claims ({}):", report.claims.len());
    if report.claims.is_empty() {
        println!("    (none)");
    }
    for c in &report.claims {
        let tag = if c.contested { " (contested)" } else { "" };
        println!(
            "    {}  held by {} until {}{}",
            c.path, c.holder, c.expires_human, tag
        );
    }
    println!();

    if let Some(cycle) = &report.deadlock {
        let mut display = cycle.clone();
        display.push(cycle[0].clone());
        println!("  DEADLOCK: cycle detected: {}", display.join(" -> "));
    } else {
        println!("  Deadlock: none");
    }
    println!();

    if !report.safe_pocket_features.is_empty() {
        println!("  Safe_pocket features found:");
        for f in &report.safe_pocket_features {
            println!("    - {}", f);
        }
        println!();
    }

    println!("  Recommendations:");
    if report.recommendations.is_empty() {
        println!("    (none -- the swarm looks healthy)");
    }
    for r in &report.recommendations {
        println!("    - {}", r);
    }
    println!();
}

fn scan_safe_pocket_features(safe_pocket_dir: Option<&Path>) -> Vec<String> {
    let dir = match safe_pocket_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(&home).join(".safe_pocket")
        }
    };

    if !dir.is_dir() {
        return Vec::new();
    }

    if safe_pocket_dir.is_some() {
        return scan_feature_dir(&dir);
    }

    let mut found = Vec::new();
    if let Ok(pocket_entries) = fs::read_dir(&dir) {
        for entry in pocket_entries.flatten() {
            let pocket = entry.path();
            let features = pocket.join("FEATURES");
            if features.is_dir() {
                found.extend(scan_feature_dir(&features));
            }
        }
    }
    found
}

fn scan_feature_dir(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(scan_feature_dir(&path));
            } else if path.extension().is_some_and(|ext| ext == "md")
                && let Ok(rel) = path.strip_prefix(dir)
            {
                found.push(rel.display().to_string());
            }
        }
    }
    found
}
