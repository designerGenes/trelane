pub mod cli;
pub mod commands;
pub mod crypto;
pub mod db;
pub mod domain;
pub mod error;
pub mod models;
pub mod prompt;
pub mod pump;
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

    match cli.command {
        None => commands::cmd_attach_project(
            cli.project,
            cli.agents.as_deref(),
            cli.no_agents.as_deref(),
            true,
        ),
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
        Some(Command::Stub { agent }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            commands::cmd_stub(&ctx, &agent)
        }
        Some(Command::Pump {
            once,
            watch,
            interval,
            launcher,
        }) => {
            let ctx = Context::open(cli.root.as_deref())?;
            if once || !watch {
                pump::tick(&ctx, launcher.as_deref())?;
                return Ok(());
            }
            let interval_s = interval.unwrap_or(ctx.config.pump.interval_s);
            eprintln!(
                "{} pump watching every {interval_s}s (ctrl-c to stop)",
                crypto::now_iso()
            );
            loop {
                match pump::tick(&ctx, launcher.as_deref()) {
                    Ok(n) => {
                        if n > 0 {
                            eprintln!("{} launched {n} agent(s)", crypto::now_iso());
                        }
                    }
                    Err(e) => {
                        eprintln!("{} tick error: {e:?}", crypto::now_iso());
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(interval_s));
            }
        }
    }
}
