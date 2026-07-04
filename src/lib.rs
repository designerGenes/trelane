pub mod biplane;
pub mod cli;
pub mod commands;
pub mod crypto;
pub mod db;
pub mod domain;
pub mod error;
pub mod logo;
pub mod models;
pub mod prompt;
pub mod prop;
pub mod pump;
pub mod splash;
pub mod store;
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
    Ok(serde_json::from_str(&text)?)
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
                    &task.subject,
                    &task.body,
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
                "[launch] {} parked task(s) still waiting (no ready replies). The prop will attempt deadlock breaking if needed.",
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

# Start the prop in the controller pane. TRELANE_SESSION lets the prop own
# the session UI: status bar refresh, key bindings, verbose marker.
tmux send-keys -t "$CONTROLLER" "TRELANE_SESSION='$SESSION' '$EXE' --root '$ROOT' prop --watch" Enter

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

    let script_path_str = script_path.display().to_string();
    let osascript = format!(
        r#"tell application "Terminal"
        activate
        do script "bash {script}"
    end tell"#,
        script = script_path_str
    );

    let result = std::process::Command::new("osascript")
        .args(["-e", &osascript])
        .output()?;

    if !result.status.success() {
        let err = String::from_utf8_lossy(&result.stderr);
        return Err(crate::error::TrelaneError::msg(format!(
            "failed to open Terminal.app: {err}"
        )));
    }

    println!(
        "[launch] Terminal.app window opened with session: {}",
        session_name
    );
    println!("[launch] The prop and agents will start automatically.");

    Ok(())
}

#[allow(dead_code)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Compute the live session state and push it to the tmux status bar.
/// Called by the prop on every watch tick.
fn refresh_session_status(ctx: &Context, session: &str) -> Result<()> {
    let agents = store::list_agents(&ctx.conn)?;
    let running = agents
        .iter()
        .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
        .count();
    let (_, cycle) = prop::wait_graph(&ctx.conn)?;
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
            desc,
            launcher_agent,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_add_agent(
                &ctx,
                &name,
                &writable,
                desc.as_deref(),
                launcher_agent.as_deref(),
            )
        }
        Some(Command::Redomain {
            agent,
            writable,
            desc,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_redomain(&ctx, &agent, &writable, desc.as_deref())
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
        Some(Command::Inbox { agent, json }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_inbox(&ctx, &agent, json)
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
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_claim(&ctx, &agent, &path, ttl, task.as_deref(), grant.as_deref())
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
            json,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            biplane::cmd_biplane(&ctx, safe_pocket_dir.as_deref(), json)
        }
        Some(Command::Stub { agent }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_stub(&ctx, &agent)
        }
        Some(Command::Prop {
            once,
            watch,
            interval,
            launcher,
            verbose,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            // The launch script exports TRELANE_SESSION so the prop can own
            // the session UI (status bar, key bindings, verbose marker).
            let session = std::env::var("TRELANE_SESSION")
                .ok()
                .filter(|s| !s.is_empty());

            if once || !watch {
                let v = verbose || splash::verbose_enabled(session.as_deref());
                prop::tick(&ctx, launcher.as_deref(), v)?;
                return Ok(());
            }

            let interval_s = interval.unwrap_or(ctx.config.prop.interval_s);

            // The controller frame is the prop's home: identify it.
            logo::print_logo();
            eprintln!(
                "{} prop watching every {interval_s}s (ctrl-c to stop)",
                crypto::now_iso()
            );
            if let Some(session) = session.as_deref() {
                eprintln!("  session : {session}");
                eprintln!(
                    "  verbose : press {} to toggle (marker: {})",
                    ctx.config.ui.keys.verbose_toggle,
                    splash::verbose_marker_path(session)
                );
                // Best-effort: a broken tmux server must not kill the prop.
                if let Err(e) = splash::setup_session_ui(session, &ctx.config.ui) {
                    eprintln!("warning: session UI setup failed: {e:?}");
                }
            }

            loop {
                let v = verbose || splash::verbose_enabled(session.as_deref());
                match prop::tick(&ctx, launcher.as_deref(), v) {
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
                std::thread::sleep(std::time::Duration::from_secs(interval_s));
            }
        }
    }
}
