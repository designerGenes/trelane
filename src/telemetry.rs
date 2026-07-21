//! OpenTelemetry-valid OTLP JSON trace exporter.
//!
//! Writes spans to `.trelane/traces/<session-id>.jsonl` in OTLP JSON
//! format. Each line is a complete OTLP `ExportTraceServiceRequest` with
//! a single span. Files can be ingested by Jaeger, Tempo, or any
//! OTLP-compatible collector.
//!
//! Span types produced:
//! - `agent.run` -- one per wake/done cycle (duration, code diff, reason)
//! - `agent.wait` -- one per park/unpark cycle (sleep duration, waiting_on)
//! - `squire.tick` -- one per squire tick (agents launched, cycle detected)
//! - `agent.rate` -- inter-agent consensus rating (optional)

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// OTLP trace ID (32 hex chars).
pub type TraceId = String;
/// OTLP span ID (16 hex chars).
pub type SpanId = String;

/// A single OTLP span, serialized as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpSpan {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub kind: u32, // 1=INTERNAL, 2=SERVER, 3=CLIENT
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub attributes: Vec<OtlpAttribute>,
    pub status: OtlpStatus,
    pub resource: OtlpResource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpAttribute {
    pub key: String,
    pub value: OtlpValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OtlpValue {
    StringValue(String),
    IntValue(i64),
    DoubleValue(f64),
    BoolValue(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpStatus {
    pub code: u32, // 0=UNSET, 1=OK, 2=ERROR
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtlpResource {
    pub attributes: Vec<OtlpAttribute>,
}

/// Manages trace output for a session.
pub struct Tracer {
    trace_dir: PathBuf,
    trace_id: TraceId,
    project_root: String,
    session_name: String,
}

impl Tracer {
    /// Create a tracer that writes to `<trelane_dir>/traces/`.
    pub fn new(trelane_dir: &Path, project_root: &str, session_name: &str) -> Result<Self> {
        let trace_dir = trelane_dir.join("traces");
        fs::create_dir_all(&trace_dir)?;
        // Persist one trace id per project instead of minting a fresh
        // random one per Tracer construction -- every call site below
        // constructs a Tracer per span (via `ephemeral`), so a random id
        // here meant an agent.run span and its own agent.wait span never
        // shared a trace, and a rate span's parent_span_id pointed into a
        // trace it wasn't part of. See get_or_create_trace_id.
        let trace_id = get_or_create_trace_id(&trace_dir)?;
        Ok(Self {
            trace_dir,
            trace_id,
            project_root: project_root.to_string(),
            session_name: session_name.to_string(),
        })
    }

    /// Create a tracer with no session (for one-off spans like `trelane metrics`).
    pub fn ephemeral(trelane_dir: &Path, project_root: &str) -> Result<Self> {
        Self::new(trelane_dir, project_root, "ephemeral")
    }

    /// Record an agent run span (wake -> done).
    #[allow(clippy::too_many_arguments)]
    pub fn record_agent_run(
        &self,
        agent: &str,
        reason: &str,
        start_ns: u64,
        end_ns: u64,
        files_changed: usize,
        lines_added: usize,
        lines_removed: usize,
        messages_processed: usize,
        messages_sent: usize,
        exit_status: &str,
    ) -> Result<SpanId> {
        let span_id = generate_span_id();
        let span = OtlpSpan {
            trace_id: self.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: None,
            name: format!("agent.run:{agent}"),
            kind: 2, // SERVER
            start_time_unix_nano: start_ns,
            end_time_unix_nano: end_ns,
            attributes: vec![
                attr_str("agent.name", agent),
                attr_str("agent.reason", reason),
                attr_str("agent.exit_status", exit_status),
                attr_int("agent.files_changed", files_changed as i64),
                attr_int("agent.lines_added", lines_added as i64),
                attr_int("agent.lines_removed", lines_removed as i64),
                attr_int("agent.messages_processed", messages_processed as i64),
                attr_int("agent.messages_sent", messages_sent as i64),
                attr_str("project.root", &self.project_root),
                attr_str("session.name", &self.session_name),
            ],
            status: OtlpStatus {
                code: if exit_status == "done" { 1 } else { 2 },
                message: exit_status.to_string(),
            },
            resource: self.resource(),
        };
        self.write_span(&span)?;
        Ok(span_id)
    }

    /// Record an agent wait span (park -> unpark/resume).
    #[allow(clippy::too_many_arguments)]
    pub fn record_agent_wait(
        &self,
        agent: &str,
        task_id: &str,
        waiting_on: &str,
        wait_type: &str,
        start_ns: u64,
        end_ns: u64,
        satisfied: bool,
    ) -> Result<SpanId> {
        let span_id = generate_span_id();
        let duration_ms = (end_ns - start_ns) / 1_000_000;
        let span = OtlpSpan {
            trace_id: self.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: None,
            name: format!("agent.wait:{agent}"),
            kind: 1, // INTERNAL
            start_time_unix_nano: start_ns,
            end_time_unix_nano: end_ns,
            attributes: vec![
                attr_str("agent.name", agent),
                attr_str("wait.task_id", task_id),
                attr_str("wait.waiting_on", waiting_on),
                attr_str("wait.type", wait_type),
                attr_int("wait.duration_ms", duration_ms as i64),
                attr_bool("wait.satisfied", satisfied),
                attr_str("project.root", &self.project_root),
                attr_str("session.name", &self.session_name),
            ],
            status: OtlpStatus {
                code: if satisfied { 1 } else { 2 },
                message: if satisfied {
                    "resolved".to_string()
                } else {
                    "timeout_or_broken".to_string()
                },
            },
            resource: self.resource(),
        };
        self.write_span(&span)?;
        Ok(span_id)
    }

    /// Record a squire tick span.
    pub fn record_squire_tick(
        &self,
        agents_launched: usize,
        agents_running: usize,
        cycle_detected: bool,
        start_ns: u64,
        end_ns: u64,
    ) -> Result<SpanId> {
        let span_id = generate_span_id();
        let span = OtlpSpan {
            trace_id: self.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: None,
            name: "squire.tick".to_string(),
            kind: 1, // INTERNAL
            start_time_unix_nano: start_ns,
            end_time_unix_nano: end_ns,
            attributes: vec![
                attr_int("squire.agents_launched", agents_launched as i64),
                attr_int("squire.agents_running", agents_running as i64),
                attr_bool("squire.cycle_detected", cycle_detected),
                attr_str("project.root", &self.project_root),
                attr_str("session.name", &self.session_name),
            ],
            status: OtlpStatus {
                code: 1,
                message: String::new(),
            },
            resource: self.resource(),
        };
        self.write_span(&span)?;
        Ok(span_id)
    }

    /// Record an inter-agent rating span.
    pub fn record_rating(
        &self,
        rater: &str,
        rated_agent: &str,
        rated_run_span_id: &str,
        rating: u8,
        rationale: &str,
    ) -> Result<SpanId> {
        let span_id = generate_span_id();
        let now = now_nanos();
        let span = OtlpSpan {
            trace_id: self.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: Some(rated_run_span_id.to_string()),
            name: format!("agent.rate:{rated_agent}"),
            kind: 1,
            start_time_unix_nano: now,
            end_time_unix_nano: now,
            attributes: vec![
                attr_str("rate.rater", rater),
                attr_str("rate.rated_agent", rated_agent),
                attr_int("rate.score", rating as i64),
                attr_str("rate.rationale", rationale),
                attr_str("project.root", &self.project_root),
            ],
            status: OtlpStatus {
                code: 1,
                message: String::new(),
            },
            resource: self.resource(),
        };
        self.write_span(&span)?;
        Ok(span_id)
    }

    /// Record a flat INTERNAL span with a caller-provided name and string
    /// attributes. Used for `squire.wake_candidate` ("why didn't X wake") and
    /// the `di.request/approve/veto/resolve` audit trail (4A/4C). As with
    /// every emitter here, callers ignore the Result (R16).
    pub fn record_event(
        &self,
        name: &str,
        attributes: &[(&str, &str)],
        start_ns: u64,
        end_ns: u64,
    ) -> Result<SpanId> {
        let span_id = generate_span_id();
        let mut attrs: Vec<OtlpAttribute> =
            attributes.iter().map(|(k, v)| attr_str(k, v)).collect();
        attrs.push(attr_str("project.root", &self.project_root));
        attrs.push(attr_str("session.name", &self.session_name));
        let span = OtlpSpan {
            trace_id: self.trace_id.clone(),
            span_id: span_id.clone(),
            parent_span_id: None,
            name: name.to_string(),
            kind: 1, // INTERNAL
            start_time_unix_nano: start_ns,
            end_time_unix_nano: end_ns,
            attributes: attrs,
            status: OtlpStatus {
                code: 1,
                message: String::new(),
            },
            resource: self.resource(),
        };
        self.write_span(&span)?;
        Ok(span_id)
    }

    fn resource(&self) -> OtlpResource {
        OtlpResource {
            attributes: vec![
                attr_str("service.name", "trelane"),
                attr_str("service.version", env!("CARGO_PKG_VERSION")),
                attr_str("project.root", &self.project_root),
                attr_str("session.name", &self.session_name),
            ],
        }
    }

    fn write_span(&self, span: &OtlpSpan) -> Result<()> {
        let path = self.trace_dir.join(format!("{}.jsonl", self.session_name));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let json = serde_json::to_string(span)?;
        writeln!(file, "{json}")?;
        Ok(())
    }

    /// Read all spans from a trace file.
    pub fn read_spans(trace_file: &Path) -> Result<Vec<OtlpSpan>> {
        let text = fs::read_to_string(trace_file)?;
        let mut spans = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<OtlpSpan>(line) {
                Ok(span) => spans.push(span),
                Err(_) => continue,
            }
        }
        Ok(spans)
    }

    /// Read all spans from all trace files in a directory.
    pub fn read_all_spans(trace_dir: &Path) -> Result<Vec<OtlpSpan>> {
        let mut spans = Vec::new();
        if !trace_dir.is_dir() {
            return Ok(spans);
        }
        for entry in fs::read_dir(trace_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                spans.extend(Self::read_spans(&path)?);
            }
        }
        Ok(spans)
    }
}

fn attr_str(key: &str, val: &str) -> OtlpAttribute {
    OtlpAttribute {
        key: key.to_string(),
        value: OtlpValue::StringValue(val.to_string()),
    }
}

fn attr_int(key: &str, val: i64) -> OtlpAttribute {
    OtlpAttribute {
        key: key.to_string(),
        value: OtlpValue::IntValue(val),
    }
}

fn attr_bool(key: &str, val: bool) -> OtlpAttribute {
    OtlpAttribute {
        key: key.to_string(),
        value: OtlpValue::BoolValue(val),
    }
}

fn generate_trace_id() -> TraceId {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex_encode(&bytes)
}

/// Read the project's persistent trace id, creating one on first use.
///
/// Stored as a plain file next to the trace jsonl files rather than in the
/// database, so every `Tracer::new`/`ephemeral` call site -- none of which
/// currently thread a `Connection` through -- can reach it with no
/// signature changes. A create-if-absent write guards the common
/// multi-process race (two `trelane` CLI invocations starting near
/// simultaneously): the loser's `create_new` fails and it falls back to
/// reading what the winner wrote.
fn get_or_create_trace_id(trace_dir: &Path) -> Result<TraceId> {
    let path = trace_dir.join(".trace_id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let fresh = generate_trace_id();
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(fresh.as_bytes())?;
            Ok(fresh)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Lost the race -- another process created it first; use theirs.
            Ok(fs::read_to_string(&path)?.trim().to_string())
        }
        Err(e) => Err(e.into()),
    }
}

/// Read the project's persistent trace id WITHOUT creating one. Returns
/// `None` if the trace id file does not yet exist -- the case where a session
/// has not yet emitted a telemetry span (so no `.trace_id` file has been
/// written). This is the read-only entry point the story-events ledger
/// calls so a freshly-appended event shares the same trace_id as OTLP spans
/// when one exists, and carries a NULL trace_id when one doesn't.
///
/// (TUI-005 / story-ledger spec: "Give every ledger event the SAME session
/// trace_id used by telemetry (read from `<trelane_dir>/traces/.trace_id`,
/// created by the telemetry fix's get_or_create_trace_id).")
pub fn current_trace_id(trelane_dir: &Path) -> Option<String> {
    let path = trelane_dir.join("traces").join(".trace_id");
    match fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(_) => None,
    }
}

fn generate_span_id() -> SpanId {
/// Count git diff lines for the project root.
/// Returns (files_changed, lines_added, lines_removed).
pub fn git_diff_stats(root: &Path) -> (usize, usize, usize) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat"])
        .output();
    let Ok(output) = output else {
        return (0, 0, 0);
    };
    if !output.status.success() {
        return (0, 0, 0);
    }
    parse_numstat(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `git diff --numstat` output into (files_changed, lines_added,
/// lines_removed). Shared by `git_diff_stats` and `diff_trees` so the two
/// diff sources (against HEAD, and between two snapshotted trees) can't
/// silently drift apart in how they count.
fn parse_numstat(text: &str) -> (usize, usize, usize) {
    let mut files = 0;
    let mut added = 0;
    let mut removed = 0;
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        files += 1;
        if parts[0] != "-" {
            added += parts[0].parse::<usize>().unwrap_or(0);
        }
        if parts[1] != "-" {
            removed += parts[1].parse::<usize>().unwrap_or(0);
        }
    }
    (files, added, removed)
}

/// Snapshot the working tree as a git tree object, without touching the
/// repository's real index, HEAD, or any ref. Uses a throwaway index file
/// (via `GIT_INDEX_FILE`) so it's safe to call mid-run, alongside an agent
/// that's actively editing files, without disturbing the user's actual
/// staging area. Returns the tree SHA, or `None` on a non-git project or if
/// the git plumbing calls fail.
///
/// This exists because `git_diff_stats` can only compare against HEAD, and
/// nothing in the live wake/done path ever commits -- so a HEAD diff reports
/// the cumulative change since the *session* started, not since this *run*
/// started. Snapshotting a tree at wake time and again at done time lets
/// `diff_trees` report exactly this run's delta instead.
pub fn snapshot_tree(root: &Path) -> Option<String> {
    let tmp_index = std::env::temp_dir().join(format!(
        "trelane-idx-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["add", "-A"])
        .output();
    let Ok(add) = add else {
        let _ = fs::remove_file(&tmp_index);
        return None;
    };
    if !add.status.success() {
        let _ = fs::remove_file(&tmp_index);
        return None;
    }
    let write_tree = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["write-tree"])
        .output();
    let _ = fs::remove_file(&tmp_index);
    let Ok(write_tree) = write_tree else {
        return None;
    };
    if !write_tree.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&write_tree.stdout)
        .trim()
        .to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Diff two tree snapshots from `snapshot_tree` for (files_changed,
/// lines_added, lines_removed). Identical trees short-circuit to zeros
/// without shelling out.
pub fn diff_trees(root: &Path, before: &str, after: &str) -> (usize, usize, usize) {
    if before == after {
        return (0, 0, 0);
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat", before, after])
        .output();
    let Ok(output) = output else {
        return (0, 0, 0);
    };
    if !output.status.success() {
        return (0, 0, 0);
    }
    parse_numstat(&String::from_utf8_lossy(&output.stdout))
}
    let Ok(output) = output else {
        return (0, 0, 0);
    };
    if !output.status.success() {
        return (0, 0, 0);
    }
    parse_numstat(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `git diff --numstat` output into (files_changed, lines_added,
/// lines_removed). Shared by `git_diff_stats` and `diff_trees` so the two
/// diff sources (against HEAD, and between two snapshotted trees) can't
/// silently drift apart in how they count.
fn parse_numstat(text: &str) -> (usize, usize, usize) {
    let mut files = 0;
    let mut added = 0;
    let mut removed = 0;
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        files += 1;
        if parts[0] != "-" {
            added += parts[0].parse::<usize>().unwrap_or(0);
        }
        if parts[1] != "-" {
            removed += parts[1].parse::<usize>().unwrap_or(0);
        }
    }
    (files, added, removed)
}

/// Snapshot the working tree as a git tree object, without touching the
/// repository's real index, HEAD, or any ref. Uses a throwaway index file
/// (via `GIT_INDEX_FILE`) so it's safe to call mid-run, alongside an agent
/// that's actively editing files, without disturbing the user's actual
/// staging area. Returns the tree SHA, or `None` on a non-git project or if
/// the git plumbing calls fail.
///
/// This exists because `git_diff_stats` can only compare against HEAD, and
/// nothing in the live wake/done path ever commits -- so a HEAD diff reports
/// the cumulative change since the *session* started, not since this *run*
/// started. Snapshotting a tree at wake time and again at done time lets
/// `diff_trees` report exactly this run's delta instead.
pub fn snapshot_tree(root: &Path) -> Option<String> {
    let tmp_index = std::env::temp_dir().join(format!(
        "trelane-idx-{}-{}",
        std::process::id(),
        now_nanos()
    ));
    let add = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["add", "-A"])
        .output();
    let Ok(add) = add else {
        let _ = fs::remove_file(&tmp_index);
        return None;
    };
    if !add.status.success() {
        let _ = fs::remove_file(&tmp_index);
        return None;
    }
    let write_tree = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .env("GIT_INDEX_FILE", &tmp_index)
        .args(["write-tree"])
        .output();
    let _ = fs::remove_file(&tmp_index);
    let Ok(write_tree) = write_tree else {
        return None;
    };
    if !write_tree.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&write_tree.stdout)
        .trim()
        .to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// Diff two tree snapshots from `snapshot_tree` for (files_changed,
/// lines_added, lines_removed). Identical trees short-circuit to zeros
/// without shelling out.
pub fn diff_trees(root: &Path, before: &str, after: &str) -> (usize, usize, usize) {
    if before == after {
        return (0, 0, 0);
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--numstat", before, after])
        .output();
    let Ok(output) = output else {
        return (0, 0, 0);
    };
    if !output.status.success() {
        return (0, 0, 0);
    }
    parse_numstat(&String::from_utf8_lossy(&output.stdout))
}

/// Aggregate metrics computed from trace spans.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSummary {
    pub total_runs: usize,
    pub total_wait_events: usize,
    pub total_squire_ticks: usize,
    pub total_run_duration_ms: u64,
    pub total_wait_duration_ms: u64,
    pub total_files_changed: usize,
    pub total_lines_added: usize,
    pub total_lines_removed: usize,
    pub total_messages_processed: usize,
    pub total_messages_sent: usize,
    pub total_deadlocks_detected: usize,
    pub per_agent: Vec<AgentMetrics>,
    pub avg_run_duration_ms: f64,
    pub avg_wait_duration_ms: f64,
    pub efficiency_ratio: f64, // run_ms / (run_ms + wait_ms)
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentMetrics {
    pub agent: String,
    pub runs: usize,
    pub wait_events: usize,
    pub run_duration_ms: u64,
    pub wait_duration_ms: u64,
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub messages_processed: usize,
    pub messages_sent: usize,
    pub avg_rating: Option<f64>,
}

/// Compute aggregate metrics from all trace spans in a directory.
pub fn compute_metrics(trace_dir: &Path) -> Result<MetricsSummary> {
    let spans = Tracer::read_all_spans(trace_dir)?;

    let mut agent_map: HashMap<String, AgentMetrics> = HashMap::new();
    let mut total_runs = 0;
    let mut total_wait_events = 0;
    let mut total_squire_ticks = 0;
    let mut total_run_duration_ms = 0u64;
    let mut total_wait_duration_ms = 0u64;
    let mut total_files_changed = 0;
    let mut total_lines_added = 0;
    let mut total_lines_removed = 0;
    let mut total_messages_processed = 0;
    let mut total_messages_sent = 0;
    let mut total_deadlocks_detected = 0;
    let mut ratings: HashMap<String, Vec<u8>> = HashMap::new();

    for span in &spans {
        match span.name.split(':').next() {
            Some("agent.run") => {
                let agent = get_attr_str(&span.attributes, "agent.name").unwrap_or_default();
                let duration_ms = (span.end_time_unix_nano - span.start_time_unix_nano) / 1_000_000;
                let files = get_attr_int(&span.attributes, "agent.files_changed") as usize;
                let added = get_attr_int(&span.attributes, "agent.lines_added") as usize;
                let removed = get_attr_int(&span.attributes, "agent.lines_removed") as usize;
                let msg_proc = get_attr_int(&span.attributes, "agent.messages_processed") as usize;
                let msg_sent = get_attr_int(&span.attributes, "agent.messages_sent") as usize;

                total_runs += 1;
                total_run_duration_ms += duration_ms;
                total_files_changed += files;
                total_lines_added += added;
                total_lines_removed += removed;
                total_messages_processed += msg_proc;
                total_messages_sent += msg_sent;

                let entry = agent_map
                    .entry(agent.clone())
                    .or_insert_with(|| AgentMetrics {
                        agent: agent.clone(),
                        runs: 0,
                        wait_events: 0,
                        run_duration_ms: 0,
                        wait_duration_ms: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        messages_processed: 0,
                        messages_sent: 0,
                        avg_rating: None,
                    });
                entry.runs += 1;
                entry.run_duration_ms += duration_ms;
                entry.files_changed += files;
                entry.lines_added += added;
                entry.lines_removed += removed;
                entry.messages_processed += msg_proc;
                entry.messages_sent += msg_sent;
            }
            Some("agent.wait") => {
                let agent = get_attr_str(&span.attributes, "agent.name").unwrap_or_default();
                let duration_ms = get_attr_int(&span.attributes, "wait.duration_ms") as u64;

                total_wait_events += 1;
                total_wait_duration_ms += duration_ms;

                let entry = agent_map
                    .entry(agent.clone())
                    .or_insert_with(|| AgentMetrics {
                        agent: agent.clone(),
                        runs: 0,
                        wait_events: 0,
                        run_duration_ms: 0,
                        wait_duration_ms: 0,
                        files_changed: 0,
                        lines_added: 0,
                        lines_removed: 0,
                        messages_processed: 0,
                        messages_sent: 0,
                        avg_rating: None,
                    });
                entry.wait_events += 1;
                entry.wait_duration_ms += duration_ms;
            }
            Some("squire.tick") => {
                total_squire_ticks += 1;
                if get_attr_bool(&span.attributes, "squire.cycle_detected") {
                    total_deadlocks_detected += 1;
                }
            }
            Some("agent.rate") => {
                let rated = get_attr_str(&span.attributes, "rate.rated_agent").unwrap_or_default();
                let score = get_attr_int(&span.attributes, "rate.score") as u8;
                ratings.entry(rated).or_default().push(score);
            }
            _ => {}
        }
    }

    // Apply ratings to agents
    for (agent, scores) in &ratings {
        if let Some(entry) = agent_map.get_mut(agent) {
            let avg = scores.iter().sum::<u8>() as f64 / scores.len() as f64;
            entry.avg_rating = Some(avg);
        }
    }

    let mut per_agent: Vec<AgentMetrics> = agent_map.into_values().collect();
    per_agent.sort_by(|a, b| a.agent.cmp(&b.agent));

    let total_active_ms = total_run_duration_ms + total_wait_duration_ms;
    let efficiency_ratio = if total_active_ms > 0 {
        total_run_duration_ms as f64 / total_active_ms as f64
    } else {
        0.0
    };

    Ok(MetricsSummary {
        total_runs,
        total_wait_events,
        total_squire_ticks,
        total_run_duration_ms,
        total_wait_duration_ms,
        total_files_changed,
        total_lines_added,
        total_lines_removed,
        total_messages_processed,
        total_messages_sent,
        total_deadlocks_detected,
        per_agent,
        avg_run_duration_ms: if total_runs > 0 {
            total_run_duration_ms as f64 / total_runs as f64
        } else {
            0.0
        },
        avg_wait_duration_ms: if total_wait_events > 0 {
            total_wait_duration_ms as f64 / total_wait_events as f64
        } else {
            0.0
        },
        efficiency_ratio,
    })
}

fn get_attr_str(attrs: &[OtlpAttribute], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| match &a.value {
            OtlpValue::StringValue(s) => Some(s.clone()),
            _ => None,
        })
}

fn get_attr_int(attrs: &[OtlpAttribute], key: &str) -> i64 {
    attrs
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| match &a.value {
            OtlpValue::IntValue(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0)
}

fn get_attr_bool(attrs: &[OtlpAttribute], key: &str) -> bool {
    attrs
        .iter()
        .find(|a| a.key == key)
        .and_then(|a| match &a.value {
            OtlpValue::BoolValue(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false)
}

/// Get the trace directory for a project.
pub fn trace_dir_for(trelane_dir: &Path) -> PathBuf {
    trelane_dir.join("traces")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_serializes_to_valid_json() {
        let span = OtlpSpan {
            trace_id: "abcdef0123456789abcdef0123456789".to_string(),
            span_id: "0123456789abcdef".to_string(),
            parent_span_id: None,
            name: "agent.run:alpha".to_string(),
            kind: 2,
            start_time_unix_nano: 1000000000,
            end_time_unix_nano: 2000000000,
            attributes: vec![
                attr_str("agent.name", "alpha"),
                attr_int("agent.files_changed", 5),
                attr_bool("test.flag", true),
            ],
            status: OtlpStatus {
                code: 1,
                message: "done".to_string(),
            },
            resource: OtlpResource {
                attributes: vec![attr_str("service.name", "trelane")],
            },
        };
        let json = serde_json::to_string(&span).unwrap();
        assert!(json.contains("trace_id"));
        assert!(json.contains("span_id"));
        assert!(json.contains("agent.name"));
        assert!(json.contains("\"kind\":2"));
    }

    #[test]
    fn metrics_summary_handles_empty() {
        let temp = tempfile::tempdir().unwrap();
        let metrics = compute_metrics(temp.path()).unwrap();
        assert_eq!(metrics.total_runs, 0);
        assert_eq!(metrics.efficiency_ratio, 0.0);
    }

    #[test]
    fn git_diff_stats_returns_zeros_without_git() {
        let temp = tempfile::tempdir().unwrap();
        let (files, added, removed) = git_diff_stats(temp.path());
        assert_eq!(files, 0);
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }
}
