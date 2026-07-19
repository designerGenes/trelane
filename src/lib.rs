pub mod bench;
pub mod bench_ui;
pub mod biplane;
pub mod biplane_ui;
pub mod cli;
pub mod config_fields;
pub mod commands;
pub mod crypto;
pub mod db;
pub mod di;
pub mod diagnostic;
pub mod domain;
pub mod entropy;
pub mod error;
pub mod logo;
pub mod models;
pub mod monitor;
pub mod prompt;
pub mod prop;
pub mod pump;
pub mod refine;
pub mod retention;
pub mod splash;
pub mod squire;
pub mod store;
pub mod telemetry;
pub mod testing;
pub mod text_input;

use crate::cli::{Cli, Command};
use crate::domain::find_root;
use crate::error::{Result, TrelaneError};
use crate::models::{Config, TRELANE_DIR};
use clap::Parser;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Resolve the global config directory, respecting XDG_CONFIG_HOME.
pub fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("trelane");
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config")
        .join("trelane")
}

/// Resolve the global config file path.
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// Ensure the global config exists, creating it with defaults if missing.
pub fn ensure_config() -> Result<PathBuf> {
    let path = config_path();
    if !path.exists() {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let config = Config::default();
        std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
        eprintln!("created global config at {}", path.display());
    } else {
        let text = std::fs::read_to_string(&path)?;
        if !text.contains("\"agents\"") {
            let config: Config = serde_json::from_str(&text)?;
            std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
        }
    }
    Ok(path)
}

/// Load the global config, creating it with defaults if missing.
pub fn load_config() -> Result<Config> {
    let path = ensure_config()?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| TrelaneError::msg(format!("cannot read config at {}: {e}", path.display())))?;
    let config: Config = serde_json::from_str(&text)?;
    // 4A config-inversion guard: a hand-edited config can invert the DI
    // temporal relationships (objection window longer than the request
    // lifetime, etc.). Catch it at load so every downstream path sees a
    // sane config rather than failing cryptically inside di::resolve_pending.
    config.di.validate()?;
    Ok(config)
}

/// Persist a config to the global config file as pretty JSON, creating the
/// parent directory if necessary.
pub fn save_config(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(config)?)?;
    Ok(())
}

pub struct Context {
    pub root: PathBuf,
    pub conn: Connection,
    pub config: Config,
}

impl Context {
    pub fn open(root: Option<&Path>) -> Result<Self> {
        let root = find_root(root)?;
        let db_path = root.join(TRELANE_DIR).join("trelane.db");
        let conn = db::open(&db_path)?;
        let config = load_config()?;
        Ok(Self { root, conn, config })
    }

    pub fn trelane_dir(&self) -> PathBuf {
        self.root.join(TRELANE_DIR)
    }

    pub fn secret(&self) -> Result<Vec<u8>> {
        crypto::load_secret(&self.trelane_dir())
    }
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    handle(cli)
}

/// The consolidated session launcher: bare `trelane` (optionally with
/// `--models`/`--agents` to configure the swarm). Resolves the project root,
/// initializes a trelane session if one doesn't exist, optionally runs Biplane
/// to propose agents, then either launches the tabbed monitor UI (default) or
/// runs headless (`--headless`).
fn run_session_command(cli: Cli) -> Result<()> {
    // `--bench-sandbox X` watches a running `trelane bench run` instead of a
    // normal project: point the monitor at <X>/scenario-run-1 read-only (the
    // bench orchestrator is that session's ticker, so no squire loop here).
    // This replaces the removed `monitor --bench-sandbox` command.
    if let Some(sandbox) = cli.bench_sandbox.as_deref() {
        let session_root = sandbox.join("scenario-run-1");
        if !session_root.join(TRELANE_DIR).is_dir() {
            return Err(TrelaneError::msg(format!(
                "no bench session at {} -- is a `trelane bench run` active with this \
                 --sandbox-root? (expected {}/.trelane)",
                session_root.display(),
                session_root.display()
            )));
        }
        let ctx = Context::open(Some(&session_root))?;
        return monitor::run_monitor(&ctx);
    }

    let root = match cli.project.as_deref().or(cli.root.as_deref()) {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };

    // Auto-init on first run so `trelane` in a fresh directory just works.
    if !root.join(TRELANE_DIR).join("trelane.db").exists() {
        commands::cmd_init(Some(root.clone()))?;
    }

    // Optional Biplane bootstrap: only when asked and no agents exist yet.
    if cli.with_biplane {
        let existing = {
            let ctx = Context::open(Some(&root))?;
            crate::store::list_agents(&ctx.conn)?
        };
        if existing.is_empty() {
            let models: Vec<String> = cli
                .models
                .as_deref()
                .unwrap_or("glm-5.2")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let primary = models.first().cloned().unwrap_or_else(|| "glm-5.2".to_string());
            let max_agents = cli.max_agents.unwrap_or(3) as usize;
            eprintln!("[trelane] running Biplane analysis with {primary}...");
            let plan = biplane::run_biplane_plan(&root, &primary, max_agents)?;
            let ctx = Context::open(Some(&root))?;
            for a in &plan.agents {
                commands::cmd_add_agent(
                    &ctx,
                    &a.name,
                    &a.writable,
                    &[],
                    if a.description.is_empty() {
                        None
                    } else {
                        Some(a.description.as_str())
                    },
                    None,
                )?;
            }
            eprintln!("[trelane] Biplane proposed {} agent(s).", plan.agents.len());
        }
    }

    // Seed agents from an existing Biplane plan. If no agents are registered
    // yet but a biplane-description.json exists (the common case after running
    // `trelane biplane`), register its domains as the session's default agents
    // and queue their planned work. Idempotent: agents that already exist are
    // re-synced, not duplicated, so this is safe to run every launch.
    {
        let ctx = Context::open(Some(&root))?;
        let has_agents = !crate::store::list_agents(&ctx.conn)?.is_empty();
        let desc_path = root.join(TRELANE_DIR).join("biplane-description.json");
        if !has_agents && desc_path.is_file() {
            match biplane::load_project_description(&desc_path) {
                Ok(desc) => match biplane::apply_description_to_session(&ctx, &desc) {
                    Ok(n) => eprintln!(
                        "[trelane] launched {n} default agent(s) from {}",
                        desc_path.display()
                    ),
                    Err(e) => eprintln!(
                        "[trelane] warning: could not apply biplane-description.json: {e}"
                    ),
                },
                Err(e) => eprintln!(
                    "[trelane] warning: could not read biplane-description.json: {e}"
                ),
            }
        }
    }

    let ctx = Context::open(Some(&root))?;

    // Clear stale running-locks from a previous session so liveness checks
    // start clean (the old launcher did this too).
    for agent in crate::store::list_agents(&ctx.conn)? {
        if crate::store::get_running_lock(&ctx.conn, &agent)?.is_some() {
            let _ = crate::store::delete_running_lock(&ctx.conn, &agent);
        }
    }

    if cli.headless {
        // True headless: the squire tick-loop in the foreground, no UI. Runs
        // until interrupted (ctrl-c), matching the old `squire --watch`.
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let interval_s = cli.interval.unwrap_or(ctx.config.squire.interval_s);
        crate::logo::print_logo();
        eprintln!(
            "{} trelane headless -- squire ticking every {interval_s}s (ctrl-c to stop)",
            crypto::now_iso()
        );
        // A never-set stop flag: headless runs until the process is signaled.
        let stop = Arc::new(AtomicBool::new(false));
        monitor::run_squire_loop(&ctx, cli.launcher.as_deref(), interval_s, cli.verbose, &stop);
        Ok(())
    } else {
        // Default: the tabbed monitor UI with the squire behind it.
        monitor::run_session(&ctx, cli.launcher.clone(), cli.verbose)
    }
}

/// Open the Biplane UI. Detection and generation belong to the projectDir
/// (`root` -- the cwd `trelane biplane` runs in); the `-i/--include` folders
/// contribute markdown ONLY. Concretely:
///  1. If a `biplane-description.json` (the editable plan) already exists in
///     `root` and `--regenerate` was not passed, that IS the plan to
///     view/edit: open the UI on it, no gathering. A `biplane-report.json`
///     (live-session snapshot) in `root` is a secondary fallback. The `-i`
///     folders are never checked for either -- a report is a projectDir
///     artifact so it stays portable with the project.
///  2. Otherwise gather markdown recursively from `root` PLUS every `-i`
///     folder, warn if the set is large, ask before submitting to a model,
///     generate a plan from those sources, persist it into `root`'s
///     `.trelane/`, and open the UI.
fn biplane_open_ui(root: &Path, include: &[PathBuf], regenerate: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};

    // Step 1: existing-artifact detection -- projectDir ONLY. The `-i` folders
    // are markdown sources, not places a report can live, so a plan generated
    // here stays portable: it's always found in the folder it was made in.
    if !regenerate {
        if let Some(desc_path) = biplane::find_project_description(root) {
            println!("[biplane] found existing plan: {}", desc_path.display());
            // Canonical location is <root>/.trelane/biplane-description.json.
            // A portable plan dropped directly in <root> is copied into place
            // so the UI (and later a session) load it uniformly.
            let ui_desc = root.join(TRELANE_DIR).join("biplane-description.json");
            if desc_path != ui_desc {
                if let Some(parent) = ui_desc.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&desc_path, &ui_desc)?;
            }
            return biplane_ui::run_with_includes(root, include);
        }
        if let Some(report_path) = biplane::find_project_report(root) {
            println!("[biplane] found existing report: {}", report_path.display());
            let ui_report = root.join(TRELANE_DIR).join("biplane-report.json");
            if report_path != ui_report {
                if let Some(parent) = ui_report.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&report_path, &ui_report)?;
            }
            return biplane_ui::run_with_includes(root, include);
        }
    }

    // Step 2: gather markdown for the size warning + confirmation. Sources are
    // root PLUS the -i folders; this gather is what lets us count/size-check
    // and confirm before the model call (generation re-scans the same dirs).
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    dirs.extend(include.iter().cloned());
    let gather = biplane::gather_markdown_files(&dirs);
    if gather.count() == 0 {
        println!(
            "[biplane] no markdown files found in {} director(ies). Opening the editor \
             with a scaffold from the project structure instead.",
            dirs.len()
        );
        return biplane_ui::run_with_includes(root, include);
    }

    let kb = gather.total_bytes / 1024;
    println!(
        "[biplane] gathered {} markdown file(s) ({} KB) from {} director(ies).",
        gather.count(),
        kb,
        dirs.len()
    );
    if gather.is_large() {
        println!(
            "  WARNING: that's a large amount of markdown ({} files, {} KB). Submitting all \
             of it may be slow, costly, or exceed the model's context window.",
            gather.count(),
            kb
        );
    }

    // Confirm before submitting to a model (skip the prompt when not a TTY --
    // a non-interactive caller can't answer, so default to not submitting).
    if !std::io::stdin().is_terminal() {
        println!(
            "[biplane] non-interactive: not submitting to a model. Re-run in a terminal to \
             confirm, or use --describe for an offline path."
        );
        return biplane_ui::run_with_includes(root, include);
    }
    print!(
        "  Submit these {} markdown file(s) to the model for report generation? [y/N] ",
        gather.count()
    );
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        println!("[biplane] skipped submission. Opening the editor on the current description.");
        return biplane_ui::run_with_includes(root, include);
    }

    // Generate a plan from the gathered sources, convert to an editable
    // description, and persist it where the UI loads from.
    let model = biplane::default_biplane_model();
    let max_agents = 3;
    println!("[biplane] submitting to {model}...");
    let plan = biplane::run_biplane_plan_from_sources(root, include, &model, max_agents)?;
    let project_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let desc = biplane::plan_to_description(&plan, project_name, max_agents);
    let desc_path = root.join(TRELANE_DIR).join("biplane-description.json");
    if let Some(parent) = desc_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&desc_path, serde_json::to_string_pretty(&desc)?)?;
    println!("[biplane] report generated. Opening the editor...");
    biplane_ui::run_with_includes(root, include)
}

#[allow(dead_code)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Compute the live session state and push it to the tmux status bar.
/// Retained for the tmux-session path even though the default launcher no
/// longer uses it.
#[allow(dead_code)]
fn refresh_session_status(ctx: &Context, session: &str) -> Result<()> {
    let agents = store::list_agents(&ctx.conn)?;
    let running = agents
        .iter()
        .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
        .count();
    let (_, cycle) = squire::wait_graph(&ctx.conn)?;
    let state = if let Some(cycle) = cycle {
        let mut display = cycle.clone();
        display.push(cycle[0].clone());
        splash::SessionState::Deadlock {
            cycle: display.join(" -> "),
        }
    } else if running > 0 {
        splash::SessionState::Active { running }
    } else {
        splash::SessionState::Idle
    };
    let project = ctx
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ctx.root.display().to_string());
    splash::set_session_status(session, &project, &state)
}

pub fn handle(cli: Cli) -> Result<()> {
    if let Some(scenario) = cli.testing.as_deref() {
        return testing::run_testing(
            scenario,
            cli.testing_runs.unwrap_or(1),
            cli.testing_report.as_deref(),
            cli.testing_sandbox_root.as_deref(),
            cli.testing_launcher.as_deref(),
        );
    }

    // Bare `trelane` (no subcommand) launches a session: the tabbed monitor UI
    // with the squire tick-loop behind it. `--models`/`--agents` still
    // configure the session; they no longer route to a separate tmux launcher.
    if cli.command.is_none() {
        return run_session_command(cli);
    }

    match cli.command {
        None => unreachable!("handled by run_session_command above"),
        Some(Command::Init { project }) => commands::cmd_init(project.or(cli.project)),
        Some(Command::Attach { project, no_inject }) => commands::cmd_attach_project(
            project.or(cli.project),
            cli.agents.as_deref(),
            cli.no_agents.as_deref(),
            !no_inject,
        ),
        Some(Command::AddAgent {
            name,
            writable,
            forbidden_write,
            desc,
            launcher_agent,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_add_agent(
                &ctx,
                &name,
                &writable,
                &forbidden_write,
                desc.as_deref(),
                launcher_agent.as_deref(),
            )
        }
        Some(Command::Redomain {
            agent,
            writable,
            forbidden_write,
            desc,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_redomain(&ctx, &agent, &writable, &forbidden_write, desc.as_deref())
        }
        Some(Command::Send {
            from,
            to,
            msg_type,
            subject,
            body,
            re,
            task,
            paths,
            urgency,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_send(
                &ctx, &from, &to, &msg_type, &urgency, &subject, &body, &re, &task, &paths,
            )
        }
        Some(Command::Inbox {
            agent,
            json,
            include_archived,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_inbox(&ctx, &agent, json, include_archived)
        }
        Some(Command::Outbox { agent, json }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_outbox(&ctx, &agent, json)
        }
        Some(Command::History {
            agent,
            include_archived,
            json,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_history(&ctx, agent.as_deref(), include_archived, json)
        }
        Some(Command::Bulletin { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_bulletin(&ctx, &action)
        }
        Some(Command::Retention { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_retention(&ctx, &action)
        }
        Some(Command::Di { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_di(&ctx, &action)
        }
        Some(Command::Split { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            refine::cmd_split(&ctx, &action)
        }
        Some(Command::Ack { agent, msg_id }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_ack(&ctx, &agent, &msg_id)
        }
        Some(Command::Claim {
            agent,
            path,
            ttl,
            task,
            grant,
            delegation,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_claim(
                &ctx,
                &agent,
                &path,
                ttl,
                task.as_deref(),
                grant.as_deref(),
                delegation.as_deref(),
            )
        }
        Some(Command::Release { agent, path, force }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_release(&ctx, &agent, &path, force)
        }
        Some(Command::Park {
            agent,
            task,
            wait_reply,
            wait_claim,
            wait_contested_claim,
            waiting_on,
            resume_hint,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_park(
                &ctx,
                &agent,
                task.as_deref(),
                &wait_reply,
                &wait_claim,
                &wait_contested_claim,
                &waiting_on,
                &resume_hint,
            )
        }
        Some(Command::Unpark { task }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_unpark(&ctx, &task)
        }
        Some(Command::Status) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_status(&ctx)
        }
        Some(Command::Wake {
            agent,
            why,
            launcher,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_wake(&ctx, &agent, why.as_deref(), launcher.as_deref())
        }
        Some(Command::SetLaunchTarget {
            agent,
            adapter,
            target,
            command,
            tmux_target,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_set_launch_target(
                &ctx,
                &agent,
                &adapter,
                &target,
                command.as_deref(),
                tmux_target.as_deref(),
            )
        }
        Some(Command::Relaunch {
            agent,
            adapter,
            target,
            command,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_relaunch(
                &ctx,
                &agent,
                adapter.as_deref(),
                target.as_deref(),
                command.as_deref(),
            )
        }
        Some(Command::Done { agent }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_done(&ctx, &agent)
        }
        Some(Command::Audit { agent }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_audit(&ctx, &agent)
        }
        Some(Command::Biplane {
            safe_pocket_dir,
            describe,
            next_steps,
            emit_plan,
            interactive,
            ui,
            accept_defaults,
            json,
            refine,
            refine_model,
            include,
            regenerate,
        }) => {
            if refine {
                // Slice 5: the deliberate, model-calling refinement pass
                // (R19). Never on the squire's wake path.
                let ctx = Context::open(cli.root.as_deref())?;
                let model = refine_model
                    .as_deref()
                    .map(str::to_string)
                    .unwrap_or_else(biplane::default_biplane_model);
                return refine::cmd_refine(&ctx, &model, json);
            }
            // `trelane biplane` always opens the Biplane UI unless an explicit
            // non-UI sub-mode was requested. This makes the bare command a
            // one-step route to the UI, matching how bare `trelane` opens the
            // monitor UI.
            let explicit_non_ui_mode =
                interactive || describe.is_some() || next_steps || emit_plan;
            if ui || !explicit_non_ui_mode {
                let root = match cli.root.as_deref() {
                    Some(p) => p.to_path_buf(),
                    None => std::env::current_dir()?,
                };
                return biplane_open_ui(&root, &include, regenerate);
            } else if interactive {
                // Interactive/describe paths need no DB, so they work even
                // before a project is initialized as a trelane session.
                let root = match cli.root.as_deref() {
                    Some(p) => p.to_path_buf(),
                    None => std::env::current_dir()?,
                };
                biplane::cmd_biplane_interactive(
                    &root,
                    describe.as_deref(),
                    cli.max_agents.map(|m| m as usize),
                    accept_defaults,
                    json,
                )
            } else if let Some(desc_path) = describe {
                let root = match cli.root.as_deref() {
                    Some(p) => p.to_path_buf(),
                    None => std::env::current_dir()?,
                };
                biplane::cmd_describe(
                    &root,
                    &desc_path,
                    next_steps,
                    emit_plan,
                    cli.max_agents.map(|m| m as usize),
                    json,
                )
            } else {
                let ctx = Context::open(cli.root.as_deref())?;
                biplane::cmd_biplane(&ctx, safe_pocket_dir.as_deref(), json)
            }
        }
        Some(Command::Stub { agent }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_stub(&ctx, &agent)
        }
        Some(Command::Metrics { json }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            let trace_dir = telemetry::trace_dir_for(&ctx.trelane_dir());
            let metrics = telemetry::compute_metrics(&trace_dir)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&metrics)?);
            } else {
                print_metrics(&metrics);
            }
            Ok(())
        }
        Some(Command::Rate {
            agent,
            rating,
            rationale,
            rater,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            let tracer =
                telemetry::Tracer::ephemeral(&ctx.trelane_dir(), &ctx.root.display().to_string())?;
            // Find the most recent agent.run span for the rated agent
            let trace_dir = telemetry::trace_dir_for(&ctx.trelane_dir());
            let spans = telemetry::Tracer::read_all_spans(&trace_dir)?;
            let last_run = spans
                .iter()
                .filter(|s| s.name == format!("agent.run:{agent}"))
                .max_by_key(|s| s.start_time_unix_nano);
            // NOTE: run_span_id resolution reconstructed during corruption
            // repair; verify None-handling matches intended UX.
            let run_span_id = match last_run {
                Some(span) => span.span_id.clone(),
                None => {
                    println!("no agent.run span found for {agent}; cannot record rating");
                    return Ok(());
                }
            };
            tracer.record_rating(&rater, &agent, &run_span_id, rating, &rationale)?;
            println!("rating recorded: {rater} rated {agent} = {rating}/10");
            Ok(())
        }
        Some(Command::Diagnostic) => {
            let ctx = Context::open(cli.root.as_deref())?;
            diagnostic::run(&ctx)
        }
        Some(Command::Config { action }) => cmd_config(&action),
        Some(Command::Bench { action }) => match action {
            cli::BenchAction::Run {
                scenario,
                runs,
                max_turns,
                model,
                report,
                sandbox_root,
                free_models_only,
                ui,
            } => bench::run_bench(
                &scenario,
                runs,
                report.as_deref(),
                sandbox_root.as_deref(),
                max_turns,
                model.as_deref(),
                free_models_only,
                ui,
            ),
            cli::BenchAction::Suite {
                dir,
                runs,
                max_turns,
                model,
                sandbox_root,
                free_models_only,
                save_baseline,
                output,
            } => bench::run_suite(
                &dir,
                runs,
                max_turns,
                model.as_deref(),
                sandbox_root.as_deref(),
                free_models_only,
                output.as_deref(),
                save_baseline.as_deref(),
            ),
            cli::BenchAction::Compare {
                baseline,
                candidate,
                threshold_ms,
                json,
            } => {
                let regressed = bench::compare_reports(&baseline, &candidate, threshold_ms, json)?;
                if regressed {
                    std::process::exit(1);
                }
                Ok(())
            }
        },
        Some(Command::Help { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_help(&ctx, &action)
        }
        Some(Command::Work { action }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_work(&ctx, &action)
        }
        Some(Command::Kill) => cmd_kill(),
    }
}

// ---------------------------------------------------------------- config cmd

/// Comma-separated list of config keys the `config` command understands, for
/// use in "unknown key" errors. Keep in sync with the match arms below.
const KNOWN_CONFIG_KEYS: &str = "squire.max_concurrent, squire.interval_s, squire.reply_timeout_s, \
     squire.breaker_escalation_count, squire.starvation_ticks, \
     di.objection_window_s, di.request_timeout_s, di.claim_contested_timeout_s, \
     retention.hot_days, retention.dormant_days, retention.purge_days, \
     claims.default_ttl_s, workspace.mode, \
     bench.default_max_turns, bench.default_model, bench.free_models, bench.slice_timeout_s";

fn unknown_config_key(key: &str) -> TrelaneError {
    TrelaneError::msg(format!(
        "unknown config key '{key}'. Known keys: {KNOWN_CONFIG_KEYS}"
    ))
}

/// Read a config value by dotted key, formatted for display.
fn config_get(config: &Config, key: &str) -> Result<String> {
    Ok(match key {
        "squire.max_concurrent" => config.squire.max_concurrent.to_string(),
        "squire.interval_s" => config.squire.interval_s.to_string(),
        "squire.reply_timeout_s" => config
            .squire
            .reply_timeout_s
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        "squire.breaker_escalation_count" => config.squire.breaker_escalation_count.to_string(),
        "squire.starvation_ticks" => config.squire.starvation_ticks.to_string(),
        "di.objection_window_s" => config.di.objection_window_s.to_string(),
        "di.request_timeout_s" => config.di.request_timeout_s.to_string(),
        "di.claim_contested_timeout_s" => config.di.claim_contested_timeout_s.to_string(),
        "retention.hot_days" => config.retention.hot_days.to_string(),
        "retention.dormant_days" => config.retention.dormant_days.to_string(),
        "retention.purge_days" => config
            .retention
            .purge_days
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        "claims.default_ttl_s" => config.claims.default_ttl_s.to_string(),
        "workspace.mode" => config.workspace.mode.as_str().to_string(),
        "bench.default_max_turns" => config.bench.default_max_turns.to_string(),
        "bench.default_model" => config
            .bench
            .default_model
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        "bench.free_models" => {
            serde_json::to_string(&config.bench.free_models).unwrap_or_else(|_| "[]".to_string())
        }
        "bench.slice_timeout_s" => config.bench.slice_timeout_s.to_string(),
        _ => return Err(unknown_config_key(key)),
    })
}

/// Parse and apply a config value by dotted key. Validates per-field.
fn config_set(config: &mut Config, key: &str, value: &str) -> Result<()> {
    let parse_u64 = |v: &str| -> Result<u64> {
        v.parse::<u64>()
            .map_err(|_| TrelaneError::msg(format!("'{v}' is not a valid integer for {key}")))
    };
    match key {
        "squire.max_concurrent" => {
            let n: usize = value.parse().map_err(|_| {
                TrelaneError::msg(format!(
                    "'{value}' is not a valid non-negative integer for {key}"
                ))
            })?;
            if n == 0 {
                return Err(TrelaneError::msg(
                    "squire.max_concurrent must be at least 1 (0 would run no agents)".to_string(),
                ));
            }
            config.squire.max_concurrent = n;
        }
        "squire.interval_s" => config.squire.interval_s = parse_u64(value)?,
        "squire.reply_timeout_s" => {
            config.squire.reply_timeout_s = match value {
                "none" | "off" | "" => None,
                v => Some(parse_u64(v)?),
            };
        }
        "squire.breaker_escalation_count" => {
            config.squire.breaker_escalation_count = parse_u64(value)? as i64
        }
        "squire.starvation_ticks" => config.squire.starvation_ticks = parse_u64(value)? as i64,
        "di.objection_window_s" => config.di.objection_window_s = parse_u64(value)?,
        "di.request_timeout_s" => config.di.request_timeout_s = parse_u64(value)?,
        "di.claim_contested_timeout_s" => config.di.claim_contested_timeout_s = parse_u64(value)?,
        "retention.hot_days" => config.retention.hot_days = parse_u64(value)?,
        "retention.dormant_days" => config.retention.dormant_days = parse_u64(value)?,
        "retention.purge_days" => {
            config.retention.purge_days = match value {
                "none" | "off" | "" => None,
                v => Some(parse_u64(v)?),
            };
        }
        "claims.default_ttl_s" => config.claims.default_ttl_s = parse_u64(value)?,
        "workspace.mode" => {
            config.workspace.mode =
                crate::models::WorkspaceMode::parse(value).ok_or_else(|| {
                    TrelaneError::msg(format!(
                        "'{value}' is not a valid workspace mode (use 'shared' or 'worktree')"
                    ))
                })?;
        }
        "bench.default_max_turns" => {
            config.bench.default_max_turns = value.parse::<u32>().map_err(|_| {
                TrelaneError::msg(format!("'{value}' is not a valid integer for {key}"))
            })?;
        }
        "bench.default_model" => {
            config.bench.default_model = match value {
                "none" | "off" | "" => None,
                v => Some(v.to_string()),
            };
        }
        "bench.free_models" => {
            config.bench.free_models = serde_json::from_str(value)
                .map_err(|_| TrelaneError::msg(format!(
                    "'{value}' is not a valid JSON array of model ids (e.g. '[\"openrouter/z-ai/glm-5.2\"]')"
                )))?;
        }
        "bench.slice_timeout_s" => config.bench.slice_timeout_s = parse_u64(value)?,
        _ => return Err(unknown_config_key(key)),
    }
    // 4A config-inversion guard: any change to a di.* key re-validates the
    // full DiConfig so a `config set` that creates an impossible combination
    // (e.g. objection_window_s > request_timeout_s) is rejected before it
    // is persisted, with an error naming the offending relationship.
    if key.starts_with("di.") {
        config.di.validate()?;
    }
    Ok(())
}

fn cmd_config(action: &cli::ConfigAction) -> Result<()> {
    match action {
        cli::ConfigAction::Get { key } => {
            let config = load_config()?;
            println!("{key} = {}", config_get(&config, key)?);
            Ok(())
        }
        cli::ConfigAction::Set { key, value } => {
            let mut config = load_config()?;
            config_set(&mut config, key, value)?;
            save_config(&config)?;
            println!("set {key} = {}", config_get(&config, key)?);
            println!("saved to {}", config_path().display());
            Ok(())
        }
        cli::ConfigAction::Explain { key } => cmd_config_explain(key),
    }
}

fn cmd_config_explain(key: &str) -> Result<()> {
    let config = load_config()?;
    let effective = config_get(&config, key)?;
    let meaning = match key {
        "squire.max_concurrent" => {
            "The maximum number of agents the squire runs SIMULTANEOUSLY. This is a\n\
             scheduling ceiling, not the number of registered agents -- a swarm may have\n\
             many more agents registered than this. When ready work exceeds the ceiling,\n\
             the extra agents are deferred to a later tick, which can look like \"agents\n\
             registered but idle\". Compiled default: 2."
        }
        "squire.interval_s" => {
            "How often (in seconds) `trelane squire --watch` runs a scheduling tick."
        }
        "squire.reply_timeout_s" => {
            "Seconds a reply-wait park may sit before the squire declares it abandoned and\n\
             wakes the waiting agent. 'none' disables timeout-based abandonment."
        }
        "squire.breaker_escalation_count" => {
            "R24: how many times the same agent may be woken as designated breaker for the\n\
             same wait-cycle before the cycle escalates (a different deterministic tie-break\n\
             is tried, then the cycle is surfaced as needing a human). Default: 3."
        }
        "squire.starvation_ticks" => {
            "R23: a wake candidate that has been valid but unchosen for this many consecutive\n\
             ticks is guaranteed one of the next tick's capacity slots, ahead of ordinary\n\
             ordering. Default: 10 (~3.3 minutes at the default 20s tick interval)."
        }
        "di.objection_window_s" => {
            "Seconds a non-owner DI approval must stand unvetoed before the request resolves\n\
             to Approved (R9). Gives the domain owner a real chance to see and veto it.\n\
             Default: 300 (5 minutes)."
        }
        "di.request_timeout_s" => {
            "Seconds a domain-intrusion request may sit with no approval and no veto before\n\
             it transitions to Expired -- never silently Approved (R25). Default: 3600."
        }
        "di.claim_contested_timeout_s" => {
            "Seconds a claim-contested park (an approved DI whose claim lost the lease race,\n\
             R26) may sit before the contention is abandoned and the requester is woken.\n\
             Default: 1800 (30 minutes)."
        }
        "retention.hot_days" => {
            "Messages untouched for longer than this many days are archived: excluded from\n\
             default queries, fully readable under --include-archived (R15). Default: 30."
        }
        "retention.dormant_days" => {
            "A whole project with zero agent activity for this many days is flagged dormant\n\
             (a marker only; no data is touched). Default: 90."
        }
        "retention.purge_days" => {
            "Real deletion threshold in days. 'none' (the default) means nothing is ever\n\
             deleted -- deletion only happens when this is explicitly configured (R15)."
        }
        "claims.default_ttl_s" => {
            "Default lease duration (in seconds) for a file claim when --ttl is not given."
        }
        "workspace.mode" => {
            "Workspace mode for delegated changes. 'shared' (default) uses the main checkout.\n\
             'worktree' creates an isolated git worktree per delegation so helpers work in\n\
             a separate directory. Worktree mode requires git."
        }
        _ => return Err(unknown_config_key(key)),
    };

    println!("key       : {key}");
    println!("effective : {effective}");
    println!("source    : {}", config_path().display());
    println!("meaning   :");
    for line in meaning.lines() {
        println!("  {line}");
    }
    println!("change    :");
    println!(
        "  config file : edit \"{key}\" in {}",
        config_path().display()
    );
    println!("  cli         : trelane config set {key} <value>");
    if key == "squire.max_concurrent" {
        println!("  single run  : trelane squire --max-concurrent <value>");
        // Best-effort live utilization, only if we're inside a project. This is
        // deliberately side-effect-free: it reads registered/running counts but
        // does NOT run the candidate scan (which records cycle-break attempts).
        if let Ok(ctx) = Context::open(None)
            && let Ok(agents) = store::list_agents(&ctx.conn)
        {
            let running = agents
                .iter()
                .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
                .count();
            let limit = ctx.config.squire.max_concurrent;
            println!("live      :");
            println!(
                "  {} registered / {} running / limit {} ({} slot(s) free)",
                agents.len(),
                running,
                limit,
                limit.saturating_sub(running),
            );
        }
    }
    Ok(())
}

fn print_metrics(m: &telemetry::MetricsSummary) {
    use std::fmt::Write as _;
    println!();
    crate::logo::print_logo();
    println!("  Trelane Metrics Summary");
    println!("  ========================");
    println!();

    let fmt_ms = |ms: u64| -> String {
        if ms < 1000 {
            format!("{ms}ms")
        } else if ms < 60_000 {
            format!("{:.1}s", ms as f64 / 1000.0)
        } else {
            format!("{:.1}m", ms as f64 / 60_000.0)
        }
    };

    println!("  Overview:");
    println!("    Total agent runs      : {}", m.total_runs);
    println!("    Total wait events     : {}", m.total_wait_events);
    println!("    Total squire ticks      : {}", m.total_squire_ticks);
    println!(
        "    Total run time        : {}",
        fmt_ms(m.total_run_duration_ms)
    );
    println!(
        "    Total wait time       : {}",
        fmt_ms(m.total_wait_duration_ms)
    );
    println!(
        "    Avg run duration      : {}",
        fmt_ms(m.avg_run_duration_ms as u64)
    );
    println!(
        "    Avg wait duration     : {}",
        fmt_ms(m.avg_wait_duration_ms as u64)
    );
    println!(
        "    Efficiency ratio      : {:.1}% (run / (run+wait))",
        m.efficiency_ratio * 100.0
    );
    println!();

    println!("  Code Production:");
    println!("    Files changed         : {}", m.total_files_changed);
    println!("    Lines added           : {}", m.total_lines_added);
    println!("    Lines removed         : {}", m.total_lines_removed);
    println!("    Messages processed    : {}", m.total_messages_processed);
    println!("    Messages sent         : {}", m.total_messages_sent);
    println!("    Deadlocks detected    : {}", m.total_deadlocks_detected);
    println!();

    if m.total_run_duration_ms > 0 {
        let lines_per_min =
            m.total_lines_added as f64 / (m.total_run_duration_ms as f64 / 60_000.0);
        println!("    Lines/min (added)     : {lines_per_min:.1}");
    }

    println!();
    println!("  Per-Agent Breakdown:");
    println!(
        "    {:<16} {:>5} {:>5} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6}",
        "Agent", "Runs", "Waits", "RunTime", "WaitTime", "Files", "Add", "Del", "Rating"
    );
    for a in &m.per_agent {
        let rating = a
            .avg_rating
            .map(|r| format!("{r:.1}"))
            .unwrap_or("-".to_string());
        println!(
            "    {:<16} {:>5} {:>5} {:>8} {:>8} {:>6} {:>6} {:>6} {:>6}",
            a.agent,
            a.runs,
            a.wait_events,
            fmt_ms(a.run_duration_ms),
            fmt_ms(a.wait_duration_ms),
            a.files_changed,
            a.lines_added,
            a.lines_removed,
            rating
        );
    }
    println!();

    let _ = write!(String::new(), ""); // suppress unused import warning
}

fn cmd_kill() -> Result<()> {
    use crate::logo;

    logo::print_logo();
    println!();
    println!("  Killing all Trelane sessions...");
    println!();

    // Find all tmux sessions whose name starts with "trelane-"
    let output = std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();

    let sessions: Vec<String> = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|s| s.starts_with("trelane-"))
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    };

    if sessions.is_empty() {
        println!("  No Trelane tmux sessions found.");
    } else {
        for session in &sessions {
            print!("  Killing session: {session}... ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            let result = std::process::Command::new("tmux")
                .args(["kill-session", "-t", session])
                .status();

            match result {
                Ok(s) if s.success() => println!("done"),
                _ => println!("failed (may already be dead)"),
            }
        }
    }

    // Also kill any lingering opencode processes spawned by trelane
    let opencode_killed = std::process::Command::new("pkill")
        .args(["-f", "opencode.*trelane"])
        .status();
    if let Ok(s) = opencode_killed
        && s.success()
    {
        println!("  Killed lingering opencode processes.");
    }

    // Kill any lingering squire processes
    let squire_killed = std::process::Command::new("pkill")
        .args(["-f", "trelane.*squire.*--watch"])
        .status();
    if let Ok(s) = squire_killed
        && s.success()
    {
        println!("  Killed lingering squire processes.");
    }

    println!();
    println!("  All Trelane sessions terminated.");
    println!("  Running 'trelane status' on any project will show all agents as stopped.");
    println!();

    Ok(())
}

/// Public entry so the diagnostic TUI can trigger the emergency kill after it
/// has restored the terminal out of raw/alternate-screen mode.
pub fn run_kill_from_diagnostic() -> Result<()> {
    cmd_kill()
}
