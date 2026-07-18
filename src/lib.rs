pub mod biplane;
pub mod biplane_ui;
pub mod cli;
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

fn cmd_launch(cli: Cli) -> Result<()> {
    let root = match cli.project.as_deref().or(cli.root.as_deref()) {
        Some(p) => p.canonicalize()?,
        None => std::env::current_dir()?.canonicalize()?,
    };

    let models: Vec<String> = cli
        .models
        .as_deref()
        .unwrap_or("glm-5.2")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let max_agents = cli.max_agents.unwrap_or(3) as usize;
    let primary_model = models
        .first()
        .cloned()
        .unwrap_or_else(|| "glm-5.2".to_string());

    crate::logo::print_logo();
    println!();
    println!("  Project   : {}", root.display());
    println!("  Models    : {}", models.join(", "));
    println!("  Max agents: {}", max_agents);
    println!();

    if !root.join(TRELANE_DIR).join("trelane.db").exists() {
        crate::commands::cmd_init(Some(root.clone()))?;
    }

    let existing_agents = {
        let ctx = Context::open(Some(&root))?;
        crate::store::list_agents(&ctx.conn)?
    };

    if existing_agents.is_empty() {
        if cli.with_biplane {
            println!(
                "[launch] Running Biplane analysis with {}...",
                primary_model
            );
            let plan = biplane::run_biplane_plan(&root, &primary_model, max_agents)?;

            println!("[launch] Biplane proposed {} agent(s):", plan.agents.len());
            for a in &plan.agents {
                println!(
                    "  - {} : {} (writable: {})",
                    a.name,
                    a.description,
                    a.writable.join(", ")
                );
            }
            println!();

            let ctx = Context::open(Some(&root))?;
            for agent in &plan.agents {
                crate::commands::cmd_add_agent(
                    &ctx,
                    &agent.name,
                    &agent.writable,
                    &[],
                    Some(&agent.description),
                    Some(&primary_model),
                )?;
            }

            for task in &plan.initial_tasks {
                crate::commands::cmd_send(
                    &ctx,
                    "user",
                    &task.agent,
                    "question",
                    "normal",
                    &task.work.subject,
                    &task.work.body,
                    &None,
                    &None,
                    &[],
                )?;
            }

            if let Some(pocket) = biplane::find_pocket_for_project(&root) {
                let report_path = pocket.join("biplane-report.json");
                let report = biplane::generate_biplane_report(&ctx, Some(&pocket))?;
                std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;
                println!("[launch] Biplane report saved to {}", report_path.display());
            }
            println!();
        } else {
            println!("[launch] No agents registered. Use --with-biplane or add agents manually.");
            println!(
                "[launch] Run: trelane {} --models {} --max-agents {} --with-biplane",
                root.display(),
                primary_model,
                max_agents
            );
            return Ok(());
        }
    } else {
        // Resume mode: agents already exist. Clear ALL running locks since
        // we're explicitly relaunching -- any existing locks are from a
        // previous session that is no longer active.
        println!(
            "[launch] Found {} existing agent(s): {}",
            existing_agents.len(),
            existing_agents.join(", ")
        );

        let ctx = Context::open(Some(&root))?;
        for agent in &existing_agents {
            crate::store::delete_running_lock(&ctx.conn, agent).ok();
        }
        println!("[launch] Cleared all running locks from previous session");

        // Summarize pending work
        let mut pending_inbox = 0;
        let mut ready_parks = 0;
        let mut stuck_parks = 0;
        for agent in &existing_agents {
            let inbox = crate::store::get_unprocessed_messages(&ctx.conn, agent)?.len();
            pending_inbox += inbox;

            for task in crate::store::list_parked_tasks_for_agent(&ctx.conn, agent)? {
                if crate::prompt::park_satisfied(&ctx.conn, &task)? {
                    ready_parks += 1;
                } else {
                    stuck_parks += 1;
                }
            }
        }

        if pending_inbox > 0 || ready_parks > 0 {
            println!(
                "[launch] Resuming: {} unprocessed message(s), {} ready parked task(s), {} waiting parked task(s)",
                pending_inbox, ready_parks, stuck_parks
            );
        } else if stuck_parks > 0 {
            println!(
                "[launch] {} parked task(s) still waiting (no ready replies). The squire will attempt deadlock breaking if needed.",
                stuck_parks
            );
        } else {
            println!(
                "[launch] No pending work found. All agents have empty inboxes and no parked tasks."
            );
            println!(
                "[launch] Assign new work with: trelane send --from user --to <agent> --type question --subject '...' --body '...'"
            );
            println!("[launch] Or run: trelane {} biplane", root.display());
            return Ok(());
        }
        println!();
    }

    println!("[launch] Starting interactive tmux session...");
    println!();

    // Write a self-contained launch script and open it in a new Terminal.app
    // window.  This ensures the tmux session is created from within a real
    // terminal with a proper TTY, which is required for tmux pane creation
    // and opencode TUI launches to work correctly.
    let exe = std::env::current_exe()?;
    let session_name = format!("trelane-{}", chrono::Utc::now().format("%Y%m%d%H%M%S"));

    // Frames are provisioned only for agents that can actually run in this
    // session: session-disabled agents (via --agents/--no-agents or
    // config.json) get no pane, and an explicit --max-agents caps the count.
    // Previously every registered agent got a frame, so a session limited to
    // two runnable agents could still open four panes.
    let ctx = Context::open(Some(&root))?;
    let all_agents = crate::store::list_agents(&ctx.conn)?;
    let enabled_agents = crate::commands::launch_enabled_agents(&ctx)?;
    let skipped: Vec<String> = all_agents
        .iter()
        .filter(|a| !enabled_agents.contains(a))
        .cloned()
        .collect();
    let mut frame_agents = enabled_agents;
    if let Some(cap) = cli.max_agents {
        frame_agents.truncate(cap as usize);
    }
    if !skipped.is_empty() {
        println!(
            "[launch] Skipping frame(s) for {} session-disabled agent(s): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    println!(
        "[launch] Creating {} frame(s): {}",
        frame_agents.len(),
        frame_agents.join(", ")
    );
    let agent_list = frame_agents.join(" ");

    let script_path = root.join(".trelane").join("launch-session.sh");
    let script_content = format!(
        r##"#!/bin/bash
# Auto-generated by trelane. Do not edit.
set -euo pipefail

SESSION="{session_name}"
EXE="{exe}"
ROOT="{root}"

echo "Creating tmux session $SESSION ..."
tmux new-session -d -s "$SESSION"
sleep 1

CONTROLLER=$(tmux list-panes -t "$SESSION" -F "#{{pane_id}}" | head -1)

# Per-session root marker: the diagnostic key bindings read it at trigger time.
echo "$ROOT" > "/tmp/trelane-$SESSION-root"

# Create one pane per agent
for AGENT in {agent_list}; do
    PANE=$(tmux split-window -d -P -F "#{{pane_id}}" -t "$CONTROLLER" 2>/dev/null || true)
    if [ -n "$PANE" ]; then
        tmux select-pane -t "$PANE" -T "$AGENT"
        "$EXE" --root "$ROOT" set-launch-target "$AGENT" --adapter tmux --target "$PANE"
        echo "  Created pane for $AGENT"
    fi
done

tmux select-layout -t "$SESSION" tiled

# Start the squire in the controller pane. TRELANE_SESSION lets the squire own
# the session UI: status bar refresh, key bindings, verbose marker.
tmux send-keys -t "$CONTROLLER" "TRELANE_SESSION='$SESSION' '$EXE' --root '$ROOT' squire --watch" Enter

echo ""
echo "Session $SESSION is ready."
echo "Attaching..."
exec tmux attach-session -t "$SESSION"
"##,
        session_name = session_name,
        exe = exe.display(),
        root = root.display(),
        agent_list = agent_list,
    );
    std::fs::write(&script_path, &script_content)?;

    // Make the script executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }

    // Open Terminal.app using `open` with a .command file instead of
    // osascript.  This avoids the repeated macOS Automation permission
    // prompts that osascript triggers every time the binary is rebuilt
    // (each rebuild changes the code signature, so TCC re-prompts).
    let command_file = script_path.with_extension("command");
    std::fs::write(
        &command_file,
        format!("#!/bin/bash\nexec bash '{}'\n", script_path.display()),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&command_file)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&command_file, perms)?;
    }

    std::process::Command::new("open")
        .arg(&command_file)
        .status()?;

    println!(
        "[launch] Terminal.app window opened with session: {}",
        session_name
    );
    println!("[launch] The squire and agents will start automatically.");

    Ok(())
}

#[allow(dead_code)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Compute the live session state and push it to the tmux status bar.
/// Called by the squire on every watch tick.
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

    if cli.models.is_some() && cli.command.is_none() {
        return cmd_launch(cli);
    }

    match cli.command {
        None => biplane::cmd_welcome(cli.project),
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
            if ui {
                let root = match cli.root.as_deref() {
                    Some(p) => p.to_path_buf(),
                    None => std::env::current_dir()?,
                };
                biplane_ui::run(&root)
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
        Some(Command::Squire {
            once,
            watch,
            interval,
            launcher,
            verbose,
            max_concurrent,
        }) => {
            let mut ctx = Context::open(cli.root.as_deref())?;
            // A `--max-concurrent N` flag overrides the configured ceiling for
            // just this squire process, without touching the saved config.
            if let Some(mc) = max_concurrent {
                ctx.config.squire.max_concurrent = mc;
                eprintln!(
                    "{} squire: max_concurrent overridden to {mc} for this run",
                    crypto::now_iso()
                );
            }
            // The launch script exports TRELANE_SESSION so the squire can own
            // the session UI (status bar, key bindings, verbose marker).
            let session = std::env::var("TRELANE_SESSION")
                .ok()
                .filter(|s| !s.is_empty());

            if once || !watch {
                let v = verbose || splash::verbose_enabled(session.as_deref());
                squire::tick(&ctx, launcher.as_deref(), v)?;
                return Ok(());
            }

            let interval_s = interval.unwrap_or(ctx.config.squire.interval_s);

            // The controller frame is the squire's home: identify it.
            logo::print_logo();
            eprintln!(
                "{} squire watching every {interval_s}s (ctrl-c to stop)",
                crypto::now_iso()
            );
            if let Some(session) = session.as_deref() {
                eprintln!("  session : {session}");
                eprintln!(
                    "  verbose : press {} to toggle (marker: {})",
                    ctx.config.ui.keys.verbose_toggle,
                    splash::verbose_marker_path(session)
                );
                // Best-effort: a broken tmux server must not kill the squire.
                if let Err(e) = splash::setup_session_ui(session, &ctx.config.ui) {
                    eprintln!("warning: session UI setup failed: {e:?}");
                }
            }
            if ctx.config.biplane.reanalyze_on_all_stop {
                eprintln!("  biplane : reanalyze_on_all_stop enabled");
            }

            let mut reanalyzed_this_stretch = false;

            loop {
                let v = verbose || splash::verbose_enabled(session.as_deref());
                match squire::tick(&ctx, launcher.as_deref(), v) {
                    Ok(n) => {
                        if n > 0 {
                            eprintln!("{} launched {n} agent(s)", crypto::now_iso());
                        }
                    }
                    Err(e) => {
                        eprintln!("{} tick error: {e:?}", crypto::now_iso());
                    }
                }
                // Refresh the status bar from the real session state on every
                // tick, so ACTIVE/IDLE/DEADLOCK tracks reality instead of the
                // value set once at bootstrap.
                if let Some(session) = session.as_deref()
                    && let Err(e) = refresh_session_status(&ctx, session)
                    && v
                {
                    eprintln!("warning: status bar refresh failed: {e:?}");
                }

                // Biplane re-analysis: when the swarm is fully quiescent
                // (no running agents, empty inboxes, no parked tasks).
                // F3: Detection (thematic deadlock reporting) is on by
                // default; auto-registration of emergent domains is opt-in.
                let any_running = crate::store::list_agents(&ctx.conn)?
                    .iter()
                    .any(|a| crate::commands::is_running(&ctx.conn, a).unwrap_or(false));
                if any_running {
                    reanalyzed_this_stretch = false;
                } else if !reanalyzed_this_stretch
                    && crate::testing::swarm_quiescent(&ctx)?
                    && (ctx.config.biplane.detect_thematic_deadlock
                        || ctx.config.biplane.reanalyze_on_all_stop)
                {
                    if let Err(e) = biplane::reanalyze_on_stop(&ctx) {
                        eprintln!("warning: biplane re-analysis failed: {e:?}");
                    }
                    reanalyzed_this_stretch = true;
                }

                std::thread::sleep(std::time::Duration::from_secs(interval_s));
            }
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
const KNOWN_CONFIG_KEYS: &str =
    "squire.max_concurrent, squire.interval_s, squire.reply_timeout_s, \
     squire.breaker_escalation_count, squire.starvation_ticks, \
     di.objection_window_s, di.request_timeout_s, di.claim_contested_timeout_s, \
     retention.hot_days, retention.dormant_days, retention.purge_days, \
     claims.default_ttl_s, workspace.mode";

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
        "di.claim_contested_timeout_s" => {
            config.di.claim_contested_timeout_s = parse_u64(value)?
        }
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
            config.workspace.mode = crate::models::WorkspaceMode::parse(value)
                .ok_or_else(|| TrelaneError::msg(format!(
                    "'{value}' is not a valid workspace mode (use 'shared' or 'worktree')"
                )))?;
        }
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
