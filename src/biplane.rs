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

/// Convert an AI-generated `BiplanePlan` into an editable `ProjectDescription`.
/// Each plan agent becomes a domain; the plan's initial tasks are attached to
/// their agent's domain as `PlannedWork`. Used so the interactive Biplane UI
/// can present AI-detected domains for review/editing.
pub fn plan_to_description(
    plan: &BiplanePlan,
    project_name: &str,
    max_agents: usize,
) -> ProjectDescription {
    let domains = plan
        .agents
        .iter()
        .map(|a| {
            let planned_work = plan
                .initial_tasks
                .iter()
                .filter(|t| t.agent == a.name)
                .map(|t| PlannedWork {
                    subject: t.subject.clone(),
                    body: t.body.clone(),
                    priority: default_work_priority(),
                })
                .collect();
            DomainSpec {
                name: a.name.clone(),
                description: a.description.clone(),
                writable: if a.writable.is_empty() {
                    vec![format!("src/{}/**", a.name)]
                } else {
                    a.writable.clone()
                },
                forbidden_write: vec![],
                depends_on: vec![],
                planned_work,
                agents: default_agent_count(),
                model: None,
            }
        })
        .collect();

    ProjectDescription {
        name: project_name.to_string(),
        description: String::new(),
        domains,
        max_agents: Some(max_agents.max(1)),
        default_model: None,
    }
}

/// The model Biplane uses for its own analysis when the caller hasn't chosen
/// one. Matches the launch path's default so `trelane` and `trelane biplane`
/// agree on which planner model runs out of the box.
pub fn default_biplane_model() -> String {
    "glm-5.2".to_string()
}

/// Register agents and queue initial tasks for a curated `ProjectDescription`
/// (e.g. one saved by the Biplane UI). Derives the deterministic plan from the
/// description, then applies it to the session: each included domain becomes a
/// registered agent (carrying its writable globs, model, and description) and
/// its planned work is queued as inbox messages. Idempotent for agents that
/// already exist. Returns the number of agents newly registered.
pub fn apply_description_to_session(
    ctx: &crate::Context,
    desc: &ProjectDescription,
) -> Result<usize> {
    validate_description(desc)?;
    let budget = desc.max_agents.unwrap_or_else(|| desc.domains.len().max(1));
    let plan = plan_from_description(desc, budget)?;
    apply_plan_to_session(ctx, desc, &plan)
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
    execute_biplane_plan(project_root, model, max_agents, &prompt)
}

/// Run the planner over an explicit list of source folders, with no automatic
/// safe-pocket detection. Every folder in `sources` is scanned for both its
/// directory structure and any markdown documentation it contains; the
/// combined material is what the model uses to propose domains. `project_root`
/// is still where the session lives (scaffolding, prompt/output files) and is
/// always included as a source. Empty `sources` falls back to just the root.
pub fn run_biplane_plan_from_sources(
    project_root: &Path,
    sources: &[PathBuf],
    model: &str,
    max_agents: usize,
) -> Result<BiplanePlan> {
    let prompt = compose_sources_prompt(project_root, sources, max_agents);
    execute_biplane_plan(project_root, model, max_agents, &prompt)
}

/// Build the planning prompt from an explicit list of source folders (plus the
/// always-included project root). Pure — no model call — so the scanning logic
/// is testable. Every folder contributes both its structure and any markdown
/// docs it contains; there is no automatic safe-pocket detection.
fn compose_sources_prompt(project_root: &Path, sources: &[PathBuf], max_agents: usize) -> String {
    // Deduplicate, always including the project root itself.
    let mut scan: Vec<PathBuf> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in std::iter::once(project_root.to_path_buf()).chain(sources.iter().cloned()) {
        let canon = p.canonicalize().unwrap_or(p);
        if seen.insert(canon.clone()) {
            scan.push(canon);
        }
    }

    let mut structure_blocks = Vec::new();
    let mut doc_blocks = Vec::new();
    for dir in &scan {
        if !dir.is_dir() {
            structure_blocks.push(format!(
                "## {} (not found or not a directory)",
                dir.display()
            ));
            continue;
        }
        structure_blocks.push(format!(
            "## {}\n{}",
            dir.display(),
            scan_project_structure(dir)
        ));
        let mut texts = Vec::new();
        collect_feature_text(dir, dir, &mut texts);
        if !texts.is_empty() {
            doc_blocks.push(format!(
                "### from {}\n\n{}",
                dir.display(),
                texts.join("\n\n---\n\n")
            ));
        }
    }

    let structure = structure_blocks.join("\n\n");
    let docs = doc_blocks.join("\n\n===\n\n");
    compose_biplane_planning_prompt(&structure, &docs, max_agents, project_root)
}

/// Shared executor: writes the prompt, runs the launcher with retries, and
/// parses the plan. Split out so both the legacy and explicit-source planners
/// share one code path.
fn execute_biplane_plan(
    project_root: &Path,
    model: &str,
    max_agents: usize,
    prompt: &str,
) -> Result<BiplanePlan> {
    let prompt_file = project_root.join(".trelane").join("biplane-plan-prompt.md");
    if let Some(parent) = prompt_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&prompt_file, prompt)?;

    let launcher_template = resolve_launcher_template(model)?;
    let cmd = launcher_template
        .replace("{prompt_file}", &prompt_file.display().to_string())
        .replace("{agent}", "biplane")
        .replace("{root}", &project_root.display().to_string());

    println!("[biplane] Launching planner with model '{}'...", model);
    println!("[biplane] Prompt: {}", prompt_file.display());
    println!("[biplane] Analyzing project structure and feature files...");
    println!();

    let mut last_error = String::new();
    let output_file = project_root
        .join(".trelane")
        .join("biplane-plan-output.txt");
    let runner_script = project_root.join(".trelane").join("biplane-runner.sh");

    // Write the runner as a standalone shell script so command substitution
    // ("$(cat prompt)") is preserved exactly. Building a nested `sh -c '...'`
    // string mangles the quoting and corrupts the request opencode sends.
    fs::write(&runner_script, format!("#!/bin/sh\n{cmd}\n"))?;

    let spinner_frames = ["|", "/", "-", "\\"];
    let mut spinner_idx = 0usize;

    for attempt in 1..=3 {
        if attempt > 1 {
            println!("[biplane] Retrying (attempt {}/3)...", attempt);
            std::thread::sleep(std::time::Duration::from_secs(3));
        }

        print!("[biplane] Thinking ");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let out_handle = std::fs::File::create(&output_file)?;
        let err_handle = out_handle.try_clone()?;

        let mut child = Command::new("sh")
            .arg(&runner_script)
            .current_dir(project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(out_handle))
            .stderr(std::process::Stdio::from(err_handle))
            .spawn()?;

        // Animate a spinner while the planner runs.
        let exit_status;
        loop {
            match child.try_wait()? {
                Some(s) => {
                    println!(" done");
                    let _ = std::io::stdout().flush();
                    exit_status = s;
                    break;
                }
                None => {
                    print!("\r[biplane] Thinking {} ", spinner_frames[spinner_idx]);
                    let _ = std::io::stdout().flush();
                    spinner_idx = (spinner_idx + 1) % spinner_frames.len();
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }

        let stdout = std::fs::read_to_string(&output_file).unwrap_or_default();

        let text = extract_text_from_json_events(&stdout);
        let cleaned = strip_ansi(&text);

        if let Ok(plan) = parse_biplane_plan(&cleaned, max_agents) {
            println!(
                "[biplane] Plan received: {} agent(s) proposed",
                plan.agents.len()
            );
            return Ok(plan);
        }

        if let Ok(plan) = parse_biplane_plan(&strip_ansi(&stdout), max_agents) {
            println!(
                "[biplane] Plan received: {} agent(s) proposed",
                plan.agents.len()
            );
            return Ok(plan);
        }

        last_error = if stdout.trim().is_empty() {
            format!("exit code: {:?}", exit_status.code())
        } else {
            let preview = if stdout.len() > 500 {
                &stdout[..500]
            } else {
                &stdout
            };
            format!("exit code: {:?}, output: {}", exit_status.code(), preview)
        };

        if attempt < 3 {
            eprintln!(
                "\n[biplane] Attempt {} failed: {}",
                attempt,
                &last_error[..200.min(last_error.len())]
            );
        }
    }

    Err(TrelaneError::msg(format!(
        "biplane planner failed after 3 attempts: {}",
        last_error
    )))
}

fn resolve_launcher_template(model: &str) -> Result<String> {
    // Determine the fully-qualified model id. Prefer the exact id from a
    // configured launcher profile so we never guess an "openrouter/{model}"
    // id -- an invalid id makes OpenRouter return an opaque
    // "Unexpected server error".
    let model_id = resolve_model_id(model)?;
    Ok(format!(
        "opencode run --model {model_id} --dir {{root}} \"$(cat {{prompt_file}})\""
    ))
}

/// Resolve a launcher label (e.g. "glm-5.2") to a fully-qualified model id
/// (e.g. "openrouter/z-ai/glm-5.2") by reading the matching launcher profile's
/// `--model` argument. Falls back to the label itself if no profile matches.
fn resolve_model_id(model: &str) -> Result<String> {
    let config = crate::load_config()?;
    if let Some(profile) = config.launcher.profiles.get(model)
        && let Some(id) = extract_model_arg(profile)
    {
        return Ok(id);
    }
    Ok(model.to_string())
}

fn extract_model_arg(profile: &str) -> Option<String> {
    let tokens: Vec<&str> = profile.split_whitespace().collect();
    for (i, t) in tokens.iter().enumerate() {
        if (*t == "--model" || *t == "-m") && i + 1 < tokens.len() {
            return Some(tokens[i + 1].to_string());
        }
    }
    None
}

fn compose_biplane_planning_prompt(
    structure: &str,
    features: &str,
    max_agents: usize,
    project_root: &Path,
) -> String {
    // Bias toward an actual split: ask for at least 2 agents whenever the cap
    // permits, so a greenfield project doesn't collapse to a single catch-all.
    let suggested_min = max_agents.min(2).max(1);
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

Analyze this project and propose a domain split of {} to {} agents. Each agent should own a distinct area of the codebase. Even for a greenfield project being built from scratch, prefer a meaningful split (for example a frontend/backend/data/tests separation) over a single catch-all agent — a single agent should only be proposed if the project is genuinely trivial. Consider:

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
- Propose {} to {} agents; favor the higher end when the project has clearly separable concerns
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
        suggested_min,
        max_agents,
        suggested_min,
        max_agents
    )
}

fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(&next) = chars.peek() {
                chars.next();
                if next == 'm' {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn extract_text_from_json_events(stdout: &str) -> String {
    let mut text_parts = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(text) = v
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
        {
            text_parts.push(text.to_string());
        }
    }
    text_parts.join("")
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

    let (_, cycle) = crate::squire::wait_graph(&ctx.conn)?;
    let deadlock = cycle.clone();

    // Determine the safe_pocket to scan: explicit override > project-linked pocket > SPOCKET_ROOT env
    let effective_pocket = safe_pocket_dir
        .map(Path::to_path_buf)
        .or_else(|| find_pocket_for_project(&ctx.root));

    let safe_pocket_features = if let Some(ref pocket) = effective_pocket {
        let features_dir = pocket.join("FEATURES");
        if features_dir.is_dir() {
            scan_feature_dir(&features_dir)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

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
                "Agent '{}' has {} unprocessed message(s) but is not running. Consider 'trelane wake {}' or 'trelane squire --once'.",
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
        recommendations.push("Deadlock detected in the wait-for graph. Run 'trelane squire --once' to trigger the designated breaker.".to_string());
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
    // 1. Check SPOCKET_ROOT env var (set when project is open in VS Code)
    if let Ok(root) = std::env::var("SPOCKET_ROOT")
        && !root.is_empty()
    {
        let pocket = PathBuf::from(&root);
        if pocket.is_dir() {
            return Some(pocket);
        }
    }

    // 2. Scan ~/.safe_pocket/*/manifest.json for a pocket linked to this project
    let home = std::env::var("HOME").ok()?;
    let pockets_root = PathBuf::from(&home).join(".safe_pocket");
    let entries = fs::read_dir(&pockets_root).ok()?;

    let project_str = project_root.display().to_string();
    let mut matches = Vec::new();

    for entry in entries.flatten() {
        let pocket = entry.path();
        let manifest_path = pocket.join("manifest.json");
        if let Ok(text) = fs::read_to_string(&manifest_path)
            && text.contains(&project_str)
        {
            matches.push(pocket);
            continue;
        }
        let agents_md = pocket.join("AGENTS.md");
        if let Ok(text) = fs::read_to_string(&agents_md)
            && text.contains(&project_str)
        {
            matches.push(pocket);
        }
    }

    match matches.len() {
        0 => None,
        1 => Some(matches.into_iter().next().unwrap()),
        _ => {
            // Multiple pockets linked to this project -- ask the user to choose.
            eprintln!();
            eprintln!("  Multiple safe_pockets are linked to this project:");
            for (i, p) in matches.iter().enumerate() {
                eprintln!("  [{}] {}", i + 1, p.display());
            }
            eprintln!();
            eprint!("  Select a pocket (1-{}): ", matches.len());
            use std::io::{self, Write};
            let _ = io::stderr().flush();
            let mut input = String::new();
            io::stdin().read_line(&mut input).ok()?;
            let choice: usize = input.trim().parse().ok()?;
            if choice >= 1 && choice <= matches.len() {
                Some(matches.into_iter().nth(choice - 1).unwrap())
            } else {
                None
            }
        }
    }
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
        println!("    trelane squire --watch        -- start the prop");
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

#[allow(dead_code)]
fn scan_safe_pocket_features(safe_pocket_dir: Option<&Path>) -> Vec<String> {
    // If an explicit dir is given, scan it directly.
    if let Some(dir) = safe_pocket_dir {
        if dir.is_dir() {
            return scan_feature_dir(dir);
        }
        return Vec::new();
    }

    // No explicit dir: try to find the pocket linked to the current project.
    // Check SPOCKET_ROOT first, then scan manifests.
    if let Ok(root) = std::env::var("SPOCKET_ROOT")
        && !root.is_empty()
    {
        let pocket = PathBuf::from(&root);
        let features = pocket.join("FEATURES");
        if features.is_dir() {
            return scan_feature_dir(&features);
        }
    }

    // Try to find via manifest scan -- but we need a project root for that.
    // In the biplane report context, we don't always have one, so fall back
    // to scanning all pockets only if no SPOCKET_ROOT is set.
    // The caller (generate_biplane_report) should pass the project root's
    // linked pocket via safe_pocket_dir to avoid scanning everything.
    Vec::new()
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

// ==================== Structured project-description format ====================
//
// A human- (or safe_pocket-) authored "high-level project description": the
// intended domain split for a project, its planned work, and the dependency
// edges between domains. This is the deterministic, offline counterpart to the
// LLM-driven `run_biplane_plan` above -- no model call, fully reproducible, and
// safe to run before a project has ever been analyzed.

/// The top-level structured description a user hands to Biplane via
/// `trelane biplane --describe <file.json>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDescription {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub domains: Vec<DomainSpec>,
    /// Optional cap on total agents. The effective cap is the min of this and
    /// any `--max-agents` passed on the CLI.
    #[serde(default)]
    pub max_agents: Option<usize>,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub writable: Vec<String>,
    #[serde(default)]
    pub forbidden_write: Vec<String>,
    /// Names of other domains that must be underway before this one. Used only
    /// for ordering the work -- never for write permissions.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub planned_work: Vec<PlannedWork>,
    /// Desired number of agents for this domain (default 1). Plan derivation
    /// keeps exactly one agent per domain so writable globs never overlap; the
    /// next-steps scheduler honours the requested count when allocating the
    /// agent budget across phases.
    #[serde(default = "default_agent_count")]
    pub agents: usize,
    /// Model/launcher profile to use for this domain's agent(s).  When set,
    /// overrides the project-level `default_model`.  Populated by the Biplane
    /// UI's model selector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

fn default_agent_count() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedWork {
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default = "default_work_priority")]
    pub priority: String,
}

fn default_work_priority() -> String {
    "normal".to_string()
}

/// Read and validate a project-description JSON file.
pub fn load_project_description(path: &Path) -> Result<ProjectDescription> {
    let text = fs::read_to_string(path).map_err(|e| {
        TrelaneError::msg(format!(
            "cannot read project description {}: {e}",
            path.display()
        ))
    })?;
    let desc: ProjectDescription = serde_json::from_str(&text).map_err(|e| {
        TrelaneError::msg(format!(
            "invalid project description JSON in {}: {e}",
            path.display()
        ))
    })?;
    validate_description(&desc)?;
    Ok(desc)
}

/// Validate structural invariants: non-empty names, unique domains, at least
/// one writable glob per domain, dependency targets that exist, and -- most
/// importantly -- no dependency cycle among domains (which would make the work
/// impossible to order).
pub fn validate_description(desc: &ProjectDescription) -> Result<()> {
    if desc.name.trim().is_empty() {
        return Err(TrelaneError::msg(
            "project description: 'name' must not be empty",
        ));
    }
    if desc.domains.is_empty() {
        return Err(TrelaneError::msg(
            "project description: at least one domain is required",
        ));
    }

    let names: std::collections::HashSet<&str> =
        desc.domains.iter().map(|d| d.name.as_str()).collect();
    if names.len() != desc.domains.len() {
        return Err(TrelaneError::msg(
            "project description: domain names must be unique",
        ));
    }

    for d in &desc.domains {
        if d.name.trim().is_empty() {
            return Err(TrelaneError::msg(
                "project description: a domain has an empty name",
            ));
        }
        if d.writable.is_empty() {
            return Err(TrelaneError::msg(format!(
                "project description: domain '{}' has no writable globs",
                d.name
            )));
        }
        if d.agents == 0 {
            return Err(TrelaneError::msg(format!(
                "project description: domain '{}' requests 0 agents (must be >= 1)",
                d.name
            )));
        }
        for dep in &d.depends_on {
            if dep == &d.name {
                return Err(TrelaneError::msg(format!(
                    "project description: domain '{}' depends on itself",
                    d.name
                )));
            }
            if !names.contains(dep.as_str()) {
                return Err(TrelaneError::msg(format!(
                    "project description: domain '{}' depends_on unknown domain '{}'",
                    d.name, dep
                )));
            }
        }
    }

    if let Some(cycle) = domain_dependency_cycle(desc) {
        let mut display = cycle.clone();
        display.push(cycle[0].clone());
        return Err(TrelaneError::msg(format!(
            "project description: dependency cycle among domains: {}",
            display.join(" -> ")
        )));
    }
    Ok(())
}

/// DFS cycle detection over the domain `depends_on` graph. Mirrors the wait-for
/// graph detector in `squire.rs`; returns the nodes on the first cycle found.
fn domain_dependency_cycle(desc: &ProjectDescription) -> Option<Vec<String>> {
    use std::collections::{HashMap, HashSet};
    let edges: HashMap<&str, &Vec<String>> = desc
        .domains
        .iter()
        .map(|d| (d.name.as_str(), &d.depends_on))
        .collect();

    let mut visited = HashSet::new();
    let mut names: Vec<&str> = desc.domains.iter().map(|d| d.name.as_str()).collect();
    names.sort();
    for start in names {
        let mut stack = Vec::new();
        let mut on_stack = HashSet::new();
        if let Some(cycle) = dep_dfs(start, &edges, &mut visited, &mut stack, &mut on_stack) {
            return Some(cycle);
        }
    }
    None
}

fn dep_dfs(
    node: &str,
    edges: &std::collections::HashMap<&str, &Vec<String>>,
    visited: &mut std::collections::HashSet<String>,
    stack: &mut Vec<String>,
    on_stack: &mut std::collections::HashSet<String>,
) -> Option<Vec<String>> {
    if on_stack.contains(node) {
        let start = stack.iter().position(|n| n == node).unwrap();
        return Some(stack[start..].to_vec());
    }
    if visited.contains(node) {
        return None;
    }
    visited.insert(node.to_string());
    stack.push(node.to_string());
    on_stack.insert(node.to_string());

    if let Some(deps) = edges.get(node) {
        for d in deps.iter() {
            if let Some(cycle) = dep_dfs(d, edges, visited, stack, on_stack) {
                return Some(cycle);
            }
        }
    }

    stack.pop();
    on_stack.remove(node);
    None
}

/// Topological order of domains, dependencies first, ties broken
/// lexicographically for determinism. Errors if a cycle prevents ordering
/// (validate_description catches this earlier, but the guard is kept so this is
/// safe to call directly).
pub fn topo_order_domains(desc: &ProjectDescription) -> Result<Vec<String>> {
    use std::collections::HashMap;
    let mut indeg: HashMap<&str, usize> = desc
        .domains
        .iter()
        .map(|d| (d.name.as_str(), d.depends_on.len()))
        .collect();

    let mut order: Vec<String> = Vec::new();
    loop {
        let mut ready: Vec<&str> = indeg
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(n, _)| *n)
            .collect();
        if ready.is_empty() {
            break;
        }
        ready.sort();
        for n in ready {
            order.push(n.to_string());
            indeg.remove(n);
            for d in &desc.domains {
                if d.depends_on.iter().any(|x| x == n)
                    && let Some(deg) = indeg.get_mut(d.name.as_str())
                {
                    *deg = deg.saturating_sub(1);
                }
            }
        }
    }

    if order.len() != desc.domains.len() {
        return Err(TrelaneError::msg(
            "project description: dependency cycle prevents ordering",
        ));
    }
    Ok(order)
}

/// Derive a concrete, sound agent plan from a description: exactly one agent per
/// domain (so writable globs never overlap), domains taken in dependency order,
/// truncated to the effective agent cap. Planned work becomes each agent's
/// initial tasks.
pub fn plan_from_description(desc: &ProjectDescription, max_agents: usize) -> Result<BiplanePlan> {
    let order = topo_order_domains(desc)?;
    let by_name: std::collections::HashMap<&str, &DomainSpec> =
        desc.domains.iter().map(|d| (d.name.as_str(), d)).collect();

    let cap = desc.max_agents.unwrap_or(max_agents).min(max_agents).max(1);

    let mut agents = Vec::new();
    let mut initial_tasks = Vec::new();
    for name in &order {
        if agents.len() >= cap {
            break;
        }
        let d = by_name[name.as_str()];
        agents.push(BiplanePlanAgent {
            name: d.name.clone(),
            description: d.description.clone(),
            writable: d.writable.clone(),
        });
        for w in &d.planned_work {
            initial_tasks.push(BiplanePlanTask {
                agent: d.name.clone(),
                subject: w.subject.clone(),
                body: w.body.clone(),
            });
        }
    }

    let kept: std::collections::HashSet<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    initial_tasks.retain(|t| kept.contains(t.agent.as_str()));
    Ok(BiplanePlan {
        agents,
        initial_tasks,
    })
}

// ----------------------------- next-steps analysis -----------------------------

#[derive(Debug, Clone, Serialize)]
pub struct NextStepsPlan {
    pub agent_budget: usize,
    pub total_domains: usize,
    pub phases: Vec<NextStepsPhase>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextStepsPhase {
    pub phase: usize,
    pub assignments: Vec<NextStepsAssignment>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextStepsAssignment {
    pub domain: String,
    pub agents: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_task: Option<String>,
}

/// Given an agent budget, schedule the domains into phases: each phase runs the
/// domains whose dependencies are already satisfied, greedily spending the
/// budget (honouring each domain's requested agent count). When there are more
/// domains than agents, the work spills into later phases -- modelling agents
/// hopping to the next unblocked domain as they finish.
pub fn next_steps_plan(desc: &ProjectDescription, agent_budget: usize) -> Result<NextStepsPlan> {
    use std::collections::HashSet;
    let budget = agent_budget.max(1);
    let by_name: std::collections::HashMap<&str, &DomainSpec> =
        desc.domains.iter().map(|d| (d.name.as_str(), d)).collect();

    let total = desc.domains.len();
    let mut completed: HashSet<String> = HashSet::new();
    let mut phases: Vec<NextStepsPhase> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let mut guard = 0;
    while completed.len() < total {
        guard += 1;
        if guard > total + 1 {
            return Err(TrelaneError::msg(
                "next-steps: failed to schedule domains (possible dependency cycle)",
            ));
        }

        let mut available: Vec<&str> = desc
            .domains
            .iter()
            .filter(|d| !completed.contains(&d.name))
            .filter(|d| d.depends_on.iter().all(|dep| completed.contains(dep)))
            .map(|d| d.name.as_str())
            .collect();
        available.sort();
        if available.is_empty() {
            return Err(TrelaneError::msg(
                "next-steps: work remains but no domain is unblocked (dependency cycle)",
            ));
        }

        let mut remaining = budget;
        let mut assignments = Vec::new();
        for name in &available {
            if remaining == 0 {
                break;
            }
            let d = by_name[*name];
            let want = d.agents.max(1).min(remaining);
            assignments.push(NextStepsAssignment {
                domain: d.name.clone(),
                agents: want,
                first_task: d.planned_work.first().map(|w| w.subject.clone()),
            });
            remaining -= want;
            completed.insert(d.name.clone());
        }
        phases.push(NextStepsPhase {
            phase: phases.len() + 1,
            assignments,
        });
    }

    if total > budget {
        notes.push(format!(
            "{total} domains, {budget} agent(s): work runs in {} phase(s). As an agent finishes its domain, it hops to the next domain whose dependencies are met.",
            phases.len()
        ));
    } else {
        notes.push(format!(
            "{total} domains within a budget of {budget} agent(s): every startable domain can run in parallel."
        ));
    }
    Ok(NextStepsPlan {
        agent_budget: budget,
        total_domains: total,
        phases,
        notes,
    })
}

/// `trelane biplane --describe <file>` entry point. Validates the description,
/// prints the analysis (or JSON), and optionally writes the derived plan.
pub fn cmd_describe(
    root: &Path,
    desc_path: &Path,
    next_steps: bool,
    emit_plan: bool,
    agent_budget: Option<usize>,
    json: bool,
) -> Result<()> {
    let desc = load_project_description(desc_path)?;
    let budget = agent_budget
        .or(desc.max_agents)
        .unwrap_or(desc.domains.len())
        .max(1);

    let order = topo_order_domains(&desc)?;
    let plan = plan_from_description(&desc, budget)?;
    let steps = if next_steps {
        Some(next_steps_plan(&desc, budget)?)
    } else {
        None
    };

    if emit_plan {
        let out = root.join(".trelane").join("biplane-plan.json");
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&out, serde_json::to_string_pretty(&plan)?)?;
        if !json {
            println!("  Derived plan written to {}", out.display());
        }
    }

    if json {
        let mut obj = serde_json::json!({
            "description": desc,
            "dependency_order": order,
            "derived_plan": plan,
        });
        if let Some(steps) = &steps {
            obj["next_steps"] = serde_json::to_value(steps)?;
        }
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        print_description_analysis(&desc, &order, &plan, steps.as_ref(), budget);
    }
    Ok(())
}

fn print_description_analysis(
    desc: &ProjectDescription,
    order: &[String],
    plan: &BiplanePlan,
    steps: Option<&NextStepsPlan>,
    budget: usize,
) {
    println!();
    crate::logo::print_logo();
    println!("  Biplane Project Description");
    println!("  ==========================");
    println!("  Project : {}", desc.name);
    if !desc.description.is_empty() {
        println!("  Summary : {}", desc.description);
    }
    println!("  Domains : {}", desc.domains.len());
    println!("  Budget  : {budget} agent(s)");
    println!();

    println!("  Domains (dependency order):");
    for name in order {
        if let Some(d) = desc.domains.iter().find(|d| &d.name == name) {
            let deps = if d.depends_on.is_empty() {
                "none".to_string()
            } else {
                d.depends_on.join(", ")
            };
            println!("    {:<16} agents={} depends_on={}", d.name, d.agents, deps);
            if !d.description.is_empty() {
                println!("      {}", d.description);
            }
            println!("      writable : {}", d.writable.join(", "));
            if !d.planned_work.is_empty() {
                println!("      work ({}):", d.planned_work.len());
                for w in &d.planned_work {
                    println!("        - [{}] {}", w.priority, w.subject);
                }
            }
        }
    }
    println!();

    println!("  Derived plan ({} agent(s)):", plan.agents.len());
    for a in &plan.agents {
        println!("    {:<16} {}", a.name, a.description);
    }
    if plan.agents.len() < desc.domains.len() {
        println!(
            "    (note: {} domain(s) exceed the agent budget and were left out of the",
            desc.domains.len() - plan.agents.len()
        );
        println!("     derived plan; see the next-steps schedule for how to phase them.)");
    }
    println!();

    if let Some(steps) = steps {
        println!("  Next steps ({} phase(s)):", steps.phases.len());
        for phase in &steps.phases {
            let summary: Vec<String> = phase
                .assignments
                .iter()
                .map(|a| format!("{} x{}", a.domain, a.agents))
                .collect();
            println!("    Phase {}: {}", phase.phase, summary.join(", "));
            for a in &phase.assignments {
                if let Some(task) = &a.first_task {
                    println!("      {} -> start: {}", a.domain, task);
                }
            }
        }
        for note in &steps.notes {
            println!("    - {note}");
        }
        println!();
    }
}

// ----------------------------- interactive biplane -----------------------------

/// One user decision about a proposed domain in the interactive flow.
#[derive(Debug, Clone)]
pub struct DomainSelection {
    pub name: String,
    pub include: bool,
    pub agents: usize,
}

/// Apply a set of include/agent-count decisions to a base description,
/// producing a refined, validated description. Dependency edges pointing at
/// excluded domains are pruned so the result stays valid. Domains with no
/// explicit selection default to kept. This is the pure core of the
/// interactive flow -- the stdin loop just gathers `DomainSelection`s and calls
/// this.
pub fn apply_domain_selection(
    base: &ProjectDescription,
    selections: &[DomainSelection],
) -> Result<ProjectDescription> {
    use std::collections::HashMap;
    let sel: HashMap<&str, &DomainSelection> =
        selections.iter().map(|s| (s.name.as_str(), s)).collect();

    let kept = |name: &str| -> bool {
        match sel.get(name) {
            Some(s) => s.include,
            None => true,
        }
    };

    let mut domains = Vec::new();
    for d in &base.domains {
        if !kept(&d.name) {
            continue;
        }
        let mut nd = d.clone();
        if let Some(s) = sel.get(d.name.as_str()) {
            nd.agents = s.agents.max(1);
        }
        // Drop dependency edges to domains that were excluded, so validation
        // does not fail on a now-missing dependency target.
        nd.depends_on.retain(|dep| kept(dep));
        domains.push(nd);
    }

    let refined = ProjectDescription {
        name: base.name.clone(),
        description: base.description.clone(),
        domains,
        max_agents: base.max_agents,
        default_model: base.default_model.clone(),
    };
    validate_description(&refined)?;
    Ok(refined)
}

/// Propose a starter description by inspecting the project's source layout:
/// one domain per immediate subdirectory of a recognized source root. Purely
/// deterministic -- no model call -- so the interactive flow can suggest a
/// sensible split even for a project that has never been analyzed.
pub fn scaffold_description_from_structure(root: &Path) -> ProjectDescription {
    let project_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());

    let mut domains: Vec<DomainSpec> = Vec::new();
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for src_root in ["src", "lib", "app", "packages", "crates"] {
        let dir = root.join(src_root);
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut subdirs: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| {
                    !n.starts_with('.')
                        && !matches!(
                            n.as_str(),
                            "target" | "node_modules" | "__pycache__" | "dist" | "build"
                        )
                })
                .collect();
            subdirs.sort();
            for sub in subdirs {
                let mut name = sub.clone();
                let mut n = 2;
                while used_names.contains(&name) {
                    name = format!("{sub}-{n}");
                    n += 1;
                }
                used_names.insert(name.clone());
                domains.push(DomainSpec {
                    name,
                    description: format!("Owns {src_root}/{sub}"),
                    writable: vec![format!("{src_root}/{sub}/**")],
                    forbidden_write: vec![],
                    depends_on: vec![],
                    planned_work: vec![],
                    agents: 1,
                    model: None,
                });
            }
        }
    }

    if domains.is_empty() {
        // Nothing recognizable -- propose a single catch-all domain.
        let writable = if root.join("src").is_dir() {
            vec!["src/**".to_string()]
        } else {
            vec!["**".to_string()]
        };
        domains.push(DomainSpec {
            name: "core".to_string(),
            description: "Owns the whole project".to_string(),
            writable,
            forbidden_write: vec![],
            depends_on: vec![],
            planned_work: vec![],
            agents: 1,
            model: None,
        });
    }

    ProjectDescription {
        name: project_name,
        description: "Scaffolded from the project's source layout.".to_string(),
        domains,
        max_agents: None,
        default_model: None,
    }
}

fn normalize_urgency(priority: &str) -> String {
    match priority {
        "low" | "normal" | "high" | "critical" => priority.to_string(),
        _ => "normal".to_string(),
    }
}

/// Register the plan's agents in a live session and queue their planned work as
/// initial questions from `user`. Existing agents are left untouched. Returns
/// the number of agents newly registered.
fn apply_plan_to_session(
    ctx: &crate::Context,
    desc: &ProjectDescription,
    plan: &BiplanePlan,
) -> Result<usize> {
    let existing = crate::store::list_agents(&ctx.conn)?;
    let spec: std::collections::HashMap<&str, &DomainSpec> =
        desc.domains.iter().map(|d| (d.name.as_str(), d)).collect();

    let mut added = 0;
    let mut resynced = 0;
    for a in &plan.agents {
        let fw = spec
            .get(a.name.as_str())
            .map(|d| d.forbidden_write.clone())
            .unwrap_or_default();
        // Each domain's own model (set per-domain in the Biplane UI's MODEL
        // column) takes precedence; the project-level default_model is only a
        // fallback for domains that didn't specify one.
        let model = spec
            .get(a.name.as_str())
            .and_then(|d| d.model.as_deref())
            .or(desc.default_model.as_deref());

        if existing.contains(&a.name) {
            // Re-sync an already-registered agent to the current description
            // instead of silently skipping it forever. Without this, an agent
            // created before its domain had a model assigned (or before the
            // user changed models) would keep that stale config -- including
            // no model at all -- no matter how many times the plan was
            // re-applied, which is exactly what left some agents stuck on the
            // (now safety-refused) default launcher while sibling agents
            // created in a later apply picked up the right one.
            let mut forbidden = vec![
                format!("{}/**", crate::models::TRELANE_DIR),
                ".git/**".to_string(),
            ];
            forbidden.extend(fw.iter().cloned());
            crate::store::upsert_agent(
                &ctx.conn,
                &a.name,
                &a.description,
                &a.writable,
                model,
                &forbidden,
                &crate::crypto::now_iso(),
            )?;
            resynced += 1;
        } else {
            crate::commands::cmd_add_agent(
                ctx,
                &a.name,
                &a.writable,
                &fw,
                Some(&a.description),
                model,
            )?;
            added += 1;
        }
    }
    if resynced > 0 {
        println!(
            "re-synced {resynced} existing agent(s) to the current description (writable/model/description)"
        );
    }

    // C1: record each planned item as a first-class task in the ledger, so the
    // durable work record no longer lives only in the initial message. The
    // message is still sent as the agent's notification, but now carries the
    // task id. Re-applying a plan must not duplicate tasks: if the owner
    // already has a task with the same subject, reuse it instead of creating a
    // new one (mirrors the agent re-sync above).
    let now = crate::crypto::now_iso();
    for t in &plan.initial_tasks {
        let urgency = spec
            .get(t.agent.as_str())
            .and_then(|d| d.planned_work.iter().find(|w| w.subject == t.subject))
            .map(|w| normalize_urgency(&w.priority))
            .unwrap_or_else(|| "normal".to_string());

        let existing = crate::store::list_tasks_for_owner(&ctx.conn, &t.agent)?;
        let task_id = if let Some(found) = existing.iter().find(|et| et.subject == t.subject) {
            found.id.clone()
        } else {
            let path_scope = spec
                .get(t.agent.as_str())
                .map(|d| d.writable.clone())
                .unwrap_or_default();
            let task = crate::models::Task {
                id: crate::crypto::new_id("task"),
                owner_agent: t.agent.clone(),
                domain: t.agent.clone(),
                parent_task: None,
                subject: t.subject.clone(),
                body: t.body.clone(),
                state: crate::models::TaskState::Ready,
                priority: urgency.clone(),
                assist_policy: crate::models::AssistPolicy::Open,
                desired_parallelism: 1,
                path_scope,
                acceptance: vec![],
                blocked_by: vec![],
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            crate::store::insert_task(&ctx.conn, &task)?;
            task.id
        };

        crate::commands::cmd_send(
            ctx,
            "user",
            &t.agent,
            "question",
            &urgency,
            &t.subject,
            &t.body,
            &None,
            &Some(task_id),
            &[],
        )?;
    }
    Ok(added)
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let ans = prompt_line(prompt)?.to_lowercase();
    if ans.is_empty() {
        return Ok(default_yes);
    }
    Ok(ans == "y" || ans == "yes")
}

/// `trelane biplane --interactive` entry point. Seeds from a `--describe` file
/// if given, otherwise scaffolds from the source layout; lets the user pick
/// domains and agent counts; shows the derived phased plan; writes it to
/// `.trelane/`; and optionally applies it to a live session.
#[allow(clippy::too_many_arguments)]
pub fn cmd_biplane_interactive(
    root: &Path,
    describe_path: Option<&Path>,
    budget_opt: Option<usize>,
    accept_defaults: bool,
    json: bool,
) -> Result<()> {
    let base = match describe_path {
        Some(p) => load_project_description(p)?,
        None => scaffold_description_from_structure(root),
    };
    validate_description(&base)?;

    let default_budget = budget_opt
        .or(base.max_agents)
        .unwrap_or_else(|| base.domains.len().clamp(1, 4));

    if !accept_defaults && !json {
        println!();
        crate::logo::print_logo();
        println!("  Interactive Biplane");
        println!("  ===================");
        println!("  Project : {}", base.name);
        println!(
            "  Source  : {}",
            if describe_path.is_some() {
                "project-description file"
            } else {
                "scaffolded from source layout"
            }
        );
        println!("  Proposed domains: {}", base.domains.len());
        println!();
    }

    let order = topo_order_domains(&base)?;

    let budget = if accept_defaults {
        default_budget
    } else {
        let ans = prompt_line(&format!("  Agent budget [{default_budget}]: "))?;
        if ans.is_empty() {
            default_budget
        } else {
            ans.parse().unwrap_or(default_budget).max(1)
        }
    };

    let mut selections = Vec::new();
    for name in &order {
        let d = base.domains.iter().find(|d| &d.name == name).unwrap();
        if accept_defaults {
            selections.push(DomainSelection {
                name: d.name.clone(),
                include: true,
                agents: d.agents,
            });
            continue;
        }
        let include = prompt_yes_no(
            &format!(
                "  Include domain '{}' (writable: {})? [Y/n] ",
                d.name,
                d.writable.join(", ")
            ),
            true,
        )?;
        let agents = if include {
            let ans = prompt_line(&format!("    agents for '{}' [{}]: ", d.name, d.agents))?;
            if ans.is_empty() {
                d.agents
            } else {
                ans.parse().unwrap_or(d.agents).max(1)
            }
        } else {
            d.agents
        };
        selections.push(DomainSelection {
            name: d.name.clone(),
            include,
            agents,
        });
    }

    let refined = apply_domain_selection(&base, &selections)?;
    if refined.domains.is_empty() {
        return Err(TrelaneError::msg(
            "interactive biplane: no domains selected",
        ));
    }

    let order2 = topo_order_domains(&refined)?;
    let plan = plan_from_description(&refined, budget)?;
    let steps = next_steps_plan(&refined, budget)?;

    let dir = root.join(".trelane");
    fs::create_dir_all(&dir)?;
    let desc_out = dir.join("biplane-description.json");
    let plan_out = dir.join("biplane-plan.json");
    fs::write(&desc_out, serde_json::to_string_pretty(&refined)?)?;
    fs::write(&plan_out, serde_json::to_string_pretty(&plan)?)?;

    let db_exists = dir.join("trelane.db").exists();
    // JSON mode is analysis-only: applying would print agent/message progress
    // to stdout and corrupt the JSON document. Consumers get the plan file path
    // and can apply with a non-JSON invocation.
    let want_apply = if json || !db_exists {
        false
    } else if accept_defaults {
        true
    } else {
        prompt_yes_no(
            "  Apply now: register agents and queue their initial tasks? [y/N] ",
            false,
        )?
    };

    let mut applied = 0usize;
    if want_apply {
        let ctx = crate::Context::open(Some(root))?;
        applied = apply_plan_to_session(&ctx, &refined, &plan)?;
    }

    if json {
        let obj = serde_json::json!({
            "description": refined,
            "dependency_order": order2,
            "derived_plan": plan,
            "next_steps": steps,
            "applied_agents": applied,
            "plan_file": plan_out.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        print_description_analysis(&refined, &order2, &plan, Some(&steps), budget);
        println!("  Plan written to {}", plan_out.display());
        if applied > 0 {
            println!("  Registered {applied} agent(s) and queued their initial task(s).");
            println!("  Start the swarm with:  trelane squire --watch");
        } else if db_exists {
            println!("  Not applied. Re-run and confirm apply, or launch with the written plan.");
        } else {
            println!("  No trelane session here yet. Run 'trelane init', then re-run to apply.");
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrated_ctx(temp: &tempfile::TempDir) -> crate::Context {
        let root = temp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        crate::Context {
            root,
            conn,
            config: crate::models::Config::default(),
        }
    }

    fn plan_domain(name: &str, model: Option<&str>) -> DomainSpec {
        DomainSpec {
            name: name.to_string(),
            description: format!("owns {name}"),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: vec![],
            depends_on: vec![],
            planned_work: vec![],
            agents: 1,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn apply_plan_uses_each_domains_own_model_not_project_default() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        let desc = ProjectDescription {
            name: "demo".into(),
            description: String::new(),
            domains: vec![
                plan_domain("frontend", Some("openrouter/z-ai/glm-5.2")),
                plan_domain("backend", Some("anthropic/claude-sonnet-4")),
            ],
            max_agents: Some(2),
            // A project-level default that must NOT override either domain's
            // own explicit choice -- this is exactly the bug: previously this
            // single value was applied to every agent, discarding the above.
            default_model: Some("should-not-be-used".to_string()),
        };
        let plan = plan_from_description(&desc, 2).unwrap();
        let added = apply_plan_to_session(&ctx, &desc, &plan).unwrap();
        assert_eq!(added, 2);

        let frontend = crate::store::get_domain(&ctx.conn, "frontend")
            .unwrap()
            .unwrap();
        let backend = crate::store::get_domain(&ctx.conn, "backend")
            .unwrap()
            .unwrap();
        assert_eq!(
            frontend.launcher_agent.as_deref(),
            Some("openrouter/z-ai/glm-5.2")
        );
        assert_eq!(
            backend.launcher_agent.as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
    }

    #[test]
    fn apply_plan_falls_back_to_project_default_when_domain_has_no_model() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        let desc = ProjectDescription {
            name: "demo".into(),
            description: String::new(),
            domains: vec![plan_domain("solo", None)],
            max_agents: Some(1),
            default_model: Some("fallback-model".to_string()),
        };
        let plan = plan_from_description(&desc, 1).unwrap();
        apply_plan_to_session(&ctx, &desc, &plan).unwrap();
        let solo = crate::store::get_domain(&ctx.conn, "solo")
            .unwrap()
            .unwrap();
        assert_eq!(solo.launcher_agent.as_deref(), Some("fallback-model"));
    }

    #[test]
    fn apply_plan_records_planned_work_as_ledger_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);
        let mut backend = plan_domain("backend", Some("opencode/x"));
        backend.planned_work = vec![
            PlannedWork {
                subject: "wire up the API".to_string(),
                body: "add the /v1 routes".to_string(),
                priority: "high".to_string(),
            },
            PlannedWork {
                subject: "add integration tests".to_string(),
                body: String::new(),
                priority: "normal".to_string(),
            },
        ];
        let desc = ProjectDescription {
            name: "demo".into(),
            description: String::new(),
            domains: vec![backend],
            max_agents: Some(1),
            default_model: None,
        };
        let plan = plan_from_description(&desc, 1).unwrap();
        apply_plan_to_session(&ctx, &desc, &plan).unwrap();

        let tasks = crate::store::list_tasks_for_owner(&ctx.conn, "backend").unwrap();
        assert_eq!(tasks.len(), 2, "each planned item becomes a ledger task");
        let api = tasks
            .iter()
            .find(|t| t.subject == "wire up the API")
            .expect("api task present");
        assert_eq!(api.state, crate::models::TaskState::Ready);
        assert_eq!(api.priority, "high");
        assert_eq!(api.owner_agent, "backend");
        assert_eq!(api.path_scope, vec!["src/backend/**".to_string()]);

        // Re-applying the same plan must not duplicate tasks.
        apply_plan_to_session(&ctx, &desc, &plan).unwrap();
        let tasks2 = crate::store::list_tasks_for_owner(&ctx.conn, "backend").unwrap();
        assert_eq!(
            tasks2.len(),
            2,
            "re-apply reuses existing tasks by subject, no duplicates"
        );
    }

    #[test]
    fn reapplying_an_updated_plan_resyncs_an_already_registered_agent() {
        // Exact real-world scenario: a domain gets applied once before the
        // user ever picks a model (agent registers with no launcher), then
        // the user assigns a model in the Biplane UI and re-applies. The
        // already-existing agent must pick up the new model -- previously it
        // was silently skipped forever and stayed stuck with no launcher no
        // matter how many times the plan was re-applied.
        let temp = tempfile::tempdir().unwrap();
        let ctx = migrated_ctx(&temp);

        let desc_v1 = ProjectDescription {
            name: "demo".into(),
            description: String::new(),
            domains: vec![plan_domain("backend", None)],
            max_agents: Some(1),
            default_model: None,
        };
        let plan_v1 = plan_from_description(&desc_v1, 1).unwrap();
        apply_plan_to_session(&ctx, &desc_v1, &plan_v1).unwrap();
        let before = crate::store::get_domain(&ctx.conn, "backend")
            .unwrap()
            .unwrap();
        assert_eq!(before.launcher_agent, None);

        let desc_v2 = ProjectDescription {
            name: "demo".into(),
            description: String::new(),
            domains: vec![plan_domain(
                "backend",
                Some("opencode/nemotron-3-ultra-free"),
            )],
            max_agents: Some(1),
            default_model: None,
        };
        let plan_v2 = plan_from_description(&desc_v2, 1).unwrap();
        let added = apply_plan_to_session(&ctx, &desc_v2, &plan_v2).unwrap();
        assert_eq!(added, 0, "the agent already existed; nothing new was added");

        let after = crate::store::get_domain(&ctx.conn, "backend")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.launcher_agent.as_deref(),
            Some("opencode/nemotron-3-ultra-free"),
            "re-applying must update the existing agent's model, not leave it stuck"
        );
    }

    #[test]
    fn planning_prompt_requests_a_multi_agent_split() {
        let prompt = compose_biplane_planning_prompt(
            "src/\n  main.rs",
            "(no safe_pocket feature files found)",
            8,
            std::path::Path::new("/tmp/proj"),
        );
        // No contradictory "up to 1" / "2-1" guidance; asks for a real range.
        assert!(prompt.contains("2 to 8 agents"));
        assert!(!prompt.contains("up to 1 agents"));
        assert!(!prompt.contains("2-1"));
        // The greenfield-split nudge is present.
        assert!(prompt.contains("greenfield"));
    }

    #[test]
    fn sources_prompt_scans_only_given_folders() {
        // A target project folder with a doc describing a juice shop, and an
        // unrelated folder that must NOT be pulled in unless listed.
        let base = std::env::temp_dir().join(format!(
            "trelane-src-test-{}-{}",
            std::process::id(),
            "scan"
        ));
        let proj = base.join("juice");
        let extra = base.join("extra_features");
        let unrelated = base.join("unrelated");
        for d in [&proj, &extra, &unrelated] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            proj.join("README.md"),
            "# Juice Shop\nA shop that sells juice.",
        )
        .unwrap();
        std::fs::write(extra.join("spec.md"), "Backend API for payment codes.").unwrap();
        std::fs::write(
            unrelated.join("trelane.md"),
            "Trelane coordination protocol internals.",
        )
        .unwrap();

        // Only proj (root) + extra are sources; unrelated is omitted.
        let prompt = compose_sources_prompt(&proj, &[extra.clone()], 8);

        assert!(prompt.contains("Juice Shop"));
        assert!(prompt.contains("payment codes"));
        // The unrelated Trelane doc must not leak in.
        assert!(!prompt.contains("coordination protocol internals"));

        std::fs::remove_dir_all(&base).ok();
    }

    fn domain(name: &str, deps: &[&str], agents: usize) -> DomainSpec {
        DomainSpec {
            name: name.to_string(),
            description: format!("owns {name}"),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: vec![],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            planned_work: vec![PlannedWork {
                subject: format!("build {name}"),
                body: String::new(),
                priority: "normal".to_string(),
            }],
            agents,
            model: None,
        }
    }

    fn desc(domains: Vec<DomainSpec>, max_agents: Option<usize>) -> ProjectDescription {
        ProjectDescription {
            name: "test-project".to_string(),
            description: "a test".to_string(),
            domains,
            max_agents,
            default_model: None,
        }
    }

    #[test]
    fn validate_rejects_dependency_cycle() {
        let d = desc(vec![domain("a", &["b"], 1), domain("b", &["a"], 1)], None);
        let err = validate_description(&d).unwrap_err();
        assert!(format!("{err:?}").contains("cycle"));
    }

    #[test]
    fn validate_rejects_unknown_dependency() {
        let d = desc(vec![domain("a", &["ghost"], 1)], None);
        let err = validate_description(&d).unwrap_err();
        assert!(format!("{err:?}").contains("unknown domain"));
    }

    #[test]
    fn topo_order_puts_dependencies_first() {
        // c depends on b depends on a  =>  a, b, c
        let d = desc(
            vec![
                domain("c", &["b"], 1),
                domain("b", &["a"], 1),
                domain("a", &[], 1),
            ],
            None,
        );
        let order = topo_order_domains(&d).unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn plan_from_description_respects_cap_and_order() {
        let d = desc(
            vec![
                domain("c", &["b"], 1),
                domain("b", &["a"], 1),
                domain("a", &[], 1),
            ],
            None,
        );
        let plan = plan_from_description(&d, 2).unwrap();
        // Cap of 2 keeps the two earliest in dependency order: a, b.
        let names: Vec<&str> = plan.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        // Tasks for the dropped domain 'c' must not survive.
        assert!(plan.initial_tasks.iter().all(|t| t.agent != "c"));
    }

    #[test]
    fn next_steps_phases_when_domains_exceed_agents() {
        // Four independent domains, budget of 2  =>  two phases of two.
        let d = desc(
            vec![
                domain("a", &[], 1),
                domain("b", &[], 1),
                domain("c", &[], 1),
                domain("d", &[], 1),
            ],
            None,
        );
        let steps = next_steps_plan(&d, 2).unwrap();
        assert_eq!(steps.phases.len(), 2);
        assert_eq!(steps.phases[0].assignments.len(), 2);
        assert_eq!(steps.phases[1].assignments.len(), 2);
    }

    #[test]
    fn next_steps_honours_requested_agent_count() {
        // 'heavy' wants 2 agents; with budget 3 it and one more run in phase 1.
        let d = desc(vec![domain("heavy", &[], 2), domain("light", &[], 1)], None);
        let steps = next_steps_plan(&d, 3).unwrap();
        assert_eq!(steps.phases.len(), 1);
        let heavy = steps.phases[0]
            .assignments
            .iter()
            .find(|a| a.domain == "heavy")
            .unwrap();
        assert_eq!(heavy.agents, 2);
    }

    #[test]
    fn apply_domain_selection_excludes_and_prunes_dependencies() {
        // a  <- b  <- c   ; exclude b  =>  c's depends_on [b] must be pruned.
        let base = desc(
            vec![
                domain("a", &[], 1),
                domain("b", &["a"], 1),
                domain("c", &["b"], 1),
            ],
            None,
        );
        let selections = vec![
            DomainSelection {
                name: "a".into(),
                include: true,
                agents: 1,
            },
            DomainSelection {
                name: "b".into(),
                include: false,
                agents: 1,
            },
            DomainSelection {
                name: "c".into(),
                include: true,
                agents: 1,
            },
        ];
        let refined = apply_domain_selection(&base, &selections).unwrap();
        let names: Vec<&str> = refined.domains.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
        let c = refined.domains.iter().find(|d| d.name == "c").unwrap();
        assert!(
            c.depends_on.is_empty(),
            "dangling dep on excluded 'b' must be pruned"
        );
    }

    #[test]
    fn apply_domain_selection_sets_agent_counts() {
        let base = desc(vec![domain("a", &[], 1)], None);
        let selections = vec![DomainSelection {
            name: "a".into(),
            include: true,
            agents: 3,
        }];
        let refined = apply_domain_selection(&base, &selections).unwrap();
        assert_eq!(refined.domains[0].agents, 3);
    }

    #[test]
    fn scaffold_proposes_one_domain_per_source_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src").join("ui")).unwrap();
        std::fs::create_dir_all(root.join("src").join("api")).unwrap();
        std::fs::create_dir_all(root.join("src").join("data")).unwrap();

        let scaffolded = scaffold_description_from_structure(root);
        let mut names: Vec<&str> = scaffolded.domains.iter().map(|d| d.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["api", "data", "ui"]);
        // Every scaffolded domain must be independently valid.
        validate_description(&scaffolded).unwrap();
    }

    #[test]
    fn scaffold_falls_back_to_core_when_no_source_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let scaffolded = scaffold_description_from_structure(tmp.path());
        assert_eq!(scaffolded.domains.len(), 1);
        assert_eq!(scaffolded.domains[0].name, "core");
    }

    #[test]
    fn extract_model_arg_reads_full_model_id() {
        let profile =
            "opencode {root} --model openrouter/z-ai/glm-5.2 --prompt \"$(cat {prompt_file})\"";
        assert_eq!(
            extract_model_arg(profile),
            Some("openrouter/z-ai/glm-5.2".to_string())
        );
    }

    #[test]
    fn extract_model_arg_supports_short_flag() {
        assert_eq!(
            extract_model_arg("opencode run -m github-copilot/gpt-5-mini --dir x"),
            Some("github-copilot/gpt-5-mini".to_string())
        );
    }

    #[test]
    fn extract_model_arg_none_when_absent() {
        assert_eq!(
            extract_model_arg("trelane --root {root} stub {agent}"),
            None
        );
    }

    #[test]
    fn new_agents_since_returns_only_unregistered() {
        let plan = BiplanePlan {
            agents: vec![
                BiplanePlanAgent {
                    name: "alpha".to_string(),
                    description: "a".to_string(),
                    writable: vec!["src/a/**".to_string()],
                },
                BiplanePlanAgent {
                    name: "beta".to_string(),
                    description: "b".to_string(),
                    writable: vec!["src/b/**".to_string()],
                },
                BiplanePlanAgent {
                    name: "gamma".to_string(),
                    description: "g".to_string(),
                    writable: vec!["src/c/**".to_string()],
                },
            ],
            initial_tasks: vec![],
        };
        let existing = vec!["alpha".to_string(), "gamma".to_string()];
        let new = new_agents_since(&existing, &plan);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].name, "beta");
    }

    #[test]
    fn new_agents_since_empty_when_all_registered() {
        let plan = BiplanePlan {
            agents: vec![BiplanePlanAgent {
                name: "alpha".to_string(),
                description: "a".to_string(),
                writable: vec!["src/a/**".to_string()],
            }],
            initial_tasks: vec![],
        };
        let existing = vec!["alpha".to_string()];
        assert!(new_agents_since(&existing, &plan).is_empty());
    }
}

/// Return only the plan agents whose name is NOT in `existing`, preserving
/// the plan's declaration order.  Used by `reanalyze_on_stop` to find
/// domains that have not yet been registered as agents.
pub fn new_agents_since(existing: &[String], plan: &BiplanePlan) -> Vec<BiplanePlanAgent> {
    plan.agents
        .iter()
        .filter(|a| !existing.iter().any(|e| e == &a.name))
        .cloned()
        .collect()
}

/// Called from the squire watch loop when the swarm is fully quiescent and
/// `biplane.reanalyze_on_all_stop` is enabled.  Loads (or scaffolds) a
/// project description, derives a plan, and registers any agents for
/// domains not yet covered -- additive-only, never touching existing
/// agents.
pub fn reanalyze_on_stop(ctx: &crate::Context) -> Result<()> {
    let desc_path = ctx.trelane_dir().join("biplane-description.json");
    let desc = if desc_path.exists() {
        load_project_description(&desc_path)?
    } else {
        scaffold_description_from_structure(&ctx.root)
    };

    // T5: Use the reconciliation engine instead of the old name-matching check.
    let report = reconcile_against_reality(ctx, &desc)?;
    let mut found_work = false;

    // Register agents for emergent domains (additive-only).
    // F3: This action is gated by reanalyze_on_all_stop (opt-in).
    if !report.emergent_domains.is_empty() && ctx.config.biplane.reanalyze_on_all_stop {
        found_work = true;
        eprintln!(
            "{} biplane re-analysis: {} emergent domain(s) discovered",
            crate::crypto::now_iso(),
            report.emergent_domains.len()
        );
        let max_agents = ctx.config.squire.max_concurrent.max(4);
        let plan = plan_from_description(&desc, max_agents)?;
        for domain in &report.emergent_domains {
            crate::commands::cmd_add_agent(
                ctx,
                &domain.name,
                &domain.writable,
                &domain.forbidden_write,
                Some(&domain.description),
                None,
            )?;
            eprintln!(
                "  + registered agent: {} ({})",
                domain.name, domain.description
            );
        }
        // Queue initial work for new agents.
        for task in &plan.initial_tasks {
            if report.emergent_domains.iter().any(|d| d.name == task.agent) {
                crate::commands::cmd_send(
                    ctx,
                    "user",
                    &task.agent,
                    "question",
                    "normal",
                    &task.subject,
                    &task.body,
                    &None,
                    &None,
                    &[],
                )?;
            }
        }
    }

    // Surface stalled domains as an explicit thematic-deadlock notice.
    // F3: Detection/reporting is gated by detect_thematic_deadlock (on by default).
    if !report.stalled_domains.is_empty() && ctx.config.biplane.detect_thematic_deadlock {
        found_work = true;
        eprintln!(
            "{} biplane: THEMATIC DEADLOCK detected -- {} stalled domain(s):",
            crate::crypto::now_iso(),
            report.stalled_domains.len()
        );
        for s in &report.stalled_domains {
            eprintln!("  ! {} -- {}", s.domain, s.evidence);
        }
        eprintln!("  Consider sending new work to these agents or re-evaluating their tasks.");
    }

    // F3: If emergent domains were found but auto-registration is disabled,
    // still report them so the user knows.
    if !report.emergent_domains.is_empty() && !ctx.config.biplane.reanalyze_on_all_stop {
        found_work = true;
        eprintln!(
            "{} biplane: {} emergent domain(s) found (auto-registration disabled):",
            crate::crypto::now_iso(),
            report.emergent_domains.len()
        );
        for d in &report.emergent_domains {
            eprintln!("  ? {} -- {}", d.name, d.description);
        }
        eprintln!(
            "  Enable biplane.reanalyze_on_all_stop in config.json to auto-register agents for these."
        );
    }

    // Log a clean outcome when genuinely nothing is wrong, so silence
    // is never ambiguous.
    if !found_work && report.healthy_domains.is_empty() {
        eprintln!(
            "{} biplane: reconciliation found no domains with agents -- nothing to report",
            crate::crypto::now_iso()
        );
    } else if !found_work {
        eprintln!(
            "{} biplane: all {} domain(s) healthy, no emergent or stalled work",
            crate::crypto::now_iso(),
            report.healthy_domains.len()
        );
    }

    Ok(())
}

// ================================================================ T4: Reconciliation

/// Evidence of real activity (or lack thereof) for a domain.  This is a
/// plain, injectable struct so the reconciliation core is a pure function
/// testable with synthetic data -- no git/filesystem/DB access needed.
#[derive(Debug, Clone, Serialize)]
pub struct DomainActivity {
    pub domain_name: String,
    pub has_recent_activity: bool,
    pub evidence: String,
}

/// The outcome of reconciling a stored project description against reality.
/// All three fields are always present (never collapsed into a single bool)
/// so the caller can distinguish "nothing new AND nothing stalled" (genuinely
/// done) from "nothing new BUT something stalled" (thematic deadlock).
#[derive(Debug, Clone, Serialize)]
pub struct ReconciliationReport {
    /// Domains present in a fresh structural scan but not in the stored
    /// description -- these need agents registered.
    pub emergent_domains: Vec<DomainSpec>,
    /// Domains with a registered agent but no evidence of recent activity.
    pub stalled_domains: Vec<StalledDomain>,
    /// Domains that are both registered AND show recent activity.
    pub healthy_domains: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StalledDomain {
    pub domain: String,
    pub evidence: String,
    /// If the stall is caused by an escalated wait-cycle, this lists the
    /// cycle members.  `None` means the stall is due to inactivity, not
    /// a cycle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by_cycle: Option<Vec<String>>,
}

/// Pure reconciliation core: compare a stored project description against
/// a fresh scaffold, existing agents, and activity evidence to produce a
/// three-way report.
///
/// This function has no I/O -- all inputs are passed in, making it fully
/// unit-testable with synthetic data.
///
/// `escalated_cycles` is an optional list of cycle member lists that have
/// been escalated by the squire (T3). Domains whose agents appear in an
/// escalated cycle are reported as stalled with `blocked_by_cycle` set.
pub fn reconcile_description_with_reality(
    desc: &ProjectDescription,
    fresh_scaffold: &ProjectDescription,
    existing_agents: &[String],
    activity: &[DomainActivity],
    escalated_cycles: &[Vec<String>],
) -> ReconciliationReport {
    let stored_names: std::collections::HashSet<&str> =
        desc.domains.iter().map(|d| d.name.as_str()).collect();

    // Emergent: in fresh scaffold but not in stored description.
    let emergent_domains: Vec<DomainSpec> = fresh_scaffold
        .domains
        .iter()
        .filter(|d| !stored_names.contains(d.name.as_str()))
        .cloned()
        .collect();

    let activity_map: std::collections::HashMap<&str, &DomainActivity> = activity
        .iter()
        .map(|a| (a.domain_name.as_str(), a))
        .collect();

    let mut stalled_domains = Vec::new();
    let mut healthy_domains = Vec::new();

    for d in &desc.domains {
        let has_agent = existing_agents.iter().any(|a| a == &d.name);
        if !has_agent {
            continue;
        }

        // F4: Check if this domain's agent is stuck in an escalated cycle.
        let cycle_match = escalated_cycles
            .iter()
            .find(|c| c.iter().any(|m| m == &d.name))
            .cloned();

        if let Some(cycle) = cycle_match {
            stalled_domains.push(StalledDomain {
                domain: d.name.clone(),
                evidence: format!("blocked by escalated wait-cycle: {}", cycle.join(" -> ")),
                blocked_by_cycle: Some(cycle),
            });
            continue;
        }

        match activity_map.get(d.name.as_str()) {
            Some(act) if act.has_recent_activity => {
                healthy_domains.push(d.name.clone());
            }
            Some(act) => {
                stalled_domains.push(StalledDomain {
                    domain: d.name.clone(),
                    evidence: act.evidence.clone(),
                    blocked_by_cycle: None,
                });
            }
            None => {
                stalled_domains.push(StalledDomain {
                    domain: d.name.clone(),
                    evidence: "no activity evidence found".to_string(),
                    blocked_by_cycle: None,
                });
            }
        }
    }

    ReconciliationReport {
        emergent_domains,
        stalled_domains,
        healthy_domains,
    }
}

/// Gather activity evidence for each domain by checking git history.
pub fn gather_domain_activity(
    root: &Path,
    desc: &ProjectDescription,
    queued_at_iso: &str,
) -> Vec<DomainActivity> {
    let is_git = root.join(".git").is_dir();
    let queued_time = chrono::DateTime::parse_from_rfc3339(queued_at_iso)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok();

    let mut results = Vec::new();
    for domain in &desc.domains {
        if domain.writable.is_empty() {
            results.push(DomainActivity {
                domain_name: domain.name.clone(),
                has_recent_activity: false,
                evidence: "domain has no writable globs".to_string(),
            });
            continue;
        }

        if is_git {
            let since = queued_at_iso;
            let mut found_activity = false;
            let mut evidence_parts = Vec::new();

            for glob in &domain.writable {
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["log", "--oneline", "--since", since, "--", glob])
                    .output();
                if let Ok(out) = output
                    && out.status.success()
                {
                    let count = String::from_utf8_lossy(&out.stdout)
                        .lines()
                        .filter(|l| !l.is_empty())
                        .count();
                    if count > 0 {
                        found_activity = true;
                        evidence_parts.push(format!("{glob}: {count} commit(s)"));
                    }
                }
            }

            results.push(DomainActivity {
                domain_name: domain.name.clone(),
                has_recent_activity: found_activity,
                evidence: if found_activity {
                    evidence_parts.join("; ")
                } else {
                    "no commits since work was queued".to_string()
                },
            });
        } else {
            let mut found = false;
            for glob in &domain.writable {
                if let Some(true) = has_recent_file_mtime(root, glob, queued_time) {
                    found = true;
                    break;
                }
            }
            results.push(DomainActivity {
                domain_name: domain.name.clone(),
                has_recent_activity: found,
                evidence: if found {
                    "files modified since work queued".to_string()
                } else {
                    "no file modifications since work queued".to_string()
                },
            });
        }
    }
    results
}

fn has_recent_file_mtime(
    root: &Path,
    glob: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<bool> {
    let since = since?;
    let base = glob.split('/').next().unwrap_or("");
    let base_path = root.join(base);
    if !base_path.is_dir() {
        return Some(false);
    }
    fn check_dir(dir: &Path, since: chrono::DateTime<chrono::Utc>) -> bool {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(meta) = fs::metadata(&path)
                    && let Ok(mtime) = meta.modified()
                    && let Ok(dt) = mtime.duration_since(std::time::UNIX_EPOCH)
                {
                    let secs = dt.as_secs() as i64;
                    if let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
                        && dt > since
                    {
                        return true;
                    }
                }
                if path.is_dir() && check_dir(&path, since) {
                    return true;
                }
            }
        }
        false
    }
    Some(check_dir(&base_path, since))
}

/// Full reconciliation: re-scaffold from the repo, gather activity, and
/// produce a report.  This is the function the live loop (T5) calls.
pub fn reconcile_against_reality(
    ctx: &crate::Context,
    desc: &ProjectDescription,
) -> Result<ReconciliationReport> {
    let fresh = scaffold_description_from_structure(&ctx.root);
    let existing = crate::store::list_agents(&ctx.conn)?;

    let queued_at = crate::store::list_agents(&ctx.conn)?
        .iter()
        .filter_map(|agent| {
            crate::store::get_unprocessed_messages(&ctx.conn, agent)
                .ok()
                .and_then(|msgs| msgs.into_iter().map(|m| m.created_at).min())
        })
        .min()
        .unwrap_or_else(crate::crypto::now_iso);

    let activity = gather_domain_activity(&ctx.root, desc, &queued_at);

    // F4: Gather escalated cycles from the DB so reconciliation can
    // distinguish "stalled because blocked by cycle" from "stalled because
    // quietly gave up".
    let escalated_cycles = gather_escalated_cycles(&ctx.conn);

    Ok(reconcile_description_with_reality(
        desc,
        &fresh,
        &existing,
        &activity,
        &escalated_cycles,
    ))
}

/// Read escalated cycles from the cycle_break_attempts table.
fn gather_escalated_cycles(conn: &rusqlite::Connection) -> Vec<Vec<String>> {
    let attempts = crate::store::list_cycle_break_attempts(conn).unwrap_or_default();
    attempts
        .into_iter()
        .filter(|a| a.escalated)
        .map(|a| a.cycle_members.split(',').map(|s| s.to_string()).collect())
        .collect()
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    fn domain(name: &str) -> DomainSpec {
        DomainSpec {
            name: name.to_string(),
            description: String::new(),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: vec![],
            depends_on: vec![],
            planned_work: vec![],
            agents: 1,
            model: None,
        }
    }

    fn desc(names: &[&str]) -> ProjectDescription {
        ProjectDescription {
            name: "test".to_string(),
            description: String::new(),
            domains: names.iter().map(|n| domain(n)).collect(),
            max_agents: None,
            default_model: None,
        }
    }

    #[test]
    fn all_active_returns_only_healthy() {
        let d = desc(&["alpha", "beta"]);
        let fresh = desc(&["alpha", "beta"]);
        let existing = vec!["alpha".to_string(), "beta".to_string()];
        let activity = vec![
            DomainActivity {
                domain_name: "alpha".to_string(),
                has_recent_activity: true,
                evidence: "commits".to_string(),
            },
            DomainActivity {
                domain_name: "beta".to_string(),
                has_recent_activity: true,
                evidence: "commits".to_string(),
            },
        ];
        let report = reconcile_description_with_reality(&d, &fresh, &existing, &activity, &[]);
        assert!(report.emergent_domains.is_empty());
        assert!(report.stalled_domains.is_empty());
        assert_eq!(report.healthy_domains.len(), 2);
    }

    #[test]
    fn stalled_domain_surfaces_explicitly() {
        let d = desc(&["alpha", "beta"]);
        let fresh = desc(&["alpha", "beta"]);
        let existing = vec!["alpha".to_string(), "beta".to_string()];
        let activity = vec![
            DomainActivity {
                domain_name: "alpha".to_string(),
                has_recent_activity: true,
                evidence: "active".to_string(),
            },
            DomainActivity {
                domain_name: "beta".to_string(),
                has_recent_activity: false,
                evidence: "no commits since queued".to_string(),
            },
        ];
        let report = reconcile_description_with_reality(&d, &fresh, &existing, &activity, &[]);
        assert!(report.emergent_domains.is_empty());
        assert_eq!(report.stalled_domains.len(), 1);
        assert_eq!(report.stalled_domains[0].domain, "beta");
        assert!(report.stalled_domains[0].blocked_by_cycle.is_none());
        assert_eq!(report.healthy_domains, vec!["alpha".to_string()]);
    }

    #[test]
    fn emergent_domain_detected_from_fresh_scaffold() {
        let d = desc(&["alpha"]);
        let fresh = desc(&["alpha", "gamma"]);
        let existing = vec!["alpha".to_string()];
        let activity = vec![DomainActivity {
            domain_name: "alpha".to_string(),
            has_recent_activity: true,
            evidence: "active".to_string(),
        }];
        let report = reconcile_description_with_reality(&d, &fresh, &existing, &activity, &[]);
        assert_eq!(report.emergent_domains.len(), 1);
        assert_eq!(report.emergent_domains[0].name, "gamma");
        assert!(report.stalled_domains.is_empty());
    }

    #[test]
    fn genuinely_done_when_no_stalled_no_emergent() {
        let d = desc(&["alpha"]);
        let fresh = desc(&["alpha"]);
        let existing = vec!["alpha".to_string()];
        let activity = vec![DomainActivity {
            domain_name: "alpha".to_string(),
            has_recent_activity: true,
            evidence: "active".to_string(),
        }];
        let report = reconcile_description_with_reality(&d, &fresh, &existing, &activity, &[]);
        assert!(report.emergent_domains.is_empty());
        assert!(report.stalled_domains.is_empty());
        assert_eq!(report.healthy_domains, vec!["alpha".to_string()]);
    }

    #[test]
    fn cycle_stalled_domain_has_blocked_by_cycle() {
        let d = desc(&["alpha", "beta"]);
        let fresh = desc(&["alpha", "beta"]);
        let existing = vec!["alpha".to_string(), "beta".to_string()];
        let activity = vec![
            DomainActivity {
                domain_name: "alpha".to_string(),
                has_recent_activity: true,
                evidence: "active".to_string(),
            },
            DomainActivity {
                domain_name: "beta".to_string(),
                has_recent_activity: false,
                evidence: "inactive".to_string(),
            },
        ];
        let escalated = vec![vec!["alpha".to_string(), "beta".to_string()]];
        let report =
            reconcile_description_with_reality(&d, &fresh, &existing, &activity, &escalated);
        // Both alpha and beta should be stalled because they're in the cycle.
        assert_eq!(report.stalled_domains.len(), 2);
        let beta_stall = report
            .stalled_domains
            .iter()
            .find(|s| s.domain == "beta")
            .unwrap();
        assert!(beta_stall.blocked_by_cycle.is_some());
        assert_eq!(
            beta_stall.blocked_by_cycle.as_ref().unwrap(),
            &vec!["alpha".to_string(), "beta".to_string()]
        );
    }
}
