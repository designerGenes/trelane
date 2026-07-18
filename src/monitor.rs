//! Trelane Monitor: the native tabbed session view.
//!
//! One tab per agent plus a Trelane diagnostics tab -- tabs are a
//! Trelane-owned concept rendered by this TUI, not tmux windows, so the agent
//! count can grow without consuming screen real estate. Each agent tab shows
//! live, frequently-updated detail about how that (headless) agent is working:
//! parsed thoughts, tool calls, and text tailed from the agent's run log,
//! plus its activity state and park reason.
//!
//! The live feed works because `cmd_wake` already redirects every agent's
//! stdout to `.trelane/agents/<name>/logs/run-<id>.log`. With a streaming
//! launcher profile (`opencode run --format json --thinking`, or claude-code's
//! `--output-format stream-json`), that file fills with newline-delimited
//! JSON events as the agent works; this module tails and renders them. With a
//! plain-text profile the same feed still works -- unparseable lines render
//! as raw output -- so the monitor is useful regardless of profile.
//!
//! House style: `MonitorState` and the event parser are pure and unit-tested;
//! `run_monitor`/`render` are the thin I/O shell, excluded from tests.

use crate::Context;
use crate::error::Result;
use crate::store;

// ---------------------------------------------------------------- events

/// One displayable event from an agent's run log, normalized across the
/// opencode `--format json` stream, claude-code's `stream-json`, and plain
/// text. The parser is total: any line becomes exactly one of these.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// Model reasoning ("thinking"/"reasoning" parts). The headline feature:
    /// what the agent is thinking while it works.
    Thinking(String),
    /// Assistant-visible text output.
    Text(String),
    /// A tool invocation (name plus a short detail when available).
    ToolUse { name: String, detail: String },
    /// A step/turn boundary with token accounting when present.
    StepFinish { detail: String },
    /// An error event from the harness.
    HarnessError(String),
    /// A line that isn't JSON (plain-text profile) or isn't a recognized
    /// event shape. Shown as-is so nothing is silently dropped.
    Raw(String),
}

impl AgentEvent {
    /// Short lowercase tag for the feed gutter.
    pub fn tag(&self) -> &'static str {
        match self {
            AgentEvent::Thinking(_) => "think",
            AgentEvent::Text(_) => "text",
            AgentEvent::ToolUse { .. } => "tool",
            AgentEvent::StepFinish { .. } => "step",
            AgentEvent::HarnessError(_) => "error",
            AgentEvent::Raw(_) => "raw",
        }
    }

    pub fn body(&self) -> String {
        match self {
            AgentEvent::Thinking(s) | AgentEvent::Text(s) | AgentEvent::Raw(s) => s.clone(),
            AgentEvent::ToolUse { name, detail } => {
                if detail.is_empty() {
                    name.clone()
                } else {
                    format!("{name}: {detail}")
                }
            }
            AgentEvent::StepFinish { detail } => detail.clone(),
            AgentEvent::HarnessError(s) => s.clone(),
        }
    }
}

/// Strip terminal control sequences from text before it can ever reach a
/// rendered `Span`. This is the fix for a real corruption bug: a launcher
/// profile that emits interactive-style output (ANSI color codes, a `\r`-
/// driven spinner redrawing in place with no real `\n`) produces a single
/// unparseable "line" full of raw control bytes. `poll_agent_feed` only
/// splits on `\n`, so that whole blob becomes one `Raw` event -- and
/// crossterm writes a Span's text to the real terminal byte-for-byte, so an
/// un-sanitized ESC or bare CR moves the actual cursor and corrupts the
/// screen (fragmented, overlapping text -- exactly what a raw spinner replay
/// looks like once it reaches a terminal that isn't expecting it).
///
/// Recognized ANSI CSI (`ESC [ ... letter`) and OSC (`ESC ] ... BEL`)
/// sequences are dropped in full, not left as visible bracket/digit debris.
/// Any other C0 control byte (bare CR, stray NL, bell, etc.) becomes a
/// single space, which preserves spacing without ever emitting a byte that
/// can move a cursor.
fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    // CSI: ESC [ <params/intermediates> <final-letter>.
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: ESC ] ... terminated by BEL (or a following ESC,
                    // left for the outer loop to consume on its own turn).
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        if next == '\u{7}' {
                            chars.next();
                            break;
                        }
                        if next == '\u{1b}' {
                            break;
                        }
                        chars.next();
                    }
                }
                _ => {
                    // Unrecognized escape form: drop just the ESC byte so we
                    // never silently swallow real content after it.
                }
            }
            continue;
        }
        if (c as u32) < 0x20 || c as u32 == 0x7f {
            out.push(' ');
            continue;
        }
        out.push(c);
    }
    out
}

impl AgentEvent {
    /// Apply `sanitize` to every string this event owns. Called once, at
    /// parse time -- not on every render -- so the stored representation is
    /// safe for any future consumer, not just the current render function.
    fn sanitized(self) -> Self {
        match self {
            AgentEvent::Thinking(s) => AgentEvent::Thinking(sanitize(&s)),
            AgentEvent::Text(s) => AgentEvent::Text(sanitize(&s)),
            AgentEvent::ToolUse { name, detail } => AgentEvent::ToolUse {
                name: sanitize(&name),
                detail: sanitize(&detail),
            },
            AgentEvent::StepFinish { detail } => AgentEvent::StepFinish {
                detail: sanitize(&detail),
            },
            AgentEvent::HarnessError(s) => AgentEvent::HarnessError(sanitize(&s)),
            AgentEvent::Raw(s) => AgentEvent::Raw(sanitize(&s)),
        }
    }
}

/// Parse one log line into events, sanitized and safe to render. Total:
/// never fails, never drops, never lets a control byte reach the terminal.
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
    parse_line_raw(line)
        .into_iter()
        .map(AgentEvent::sanitized)
        .collect()
}

/// Clip a string to at most `width` characters, appending an ellipsis when it
/// was cut. Truncation is by `char`, not byte, so it never splits a multibyte
/// character. This is what keeps the agent feed to exactly ONE terminal row
/// per event: a body longer than the pane can't wrap onto extra rows, which
/// is the root cause of the leftover-fragment artifacts (a long line wraps to
/// several rows, then when the feed scrolls, the vacated wrap-rows aren't
/// reliably cleared). One row per event makes rendered-rows == event-count, so
/// the height-based windowing below is exact.
pub fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let char_count = s.chars().count();
    if char_count <= width {
        return s.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Recognized shapes, in order of attempt:
/// - opencode `--format json`: `{"type": "...", "part": {...}}` where
///   part.type distinguishes text/thinking/tool; top-level types include
///   step_start, text, tool_use, tool_result, step_finish, error, and
///   message.part.updated (whose part.type may be thinking/reasoning).
/// - claude-code `stream-json`: `{"type":"assistant","message":{"content":
///   [{"type":"text"|"thinking"|"tool_use",...}]}}` plus system/result lines.
/// - anything else: Raw (subject to `sanitize` via the public `parse_line`).
fn parse_line_raw(line: &str) -> Vec<AgentEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return vec![AgentEvent::Raw(trimmed.to_string())];
    };
    let Some(t) = v.get("type").and_then(|t| t.as_str()) else {
        return vec![AgentEvent::Raw(trimmed.to_string())];
    };

    match t {
        // ---- opencode stream ----
        "text" => part_text(&v)
            .map(|s| vec![AgentEvent::Text(s)])
            .unwrap_or_default(),
        "message.part.updated" => {
            let part_type = v
                .pointer("/part/type")
                .and_then(|p| p.as_str())
                .unwrap_or("");
            match part_type {
                "thinking" | "reasoning" => part_text(&v)
                    .map(|s| vec![AgentEvent::Thinking(s)])
                    .unwrap_or_default(),
                "text" => part_text(&v)
                    .map(|s| vec![AgentEvent::Text(s)])
                    .unwrap_or_default(),
                "tool" => vec![AgentEvent::ToolUse {
                    name: v
                        .pointer("/part/name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool")
                        .to_string(),
                    detail: v
                        .pointer("/part/state")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string(),
                }],
                _ => vec![],
            }
        }
        "tool_use" => vec![AgentEvent::ToolUse {
            name: v
                .pointer("/part/tool")
                .or_else(|| v.pointer("/part/name"))
                .and_then(|n| n.as_str())
                .unwrap_or("tool")
                .to_string(),
            detail: String::new(),
        }],
        "tool_result" => vec![],
        "step_start" => vec![],
        "step_finish" => {
            let tokens = v
                .pointer("/part/tokens/total")
                .or_else(|| v.pointer("/part/tokens/output"))
                .and_then(|n| n.as_i64());
            let reason = v
                .pointer("/part/reason")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            let detail = match tokens {
                Some(n) => format!("step done ({reason}, {n} tokens)"),
                None => format!("step done ({reason})"),
            };
            vec![AgentEvent::StepFinish { detail }]
        }
        "error" => vec![AgentEvent::HarnessError(
            v.pointer("/error/data/message")
                .or_else(|| v.pointer("/error/name"))
                .and_then(|m| m.as_str())
                .unwrap_or("harness error")
                .to_string(),
        )],

        // ---- claude-code stream-json ----
        "assistant" => {
            let mut out = Vec::new();
            if let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) {
                for block in content {
                    match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "thinking" => {
                            if let Some(s) = block.get("thinking").and_then(|s| s.as_str()) {
                                out.push(AgentEvent::Thinking(s.to_string()));
                            }
                        }
                        "text" => {
                            if let Some(s) = block.get("text").and_then(|s| s.as_str()) {
                                out.push(AgentEvent::Text(s.to_string()));
                            }
                        }
                        "tool_use" => out.push(AgentEvent::ToolUse {
                            name: block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string(),
                            detail: String::new(),
                        }),
                        _ => {}
                    }
                }
            }
            out
        }
        "system" => vec![],
        "user" => vec![],
        "result" => vec![AgentEvent::StepFinish {
            detail: "run finished".to_string(),
        }],

        _ => vec![AgentEvent::Raw(trimmed.to_string())],
    }
}

fn part_text(v: &serde_json::Value) -> Option<String> {
    v.pointer("/part/text")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- log picking

/// Choose which run log to tail for an agent: the newest by filename.
/// `crypto::new_id` embeds a UTC timestamp (`run-r-20260718T142530Z-xx.log`),
/// so lexicographic max IS newest -- pure, no mtime dependency. Returns None
/// when the agent has never run.
pub fn newest_run_log(names: &[String]) -> Option<String> {
    names
        .iter()
        .filter(|n| n.starts_with("run-") && n.ends_with(".log"))
        .max()
        .cloned()
}

// ---------------------------------------------------------------- tab state

/// Cap on retained events per agent feed. Old events fall off the front; the
/// full history is still on disk in the run logs.
pub const FEED_CAP: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorTab {
    /// The diagnostics/settings view (session summary, agents, health).
    Trelane,
    /// One agent's live feed.
    Agent(String),
}

impl MonitorTab {
    pub fn title(&self) -> String {
        match self {
            MonitorTab::Trelane => "Trelane".to_string(),
            MonitorTab::Agent(name) => name.clone(),
        }
    }
}

/// Per-agent feed state: the tailed file, our byte cursor into it, and the
/// parsed ring buffer.
#[derive(Debug, Clone, Default)]
pub struct AgentFeed {
    /// Filename (not full path) of the run log currently tailed.
    pub log_name: Option<String>,
    /// Byte position already consumed in that file.
    pub pos: u64,
    /// Parsed events, capped at FEED_CAP.
    pub events: Vec<AgentEvent>,
    /// Scroll offset from the BOTTOM of the feed (0 = following live tail).
    pub scroll_from_bottom: usize,
    /// Header line: activity state + reason, refreshed each poll.
    pub status_line: String,
}

impl AgentFeed {
    /// Register that a (possibly new) run log was selected. A changed name
    /// resets the cursor -- a fresh wake means a fresh file. Events are kept:
    /// the feed spans wakes, which is exactly what "why did it sleep and what
    /// happened when it woke" needs.
    pub fn select_log(&mut self, name: Option<String>) {
        if name != self.log_name {
            self.log_name = name;
            self.pos = 0;
        }
    }

    /// Append parsed events, enforcing the cap. When following (offset 0) the
    /// view stays pinned to the tail; when scrolled up, the offset grows so
    /// the visible window doesn't shift under the reader.
    pub fn push_events(&mut self, new: Vec<AgentEvent>) {
        if new.is_empty() {
            return;
        }
        let added = new.len();
        self.events.extend(new);
        if self.events.len() > FEED_CAP {
            let overflow = self.events.len() - FEED_CAP;
            self.events.drain(..overflow);
            // Dropping from the front pulls the whole buffer down; if the
            // reader is scrolled up, shrink the offset by the same amount so
            // they keep looking at the same events (until those fall off).
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(overflow);
        }
        if self.scroll_from_bottom > 0 {
            self.scroll_from_bottom = (self.scroll_from_bottom + added).min(self.events.len());
        }
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_from_bottom < self.events.len() {
            self.scroll_from_bottom += 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
    }

    /// Jump back to following the live tail.
    pub fn follow(&mut self) {
        self.scroll_from_bottom = 0;
    }
}

/// Whether a tab shows its live feed/summary or its diagnostic detail. Each
/// tab remembers its own mode independently (flipping the worldgen tab to
/// diagnostics doesn't flip the engine tab).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// The default: agent tabs show the live feed, the Trelane tab shows the
    /// session summary.
    Normal,
    /// Diagnostic detail: agent tabs show model/launcher/domain detail; the
    /// Trelane tab shows the row-based config editor.
    Diagnostic,
}

#[derive(Debug, Clone)]
pub struct MonitorState {
    pub tabs: Vec<MonitorTab>,
    pub active: usize,
    /// Feeds keyed by agent name. Entries persist across agent-list refreshes
    /// so switching away from a tab doesn't lose its history.
    pub feeds: std::collections::HashMap<String, AgentFeed>,
    pub should_quit: bool,
    /// Session summary for the Trelane tab, refreshed each poll.
    pub session_line: String,
    pub agent_rows: Vec<(String, String, String)>, // (name, state, reason)
    /// Per-tab view mode, keyed by tab TITLE (stable across agent-list
    /// re-sync, unlike index). Absent = Normal.
    pub view_modes: std::collections::HashMap<String, ViewMode>,
    /// The Trelane tab's editable config rows, loaded when its diagnostic is
    /// first opened. None until then.
    pub config_fields: Option<Vec<crate::config_fields::ConfigField>>,
    /// Cursor into `config_fields` while editing.
    pub config_cursor: usize,
    /// True once any config row has been edited since load; drives the "unsaved"
    /// hint and gates the save action.
    pub config_dirty: bool,
    /// Transient status line for the config editor (e.g. "saved", an error).
    pub config_status: Option<String>,
    /// Per-agent diagnostic detail rows (label, value), keyed by agent name,
    /// refreshed when that agent's diagnostic is shown.
    pub agent_detail: std::collections::HashMap<String, Vec<(String, String)>>,
    /// Cached session-paused flag, refreshed each poll, shown on the Trelane
    /// diagnostic and used to label the pause/resume action.
    pub session_paused: bool,
    /// True while a kill confirmation is pending (the next y/n resolves it).
    /// Kill is destructive, so it requires an explicit confirm like the
    /// standalone diagnostic's kill.
    pub kill_confirm_pending: bool,
    /// Transient session-control status (e.g. "paused", "killed 3 agent(s)").
    pub session_status: Option<String>,
}

impl MonitorState {
    /// Build from an agent list. Tab 0 is always Trelane.
    pub fn new(agents: &[String]) -> Self {
        let mut tabs = vec![MonitorTab::Trelane];
        tabs.extend(agents.iter().map(|a| MonitorTab::Agent(a.clone())));
        Self {
            tabs,
            active: 0,
            feeds: std::collections::HashMap::new(),
            should_quit: false,
            session_line: String::new(),
            agent_rows: Vec::new(),
            view_modes: std::collections::HashMap::new(),
            config_fields: None,
            config_cursor: 0,
            config_dirty: false,
            config_status: None,
            agent_detail: std::collections::HashMap::new(),
            session_paused: false,
            kill_confirm_pending: false,
            session_status: None,
        }
    }

    /// The active tab's current view mode (Normal unless flipped).
    pub fn active_mode(&self) -> ViewMode {
        self.tabs
            .get(self.active)
            .and_then(|t| self.view_modes.get(&t.title()))
            .copied()
            .unwrap_or(ViewMode::Normal)
    }

    /// Flip the active tab between Normal and Diagnostic.
    pub fn toggle_active_mode(&mut self) {
        if let Some(t) = self.tabs.get(self.active) {
            let key = t.title();
            let next = match self.active_mode() {
                ViewMode::Normal => ViewMode::Diagnostic,
                ViewMode::Diagnostic => ViewMode::Normal,
            };
            self.view_modes.insert(key, next);
        }
    }

    // ---- Trelane-tab config editor (mirrors diagnostic.rs, shared primitives) ----

    /// Ensure the config rows are loaded from `config`. Idempotent: only loads
    /// the first time (or after a save resets them), so in-progress edits
    /// aren't clobbered by a re-poll.
    pub fn ensure_config_loaded(&mut self, config: &crate::models::Config) {
        if self.config_fields.is_none() {
            self.config_fields = Some(crate::config_fields::fields_from_config(config));
            self.config_cursor = 0;
            self.config_dirty = false;
        }
    }

    pub fn config_cursor_up(&mut self) {
        self.config_cursor = self.config_cursor.saturating_sub(1);
    }

    pub fn config_cursor_down(&mut self) {
        if let Some(fields) = &self.config_fields {
            if self.config_cursor + 1 < fields.len() {
                self.config_cursor += 1;
            }
        }
    }

    pub fn config_adjust(&mut self, increase: bool) {
        if let Some(fields) = &mut self.config_fields {
            if let Some(f) = fields.get_mut(self.config_cursor) {
                f.adjust(increase);
                self.config_dirty = true;
                self.config_status = None;
            }
        }
    }

    pub fn config_toggle(&mut self) {
        if let Some(fields) = &mut self.config_fields {
            if let Some(f) = fields.get_mut(self.config_cursor) {
                f.toggle();
                self.config_dirty = true;
                self.config_status = None;
            }
        }
    }

    /// Mark the config saved (called after a successful write) so the fields
    /// reload cleanly from the freshly-written config on the next open.
    pub fn mark_config_saved(&mut self) {
        self.config_dirty = false;
        self.config_fields = None; // force reload from the saved config
        self.config_status = Some("saved".to_string());
    }

    /// Re-sync tabs with the current agent list without disturbing the active
    /// selection when possible. New agents append; removed agents drop (their
    /// feed state is retained in `feeds` in case they return).
    pub fn sync_agents(&mut self, agents: &[String]) {
        let active_tab = self.tabs.get(self.active).cloned();
        let mut tabs = vec![MonitorTab::Trelane];
        tabs.extend(agents.iter().map(|a| MonitorTab::Agent(a.clone())));
        self.tabs = tabs;
        // Restore the previously-active tab if it still exists; else clamp.
        self.active = active_tab
            .and_then(|t| self.tabs.iter().position(|x| *x == t))
            .unwrap_or(0);
    }

    pub fn next_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + 1) % self.tabs.len();
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.tabs.is_empty() {
            self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        }
    }

    /// Jump straight to tab N (0-indexed; number-key navigation). Out-of-range
    /// is a no-op rather than a clamp, so a stray keypress does nothing.
    pub fn jump_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    pub fn active_agent(&self) -> Option<&str> {
        match self.tabs.get(self.active) {
            Some(MonitorTab::Agent(name)) => Some(name.as_str()),
            _ => None,
        }
    }

    pub fn feed_mut(&mut self, agent: &str) -> &mut AgentFeed {
        self.feeds.entry(agent.to_string()).or_default()
    }
}

// ---------------------------------------------------------------- I/O shell

/// Read any new bytes from the agent's selected run log, parse them, and push
/// into the feed. Thin: all decisions live in the pure functions above.
fn poll_agent_feed(ctx: &Context, agent: &str, feed: &mut AgentFeed) -> Result<()> {
    let log_dir = ctx.trelane_dir().join("agents").join(agent).join("logs");
    let names: Vec<String> = std::fs::read_dir(&log_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    feed.select_log(newest_run_log(&names));

    let Some(name) = feed.log_name.clone() else {
        return Ok(());
    };
    let path = log_dir.join(&name);
    let Ok(mut file) = std::fs::File::open(&path) else {
        return Ok(());
    };
    use std::io::{Read, Seek, SeekFrom};
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len <= feed.pos {
        return Ok(());
    }
    file.seek(SeekFrom::Start(feed.pos))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    // Only consume complete lines; a partially-written trailing line waits
    // for the next poll so we never parse a half-flushed JSON object.
    let consumed = match buf.rfind('\n') {
        Some(idx) => idx + 1,
        None => return Ok(()),
    };
    let complete = &buf[..consumed];
    feed.pos += consumed as u64;
    let mut events = Vec::new();
    for line in complete.lines() {
        events.extend(parse_line(line));
    }
    feed.push_events(events);
    Ok(())
}

/// Refresh the Trelane-tab summary and per-agent status lines.
fn poll_statuses(ctx: &Context, state: &mut MonitorState) {
    state.session_paused = store::is_session_paused(&ctx.conn).unwrap_or(false);
    if let Ok(statuses) = crate::squire::agent_activity_statuses(ctx) {
        state.agent_rows = statuses
            .iter()
            .map(|s| {
                (
                    s.agent.clone(),
                    s.state.as_str().to_string(),
                    s.reason.clone(),
                )
            })
            .collect();
        let running = statuses
            .iter()
            .filter(|s| matches!(s.state, crate::models::AgentActivityState::Running))
            .count();
        state.session_line = format!(
            "{} agent(s) | {} running | {} asleep",
            statuses.len(),
            running,
            statuses.len() - running
        );
        for s in &statuses {
            let line = if s.reason.is_empty() {
                s.state.as_str().to_string()
            } else {
                format!("{} -- {}", s.state.as_str(), s.reason)
            };
            state.feed_mut(&s.agent).status_line = line;
        }
    }
}

/// Load an agent's diagnostic detail (model/launcher, domain, tier, lineage)
/// into state.agent_detail. Read-only; best-effort, so a missing domain just
/// yields a short "(no domain record)" row rather than failing the view.
fn poll_agent_detail(ctx: &Context, agent: &str, state: &mut MonitorState) {
    let rows = match store::get_domain(&ctx.conn, agent) {
        Ok(Some(dom)) => {
            let launcher = dom
                .launcher_agent
                .clone()
                .unwrap_or_else(|| "(default — none chosen)".to_string());
            // Resolve what the launcher actually means: a profile name maps to
            // its command; any other non-empty value is a raw model id.
            let resolved = if ctx.config.launcher.profiles.contains_key(&launcher) {
                format!(
                    "profile '{launcher}' -> {}",
                    ctx.config.launcher.profiles.get(&launcher).unwrap()
                )
            } else if dom.launcher_agent.is_some() {
                format!("model id '{launcher}'")
            } else {
                "(no launcher chosen — will not auto-launch)".to_string()
            };
            vec![
                ("Launcher/model".to_string(), launcher),
                ("Resolved to".to_string(), resolved),
                ("Granularity tier".to_string(), dom.granularity_tier.clone()),
                (
                    "Parent domain".to_string(),
                    dom.parent_domain.clone().unwrap_or_else(|| "(none)".to_string()),
                ),
                (
                    "Writable globs".to_string(),
                    if dom.writable.is_empty() {
                        "(none)".to_string()
                    } else {
                        dom.writable.join(", ")
                    },
                ),
                (
                    "Forbidden".to_string(),
                    if dom.forbidden_write.is_empty() {
                        "(none)".to_string()
                    } else {
                        dom.forbidden_write.join(", ")
                    },
                ),
            ]
        }
        _ => vec![("Domain".to_string(), "(no domain record)".to_string())],
    };
    state.agent_detail.insert(agent.to_string(), rows);
}

/// Write the monitor's edited config rows to disk. Applies the rows onto the
/// current on-disk config (so keys the editor doesn't cover are preserved),
/// validates, and saves. On success, marks the state saved so it reloads
/// cleanly.
fn save_monitor_config(state: &mut MonitorState) -> Result<()> {
    let Some(fields) = state.config_fields.clone() else {
        return Ok(());
    };
    // Start from the CURRENT on-disk config so unedited/uncovered keys survive.
    let mut config = crate::load_config()?;
    crate::config_fields::apply_fields_to_config(&fields, &mut config);
    // Honor the same DI validation the standalone editor and load path use.
    config.di.validate()?;
    crate::save_config(&config)?;
    state.mark_config_saved();
    Ok(())
}

/// Set or clear the session pause flag. The squire's next tick reads it and
/// launches (or skips launching) accordingly -- cross-process control via the
/// shared DB, since the monitor has no direct handle to the squire.
fn set_session_pause(ctx: &Context, paused: bool) -> Result<()> {
    store::set_session_paused(&ctx.conn, paused)
}

/// Terminate every running agent subprocess by its stored PID, then clear its
/// running-lock. Returns (killed, failed) counts. SIGTERM (not SIGKILL) so the
/// agent's harness can flush and exit cleanly; a stuck process would need a
/// manual follow-up, which is the honest tradeoff (SIGKILL can orphan child
/// processes and leave partial writes). PIDs of -1 are headless placeholders
/// (see insert_running_lock call sites) and are skipped, not signaled.
fn kill_session_agents(ctx: &Context) -> Result<(usize, usize)> {
    let locks = store::list_running_locks(&ctx.conn)?;
    let mut killed = 0usize;
    let mut failed = 0usize;
    for lock in locks {
        if lock.pid > 0 {
            // SIGTERM; treat "already dead" (ESRCH) as success, not failure.
            let rc = unsafe { libc::kill(lock.pid, libc::SIGTERM) };
            if rc == 0 {
                killed += 1;
            } else {
                // Portable errno read (macOS __error vs Linux __errno_location
                // differ; std normalizes them). ESRCH == no such process ==
                // already gone, which we count as success.
                let already_gone = std::io::Error::last_os_error().raw_os_error()
                    == Some(libc::ESRCH);
                if already_gone {
                    killed += 1;
                } else {
                    failed += 1;
                }
            }
        }
        // Clear the lock regardless: a signaled process is exiting, and a
        // placeholder/dead one shouldn't keep a stale lock.
        let _ = store::delete_running_lock(&ctx.conn, &lock.agent);
    }
    Ok((killed, failed))
}

/// Entry point: `trelane monitor`. Polls agent list, statuses, and the active
/// tab's log on an interval; renders tabs; handles navigation keys.
pub fn run_monitor(ctx: &Context) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    let agents = store::list_agents(&ctx.conn)?;
    let mut state = MonitorState::new(&agents);
    poll_statuses(ctx, &mut state);

    enable_raw_mode()?;
    // Draw to /dev/tty directly, not std::io::stdout(). When the monitor
    // co-runs with a background squire tick-loop (the default `trelane`
    // session), that loop prints progress to stdout; drawing to /dev/tty keeps
    // the TUI immune to it (stdout is captured to a session log by the caller).
    // Falls back to stdout when /dev/tty is unavailable (not a real terminal).
    let mut tty: Box<dyn std::io::Write + Send> =
        match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stdout()),
        };
    execute!(tty, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(tty);
    let mut terminal = Terminal::new(backend)?;
    // Force a full clear before the first draw. Some terminals/multiplexers
    // don't guarantee a blank alternate screen on entry, and ratatui only
    // diffs against its OWN prior frame -- so without this, a cell that no
    // widget happens to touch this frame (a quiet gap in the layout) can go
    // on showing whatever was there before this session started.
    terminal.clear()?;

    let outcome = (|| -> Result<()> {
        let mut last_poll = std::time::Instant::now() - std::time::Duration::from_secs(10);
        loop {
            // Poll on an interval, not every frame: agent list + statuses are
            // DB queries, and the active feed is a file read.
            if last_poll.elapsed() >= std::time::Duration::from_millis(700) {
                let agents = store::list_agents(&ctx.conn)?;
                state.sync_agents(&agents);
                poll_statuses(ctx, &mut state);
                if let Some(agent) = state.active_agent().map(str::to_string) {
                    // Feed vs detail depends on the tab's current mode.
                    if state.active_mode() == ViewMode::Diagnostic {
                        poll_agent_detail(ctx, &agent, &mut state);
                    } else {
                        let mut feed = state.feeds.remove(&agent).unwrap_or_default();
                        let _ = poll_agent_feed(ctx, &agent, &mut feed);
                        state.feeds.insert(agent, feed);
                    }
                }
                last_poll = std::time::Instant::now();
            }

            // Drain every already-queued key event before drawing, instead of
            // drawing once per keypress. This is one of the two fixes for the
            // tab-switch artifacts: mashing Tab used to fire one full-screen
            // redraw per keystroke, back-to-back with no gap -- a burst a real
            // terminal can fall behind on. Collapsing the burst into a single
            // state update plus one draw for the final state removes it.
            // event::poll(0) is a non-blocking check, so this never waits.
            // (The other fix is the per-frame Clear + one-row-per-event
            // rendering in render(), which removes the debris at its source.)
            loop {
                if !event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                if let Event::Key(key) = event::read()? {
                    let mode = state.active_mode();
                    let on_trelane =
                        matches!(state.tabs.get(state.active), Some(MonitorTab::Trelane));

                    // A pending kill confirmation captures the very next key --
                    // before any global key -- so 'q'/Tab/'d' can't slip past
                    // an unanswered "kill? y/n".
                    if state.kill_confirm_pending {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                state.kill_confirm_pending = false;
                                match kill_session_agents(ctx) {
                                    Ok((k, f)) => {
                                        state.session_status = Some(if f == 0 {
                                            format!("killed {k} agent(s)")
                                        } else {
                                            format!("killed {k}, {f} failed")
                                        });
                                    }
                                    Err(e) => {
                                        state.session_status = Some(format!("kill failed: {e}"));
                                    }
                                }
                            }
                            _ => {
                                state.kill_confirm_pending = false;
                                state.session_status = Some("kill cancelled".to_string());
                            }
                        }
                        continue; // key consumed by the confirmation
                    }

                    match key.code {
                        // Global keys, active in every mode.
                        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
                        KeyCode::Tab => state.next_tab(),
                        KeyCode::BackTab => state.prev_tab(),
                        KeyCode::Char(c @ '0'..='9') => {
                            state.jump_to(c as usize - '0' as usize);
                        }
                        // 'd' flips the active tab's diagnostic mode. On entering
                        // the Trelane tab's diagnostic, load the config rows.
                        KeyCode::Char('d') => {
                            state.toggle_active_mode();
                            if on_trelane && state.active_mode() == ViewMode::Diagnostic {
                                state.ensure_config_loaded(&ctx.config);
                            }
                        }

                        // Mode-specific keys.
                        _ if mode == ViewMode::Diagnostic && on_trelane => match key.code {
                            // Config editor.
                            KeyCode::Up => state.config_cursor_up(),
                            KeyCode::Down => state.config_cursor_down(),
                            KeyCode::Left => state.config_adjust(false),
                            KeyCode::Right => state.config_adjust(true),
                            KeyCode::Char(' ') | KeyCode::Enter => state.config_toggle(),
                            KeyCode::Char('s') => {
                                if let Err(e) = save_monitor_config(&mut state) {
                                    state.config_status = Some(format!("save failed: {e}"));
                                }
                            }
                            // Session control (pause/resume are reversible, no
                            // confirm; kill arms the y/n confirmation handled at
                            // the top of the input loop).
                            KeyCode::Char('p') => match set_session_pause(ctx, true) {
                                Ok(()) => {
                                    state.session_paused = true;
                                    state.session_status = Some("session paused".to_string());
                                }
                                Err(e) => {
                                    state.session_status = Some(format!("pause failed: {e}"));
                                }
                            },
                            KeyCode::Char('r') => match set_session_pause(ctx, false) {
                                Ok(()) => {
                                    state.session_paused = false;
                                    state.session_status = Some(
                                        "session resumed -- squire launches next tick".to_string(),
                                    );
                                }
                                Err(e) => {
                                    state.session_status = Some(format!("resume failed: {e}"));
                                }
                            },
                            KeyCode::Char('k') | KeyCode::Char('K') => {
                                state.kill_confirm_pending = true;
                                state.session_status = None;
                            }
                            _ => {}
                        },
                        // Agent-tab diagnostic is read-only detail: only the
                        // global keys above apply (d to flip back, Tab to leave).
                        _ if mode == ViewMode::Diagnostic => {}

                        // Normal mode: feed navigation (agent tabs).
                        KeyCode::Right => state.next_tab(),
                        KeyCode::Left => state.prev_tab(),
                        KeyCode::Up => {
                            if let Some(a) = state.active_agent().map(str::to_string) {
                                state.feed_mut(&a).scroll_up();
                            }
                        }
                        KeyCode::Down => {
                            if let Some(a) = state.active_agent().map(str::to_string) {
                                state.feed_mut(&a).scroll_down();
                            }
                        }
                        KeyCode::End | KeyCode::Char('f') => {
                            if let Some(a) = state.active_agent().map(str::to_string) {
                                state.feed_mut(&a).follow();
                            }
                        }
                        _ => {}
                    }
                }
                if state.should_quit {
                    return Ok(());
                }
            }

            // A tab switch changes the pane's content entirely; the per-frame
            // Clear widget in render() wipes the content area before each draw,
            // so the switch needs no extra between-frames clear here.

            terminal.draw(|f| render(f, &state))?;

            // Block briefly for the next event so the loop doesn't busy-spin
            // when idle. Deliberately not consumed here: if an event arrives
            // during this wait, it stays queued and the drain loop above
            // picks it up on the next iteration.
            event::poll(std::time::Duration::from_millis(120))?;
        }
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

/// Redirect process stdout (fd 1) to a file for this guard's lifetime,
/// restoring it on drop. Used so a background squire tick-loop's progress
/// prints land in a session log instead of corrupting the monitor's screen
/// (the monitor draws to /dev/tty, so it's unaffected by this redirect). Same
/// technique as bench_ui's capture; see run_session.
struct StdoutCapture {
    saved_fd: i32,
}

impl StdoutCapture {
    fn to_file(path: &std::path::Path) -> Self {
        use std::os::unix::io::IntoRawFd;
        let saved_fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if let Ok(file) = std::fs::File::create(path) {
            let file_fd = file.into_raw_fd();
            unsafe {
                libc::dup2(file_fd, libc::STDOUT_FILENO);
                libc::close(file_fd);
            }
        }
        StdoutCapture { saved_fd }
    }
}

impl Drop for StdoutCapture {
    fn drop(&mut self) {
        if self.saved_fd >= 0 {
            unsafe {
                libc::dup2(self.saved_fd, libc::STDOUT_FILENO);
                libc::close(self.saved_fd);
            }
        }
    }
}

/// Run the squire tick-loop until `stop` is set. This is the ticking engine
/// that launches agents on `interval_s`; it runs on a background thread under
/// `run_session`, or in the foreground for `--headless`. Errors from a single
/// tick are logged and swallowed so one bad tick never ends the loop.
pub fn run_squire_loop(
    ctx: &Context,
    launcher: Option<&str>,
    interval_s: u64,
    verbose: bool,
    stop: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let mut reanalyzed_this_stretch = false;
    while !stop.load(Ordering::Relaxed) {
        match crate::squire::tick(ctx, launcher, verbose) {
            Ok(n) => {
                if n > 0 {
                    println!("{} launched {n} agent(s)", crate::crypto::now_iso());
                }
            }
            Err(e) => println!("{} tick error: {e:?}", crate::crypto::now_iso()),
        }

        // Biplane re-analysis on full quiescence (matches the old squire loop).
        let any_running = crate::store::list_agents(&ctx.conn)
            .map(|ags| {
                ags.iter()
                    .any(|a| crate::commands::is_running(&ctx.conn, a).unwrap_or(false))
            })
            .unwrap_or(false);
        if any_running {
            reanalyzed_this_stretch = false;
        } else if !reanalyzed_this_stretch
            && crate::testing::swarm_quiescent(ctx).unwrap_or(false)
            && (ctx.config.biplane.detect_thematic_deadlock
                || ctx.config.biplane.reanalyze_on_all_stop)
        {
            if let Err(e) = crate::biplane::reanalyze_on_stop(ctx) {
                println!("warning: biplane re-analysis failed: {e:?}");
            }
            reanalyzed_this_stretch = true;
        }

        // Sleep in short slices so a quit is honored promptly, not after a
        // full interval.
        let mut slept = 0u64;
        while slept < interval_s && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            slept += 1;
        }
    }
}

/// The single-command Trelane session: the tabbed monitor UI on the main
/// thread with the squire tick-loop running on a background thread. This is
/// what bare `trelane` launches. The monitor draws to /dev/tty and the squire
/// thread's stdout is captured to `.trelane/session.log`, so the two never
/// fight over the terminal. When the user quits the monitor (q), the squire
/// thread is signaled and joined.
pub fn run_session(ctx: &Context, launcher: Option<String>, verbose: bool) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let interval_s = ctx.config.squire.interval_s;
    let stop = Arc::new(AtomicBool::new(false));

    // Capture the squire thread's stdout to a session log for the UI's
    // lifetime. The monitor is on /dev/tty, so this can't blank its screen.
    let log_path = ctx.trelane_dir().join("session.log");
    let capture = StdoutCapture::to_file(&log_path);

    // The squire loop needs its own Context (Connection isn't Sync). Open a
    // second one against the same root; SQLite handles the concurrent access.
    let squire_root = ctx.root.clone();
    let launcher_owned = launcher.clone();
    let stop_for_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        match Context::open(Some(&squire_root)) {
            Ok(sctx) => run_squire_loop(
                &sctx,
                launcher_owned.as_deref(),
                interval_s,
                verbose,
                &stop_for_thread,
            ),
            Err(e) => println!("squire thread failed to open context: {e:?}"),
        }
    });

    // Monitor on the main thread. Returns when the user presses q.
    let ui_result = run_monitor(ctx);

    // Signal the squire loop to stop and wait for it to finish its current
    // tick, so we don't leave a launching thread behind.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = handle.join();

    // Restore stdout before printing any exit message.
    drop(capture);
    eprintln!(
        "[trelane] session ended. Squire log: {}",
        log_path.display()
    );
    ui_result
}

/// Render: tab bar, then the active tab's content, then a key-hint footer.
fn render(f: &mut ratatui::Frame, state: &MonitorState) {
    use crate::diagnostic::{THEME_DIM, THEME_OK, THEME_TRELANE_ACCENT, THEME_WARN, theme_color};
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs};

    let accent = theme_color(THEME_TRELANE_ACCENT);
    let dim = theme_color(THEME_DIM);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Tab bar. Numbered so the 0-9 jump keys are discoverable at a glance.
    let titles: Vec<Line> = state
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = if i < 10 {
                format!("{i}:{}", t.title())
            } else {
                t.title()
            };
            Line::from(label)
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(state.active)
        .highlight_style(
            Style::default()
                .fg(accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Trelane Monitor ")
                .border_style(Style::default().fg(accent)),
        );
    f.render_widget(tabs, chunks[0]);

    // Wipe the content area every frame before drawing into it. This is the
    // reliable ratatui idiom (see diagnostic.rs) for guaranteeing no cell from
    // a previous frame -- e.g. a taller agent tab's output -- survives when the
    // current frame's content is shorter. terminal.clear() between frames is
    // less dependable than an in-frame Clear over the exact area.
    f.render_widget(Clear, chunks[1]);

    // Diagnostic mode replaces the content area with detail/editor views.
    if state.active_mode() == ViewMode::Diagnostic {
        render_diagnostic(f, state, chunks[1], accent, dim);
        render_footer(f, state, chunks[2], dim);
        return;
    }

    match state.tabs.get(state.active) {
        Some(MonitorTab::Trelane) | None => {
            let mut lines: Vec<Line> = vec![
                Line::from(Span::styled(
                    state.session_line.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];
            for (name, agent_state, reason) in &state.agent_rows {
                let color = match agent_state.as_str() {
                    "running" => theme_color(THEME_OK),
                    "blocked" | "disabled" => theme_color(THEME_WARN),
                    _ => dim,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{name:<16}"), Style::default().fg(accent)),
                    Span::styled(format!("{agent_state:<22}"), Style::default().fg(color)),
                    Span::styled(reason.clone(), Style::default().fg(dim)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "For config editing use `trelane diagnostic`; this tab is a live summary.",
                Style::default().fg(dim),
            )));
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Session ")
                    .border_style(Style::default().fg(dim)),
            );
            f.render_widget(para, chunks[1]);
        }
        Some(MonitorTab::Agent(name)) => {
            let feed = state.feeds.get(name);
            let status = feed.map(|fd| fd.status_line.clone()).unwrap_or_default();
            let mut lines: Vec<Line> = Vec::new();
            let events: &[AgentEvent] = feed.map(|fd| fd.events.as_slice()).unwrap_or(&[]);
            let offset = feed.map(|fd| fd.scroll_from_bottom).unwrap_or(0);
            // Each event renders as EXACTLY one row: a fixed 6-col tag gutter
            // plus a body truncated to the remaining inner width. With wrap
            // disabled (below), rendered rows == events taken, so taking
            // `visible_rows` events fills the pane exactly with no overflow and
            // no wrapped-line residue.
            let inner_height = chunks[1].height.saturating_sub(2) as usize; // borders
            let inner_width = chunks[1].width.saturating_sub(2) as usize; // borders
            let tag_col = 6usize;
            let body_width = inner_width.saturating_sub(tag_col);
            let end = events.len().saturating_sub(offset);
            let start = end.saturating_sub(inner_height.max(1));
            for ev in &events[start..end] {
                let (tag_color, body_style) = match ev {
                    AgentEvent::Thinking(_) => (dim, Style::default().fg(dim)),
                    AgentEvent::Text(_) => (theme_color(THEME_OK), Style::default()),
                    AgentEvent::ToolUse { .. } => (accent, Style::default().fg(accent)),
                    AgentEvent::StepFinish { .. } => (dim, Style::default().fg(dim)),
                    AgentEvent::HarnessError(_) => {
                        (theme_color(THEME_WARN), Style::default().fg(theme_color(THEME_WARN)))
                    }
                    AgentEvent::Raw(_) => (dim, Style::default()),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<6}", ev.tag()), Style::default().fg(tag_color)),
                    Span::styled(truncate_to_width(&ev.body(), body_width), body_style),
                ]));
            }
            if events.is_empty() {
                lines.push(Line::from(Span::styled(
                    truncate_to_width(
                        "(no run output yet -- the feed fills when this agent next wakes; \
                         use a streaming launcher profile for live thoughts)",
                        inner_width,
                    ),
                    Style::default().fg(dim),
                )));
            }
            let following = offset == 0;
            let title = format!(
                " {name} -- {status}{} ",
                if following { "" } else { "  [scrolled: End/f to follow]" }
            );
            // No .wrap(): every line is pre-truncated to one row, so wrapping
            // would only reintroduce the overflow this fix removes.
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(if following { accent } else { dim })),
            );
            f.render_widget(para, chunks[1]);
        }
    }

    render_footer(f, state, chunks[2], dim);
}

/// The key-hint footer, with hints appropriate to the active mode.
fn render_footer(
    f: &mut ratatui::Frame,
    state: &MonitorState,
    area: ratatui::layout::Rect,
    dim: ratatui::style::Color,
) {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    let on_trelane = matches!(state.tabs.get(state.active), Some(MonitorTab::Trelane));
    let hint = match (state.active_mode(), on_trelane) {
        (ViewMode::Diagnostic, true) => {
            "Tab switch  d feed  ↑↓ row  ←→ adjust  space toggle  s save  p/r pause/resume  k kill  q quit"
        }
        (ViewMode::Diagnostic, false) => "Tab switch  d back to feed  q quit",
        (ViewMode::Normal, _) => "Tab/←→ switch  0-9 jump  d diagnostic  ↑↓ scroll  End/f follow  q quit",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim)))),
        area,
    );
}

/// Render the active tab's diagnostic view: the config editor for the Trelane
/// tab, per-agent detail for an agent tab.
fn render_diagnostic(
    f: &mut ratatui::Frame,
    state: &MonitorState,
    area: ratatui::layout::Rect,
    accent: ratatui::style::Color,
    dim: ratatui::style::Color,
) {
    use crate::diagnostic::{THEME_OK, THEME_WARN, theme_color};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph};

    match state.tabs.get(state.active) {
        Some(MonitorTab::Trelane) | None => {
            // Row-based config editor -- same rows as `trelane diagnostic`.
            let mut lines: Vec<Line> = Vec::new();
            match &state.config_fields {
                Some(fields) => {
                    let inner_width = area.width.saturating_sub(2) as usize;
                    for (i, field) in fields.iter().enumerate() {
                        let selected = i == state.config_cursor;
                        let marker = if selected { "> " } else { "  " };
                        let label_style = if selected {
                            Style::default().fg(accent).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        let row = format!(
                            "{marker}{:<34}{}",
                            field.label,
                            field.display_value()
                        );
                        lines.push(Line::from(Span::styled(
                            truncate_to_width(&row, inner_width),
                            label_style,
                        )));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "(loading config...)",
                    Style::default().fg(dim),
                ))),
            }
            lines.push(Line::from(""));
            let status = if let Some(s) = &state.config_status {
                s.clone()
            } else if state.config_dirty {
                "unsaved changes -- press s to save".to_string()
            } else {
                format!(
                    "editing {}/.config/trelane/config.json",
                    std::env::var("HOME").unwrap_or_else(|_| "~".to_string())
                )
            };
            let status_color = if state.config_dirty {
                theme_color(THEME_WARN)
            } else {
                dim
            };
            lines.push(Line::from(Span::styled(
                status,
                Style::default().fg(status_color),
            )));

            // Session control section.
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "── Session control ──",
                Style::default().fg(dim),
            )));
            let (state_word, state_color) = if state.session_paused {
                ("PAUSED", theme_color(THEME_WARN))
            } else {
                ("running", theme_color(THEME_OK))
            };
            lines.push(Line::from(vec![
                Span::styled("state: ", Style::default().fg(dim)),
                Span::styled(
                    state_word,
                    Style::default().fg(state_color).add_modifier(Modifier::BOLD),
                ),
            ]));
            if state.kill_confirm_pending {
                lines.push(Line::from(Span::styled(
                    "kill ALL running agents? press y to confirm, any other key to cancel",
                    Style::default()
                        .fg(theme_color(THEME_WARN))
                        .add_modifier(Modifier::BOLD),
                )));
            } else if let Some(s) = &state.session_status {
                lines.push(Line::from(Span::styled(
                    s.clone(),
                    Style::default().fg(dim),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "p pause   r resume   k kill agents",
                    Style::default().fg(dim),
                )));
            }

            let title = if state.config_dirty {
                " Config (unsaved) ".to_string()
            } else {
                " Config ".to_string()
            };
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(para, area);
        }
        Some(MonitorTab::Agent(name)) => {
            // Read-only per-agent detail.
            let inner_width = area.width.saturating_sub(2) as usize;
            let mut lines: Vec<Line> = Vec::new();
            match state.agent_detail.get(name) {
                Some(rows) if !rows.is_empty() => {
                    for (label, value) in rows {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{label:<18}"),
                                Style::default().fg(theme_color(THEME_OK)),
                            ),
                            Span::styled(
                                truncate_to_width(value, inner_width.saturating_sub(18)),
                                Style::default(),
                            ),
                        ]));
                    }
                }
                _ => lines.push(Line::from(Span::styled(
                    "(loading agent detail...)",
                    Style::default().fg(dim),
                ))),
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Read-only. Press d to return to the live feed.",
                Style::default().fg(dim),
            )));
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {name} — diagnostic "))
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(para, area);
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- parser: opencode stream ----------------

    #[test]
    fn opencode_text_event_parses() {
        let line = r#"{"type":"text","sessionID":"ses_x","part":{"type":"text","text":"The answer is 4."}}"#;
        assert_eq!(
            parse_line(line),
            vec![AgentEvent::Text("The answer is 4.".to_string())]
        );
    }

    #[test]
    fn opencode_thinking_part_parses() {
        let line = r#"{"type":"message.part.updated","part":{"type":"thinking","text":"Let me analyze..."}}"#;
        assert_eq!(
            parse_line(line),
            vec![AgentEvent::Thinking("Let me analyze...".to_string())]
        );
    }

    #[test]
    fn opencode_reasoning_part_parses_as_thinking() {
        let line = r#"{"type":"message.part.updated","part":{"type":"reasoning","text":"consider edge cases"}}"#;
        assert_eq!(
            parse_line(line),
            vec![AgentEvent::Thinking("consider edge cases".to_string())]
        );
    }

    #[test]
    fn opencode_step_finish_carries_tokens() {
        let line = r#"{"type":"step_finish","part":{"reason":"stop","tokens":{"total":11168,"input":2,"output":34}}}"#;
        let evs = parse_line(line);
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], AgentEvent::StepFinish { detail } if detail.contains("11168")));
    }

    #[test]
    fn opencode_error_event_parses() {
        let line = r#"{"type":"error","error":{"name":"APIError","data":{"message":"Rate limit exceeded"}}}"#;
        assert_eq!(
            parse_line(line),
            vec![AgentEvent::HarnessError("Rate limit exceeded".to_string())]
        );
    }

    #[test]
    fn opencode_step_start_is_silent() {
        let line = r#"{"type":"step_start","part":{"type":"step-start"}}"#;
        assert!(parse_line(line).is_empty());
    }

    // ---------------- parser: claude-code stream-json ----------------

    #[test]
    fn claude_assistant_thinking_and_text_parse() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm, tests first"},{"type":"text","text":"I'll add tests."}]}}"#;
        assert_eq!(
            parse_line(line),
            vec![
                AgentEvent::Thinking("hmm, tests first".to_string()),
                AgentEvent::Text("I'll add tests.".to_string()),
            ]
        );
    }

    #[test]
    fn claude_tool_use_parses() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#;
        assert_eq!(
            parse_line(line),
            vec![AgentEvent::ToolUse {
                name: "Bash".to_string(),
                detail: String::new()
            }]
        );
    }

    #[test]
    fn claude_system_lines_are_silent() {
        assert!(parse_line(r#"{"type":"system","subtype":"init"}"#).is_empty());
    }

    // ---------------- parser: totality ----------------

    #[test]
    fn plain_text_becomes_raw() {
        assert_eq!(
            parse_line("agent starting up..."),
            vec![AgentEvent::Raw("agent starting up...".to_string())]
        );
    }

    #[test]
    fn unknown_json_type_becomes_raw() {
        let line = r#"{"type":"someday-new-event","x":1}"#;
        assert!(matches!(&parse_line(line)[0], AgentEvent::Raw(_)));
    }

    #[test]
    fn empty_line_is_dropped() {
        assert!(parse_line("   ").is_empty());
    }

    // ---------------- sanitize: the screenshot corruption bug ----------------
    //
    // Root cause: poll_agent_feed splits the log on '\n' only. A launcher
    // profile that emits interactive-style output (spinners redrawing via
    // bare '\r', ANSI color codes) with no real newline until the whole
    // animation finishes becomes ONE Raw event full of control bytes. Since
    // AgentEvent text is written to a real Span, an un-sanitized ESC or CR
    // reaches crossterm and moves the actual terminal cursor -- producing
    // exactly the fragmented, overlapping text from the screenshots. These
    // tests lock in that `parse_line` can never emit a dangerous byte.

    #[test]
    fn sanitize_strips_ansi_csi_color_codes() {
        // "\x1b[32mHull\x1b[0m" -- a green-colored word, as a spinner/banner
        // might emit. The escape sequences must vanish completely, not leave
        // "[32m" / "[0m" fragments.
        assert_eq!(sanitize("\x1b[32mHull\x1b[0m"), "Hull");
    }

    #[test]
    fn sanitize_strips_ansi_cursor_movement() {
        // ESC[2K (clear line) + ESC[1A (cursor up) -- classic spinner debris.
        assert_eq!(sanitize("a\x1b[2K\x1b[1Ab"), "ab");
    }

    #[test]
    fn sanitize_strips_osc_sequences() {
        // OSC 0 sets a terminal title, terminated by BEL.
        assert_eq!(sanitize("\x1b]0;window title\x07visible"), "visible");
    }

    #[test]
    fn sanitize_replaces_bare_cr_with_space_not_dropped() {
        // A spinner redrawing in place: "Hits\rHull\rDone" with NO real
        // newline anywhere. Must never leave a raw \r that could jump the
        // real cursor; spacing is preserved so words don't fuse together.
        assert_eq!(sanitize("Hits\rHull\rDone"), "Hits Hull Done");
    }

    #[test]
    fn sanitize_never_leaves_a_raw_control_byte() {
        let poisoned = "> build \x1b[36m\u{b7}\x1b[0m nvidia/nemotron\rHits\rHull\r\x07done";
        let clean = sanitize(poisoned);
        for c in clean.chars() {
            assert!(
                (c as u32) >= 0x20 && c as u32 != 0x7f,
                "control byte {:#x} survived sanitization",
                c as u32
            );
        }
        // And the real words are still legible, just space-joined.
        assert!(clean.contains("Hits"));
        assert!(clean.contains("Hull"));
        assert!(clean.contains("nvidia/nemotron"));
    }

    #[test]
    fn sanitize_is_identity_on_clean_text() {
        assert_eq!(
            sanitize("The answer is 4."),
            "The answer is 4.".to_string()
        );
    }

    #[test]
    fn parse_line_end_to_end_sanitizes_a_raw_spinner_blob() {
        // The exact failure shape: unparseable-as-JSON (so it falls to Raw),
        // containing the control bytes a plain (non-streaming) launcher's
        // spinner would leave if captured to a file with no real newline
        // until the very end.
        let line = "\x1b[2K\x1b[1G> build \u{b7} nvidia/nemotron-3-super\rHits\rHull\r";
        let evs = parse_line(line);
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            AgentEvent::Raw(s) => {
                assert!(!s.contains('\x1b'), "ESC survived: {s:?}");
                assert!(!s.contains('\r'), "bare CR survived: {s:?}");
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    // ---------------- truncate_to_width: one row per event ----------------
    //
    // The artifact root cause was long event bodies wrapping to multiple rows,
    // then their vacated wrap-rows not clearing on scroll. Every rendered body
    // now passes through this, so a body can never exceed one row.

    #[test]
    fn truncate_leaves_short_strings_untouched() {
        assert_eq!(truncate_to_width("hello", 20), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello"); // exactly fits
    }

    #[test]
    fn truncate_clips_long_strings_with_ellipsis() {
        // 5 chars into width 4 -> 3 chars + ellipsis == 4 display cols.
        let out = truncate_to_width("abcdefgh", 4);
        assert_eq!(out.chars().count(), 4);
        assert_eq!(out, "abc…");
    }

    #[test]
    fn truncate_never_exceeds_width() {
        let long = "a".repeat(500);
        for w in [0, 1, 2, 10, 80, 200] {
            assert!(
                truncate_to_width(&long, w).chars().count() <= w.max(0),
                "width {w} exceeded"
            );
        }
    }

    #[test]
    fn truncate_width_one_is_just_ellipsis() {
        assert_eq!(truncate_to_width("anything", 1), "…");
    }

    #[test]
    fn truncate_width_zero_is_empty() {
        assert_eq!(truncate_to_width("anything", 0), "");
    }

    #[test]
    fn truncate_does_not_split_multibyte_chars() {
        // Cutting a run of multibyte chars must land on a char boundary; if it
        // didn't, .chars() would have panicked building the result.
        let s = "日本語のテキストです"; // 10 chars, 3 bytes each
        let out = truncate_to_width(s, 4);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
    }

    // ---------------- newest_run_log ----------------

    #[test]
    fn newest_log_is_lexicographic_max() {
        let names = vec![
            "run-r-20260718T140000Z-aa.log".to_string(),
            "run-r-20260718T150000Z-bb.log".to_string(),
            "run-r-20260717T230000Z-zz.log".to_string(),
            "notes.txt".to_string(),
        ];
        assert_eq!(
            newest_run_log(&names).as_deref(),
            Some("run-r-20260718T150000Z-bb.log")
        );
    }

    #[test]
    fn no_run_logs_yields_none() {
        assert!(newest_run_log(&["notes.txt".to_string()]).is_none());
        assert!(newest_run_log(&[]).is_none());
    }

    // ---------------- feed lifecycle ----------------

    #[test]
    fn new_log_resets_cursor_keeps_events() {
        let mut feed = AgentFeed::default();
        feed.select_log(Some("run-a.log".to_string()));
        feed.pos = 400;
        feed.push_events(vec![AgentEvent::Text("from run a".into())]);
        feed.select_log(Some("run-b.log".to_string()));
        assert_eq!(feed.pos, 0, "fresh file, fresh cursor");
        assert_eq!(feed.events.len(), 1, "history spans wakes");
        // Same log again: cursor untouched.
        feed.pos = 120;
        feed.select_log(Some("run-b.log".to_string()));
        assert_eq!(feed.pos, 120);
    }

    #[test]
    fn feed_cap_is_enforced() {
        let mut feed = AgentFeed::default();
        for i in 0..(FEED_CAP + 50) {
            feed.push_events(vec![AgentEvent::Text(format!("e{i}"))]);
        }
        assert_eq!(feed.events.len(), FEED_CAP);
        // Oldest were dropped: the first survivor is e50.
        assert_eq!(feed.events[0], AgentEvent::Text("e50".to_string()));
    }

    #[test]
    fn scroll_holds_position_as_events_arrive() {
        let mut feed = AgentFeed::default();
        for i in 0..10 {
            feed.push_events(vec![AgentEvent::Text(format!("e{i}"))]);
        }
        feed.scroll_up();
        feed.scroll_up();
        assert_eq!(feed.scroll_from_bottom, 2);
        // Two more events arrive; the offset grows so the window is stable.
        feed.push_events(vec![
            AgentEvent::Text("new1".into()),
            AgentEvent::Text("new2".into()),
        ]);
        assert_eq!(feed.scroll_from_bottom, 4);
        feed.follow();
        assert_eq!(feed.scroll_from_bottom, 0);
    }

    #[test]
    fn scroll_saturates_at_both_ends() {
        let mut feed = AgentFeed::default();
        feed.push_events(vec![AgentEvent::Text("only".into())]);
        feed.scroll_down();
        assert_eq!(feed.scroll_from_bottom, 0, "can't scroll below the tail");
        feed.scroll_up();
        feed.scroll_up();
        assert_eq!(feed.scroll_from_bottom, 1, "can't scroll past the top");
    }

    // ---------------- tab navigation ----------------

    fn agents() -> Vec<String> {
        vec!["alpha".to_string(), "beta".to_string()]
    }

    #[test]
    fn tab_zero_is_trelane_then_agents() {
        let s = MonitorState::new(&agents());
        assert_eq!(s.tabs.len(), 3);
        assert_eq!(s.tabs[0], MonitorTab::Trelane);
        assert_eq!(s.tabs[1], MonitorTab::Agent("alpha".to_string()));
    }

    #[test]
    fn next_prev_wrap() {
        let mut s = MonitorState::new(&agents());
        s.prev_tab();
        assert_eq!(s.active, 2, "prev from 0 wraps to last");
        s.next_tab();
        assert_eq!(s.active, 0, "next from last wraps to 0");
    }

    #[test]
    fn jump_out_of_range_is_noop() {
        let mut s = MonitorState::new(&agents());
        s.jump_to(2);
        assert_eq!(s.active, 2);
        s.jump_to(9);
        assert_eq!(s.active, 2, "stray number key does nothing");
    }

    #[test]
    fn sync_preserves_active_tab_when_it_survives() {
        let mut s = MonitorState::new(&agents());
        s.jump_to(2); // beta
        s.sync_agents(&[
            "alpha".to_string(),
            "gamma".to_string(),
            "beta".to_string(),
        ]);
        assert_eq!(s.tabs.len(), 4);
        assert_eq!(s.active, 3, "still on beta after it moved");
        // Beta removed entirely -> falls back to Trelane.
        s.sync_agents(&["alpha".to_string()]);
        assert_eq!(s.active, 0);
    }

    #[test]
    fn active_agent_is_none_on_trelane_tab() {
        let mut s = MonitorState::new(&agents());
        assert!(s.active_agent().is_none());
        s.jump_to(1);
        assert_eq!(s.active_agent(), Some("alpha"));
    }

    // ---------------- diagnostic mode ----------------

    #[test]
    fn view_mode_defaults_normal_and_toggles_per_tab() {
        let mut s = MonitorState::new(&agents());
        assert_eq!(s.active_mode(), ViewMode::Normal);
        s.toggle_active_mode();
        assert_eq!(s.active_mode(), ViewMode::Diagnostic);
        // Switching to another tab shows ITS mode (still Normal), independently.
        s.jump_to(1);
        assert_eq!(s.active_mode(), ViewMode::Normal);
        // Back to tab 0: its Diagnostic mode was remembered.
        s.jump_to(0);
        assert_eq!(s.active_mode(), ViewMode::Diagnostic);
        s.toggle_active_mode();
        assert_eq!(s.active_mode(), ViewMode::Normal);
    }

    #[test]
    fn view_mode_keyed_by_title_survives_agent_resync() {
        let mut s = MonitorState::new(&agents());
        s.jump_to(1); // alpha
        s.toggle_active_mode(); // alpha -> Diagnostic
        // A new agent appears before alpha in the list; alpha's index shifts.
        s.sync_agents(&["aa".to_string(), "alpha".to_string(), "beta".to_string()]);
        let alpha_idx = s.tabs.iter().position(|t| t.title() == "alpha").unwrap();
        s.jump_to(alpha_idx);
        assert_eq!(
            s.active_mode(),
            ViewMode::Diagnostic,
            "alpha's mode follows its title, not its index"
        );
    }

    fn test_config() -> crate::models::Config {
        crate::models::Config::default()
    }

    #[test]
    fn config_loads_once_and_edits_track_dirty() {
        let mut s = MonitorState::new(&agents());
        assert!(s.config_fields.is_none());
        s.ensure_config_loaded(&test_config());
        assert!(s.config_fields.is_some());
        assert!(!s.config_dirty);
        let n = s.config_fields.as_ref().unwrap().len();
        assert!(n > 0, "expected editable rows");
        // A re-load must not clobber (idempotent).
        s.config_cursor = 3;
        s.ensure_config_loaded(&test_config());
        assert_eq!(s.config_cursor, 3, "reload didn't reset cursor");
        // An adjustment sets dirty.
        s.config_adjust(true);
        assert!(s.config_dirty);
    }

    #[test]
    fn config_cursor_stays_in_bounds() {
        let mut s = MonitorState::new(&agents());
        s.ensure_config_loaded(&test_config());
        let n = s.config_fields.as_ref().unwrap().len();
        for _ in 0..(n + 5) {
            s.config_cursor_down();
        }
        assert_eq!(s.config_cursor, n - 1, "cursor clamps at last row");
        for _ in 0..(n + 5) {
            s.config_cursor_up();
        }
        assert_eq!(s.config_cursor, 0, "cursor clamps at first row");
    }

    #[test]
    fn mark_config_saved_clears_dirty_and_forces_reload() {
        let mut s = MonitorState::new(&agents());
        s.ensure_config_loaded(&test_config());
        s.config_adjust(true);
        assert!(s.config_dirty);
        s.mark_config_saved();
        assert!(!s.config_dirty);
        assert!(s.config_fields.is_none(), "fields cleared for reload");
        assert_eq!(s.config_status.as_deref(), Some("saved"));
    }

    // ---------------- session control ----------------

    #[test]
    fn kill_confirm_defaults_off() {
        let s = MonitorState::new(&agents());
        assert!(!s.kill_confirm_pending);
        assert!(!s.session_paused);
    }

    #[test]
    fn session_status_and_paused_are_independent_of_config() {
        // Session-control state lives alongside, but separate from, config
        // editing state -- editing config must not touch pause/kill state.
        let mut s = MonitorState::new(&agents());
        s.session_paused = true;
        s.ensure_config_loaded(&test_config());
        s.config_adjust(true);
        assert!(s.session_paused, "config edit left pause flag alone");
        assert!(!s.kill_confirm_pending);
    }
}
