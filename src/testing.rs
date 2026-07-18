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
    /// Bench mode: headless free-model agents launched as subprocesses with
    /// --max-turns. Like Stub mode in structure (squire::tick + wait_for_idle)
    /// but with a longer slice timeout and an events file the live TUI tails.
    Bench,
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
    // "prop" is the current name for the scheduler formerly called "the pump"
    // (now `crate::squire`). Accept it as an alias so scenario fixtures written
    // in the newer vocabulary still deserialize to this step.
    #[serde(alias = "Prop")]
    Pump {
        explanation: String,
        ticks: u32,
    },
    Wake {
        explanation: String,
        agent: String,
        why: String,
    },
    #[serde(alias = "PropWatch")]
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
    /// Verify a file exists in the sandbox project. The floor assertion for
    /// "did this run actually generate anything": a Stub run with
    /// hand-placed files passes; a free-model run that produced nothing fails.
    AssertFileExists {
        explanation: String,
        path: String,
    },
    /// Verify a file's contents include a substring. Stronger than
    /// AssertFileExists: catches an agent that created a placeholder file
    /// with no real content. Substring (not regex) to keep scenarios portable.
    AssertFileContains {
        explanation: String,
        path: String,
        contains: String,
    },
    /// Verify a task is in a named state ("ready"/"active"/"done"/...).
    /// Asserts the project's task ledger reflects what the scenario expected,
    /// not just that no deadlock remains.
    AssertTaskState {
        explanation: String,
        task_id: String,
        state: String,
    },
    /// Verify an agent's derived activity state ("idle"/"running"/"blocked"/
    /// "owned-work-ready"/...). Delegates to squire::agent_activity_status so
    /// the assertion uses the same derivation the squire itself uses.
    AssertAgentState {
        explanation: String,
        agent: String,
        state: String,
    },
    /// Biplane --describe as a setup phase. Loads the *.describe.json, runs
    /// Biplane planning (validate -> plan -> apply), and provisions the
    /// plan's agents + tasks into the live session. This is the
    /// Biplane->Trelane handoff exercised end-to-end. The describe path is
    /// resolved relative to the scenario file's parent (so a fixture can
    /// reference a sibling *.describe.json by bare filename) or as absolute.
    BiplaneDescribe {
        explanation: String,
        describe_path: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            Some(scenario_path),
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
    scenario_path: Option<&Path>,
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

    // Bench mode: open the events file the live TUI (step 4) will tail.
    // One file per run, in the sandbox, so concurrent runs don't collide.
    let mut bench_events = if matches!(scenario.mode, ScenarioMode::Bench) {
        Some(crate::bench::BenchEvents::create(
            &run_dir.join("bench-events.jsonl"),
        )?)
    } else {
        None
    };

    for agent in &scenario.agents {
        println!("[testing] add-agent {}", agent.name);
        commands::cmd_add_agent(
            &ctx,
            &agent.name,
            &agent.writable,
            &agent.forbidden_write,
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
                    &None,
                    waiting_on,
                    resume_hint,
                )?;
            }
            ScenarioStep::Pump {
                explanation: _,
                ticks,
            } => {
                let launcher = resolve_pump_launcher(scenario, launcher_override);
                let override_arg = (!launcher.is_empty()).then_some(launcher.as_str());
                for tick in 0..*ticks {
                    println!("[testing] pump tick {} of {}", tick + 1, ticks);
                    let launched = crate::squire::tick(&ctx, override_arg, false)?;
                    if matches!(scenario.mode, ScenarioMode::Stub) {
                        wait_for_idle(&ctx, 40, std::time::Duration::from_millis(250))?;
                    } else if matches!(scenario.mode, ScenarioMode::Bench) {
                        let running = count_running(&ctx);
                        if let Some(ref mut events) = bench_events {
                            events.after_tick(&ctx.conn, tick + 1, launched, running)?;
                        }
                        // Free-model slices can take minutes; wait up to the
                        // configured slice timeout before declaring a stall.
                        let timeout_s = ctx.config.bench.slice_timeout_s;
                        wait_for_idle(
                            &ctx,
                            ((timeout_s / 2).max(1)) as usize,
                            std::time::Duration::from_secs(2),
                        )?;
                    }
                    counters.pumps += 1;
                }
            }
            ScenarioStep::Wake {
                explanation: _,
                agent,
                why,
            } => {
                commands::cmd_wake(&ctx, agent, Some(why.as_str()), None)?;
            }
            ScenarioStep::PumpWatch {
                explanation: _,
                interval_s,
                max_ticks,
                idle_grace_ticks,
            } => {
                let launcher = resolve_pump_launcher(scenario, launcher_override);
                let override_arg = (!launcher.is_empty()).then_some(launcher.as_str());
                let mut idle_ticks = 0u32;
                let mut quiesced = false;
                for tick in 0..*max_ticks {
                    println!("[testing] pump watch tick {} of {}", tick + 1, max_ticks);
                    let launched = crate::squire::tick(&ctx, override_arg, false)?;
                    counters.pumps += 1;
                    if matches!(scenario.mode, ScenarioMode::Stub) {
                        wait_for_idle(&ctx, 40, std::time::Duration::from_millis(250))?;
                    } else if matches!(scenario.mode, ScenarioMode::Bench) {
                        let running = count_running(&ctx);
                        if let Some(ref mut events) = bench_events {
                            events.after_tick(&ctx.conn, tick + 1, launched, running)?;
                        }
                        let timeout_s = ctx.config.bench.slice_timeout_s;
                        wait_for_idle(
                            &ctx,
                            ((timeout_s / 2).max(1)) as usize,
                            std::time::Duration::from_secs(2),
                        )?;
                    }
                    if swarm_quiescent(&ctx)? {
                        idle_ticks += 1;
                        if idle_ticks >= *idle_grace_ticks {
                            println!(
                                "[testing] swarm quiescent for {} consecutive tick(s); stopping watch loop",
                                idle_ticks
                            );
                            quiesced = true;
                            break;
                        }
                    } else {
                        idle_ticks = 0;
                    }
                    // Don't sleep after the final tick -- nothing follows it.
                    if tick + 1 < *max_ticks {
                        std::thread::sleep(std::time::Duration::from_secs(*interval_s));
                    }
                }
                // Only a genuine failure if, after the whole budget, the swarm is
                // still not quiescent. Reaching quiescence exactly on the final
                // tick (before accumulating idle_grace_ticks consecutive idles)
                // is success, not exhaustion -- the previous code erred here even
                // when the swarm had actually settled.
                if !quiesced && !swarm_quiescent(&ctx)? {
                    return Err(TrelaneError::msg(
                        "pump watch exhausted max_ticks before the swarm became quiescent",
                    ));
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
                commands::cmd_redomain(&ctx, agent, writable, &[], desc.as_deref())?;
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
            ScenarioStep::AssertFileExists {
                explanation: _,
                path,
            } => {
                let abs = ctx.root.join(path);
                if !abs.exists() {
                    return Err(TrelaneError::msg(format!(
                        "scenario assertion failed: expected file to exist but it does not: {}",
                        path
                    )));
                }
            }
            ScenarioStep::AssertFileContains {
                explanation: _,
                path,
                contains,
            } => {
                let abs = ctx.root.join(path);
                let contents = fs::read_to_string(&abs).map_err(|e| {
                    TrelaneError::msg(format!(
                        "scenario assertion failed: cannot read {} for substring check: {e}",
                        path
                    ))
                })?;
                if !contents.contains(contains.as_str()) {
                    return Err(TrelaneError::msg(format!(
                        "scenario assertion failed: {} does not contain expected substring {:?} \
                         (file has {} bytes)",
                        path,
                        contains,
                        contents.len()
                    )));
                }
            }
            ScenarioStep::AssertTaskState {
                explanation: _,
                task_id,
                state,
            } => {
                let task = crate::store::get_task(&ctx.conn, task_id)?.ok_or_else(|| {
                    TrelaneError::msg(format!(
                        "scenario assertion failed: task '{task_id}' not found"
                    ))
                })?;
                if task.state.as_str() != state.as_str() {
                    return Err(TrelaneError::msg(format!(
                        "scenario assertion failed: task '{task_id}' expected state '{state}' \
                         but is '{}'",
                        task.state.as_str()
                    )));
                }
            }
            ScenarioStep::AssertAgentState {
                explanation: _,
                agent,
                state,
            } => {
                let status = crate::squire::agent_activity_status(&ctx, agent)?;
                if status.state.as_str() != state.as_str() {
                    return Err(TrelaneError::msg(format!(
                        "scenario assertion failed: agent '{agent}' expected activity state \
                         '{state}' but is '{}' ({})",
                        status.state.as_str(),
                        status.reason
                    )));
                }
            }
            ScenarioStep::BiplaneDescribe {
                explanation: _,
                describe_path,
            } => {
                // Resolve the describe path: absolute as-is, otherwise
                // relative to the scenario file's parent (so a fixture can
                // reference a sibling *.describe.json by bare filename).
                let p = std::path::Path::new(describe_path);
                let resolved = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    match scenario_path.and_then(|sp| sp.parent()) {
                        Some(base) => base.join(p),
                        None => {
                            return Err(TrelaneError::msg(format!(
                                "BiplaneDescribe step cannot resolve relative path '{describe_path}' \
                                 without a scenario file path (in-memory scenarios must use an \
                                 absolute describe_path)"
                            )));
                        }
                    }
                };
                let desc = crate::biplane::load_project_description(&resolved)?;
                let added = crate::biplane::apply_description_to_session(&ctx, &desc)?;
                println!(
                    "[testing] biplane-describe applied '{}' -> provisioned {} agent(s)/task(s)",
                    resolved.display(),
                    added
                );
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
            ScenarioMode::Bench => "bench".to_string(),
        },
    })
}

fn step_name(step: &ScenarioStep) -> &'static str {
    match step {
        ScenarioStep::Send { .. } => "send",
        ScenarioStep::Park { .. } => "park",
        ScenarioStep::Pump { .. } => "pump",
        ScenarioStep::Wake { .. } => "wake",
        ScenarioStep::PumpWatch { .. } => "pump-watch",
        ScenarioStep::ClaimExpectDenied { .. } => "claim-expect-denied",
        ScenarioStep::Redomain { .. } => "redomain",
        ScenarioStep::AssertNoDeadlock { .. } => "assert-no-deadlock",
        ScenarioStep::AssertParkedCount { .. } => "assert-parked-count",
        ScenarioStep::AssertFileExists { .. } => "assert-file-exists",
        ScenarioStep::AssertFileContains { .. } => "assert-file-contains",
        ScenarioStep::AssertTaskState { .. } => "assert-task-state",
        ScenarioStep::AssertAgentState { .. } => "assert-agent-state",
        ScenarioStep::BiplaneDescribe { .. } => "biplane-describe",
    }
}

fn step_explanation(step: &ScenarioStep) -> &str {
    match step {
        ScenarioStep::Send { explanation, .. }
        | ScenarioStep::Park { explanation, .. }
        | ScenarioStep::Pump { explanation, .. }
        | ScenarioStep::Wake { explanation, .. }
        | ScenarioStep::PumpWatch { explanation, .. }
        | ScenarioStep::ClaimExpectDenied { explanation, .. }
        | ScenarioStep::Redomain { explanation, .. }
        | ScenarioStep::AssertNoDeadlock { explanation }
        | ScenarioStep::AssertParkedCount { explanation, .. }
        | ScenarioStep::AssertFileExists { explanation, .. }
        | ScenarioStep::AssertFileContains { explanation, .. }
        | ScenarioStep::AssertTaskState { explanation, .. }
        | ScenarioStep::AssertAgentState { explanation, .. }
        | ScenarioStep::BiplaneDescribe { explanation, .. } => explanation,
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

pub(crate) fn swarm_quiescent(ctx: &Context) -> Result<bool> {
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

/// Resolve the launcher template a Pump/PumpWatch step should pass to
/// `squire::tick`. In Stub mode we always fall back to the token-free stub
/// launcher when nothing was explicitly provided. In Interactive mode we never
/// force the stub -- an empty string means "no override", so `squire::tick` will
/// fall through to each agent's own `launcher_agent` -> `launcher.profiles`
/// resolution in `cmd_wake` (which is what lets different agents run under
/// different real models). Returns an empty string to mean "no override".
fn resolve_pump_launcher(scenario: &Scenario, launcher_override: Option<&str>) -> String {
    match scenario.mode {
        ScenarioMode::Stub => launcher_override
            .map(str::to_string)
            .unwrap_or_else(default_stub_launcher),
        ScenarioMode::Interactive => launcher_override
            .map(str::to_string)
            .or_else(|| scenario.launcher.clone())
            .unwrap_or_default(),
        ScenarioMode::Bench => launcher_override.map(str::to_string).unwrap_or_default(),
    }
}

/// Count how many registered agents currently have a running lock.
fn count_running(ctx: &Context) -> usize {
    crate::store::list_agents(&ctx.conn)
        .map(|ags| {
            ags.iter()
                .filter(|a| commands::is_running(&ctx.conn, a).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
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
        "TRELANE_TESTING_WORKER=1".to_string(),
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
    std::process::Command::new("tmux")
        .args(["select-pane", "-t", &controller_pane, "-T", "controller"])
        .status()?;

    // Write the per-session root marker BEFORE binding the diagnostic keys, since
    // those bindings read it at trigger time.
    std::fs::write(
        format!("/tmp/trelane-{}-root", session_name),
        ctx.root.display().to_string(),
    )?;

    crate::splash::set_session_status(
        &session_name,
        &scenario.name,
        &crate::splash::SessionState::Idle,
    )?;
    crate::splash::setup_session_ui(&session_name, &ctx.config.ui)?;

    let mut pane_ids = Vec::new();
    if !scenario.agents.is_empty() {
        let first = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-h",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &controller_pane,
            ])
            .output()?;
        if !first.status.success() {
            return Err(TrelaneError::msg(
                "failed to create first tmux pane for interactive scenario",
            ));
        }
        pane_ids.push(String::from_utf8_lossy(&first.stdout).trim().to_string());
    }
    for _ in 1..scenario.agents.len() {
        let output = std::process::Command::new("tmux")
            .args([
                "split-window",
                "-v",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &pane_ids[0],
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
        std::process::Command::new("tmux")
            .args(["select-pane", "-t", pane_id, "-T", &agent.name])
            .status()?;
        crate::splash::send_splash_to_pane(
            pane_id,
            &agent.name,
            "interactive test bootstrap",
            &ctx.root.display().to_string(),
        )?;
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
pub(crate) fn bench_test_ctx(temp: &tempfile::TempDir) -> Context {
    let root = temp.path().to_path_buf();
    let db_path = root.join(".trelane").join("trelane.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let conn = crate::db::open(&db_path).unwrap();
    Context {
        root,
        conn,
        config: crate::models::Config::default(),
    }
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
        assert_eq!(
            step_name(&ScenarioStep::AssertFileExists {
                explanation: "x".to_string(),
                path: "src/a.rs".to_string()
            }),
            "assert-file-exists"
        );
        assert_eq!(
            step_name(&ScenarioStep::AssertFileContains {
                explanation: "x".to_string(),
                path: "src/a.rs".to_string(),
                contains: "fn main".to_string()
            }),
            "assert-file-contains"
        );
        assert_eq!(
            step_name(&ScenarioStep::AssertTaskState {
                explanation: "x".to_string(),
                task_id: "t1".to_string(),
                state: "done".to_string()
            }),
            "assert-task-state"
        );
        assert_eq!(
            step_name(&ScenarioStep::AssertAgentState {
                explanation: "x".to_string(),
                agent: "alpha".to_string(),
                state: "idle".to_string()
            }),
            "assert-agent-state"
        );
    }

    /// The new Assert* variants deserialize from scenario JSON with the
    /// expected fields. This is the parse contract the fixture files rely on.
    #[test]
    fn load_scenario_parses_assert_steps() {
        let json = r#"{
          "name": "asserts",
          "description": "demo assert scenario",
          "project": { "files": [{ "path": "README.md", "contents": "hi" }] },
          "agents": [{ "name": "alpha", "description": "ui", "writable": ["src/**"] }],
          "steps": [
            { "type": "AssertFileExists", "explanation": "a", "path": "README.md" },
            { "type": "AssertFileContains", "explanation": "b", "path": "README.md", "contains": "hi" },
            { "type": "AssertAgentState", "explanation": "c", "agent": "alpha", "state": "idle" }
          ]
        }"#;
        let scenario: Scenario = serde_json::from_str(json).unwrap();
        assert_eq!(scenario.steps.len(), 3);
        match &scenario.steps[0] {
            ScenarioStep::AssertFileExists { path, .. } => assert_eq!(path, "README.md"),
            other => panic!("expected AssertFileExists, got {:?}", other),
        }
        match &scenario.steps[1] {
            ScenarioStep::AssertFileContains { path, contains, .. } => {
                assert_eq!(path, "README.md");
                assert_eq!(contains, "hi");
            }
            other => panic!("expected AssertFileContains, got {:?}", other),
        }
        match &scenario.steps[2] {
            ScenarioStep::AssertAgentState { agent, state, .. } => {
                assert_eq!(agent, "alpha");
                assert_eq!(state, "idle");
            }
            other => panic!("expected AssertAgentState, got {:?}", other),
        }
    }

    /// End-to-end through run_once (the real scenario runner path): a Stub
    /// scenario with AssertFileExists/AssertFileContains against a
    /// project.files entry passes, and AssertAgentState passes for an idle
    /// freshly-registered agent. Verifies the new steps execute correctly
    /// inside the step loop against a real sandbox.
    #[test]
    fn run_once_passes_assert_steps_against_intact_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let scenario = Scenario {
            name: "assert-pass".to_string(),
            description: "asserts pass against hand-placed files".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            project: ScenarioProject {
                files: vec![
                    ScenarioFile {
                        path: "README.md".to_string(),
                        contents: "# Demo\nfn main placeholder.\n".to_string(),
                    },
                    ScenarioFile {
                        path: "src/lib.rs".to_string(),
                        contents: "pub fn answer() -> u8 { 42 }\n".to_string(),
                    },
                ],
            },
            agents: vec![ScenarioAgent {
                name: "alpha".to_string(),
                description: "ui".to_string(),
                writable: vec!["src/**".to_string()],
                forbidden_write: vec![],
                launcher_agent: None,
            }],
            steps: vec![
                ScenarioStep::AssertFileExists {
                    explanation: "README present".to_string(),
                    path: "README.md".to_string(),
                },
                ScenarioStep::AssertFileContains {
                    explanation: "README has the demo heading".to_string(),
                    path: "README.md".to_string(),
                    contains: "# Demo".to_string(),
                },
                ScenarioStep::AssertFileContains {
                    explanation: "lib has the answer fn".to_string(),
                    path: "src/lib.rs".to_string(),
                    contains: "fn answer".to_string(),
                },
                ScenarioStep::AssertAgentState {
                    explanation: "alpha is idle before any work".to_string(),
                    agent: "alpha".to_string(),
                    state: "idle".to_string(),
                },
            ],
            metrics: vec![],
        };
        let report = run_once(&scenario, 1, &sandbox, None, None).unwrap();
        assert_eq!(report.result, "ok", "run should pass all asserts");
    }

    /// AssertFileExists on a missing path must fail the run -- this is the
    /// floor assertion's whole point: a run that produced nothing cannot
    /// pass. Confirms the assertion errors propagate out of run_once
    /// rather than being silently swallowed.
    #[test]
    fn run_once_fails_when_assert_file_exists_targets_missing_path() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let scenario = Scenario {
            name: "assert-fail".to_string(),
            description: "missing file fails the run".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            project: ScenarioProject {
                files: vec![ScenarioFile {
                    path: "README.md".to_string(),
                    contents: "hi".to_string(),
                }],
            },
            agents: vec![ScenarioAgent {
                name: "alpha".to_string(),
                description: "ui".to_string(),
                writable: vec!["src/**".to_string()],
                forbidden_write: vec![],
                launcher_agent: None,
            }],
            steps: vec![ScenarioStep::AssertFileExists {
                explanation: "nonexistent file must fail".to_string(),
                path: "src/never_created.rs".to_string(),
            }],
            metrics: vec![],
        };
        let err = run_once(&scenario, 1, &sandbox, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("src/never_created.rs"),
            "error names the missing path: {msg}"
        );
        assert!(
            msg.contains("but it does not"),
            "error says the file is absent: {msg}"
        );
    }

    /// AssertFileContains on a present file with the wrong substring must
    /// fail. Catches the agent that wrote a placeholder with no real content.
    #[test]
    fn run_once_fails_when_assert_file_contains_misses() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let scenario = Scenario {
            name: "assert-contains-fail".to_string(),
            description: "wrong substring fails".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            project: ScenarioProject {
                files: vec![ScenarioFile {
                    path: "src/lib.rs".to_string(),
                    contents: "// nothing here\n".to_string(),
                }],
            },
            agents: vec![ScenarioAgent {
                name: "alpha".to_string(),
                description: "ui".to_string(),
                writable: vec!["src/**".to_string()],
                forbidden_write: vec![],
                launcher_agent: None,
            }],
            steps: vec![ScenarioStep::AssertFileContains {
                explanation: "must contain the real impl".to_string(),
                path: "src/lib.rs".to_string(),
                contains: "fn answer".to_string(),
            }],
            metrics: vec![],
        };
        let err = run_once(&scenario, 1, &sandbox, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("fn answer"),
            "error names expected substring: {msg}"
        );
        assert!(msg.contains("src/lib.rs"), "error names the file: {msg}");
    }

    /// AssertTaskState on an unknown task must fail the run with the task id
    /// in the message -- catches a scenario that asserts against a task the
    /// setup never created.
    #[test]
    fn run_once_fails_when_assert_task_state_targets_unknown_task() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let scenario = Scenario {
            name: "assert-task-fail".to_string(),
            description: "unknown task fails".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            project: ScenarioProject {
                files: vec![ScenarioFile {
                    path: "README.md".to_string(),
                    contents: "hi".to_string(),
                }],
            },
            agents: vec![ScenarioAgent {
                name: "alpha".to_string(),
                description: "ui".to_string(),
                writable: vec!["src/**".to_string()],
                forbidden_write: vec![],
                launcher_agent: None,
            }],
            steps: vec![ScenarioStep::AssertTaskState {
                explanation: "unknown task must fail".to_string(),
                task_id: "task-nope".to_string(),
                state: "done".to_string(),
            }],
            metrics: vec![],
        };
        let err = run_once(&scenario, 1, &sandbox, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("task-nope"),
            "error names the missing task: {msg}"
        );
        assert!(msg.contains("not found"), "error says not found: {msg}");
    }

    /// BiplaneDescribe step loads a *.describe.json, runs Biplane planning,
    /// and provisions the plan's agents + tasks into the live session.
    /// Verified end-to-end against the real space_rogue.describe.json fixture:
    /// after the step, all four planned domains are registered agents and
    /// their planned_work items exist as Ready tasks owned by the right agent.
    /// This is the Biplane->Trelane handoff the bench framework depends on.
    #[test]
    fn biplane_describe_provisions_agents_and_tasks_from_fixture() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let describe_path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/space_rogue.describe.json"
        );
        let scenario = Scenario {
            name: "biplane-setup".to_string(),
            description: "Biplane describe provisions the session".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            // No hand-authored agents: the BiplaneDescribe step provisions
            // every agent from the plan. This is the Biplane-driven setup path.
            project: ScenarioProject { files: vec![] },
            agents: vec![],
            steps: vec![ScenarioStep::BiplaneDescribe {
                explanation: "provision from space_rogue describe".to_string(),
                describe_path: describe_path.to_string(),
            }],
            metrics: vec![],
        };
        let report = run_once(&scenario, 1, &sandbox, None, None).unwrap();
        assert_eq!(report.result, "ok");

        // Direct ledger checks: the budgeted domains (max_agents=3 in the
        // fixture, so the first three in topo order -- engine, worldgen,
        // combat -- are provisioned; ui is the fourth and is correctly cut by
        // the cap) are registered as agents, and the planned_work items became
        // Ready tasks owned by the right domain agent. These run against the
        // same ctx the step loop used, confirming the provisioning landed in
        // the DB rather than merely that no step errored.
        let ctx = crate::Context::open(Some(&sandbox.join("scenario-run-1"))).unwrap();
        let agents = crate::store::list_agents(&ctx.conn).unwrap();
        for expected in ["engine", "worldgen", "combat"] {
            assert!(
                agents.iter().any(|a| a == expected),
                "planned domain '{expected}' was not registered as an agent; got {agents:?}"
            );
        }
        // The fixture's max_agents=3 caps the plan; ui is the fourth domain
        // in dependency order and is intentionally NOT provisioned. Confirming
        // the cap is honored is part of verifying the Biplane->Trelane
        // handoff matches the plan, not just the description.
        assert!(
            !agents.iter().any(|a| a == "ui"),
            "ui should be cut by the max_agents=3 budget, but was provisioned: {agents:?}"
        );
        let tasks = crate::store::list_tasks(&ctx.conn).unwrap();
        // engine has 2 planned_work, worldgen 1, combat 1 = 4 (ui's 2 are cut
        // by the max_agents=3 budget). The count confirms every budgeted
        // domain's planned_work became a task, not just that some did.
        assert!(
            tasks.len() >= 4,
            "expected at least 4 planned tasks from the three budgeted domains, found {}",
            tasks.len()
        );
        let turn_loop = tasks
            .iter()
            .find(|t| t.subject == "Turn loop and scheduler")
            .expect("planned_work 'Turn loop and scheduler' became a task");
        assert_eq!(turn_loop.owner_agent, "engine");
        assert_eq!(turn_loop.state, crate::models::TaskState::Ready);
    }

    /// BiplaneDescribe with a relative describe_path resolves against the
    /// scenario file's parent directory. Verified by giving run_once a real
    /// scenario_path pointing at tests/ and a bare filename -- the same
    /// resolution a fixture file uses to reference a sibling *.describe.json.
    #[test]
    fn biplane_describe_resolves_relative_path_against_scenario_file() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = temp.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let scenario_path =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dummy.json");
        let scenario = Scenario {
            name: "biplane-rel".to_string(),
            description: "relative describe path resolves against scenario file".to_string(),
            launcher: None,
            mode: ScenarioMode::Stub,
            project: ScenarioProject { files: vec![] },
            agents: vec![],
            steps: vec![ScenarioStep::BiplaneDescribe {
                explanation: "provision via sibling reference".to_string(),
                describe_path: "space_rogue.describe.json".to_string(),
            }],
            metrics: vec![],
        };
        let report = run_once(&scenario, 1, &sandbox, None, Some(&scenario_path)).unwrap();
        assert_eq!(report.result, "ok");
    }
}
