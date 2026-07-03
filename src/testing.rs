use crate::Context;
use crate::commands;
use crate::error::{Result, TrelaneError};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub launcher: Option<String>,
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
    pub launcher_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScenarioStep {
    Send {
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
        agent: String,
        #[serde(default)]
        task: Option<String>,
        wait_reply_ref: String,
        waiting_on: String,
        resume_hint: String,
    },
    Pump {
        ticks: u32,
    },
    ClaimExpectDenied {
        agent: String,
        path: String,
    },
    Redomain {
        agent: String,
        writable: Vec<String>,
        #[serde(default)]
        desc: Option<String>,
    },
    AssertNoDeadlock,
    AssertParkedCount {
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
    }

    let started_at = chrono::Utc::now();
    let mut refs = std::collections::HashMap::<String, String>::new();
    let mut counters = Counters::default();

    for (index, step) in scenario.steps.iter().enumerate() {
        println!("[testing] step {}: {}", index + 1, step_name(step));
        match step {
            ScenarioStep::Send {
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
            ScenarioStep::Pump { ticks } => {
                let launcher = launcher_override
                    .map(str::to_string)
                    .unwrap_or_else(default_stub_launcher);
                for tick in 0..*ticks {
                    println!("[testing] pump tick {} of {}", tick + 1, ticks);
                    crate::pump::tick(&ctx, Some(launcher.as_str()))?;
                    wait_for_idle(&ctx, 40, std::time::Duration::from_millis(250))?;
                    counters.pumps += 1;
                }
            }
            ScenarioStep::ClaimExpectDenied { agent, path } => {
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
                agent,
                writable,
                desc,
            } => {
                commands::cmd_redomain(&ctx, agent, writable, desc.as_deref())?;
                counters.redomains += 1;
            }
            ScenarioStep::AssertNoDeadlock => {
                let (_, cycle) = crate::pump::wait_graph(&ctx.conn)?;
                if cycle.is_some() {
                    return Err(TrelaneError::msg(
                        "scenario assertion failed: deadlock still present",
                    ));
                }
            }
            ScenarioStep::AssertParkedCount { count } => {
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
    })
}

fn step_name(step: &ScenarioStep) -> &'static str {
    match step {
        ScenarioStep::Send { .. } => "send",
        ScenarioStep::Park { .. } => "park",
        ScenarioStep::Pump { .. } => "pump",
        ScenarioStep::ClaimExpectDenied { .. } => "claim-expect-denied",
        ScenarioStep::Redomain { .. } => "redomain",
        ScenarioStep::AssertNoDeadlock => "assert-no-deadlock",
        ScenarioStep::AssertParkedCount { .. } => "assert-parked-count",
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

fn default_stub_launcher() -> String {
    std::env::current_exe()
        .map(|path| format!("{} --root {{root}} stub {{agent}}", path.display()))
        .unwrap_or_else(|_| "trelane --root {root} stub {agent}".to_string())
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
              "from": "alpha",
              "to": "alpha",
              "msg_type": "info",
              "subject": "self",
              "save_as": "m1"
            },
            {
              "type": "Redomain",
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
            step_name(&ScenarioStep::AssertNoDeadlock),
            "assert-no-deadlock"
        );
    }
}
