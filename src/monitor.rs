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

/// Parse one log line into an event. Total: never fails, never drops.
///
/// Recognized shapes, in order of attempt:
/// - opencode `--format json`: `{"type": "...", "part": {...}}` where
///   part.type distinguishes text/thinking/tool; top-level types include
///   step_start, text, tool_use, tool_result, step_finish, error, and
///   message.part.updated (whose part.type may be thinking/reasoning).
/// - claude-code `stream-json`: `{"type":"assistant","message":{"content":
///   [{"type":"text"|"thinking"|"tool_use",...}]}}` plus system/result lines.
/// - anything else: Raw.
pub fn parse_line(line: &str) -> Vec<AgentEvent> {
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
        }
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
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

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
                    let mut feed = state.feeds.remove(&agent).unwrap_or_default();
                    let _ = poll_agent_feed(ctx, &agent, &mut feed);
                    state.feeds.insert(agent, feed);
                }
                last_poll = std::time::Instant::now();
            }

            terminal.draw(|f| render(f, &state))?;

            if event::poll(std::time::Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
                        KeyCode::Tab | KeyCode::Right => state.next_tab(),
                        KeyCode::BackTab | KeyCode::Left => state.prev_tab(),
                        KeyCode::Char(c @ '0'..='9') => {
                            state.jump_to(c as usize - '0' as usize);
                        }
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
            }
            if state.should_quit {
                return Ok(());
            }
        }
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

/// Render: tab bar, then the active tab's content, then a key-hint footer.
fn render(f: &mut ratatui::Frame, state: &MonitorState) {
    use crate::diagnostic::{THEME_DIM, THEME_OK, THEME_TRELANE_ACCENT, THEME_WARN, theme_color};
    use ratatui::layout::{Constraint, Direction, Layout};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Tabs};

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
            // Visible slice: everything up to (len - offset); the Paragraph
            // shows the last N lines that fit by rendering from that window's
            // tail. Simple line-per-event model; long bodies wrap.
            let end = events.len().saturating_sub(offset);
            let visible_rows = chunks[1].height.saturating_sub(2) as usize;
            let start = end.saturating_sub(visible_rows.max(1));
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
                    Span::styled(ev.body(), body_style),
                ]));
            }
            if events.is_empty() {
                lines.push(Line::from(Span::styled(
                    "(no run output yet -- the feed fills when this agent next wakes; \
                     use a streaming launcher profile for live thoughts)",
                    Style::default().fg(dim),
                )));
            }
            let following = offset == 0;
            let title = format!(
                " {name} -- {status}{} ",
                if following { "" } else { "  [scrolled: End/f to follow]" }
            );
            let para = Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(if following { accent } else { dim })),
                )
                .wrap(ratatui::widgets::Wrap { trim: false });
            f.render_widget(para, chunks[1]);
        }
    }

    let hint = "Tab/←→ switch  0-9 jump  ↑↓ scroll  End/f follow  q quit";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim)))),
        chunks[2],
    );
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
}
