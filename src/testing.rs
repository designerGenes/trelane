use crate::Context;
use crate::commands;
use crate::crypto;
use crate::error::{Result, TrelaneError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::IsTerminal;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub launcher: Option<String>,
    #[serde(default)]
    pub mode: ScenarioMode,
    pub project: ScenarioProject,
    pub agents: Vec<ScenarioAgent>,
    pub steps: Vec<ScenarioStep>,
    #[serde(default)]
    pub metrics: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioProject {
    pub files: Vec<ScenarioFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioFile {
    pub path: String,
    pub contents: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioAgent {
    pub name: String,
    pub description: String,
    pub writable: Vec<String>,
    #[serde(default)]
    pub forbidden_write: Vec<String>,
    #[serde(default)]
    pub launcher_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ScenarioMode {
    #[default]
    Stub,
    Interactive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScenarioStep {
    Send {
        explanation: String,
        from: String,
        to: String,
        msg_type: String,
        subject: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        urgency: Option<String>,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        save_as: Option<String>,
    },
    Park {
        explanation: String,
        agent: String,
        #[serde(default)]
        task: Option<String>,
        wait_reply_ref: String,
        waiting_on: String,
        resume_hint: String,
    },
    Pump {
        explanation: String,
        ticks: u32,
    },
    PumpWatch {
        explanation: String,
        interval_s: u64,
        max_ticks: u32,
        idle_grace_ticks: u32,
    },
    ClaimExpectDenied {
        explanation: String,
        agent: String,
        path: String,
    },
    Redomain {
        explanation: String,
        agent: String,
        writable: Vec<String>,
        #[serde(default)]
        desc: Option<String>,
    },
    AssertNoDeadlock {
        explanation: String,
    },
    AssertParkedCount {
        explanation: String,
        count: usize,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub run: u32,
    pub scenario: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: i64,
    pub result: String,
    pub sandbox: String,
    pub messages_sent: usize,
    pub pumps: u32,
    pub redomains: u32,
    pub deadlocks_detected: usize,
    pub metrics: Vec<String>,
    pub mode: String,
}

#[derive(Default)]
struct Counters {
    messages_sent: usize,
    pumps: u32,
    redomains: u32,
}

struct SendCaptureInput<'a> {
    from: &'a str,
    to: &'a str,
    msg_type: &'a str,
    urgency: &'a str,
    subject: &'a str,
    body: &'a str,
    paths: &'a [String],
}

pub fn run_testing(
    scenario_path: &Path,
    runs: u32,
    report_path: Option<&Path>,
    sandbox_root: Option<&Path>,
    launcher_override: Option<&str>,
) -> Result<()> {
    let scenario = load_scenario(scenario_path)?;
    if matches!(scenario.mode, ScenarioMode::Interactive)
        && std::env::var("TRELANE_TESTING_WORKER").ok().as_deref() != Some("1")
    {
        return launch_interactive_tmux_supervisor(
            scenario_path,
            runs,
            report_path,
            sandbox_root,
            launcher_override,
        );
    }

    let report_path = report_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| scenario_path.with_extension("report.jsonl"));
    let sandbox_root = sandbox_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("trelane-testing"));

    if let Some(parent) = report_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir_all(&sandbox_root)?;
    if report_path.exists() {
        fs::remove_file(&report_path)?;
    }

    for run in 1..=runs.max(1) {
        let report = run_once(
            &scenario,
            run,
            &sandbox_root,
            launcher_override.or(scenario.launcher.as_deref()),
        )?;
        println!(
            "[testing] run {run} finished result={} duration_ms={}",
            report.result, report.duration_ms
        );
        let line = serde_json::to_string(&report)?;
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&report_path)?;
        writeln!(file, "{line}")?;
    }

    println!("[testing] report written to {}", report_path.display());
    Ok(())
}

fn load_scenario(path: &Path) -> Result<Scenario> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

fn run_once(
    scenario: &Scenario,
    run: u32,
    sandbox_root: &Path,
    launcher_override: Option<&str>,
) -> Result<ScenarioReport> {
    let run_dir = sandbox_root.join(format!("scenario-run-{run}"));
    if run_dir.exists() {
        fs::remove_dir_all(&run_dir)?;
    }
    fs::create_dir_all(&run_dir)?;
    init_git_repo(&run_dir)?;

    println!(
        "[testing] scenario={} run={} setup sandbox={}",
        scenario.name,
        run,
        run_dir.display()
    );
    for file in &scenario.project.files {
        let path = run_dir.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        println!("[testing] write file {}", file.path);
        fs::write(path, &file.contents)?;
    }

    commands::cmd_init(Some(run_dir.clone()))?;
    commands::cmd_attach_project(Some(run_dir.clone()), None, None, false)?;

    let ctx = Context::open(Some(&run_dir))?;
    for agent in &scenario.agents {
        println!("[testing] add-agent {}", agent.name);
        commands::cmd_add_agent(
            &ctx,
            &agent.name,
            &agent.writable,
            Some(&agent.description),
            agent.launcher_agent.as_deref(),
        )?;
        if !agent.forbidden_write.is_empty() {
            crate::store::upsert_agent(
                &ctx.conn,
                &agent.name,
                &agent.description,
                &agent.writable,
                agent.launcher_agent.as_deref(),
                &agent.forbidden_write,
                &crypto::now_iso(),
            )?;
        }
    }

    if matches!(scenario.mode, ScenarioMode::Interactive) {
        validate_interactive_setup(&ctx, scenario)?;
        provision_interactive_tmux_layout(&ctx, scenario)?;
    }

    let started_at = chrono::Utc::now();
    let mut refs = std::collections::HashMap::<String, String>::new();
    let mut counters = Counters::default();

    for (index, step) in scenario.steps.iter().enumerate() {
        println!(
            "[testing] step {}: {} - {}",
            index + 1,
            step_name(step),
            step_explanation(step)
        );
        match step {
            ScenarioStep::Send {
                explanation: _,
                from,
                to,
                msg_type,
                subject,
                body,
                urgency,
                paths,
                save_as,
            } => {
                let full_paths: Vec<String> = paths
                    .iter()
                    .map(|path| ctx.root.join(path).display().to_string())
                    .collect();
                let msg_id = send_and_capture(
                    &ctx,
                    SendCaptureInput {
                        from,
                        to,
                        msg_type,
                        urgency: urgency.as_deref().unwrap_or("normal"),
                        subject,
                        body,
                        paths: &full_paths,
                    },
                )?;
                if let Some(save_as) = save_as {
                    refs.insert(save_as.clone(), msg_id);
                }
                counters.messages_sent += 1;
            }
            ScenarioStep::Park {
                explanation: _,
                agent,
                task,
                wait_reply_ref,
                waiting_on,
                resume_hint,
            } => {
                let msg_id = refs.get(wait_reply_ref).ok_or_else(|| {
                    TrelaneError::msg(format!("missing saved reference '{wait_reply_ref}'"))
                })?;
                commands::cmd_park(
                    &ctx,
                    agent,
                    task.as_deref(),
                    &Some(msg_id.clone()),
                    &None,
                    waiting_on,
                    resume_hint,
                )?;
            }
            ScenarioStep::Pump {
                explanation: _,
                ticks,
            } => {
                let launcher = match scenario.mode {
                    ScenarioMode::Stub => launcher_override
                        .map(str::to_string)
                        .unwrap_or_else(default_stub_launcher),
                    ScenarioMode::Interactive => launcher_override
                        .map(str::to_string)
                        .or_else(|| scenario.launcher.clone())
                        .unwrap_or_default(),
                };
                for tick in 0..*ticks {
                    println!("[testing] pump tick {} of {}", tick + 1, ticks);
                    let override_arg = if launcher.is_empty() {
                        None
                    } else {
                        Some(launcher.as_str())
                    };
                    crate::pump::tick(&ctx, override_arg)?;
                    if matches!(scenario.mode, ScenarioMode::Stub) {
                        wait_for_idle(&ctx, 40, std::time::Duration::from_millis(250))?;
                    }
                    counters.pumps += 1;
                }
            }
            ScenarioStep::PumpWatch {
                explanation: _,
                interval_s,
                max_ticks,
                idle_grace_ticks,
            } => {
                let launcher = match scenario.mode {
                    ScenarioMode::Stub => launcher_override
                        .map(str::to_string)
                        .unwrap_or_else(default_stub_launcher),
                    ScenarioMode::Interactive => launcher_override
                        .map(str::to_string)
                        .or_else(|| scenario.launcher.clone())
                        .unwrap_or_default(),
                };
                let override_arg = if launcher.is_empty() {
                    None
                } else {
                    Some(launcher.as_str())
                };
                let mut idle_ticks = 0u32;
                for tick in 0..*max_ticks {
                    println!("[testing] pump watch tick {} of {}", tick + 1, max_ticks);
                    crate::pump::tick(&ctx, override_arg)?;
                    counters.pumps += 1;
                    if matches!(scenario.mode, ScenarioMode::Stub) {
                        wait_for_idle(&ctx, 40, std::time::Duration::from_millis(250))?;
                    }
                    if swarm_quiescent(&ctx)? {
                        idle_ticks += 1;
                        if idle_ticks >= *idle_grace_ticks {
                            println!(
                                "[testing] swarm quiescent for {} consecutive tick(s); stopping watch loop",
                                idle_ticks
                            );
                            break;
                        }
                    } else {
                        idle_ticks = 0;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(*interval_s));
                    if tick + 1 == *max_ticks {
                        return Err(TrelaneError::msg(
                            "pump watch exhausted max_ticks before the swarm became quiescent",
                        ));
                    }
                }
            }
            ScenarioStep::ClaimExpectDenied {
                explanation: _,
                agent,
                path,
            } => {
                let rel =
                    crate::domain::norm_rel(&ctx.root, &ctx.root.join(path).display().to_string())?;
                let dom = crate::store::get_domain(&ctx.conn, agent)?
                    .ok_or_else(|| TrelaneError::msg(format!("unknown agent '{agent}'")))?;
                let compiled = crate::domain::CompiledDomain::from_domain(&dom)?;
                let others = commands::owners_of(&ctx.conn, &rel, Some(agent))?;
                if compiled.is_writable(&rel) || others.is_empty() {
                    return Err(TrelaneError::msg(format!(
                        "expected denied claim for {agent} on {rel}, but scenario setup does not make it cross-domain"
                    )));
                }
                println!("[testing] verified claim should be denied for {agent} on {rel}");
            }
            ScenarioStep::Redomain {
                explanation: _,
                agent,
                writable,
                desc,
            } => {
                commands::cmd_redomain(&ctx, agent, writable, desc.as_deref())?;
                counters.redomains += 1;
            }
            ScenarioStep::AssertNoDeadlock { explanation: _ } => {
                let (_, cycle) = crate::pump::wait_graph(&ctx.conn)?;
                if cycle.is_some() {
                    return Err(TrelaneError::msg(
                        "scenario assertion failed: deadlock still present",
                    ));
                }
            }
            ScenarioStep::AssertParkedCount {
                explanation: _,
                count,
            } => {
                let parked = crate::store::list_parked_tasks(&ctx.conn)?;
                if parked.len() != *count {
                    return Err(TrelaneError::msg(format!(
                        "scenario assertion failed: expected {count} parked tasks, found {}",
                        parked.len()
                    )));
                }
            }
        }
    }

    let ended_at = chrono::Utc::now();
    let (_, cycle) = crate::pump::wait_graph(&ctx.conn)?;
    let deadlocks_detected = usize::from(cycle.is_some());

    Ok(ScenarioReport {
        run,
        scenario: scenario.name.clone(),
        started_at: started_at.to_rfc3339(),
        ended_at: ended_at.to_rfc3339(),
        duration_ms: (ended_at - started_at).num_milliseconds(),
        result: if deadlocks_detected == 0 {
            "ok".to_string()
        } else {
            "failed".to_string()
        },
        sandbox: run_dir.display().to_string(),
        messages_sent: counters.messages_sent,
        pumps: counters.pumps,
        redomains: counters.redomains,
        deadlocks_detected,
        metrics: scenario.metrics.clone(),
        mode: match scenario.mode {
            ScenarioMode::Stub => "stub".to_string(),
            ScenarioMode::Interactive => "interactive".to_string(),
        },
    })
}

fn step_name(step: &ScenarioStep) -> &'static str {
    match step {
        ScenarioStep::Send { .. } => "send",
        ScenarioStep::Park { .. } => "park",
        ScenarioStep::Pump { .. } => "pump",
        ScenarioStep::PumpWatch { .. } => "pump-watch",
        ScenarioStep::ClaimExpectDenied { .. } => "claim-expect-denied",
        ScenarioStep::Redomain { .. } => "redomain",
        ScenarioStep::AssertNoDeadlock { .. } => "assert-no-deadlock",
        ScenarioStep::AssertParkedCount { .. } => "assert-parked-count",
    }
}

fn step_explanation(step: &ScenarioStep) -> &str {
    match step {
        ScenarioStep::Send { explanation, .. }
        | ScenarioStep::Park { explanation, .. }
        | ScenarioStep::Pump { explanation, .. }
        | ScenarioStep::PumpWatch { explanation, .. }
        | ScenarioStep::ClaimExpectDenied { explanation, .. }
        | ScenarioStep::Redomain { explanation, .. }
        | ScenarioStep::AssertNoDeadlock { explanation }
        | ScenarioStep::AssertParkedCount { explanation, .. } => explanation,
    }
}

fn init_git_repo(root: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["-C", &root.display().to_string(), "init", "-q"])
        .status()?;
    if !status.success() {
        return Err(TrelaneError::msg(format!(
            "failed to initialize git repository at {}",
            root.display()
        )));
    }
    let ignore_path = root.join("src/ui/secrets");
    fs::create_dir_all(&ignore_path)?;
    fs::write(ignore_path.join("token.txt"), "do-not-touch\n")?;
    Ok(())
}

fn wait_for_idle(ctx: &Context, attempts: usize, delay: std::time::Duration) -> Result<()> {
    for _ in 0..attempts {
        let any_running = crate::store::list_agents(&ctx.conn)?
            .iter()
            .any(|agent| crate::commands::is_running(&ctx.conn, agent).unwrap_or(false));
        if !any_running {
            return Ok(());
        }
        std::thread::sleep(delay);
    }
    Err(TrelaneError::msg(
        "timed out waiting for launched agents to finish their testing slice",
    ))
}

fn swarm_quiescent(ctx: &Context) -> Result<bool> {
    let any_running = crate::store::list_agents(&ctx.conn)?
        .iter()
        .any(|agent| crate::commands::is_running(&ctx.conn, agent).unwrap_or(false));
    if any_running {
        return Ok(false);
    }

    let any_inbox = crate::store::list_agents(&ctx.conn)?.iter().any(|agent| {
        !crate::store::get_unprocessed_messages(&ctx.conn, agent)
            .unwrap_or_default()
            .is_empty()
    });
    if any_inbox {
        return Ok(false);
    }

    Ok(crate::store::list_parked_tasks(&ctx.conn)?.is_empty())
}

fn default_stub_launcher() -> String {
    std::env::current_exe()
        .map(|path| format!("{} --root {{root}} stub {{agent}}", path.display()))
        .unwrap_or_else(|_| "trelane --root {root} stub {agent}".to_string())
}

fn launch_interactive_tmux_supervisor(
    scenario_path: &Path,
    runs: u32,
    report_path: Option<&Path>,
    sandbox_root: Option<&Path>,
    launcher_override: Option<&str>,
) -> Result<()> {
    let session_name = format!(
        "trelane-testing-{}",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    );
    let exe = std::env::current_exe()?;
    let report_path = report_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| scenario_path.with_extension("report.jsonl"));
    let sandbox_root = sandbox_root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("trelane-testing"));

    let mut worker_cmd = vec![
        format!("TRELANE_TESTING_WORKER=1"),
        format!("TRELANE_TESTING_SESSION={}", shell_env_quote(&session_name)),
        shell_path_quote(&exe),
        "--testing".to_string(),
        shell_path_quote(scenario_path),
        "--testing-runs".to_string(),
        runs.max(1).to_string(),
        "--testing-report".to_string(),
        shell_path_quote(&report_path),
        "--testing-sandbox-root".to_string(),
        shell_path_quote(&sandbox_root),
    ];
    if let Some(launcher) = launcher_override {
        worker_cmd.push("--testing-launcher".to_string());
        worker_cmd.push(shell_env_quote(launcher));
    }
    let worker_cmd = worker_cmd.join(" ");

    let create = std::process::Command::new("tmux")
        .args(["new-session", "-d", "-s", &session_name, &worker_cmd])
        .status()?;
    if !create.success() {
        return Err(TrelaneError::msg(format!(
            "failed to create tmux testing session '{session_name}'"
        )));
    }

    if std::io::stdout().is_terminal() {
        let attach = std::process::Command::new("tmux")
            .args(["attach-session", "-t", &session_name])
            .status()?;
        if !attach.success() {
            return Err(TrelaneError::msg(format!(
                "failed to attach to tmux testing session '{session_name}'"
            )));
        }
    } else {
        println!("interactive tmux session created: {session_name}");
        println!("attach with: tmux attach-session -t {session_name}");
    }

    Ok(())
}

fn shell_path_quote(path: &Path) -> String {
    shell_env_quote(&path.display().to_string())
}

fn shell_env_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn provision_interactive_tmux_layout(ctx: &Context, scenario: &Scenario) -> Result<()> {
    let session_name = std::env::var("TRELANE_TESTING_SESSION").map_err(|_| {
        TrelaneError::msg("interactive testing worker missing TRELANE_TESTING_SESSION")
    })?;
    let controller_pane = std::env::var("TMUX_PANE")
        .map_err(|_| TrelaneError::msg("interactive testing worker must run inside tmux"))?;

    std::process::Command::new("tmux")
        .args(["rename-window", "-t", &session_name, &scenario.name])
        .status()?;

    let mut pane_ids = Vec::new();
    for _ in &scenario.agents {
        let output = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &controller_pane,
            ])
            .output()?;
        if !output.status.success() {
            return Err(TrelaneError::msg(
                "failed to create tmux pane for interactive scenario",
            ));
        }
        pane_ids.push(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    std::process::Command::new("tmux")
        .args(["select-layout", "-t", &session_name, "tiled"])
        .status()?;

    for (agent, pane_id) in scenario.agents.iter().zip(pane_ids.iter()) {
        commands::cmd_set_launch_target(ctx, &agent.name, "tmux", pane_id, None, None)?;
    }

    Ok(())
}

fn validate_interactive_setup(ctx: &Context, scenario: &Scenario) -> Result<()> {
    for agent in &scenario.agents {
        let launcher_agent = agent.launcher_agent.as_deref().ok_or_else(|| {
            TrelaneError::msg(format!(
                "interactive scenario agent '{}' is missing launcher_agent",
                agent.name
            ))
        })?;

        let template = ctx
            .config
            .launcher
            .profiles
            .get(launcher_agent)
            .ok_or_else(|| {
                TrelaneError::msg(format!(
                    "interactive scenario agent '{}' requires launcher.profiles.{} in config.json",
                    agent.name, launcher_agent
                ))
            })?;

        let template_lc = template.to_ascii_lowercase();
        let looks_low_cost = template_lc.contains("haiku")
            || template_lc.contains("gpt-5-mini")
            || template_lc.contains("gpt5-mini")
            || template_lc.contains("stub");
        if !looks_low_cost {
            return Err(TrelaneError::msg(format!(
                "interactive scenario agent '{}' uses launcher profile '{}' that does not appear to be low-cost",
                agent.name, launcher_agent
            )));
        }

        crate::commands::ensure_tmux_target(&format!("trelane-{}", agent.name))?;
    }

    Ok(())
}

fn send_and_capture(ctx: &Context, input: SendCaptureInput<'_>) -> Result<String> {
    commands::cmd_send(
        ctx,
        input.from,
        input.to,
        input.msg_type,
        input.urgency,
        input.subject,
        input.body,
        &None,
        &None,
        input.paths,
    )?;
    let after = crate::store::get_unprocessed_messages(&ctx.conn, input.to)?;
    after
        .last()
        .map(|m| m.id.clone())
        .ok_or_else(|| TrelaneError::msg("failed to capture message id after send"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_scenario_parses_send_and_redomain_steps() {
        let json = r#"{
          "name": "demo",
          "description": "demo scenario",
          "project": { "files": [{ "path": "README.md", "contents": "hi" }] },
          "agents": [{ "name": "alpha", "description": "ui", "writable": ["src/ui/**"] }],
          "steps": [
            {
              "type": "Send",
              "explanation": "demo send",
              "from": "alpha",
              "to": "alpha",
              "msg_type": "info",
              "subject": "self",
              "save_as": "m1"
            },
            {
              "type": "Redomain",
              "explanation": "demo redomain",
              "agent": "alpha",
              "writable": ["src/**"]
            }
          ],
          "metrics": ["messages_sent"]
        }"#;
        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(scenario.name, "demo");
        assert_eq!(scenario.steps.len(), 2);
        match &scenario.steps[1] {
            ScenarioStep::Redomain {
                agent, writable, ..
            } => {
                assert_eq!(agent, "alpha");
                assert_eq!(writable, &vec!["src/**".to_string()]);
            }
            _ => panic!("expected redomain step"),
        }
    }

    #[test]
    fn step_name_is_stable() {
        assert_eq!(
            step_name(&ScenarioStep::AssertNoDeadlock {
                explanation: "x".to_string()
            }),
            "assert-no-deadlock"
        );
    }
}
