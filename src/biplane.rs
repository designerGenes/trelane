use crate::error::{Result, TrelaneError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const BIPLANE_REPORT_FILENAME: &str = "biplane-report.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiplanePlan {
    pub agents: Vec<BiplanePlanAgent>,
    pub initial_tasks: Vec<BiplanePlanTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiplanePlanAgent {
    pub name: String,
    pub description: String,
    pub writable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiplanePlanTask {
    pub agent: String,
    pub subject: String,
    pub body: String,
}

pub fn run_biplane_plan(
    project_root: &Path,
    model: &str,
    max_agents: usize,
) -> Result<BiplanePlan> {
    let project_structure = scan_project_structure(project_root);
    let safe_pocket_features = collect_safe_pocket_feature_text(project_root);

    let prompt = compose_biplane_planning_prompt(
        &project_structure,
        &safe_pocket_features,
        max_agents,
        project_root,
    );

    let prompt_file = project_root.join(".trelane").join("biplane-plan-prompt.md");
    if let Some(parent) = prompt_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&prompt_file, &prompt)?;

    let launcher_template = resolve_launcher_template(model)?;
    let cmd = launcher_template
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{agent}", "biplane")
        .replace("{root}", &project_root.display().to_string());

    println!("[biplane] Launching planner with model '{}'...", model);
    println!("[biplane] Prompt: {}", prompt_file.display());

    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(project_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TrelaneError::msg(format!(
            "biplane planner failed: {}",
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let plan = parse_biplane_plan(&stdout, max_agents)?;

    Ok(plan)
}

fn resolve_launcher_template(model: &str) -> Result<String> {
    Ok(format!(
        "opencode run --model openrouter/{} --dir {{root}} \"$(cat {{prompt_file}})\"",
        model
    ))
}

fn compose_biplane_planning_prompt(
    structure: &str,
    features: &str,
    max_agents: usize,
    project_root: &Path,
) -> String {
    format!(
        r#"# Biplane Project Analysis

You are analyzing the project at `{}` to determine how to split it across multiple AI agents using the Trelane coordination protocol.

## Project Structure

```
{}
```

## Feature Files

{}

## Your Task

Analyze this project and propose a domain split for up to {} agents. Each agent should own a distinct area of the codebase. Consider:

- Natural separation of concerns (e.g., UI vs API vs data vs tests)
- File paths that can be grouped into writable globs
- Dependencies between areas (agents that need to coordinate)
- Balanced workload

Output your plan as a JSON object with this exact structure (and nothing else after the JSON):

```json
{{
  "agents": [
    {{
      "name": "short-name",
      "description": "what this agent owns",
      "writable": ["src/path/**", "other/path/**"]
    }}
  ],
  "initial_tasks": [
    {{
      "agent": "short-name",
      "subject": "first task for this agent",
      "body": "detailed instructions for what to build first"
    }}
  ]
}}
```

Rules:
- Use 2-{} agents
- Agent names must be lowercase with hyphens only (e.g., "frontend", "data-model")
- Each agent must have at least one writable glob
- writable globs should be specific enough to avoid overlap
- Provide one initial task per agent
- Do not include .trelane/** or .git/** in writable (those are forbidden automatically)
"#,
        project_root.display(),
        structure,
        if features.is_empty() {
            "(no safe_pocket feature files found)"
        } else {
            features
        },
        max_agents,
        max_agents
    )
}

fn parse_biplane_plan(output: &str, max_agents: usize) -> Result<BiplanePlan> {
    let json_start = output
        .find('{')
        .ok_or_else(|| TrelaneError::msg("biplane planner did not produce JSON output"))?;
    let json_end = output
        .rfind('}')
        .ok_or_else(|| TrelaneError::msg("biplane planner JSON output is incomplete"))?;
    let json_str = &output[json_start..=json_end];

    let mut plan: BiplanePlan = serde_json::from_str(json_str)
        .map_err(|e| TrelaneError::msg(format!("failed to parse biplane plan JSON: {e}")))?;

    if plan.agents.len() > max_agents {
        plan.agents.truncate(max_agents);
    }
    if plan.agents.is_empty() {
        return Err(TrelaneError::msg("biplane planner proposed zero agents"));
    }

    for a in &mut plan.agents {
        if a.name.is_empty() {
            return Err(TrelaneError::msg(
                "biplane planner produced an agent with an empty name",
            ));
        }
        a.name = a.name.to_lowercase().replace(' ', "-");
        if a.writable.is_empty() {
            a.writable.push(format!("src/{}/**", a.name));
        }
    }

    plan.initial_tasks
        .retain(|t| plan.agents.iter().any(|a| a.name == t.agent));

    Ok(plan)
}

fn scan_project_structure(root: &Path) -> String {
    let mut result = Vec::new();
    scan_dir(root, root, 0, 3, &mut result);
    result.join("\n")
}

fn scan_dir(root: &Path, dir: &Path, depth: usize, max_depth: usize, result: &mut Vec<String>) {
    if depth > max_depth {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if name_str.starts_with('.') && name_str != ".env" {
                continue;
            }
            if matches!(
                name_str.as_ref(),
                "target" | "node_modules" | "__pycache__" | "dist" | "build"
            ) {
                continue;
            }

            let rel = path.strip_prefix(root).unwrap_or(&path);
            let prefix = "  ".repeat(depth);
            if path.is_dir() {
                result.push(format!("{}{}/", prefix, rel.display()));
                scan_dir(root, &path, depth + 1, max_depth, result);
            } else {
                result.push(format!("{}{}", prefix, rel.display()));
            }
        }
    }
}

fn collect_safe_pocket_feature_text(project_root: &Path) -> String {
    let pocket = match find_pocket_for_project(project_root) {
        Some(p) => p,
        None => return String::new(),
    };
    let features_dir = pocket.join("FEATURES");
    if !features_dir.is_dir() {
        return String::new();
    }

    let mut texts = Vec::new();
    collect_feature_text(&features_dir, &features_dir, &mut texts);
    texts.join("\n\n---\n\n")
}

fn collect_feature_text(base: &Path, dir: &Path, texts: &mut Vec<String>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_feature_text(base, &path, texts);
            } else if path.extension().is_some_and(|ext| ext == "md")
                && let Ok(rel) = path.strip_prefix(base)
                && let Ok(content) = fs::read_to_string(&path)
            {
                let truncated = if content.len() > 2000 {
                    format!("{}...(truncated)", &content[..2000])
                } else {
                    content
                };
                texts.push(format!("## {}\n\n{}", rel.display(), truncated));
            }
        }
    }
}

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
    let report = generate_biplane_report(ctx, safe_pocket_dir)?;

    if let Some(pocket) = find_pocket_for_project(&ctx.root) {
        let report_path = pocket.join(BIPLANE_REPORT_FILENAME);
        fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;
        if !json {
            println!("  Biplane report saved to {}", report_path.display());
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_biplane_report(&report);
    }

    Ok(())
}

pub fn generate_biplane_report(
    ctx: &crate::Context,
    safe_pocket_dir: Option<&Path>,
) -> Result<BiplaneReport> {
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

    Ok(report)
}

pub fn find_pocket_for_project(project_root: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let pockets_root = PathBuf::from(&home).join(".safe_pocket");
    let entries = fs::read_dir(&pockets_root).ok()?;
    for entry in entries.flatten() {
        let pocket = entry.path();
        let manifest_path = pocket.join("manifest.json");
        if let Ok(text) = fs::read_to_string(&manifest_path)
            && text.contains(&project_root.display().to_string())
        {
            return Some(pocket);
        }
        let agents_md = pocket.join("AGENTS.md");
        if let Ok(text) = fs::read_to_string(&agents_md)
            && text.contains(&project_root.display().to_string())
        {
            return Some(pocket);
        }
    }
    None
}

pub fn has_existing_biplane_report(project_root: &Path) -> Option<PathBuf> {
    let pocket = find_pocket_for_project(project_root)?;
    let report_path = pocket.join(BIPLANE_REPORT_FILENAME);
    if report_path.exists() {
        Some(report_path)
    } else {
        None
    }
}

pub fn cmd_welcome(project: Option<PathBuf>) -> Result<()> {
    let root = match project {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };

    crate::logo::print_logo();
    println!();

    let already_trelane = root.join(".trelane").join("trelane.db").exists();
    let pocket = find_pocket_for_project(&root);

    if let Some(ref pocket_path) = pocket {
        println!("  Safe_pocket detected: {}", pocket_path.display());
        let features_dir = pocket_path.join("FEATURES");
        if features_dir.is_dir() {
            let features = scan_feature_dir(&features_dir);
            if !features.is_empty() {
                println!("  Feature files found: {}", features.len());
            }
        }
        println!();
    }

    if let Some(ref report_path) = has_existing_biplane_report(&root) {
        println!("  This project has already been analyzed by Biplane.");
        println!("  Report: {}", report_path.display());
        println!();
        println!("  To view the report:  trelane biplane");
        println!(
            "  To re-analyze:       trelane biplane --safe-pocket {}",
            pocket
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
        println!();
    } else if pocket.is_some() {
        println!("  This safe_pocket project has not been analyzed by Biplane yet.");
        println!();
        print!("  Run Biplane to analyze this project? [Y/n] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();
        if input.is_empty() || input == "y" || input == "yes" {
            if !already_trelane {
                crate::commands::cmd_init(Some(root.clone()))?;
            }
            let ctx = crate::Context::open(Some(&root))?;
            return cmd_biplane(&ctx, pocket.as_deref(), false);
        }
        println!();
        println!("  You can run it later with:  trelane biplane");
        println!();
    }

    if already_trelane {
        println!(
            "  Trelane session: ACTIVE at {}",
            root.join(".trelane").display()
        );
        println!();
        println!("  Common commands:");
        println!("    trelane status              -- show swarm state");
        println!("    trelane biplane             -- analyze project and get recommendations");
        println!("    trelane add-agent NAME --writable 'glob'  -- register an agent");
        println!(
            "    trelane send --from user --to AGENT --type question --subject '...'  -- assign work"
        );
        println!("    trelane pump --watch        -- start the pump");
        println!("    trelane --testing tests/full-usage-scenario.json  -- run the test harness");
        println!();
    } else {
        println!("  No Trelane session found at this location.");
        println!();
        println!("  Getting started:");
        println!("    trelane init                -- initialize a session here");
        println!("    trelane .                   -- attach to the current project");
        println!("    trelane biplane             -- analyze a project and get recommendations");
        println!();
        println!("  Test harness (zero tokens):");
        println!("    trelane --testing tests/full-usage-scenario.json");
        println!();
        println!("  Interactive test (real AI in tmux):");
        println!("    trelane --testing tests/full-usage-scenario-interactive.json");
        println!();
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
