use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "trelane",
    version,
    about = "Squire-based multi-agent coordination protocol",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[arg(long, global = true, help = "project root (default: walk up from cwd)")]
    pub root: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        help = "comma-separated models allowed for this session (e.g. glm-5.2)"
    )]
    pub models: Option<String>,

    #[arg(
        long = "max-agents",
        global = true,
        help = "maximum number of agents to spawn"
    )]
    pub max_agents: Option<u32>,

    #[arg(
        long = "with-biplane",
        global = true,
        help = "run Biplane analysis before launching agents to determine domains"
    )]
    pub with_biplane: bool,

    #[arg(
        long,
        global = true,
        help = "run a full usage scenario from a test file"
    )]
    pub testing: Option<PathBuf>,

    #[arg(long = "testing-runs", global = true, help = "number of scenario runs")]
    pub testing_runs: Option<u32>,

    #[arg(
        long = "testing-report",
        global = true,
        help = "path to JSONL report output"
    )]
    pub testing_report: Option<PathBuf>,

    #[arg(
        long = "testing-sandbox-root",
        global = true,
        help = "sandbox root for scenario runs"
    )]
    pub testing_sandbox_root: Option<PathBuf>,

    #[arg(
        long = "testing-launcher",
        global = true,
        help = "launcher template override for testing squires"
    )]
    pub testing_launcher: Option<String>,

    #[arg(
        long,
        global = true,
        help = "comma-separated agents/models to enable for this session"
    )]
    pub agents: Option<String>,

    #[arg(
        long = "no-agents",
        global = true,
        help = "comma-separated agents/models to disable for this session"
    )]
    pub no_agents: Option<String>,

    #[arg(
        value_name = "PROJECT",
        help = "attach/init a trelane session for an existing project"
    )]
    pub project: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Initialize a new trelane session in a project
    Init {
        #[arg(long)]
        project: Option<PathBuf>,
    },

    /// Attach Trelane to an existing project and inject AGENTS.md instructions
    Attach {
        project: Option<PathBuf>,
        #[arg(long = "no-inject")]
        no_inject: bool,
    },

    /// Register a new agent with a domain
    AddAgent {
        name: String,
        #[arg(long = "writable")]
        writable: Vec<String>,
        #[arg(long = "forbidden-write")]
        forbidden_write: Vec<String>,
        #[arg(long = "desc")]
        desc: Option<String>,
        #[arg(long = "launcher-agent")]
        launcher_agent: Option<String>,
    },

    /// Update an agent's domain and notify peers
    Redomain {
        agent: String,
        #[arg(long = "writable")]
        writable: Vec<String>,
        #[arg(long = "forbidden-write")]
        forbidden_write: Vec<String>,
        #[arg(long = "desc")]
        desc: Option<String>,
    },

    /// Send a signed message to another agent's inbox
    Send {
        #[arg(long = "from")]
        from: String,
        #[arg(long = "to")]
        to: String,
        #[arg(long = "type")]
        msg_type: String,
        #[arg(long = "subject")]
        subject: String,
        #[arg(long = "body", default_value = "")]
        body: String,
        #[arg(long = "re")]
        re: Option<String>,
        #[arg(long = "task")]
        task: Option<String>,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long = "urgency", default_value = "normal")]
        urgency: String,
    },

    /// List unprocessed messages for an agent
    Inbox {
        agent: String,
        #[arg(long = "json")]
        json: bool,
    },

    /// Mark a message as processed
    Ack { agent: String, msg_id: String },

    /// Acquire a file lease (claim)
    Claim {
        agent: String,
        path: String,
        #[arg(long = "ttl")]
        ttl: Option<u64>,
        #[arg(long = "task")]
        task: Option<String>,
        #[arg(long = "grant")]
        grant: Option<String>,
        #[arg(long, help = "active delegation id for a cross-domain claim")]
        delegation: Option<String>,
    },

    /// Release a file lease
    Release {
        agent: String,
        path: String,
        #[arg(long = "force")]
        force: bool,
    },

    /// Park a blocked task as a durable continuation
    Park {
        agent: String,
        #[arg(long = "task")]
        task: Option<String>,
        #[arg(long = "wait-reply")]
        wait_reply: Option<String>,
        #[arg(long = "wait-claim")]
        wait_claim: Option<String>,
        #[arg(long = "waiting-on", required = true)]
        waiting_on: String,
        #[arg(long = "resume-hint", default_value = "")]
        resume_hint: String,
    },

    /// Remove a parked task
    Unpark { task: String },

    /// Show full swarm status
    Status,

    /// Launch an agent process
    Wake {
        agent: String,
        #[arg(long = "why")]
        why: Option<String>,
        #[arg(long = "launcher")]
        launcher: Option<String>,
    },

    /// Store a terminal relaunch target for an agent
    SetLaunchTarget {
        agent: String,
        #[arg(long = "adapter")]
        adapter: String,
        #[arg(long = "target")]
        target: String,
        #[arg(long = "command")]
        command: Option<String>,
        #[arg(long = "tmux-target")]
        tmux_target: Option<String>,
    },

    /// Inject a wake command into an attached terminal session
    Relaunch {
        agent: String,
        #[arg(long = "adapter")]
        adapter: Option<String>,
        #[arg(long = "target")]
        target: Option<String>,
        #[arg(long = "command")]
        command: Option<String>,
    },

    /// Mark an agent as done (release running lock)
    Done { agent: String },

    /// The dutiful squire -- relaunches agents that have a reason to wake
    /// (`prop` and `pump` still work as aliases)
    #[command(alias = "pump", alias = "prop")]
    Squire {
        #[arg(long = "once")]
        once: bool,
        #[arg(long = "watch")]
        watch: bool,
        #[arg(long = "interval")]
        interval: Option<u64>,
        #[arg(long = "launcher")]
        launcher: Option<String>,
        #[arg(
            long = "verbose",
            short = 'v',
            help = "narrate normally-quiet events (e.g. concurrency-budget deferrals)"
        )]
        verbose: bool,
        #[arg(
            long = "max-concurrent",
            help = "override squire.max_concurrent for this run (simultaneous agent ceiling)"
        )]
        max_concurrent: Option<usize>,
    },

    /// Token-free scripted agent for demos and testing
    Stub { agent: String },

    /// Audit an agent's run for out-of-domain file changes
    Audit { agent: String },

    /// Biplane -- analyze the current project and generate a state report
    Biplane {
        #[arg(long = "safe-pocket")]
        safe_pocket_dir: Option<PathBuf>,
        #[arg(
            long = "describe",
            help = "analyze a structured project-description JSON file (offline, no model call)"
        )]
        describe: Option<PathBuf>,
        #[arg(
            long = "next-steps",
            help = "include a phased next-steps schedule (use with --describe)"
        )]
        next_steps: bool,
        #[arg(
            long = "emit-plan",
            help = "write the derived agent plan to .trelane/biplane-plan.json (use with --describe)"
        )]
        emit_plan: bool,
        #[arg(
            long = "interactive",
            help = "interactively choose domains and agent assignment, then optionally apply"
        )]
        interactive: bool,
        #[arg(
            long = "ui",
            help = "open the interactive Biplane editor TUI (amber-themed) to review and edit domains"
        )]
        ui: bool,
        #[arg(
            long = "accept-defaults",
            help = "non-interactive: accept all proposed domains and defaults (use with --interactive)"
        )]
        accept_defaults: bool,
        #[arg(long)]
        json: bool,
    },

    /// Show aggregate metrics from OpenTelemetry traces
    Metrics {
        #[arg(long)]
        json: bool,
    },

    /// Rate another agent's run (inter-agent consensus)
    Rate {
        agent: String,
        rating: u8,
        #[arg(long)]
        rationale: String,
        #[arg(long)]
        rater: String,
    },

    /// Interactive diagnostic view for the main Trelane session (TUI)
    Diagnostic,

    /// Inspect or change Trelane configuration values
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Request, offer, grant, deny, or revoke bounded cross-domain help
    Help {
        #[command(subcommand)]
        action: HelpAction,
    },

    /// Inspect and manage the durable work ledger (C1)
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },

    /// Kill all trelane tmux sessions and stop all running agents
    Kill,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print a config value, e.g. `trelane config get squire.max_concurrent`
    Get { key: String },
    /// Set a config value and save it to the global config file
    Set { key: String, value: String },
    /// Explain a config value: what it means, its effective value, and how to change it
    Explain { key: String },
}

#[derive(Subcommand)]
pub enum HelpAction {
    /// Ask a specific helper to assist an owned task
    Request {
        #[arg(long = "from")]
        from: String,
        #[arg(long = "to")]
        to: String,
        #[arg(long)]
        task: String,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long)]
        need: String,
    },
    /// Offer bounded assistance to a specific task owner
    Offer {
        #[arg(long = "from")]
        from: String,
        #[arg(long = "to")]
        to: String,
        #[arg(long)]
        task: String,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long)]
        plan: String,
        #[arg(long)]
        deliverable: String,
        #[arg(long = "allowed-op")]
        allowed_ops: Vec<String>,
    },
    /// Accept and optionally narrow a pending help offer
    Accept {
        id: String,
        #[arg(long)]
        by: String,
        #[arg(long = "path")]
        paths: Vec<String>,
        #[arg(long = "allowed-op")]
        allowed_ops: Vec<String>,
        #[arg(long)]
        ttl: u64,
    },
    /// Deny a pending help offer
    Deny {
        id: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
    },
    /// Revoke an offered, active, or submitted delegation
    Revoke {
        id: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
pub enum WorkAction {
    /// List tasks in the ledger (optionally filtered by owner and/or state)
    List {
        #[arg(long)]
        owner: Option<String>,
        #[arg(
            long,
            help = "filter by state: draft|ready|active|blocked|review|done|cancelled"
        )]
        state: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long, help = "only open, non-owned tasks accepting assistance")]
        assistable: bool,
        #[arg(long, help = "prospective helper; excludes their own tasks")]
        agent: Option<String>,
    },
    /// Show a single task with its assignments, delegations, and reviews
    Show { id: String },
    /// Add a task to the ledger
    Add {
        #[arg(long)]
        owner: String,
        #[arg(long)]
        subject: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long, default_value = "normal", help = "low|normal|high|critical")]
        priority: String,
        #[arg(long = "path", help = "path-scope glob (repeatable)")]
        paths: Vec<String>,
        #[arg(long = "accept", help = "acceptance criterion (repeatable)")]
        acceptance: Vec<String>,
        #[arg(
            long = "blocked-by",
            help = "task id this task depends on (repeatable)"
        )]
        blocked_by: Vec<String>,
        #[arg(
            long = "parallelism",
            default_value_t = 1,
            help = "how many agents may work it at once"
        )]
        parallelism: u32,
        #[arg(
            long = "assist",
            default_value = "open",
            help = "assist policy: open|solo"
        )]
        assist: String,
    },
    /// Submit a validated Git commit for owner/reviewer review
    Submit {
        task: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        tests: String,
    },
    /// Review a helper submission
    #[command(group(
        clap::ArgGroup::new("decision")
            .required(true)
            .multiple(false)
            .args(["accept", "request_changes", "reject"])
    ))]
    Review {
        task: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        accept: bool,
        #[arg(long = "request-changes")]
        request_changes: bool,
        #[arg(long)]
        reject: bool,
        #[arg(long, default_value = "")]
        notes: String,
    },
}
