//! Interactive diagnostic view for the main Trelane session split.
//!
//! Architecture: the entire view state and every state transition (cursor
//! movement, tab switching, field editing, boolean toggles, dirty-tracking,
//! and the config<->editable-field mapping) live in `DiagnosticState` and are
//! pure and unit-tested. The render loop and terminal/event I/O are a thin
//! shell in `run()` at the bottom, gated behind a TTY so the pure core can be
//! exercised in tests without a terminal.
//!
//! Theme: Trelane's diagnostic UI uses a teal/cyan accent. Biplane's future
//! UI is specified to use a *different* theme (amber), so the two are visually
//! distinguishable at a glance -- the palette constants live here so both can
//! reference a single source of truth.

use crate::error::Result;
use crate::models::Config;

/// Trelane theme accent (teal/cyan). Biplane uses AMBER (defined for reuse).
pub const THEME_TRELANE_ACCENT: (u8, u8, u8) = (0x2d, 0xd4, 0xbf); // teal
pub const THEME_BIPLANE_ACCENT: (u8, u8, u8) = (0xf5, 0x9e, 0x0b); // amber
pub const THEME_DIM: (u8, u8, u8) = (0x6b, 0x72, 0x80);
pub const THEME_OK: (u8, u8, u8) = (0x22, 0xc5, 0x5e);
pub const THEME_WARN: (u8, u8, u8) = (0xef, 0x44, 0x44);

/// Which top-level panel is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Agents,
    Config,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Overview, Tab::Agents, Tab::Config];

    pub fn title(&self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Agents => "Agents",
            Tab::Config => "Config",
        }
    }

    fn index(&self) -> usize {
        Tab::ALL.iter().position(|t| t == self).unwrap()
    }

    fn next(&self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    fn prev(&self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// The kind of value an editable config field holds, which determines how
/// keypresses mutate it.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    /// Unsigned integer with an inclusive [min, max] clamp and a step.
    Uint { value: u64, min: u64, max: u64, step: u64 },
    /// Optional unsigned integer (None renders as "off"); toggling from None
    /// yields `default_on`.
    OptUint { value: Option<u64>, default_on: u64, min: u64, max: u64, step: u64 },
}

/// A single editable row in the Config tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigField {
    pub key: &'static str,
    pub label: &'static str,
    pub value: FieldValue,
}

impl ConfigField {
    /// A short human-readable rendering of the current value.
    pub fn display_value(&self) -> String {
        match &self.value {
            FieldValue::Bool(b) => (if *b { "[x]" } else { "[ ]" }).to_string(),
            FieldValue::Uint { value, .. } => value.to_string(),
            FieldValue::OptUint { value, .. } => {
                value.map(|v| v.to_string()).unwrap_or_else(|| "off".to_string())
            }
        }
    }

    /// Apply a leftward/decrement or rightward/increment adjustment.
    fn adjust(&mut self, increase: bool) {
        match &mut self.value {
            FieldValue::Bool(b) => *b = !*b,
            FieldValue::Uint { value, min, max, step } => {
                *value = if increase {
                    (*value).saturating_add(*step).min(*max)
                } else {
                    (*value).saturating_sub(*step).max(*min)
                };
            }
            FieldValue::OptUint { value, default_on, min, max, step } => {
                match value {
                    None => {
                        if increase {
                            *value = Some((*default_on).clamp(*min, *max));
                        }
                        // decrement from "off" stays off
                    }
                    Some(v) => {
                        if increase {
                            *v = (*v).saturating_add(*step).min(*max);
                        } else if *v <= *min {
                            *value = None; // stepping below min turns it off
                        } else {
                            *v = (*v).saturating_sub(*step).max(*min);
                        }
                    }
                }
            }
        }
    }

    /// Toggle: meaningful for Bool and OptUint (on/off); flips Uint between
    /// min and max as a coarse convenience.
    fn toggle(&mut self) {
        match &mut self.value {
            FieldValue::Bool(b) => *b = !*b,
            FieldValue::OptUint { value, default_on, min, max, .. } => {
                *value = match value {
                    Some(_) => None,
                    None => Some((*default_on).clamp(*min, *max)),
                };
            }
            FieldValue::Uint { value, min, max, .. } => {
                *value = if *value == *max { *min } else { *max };
            }
        }
    }
}

/// The complete, self-contained state of the diagnostic view. All mutation
/// happens through the methods below, which are pure (no I/O) and tested.
#[derive(Debug, Clone)]
pub struct DiagnosticState {
    pub tab: Tab,
    pub project: String,
    pub session_line: String,
    pub agents: Vec<AgentRow>,
    pub deadlock: Option<String>,
    pub fields: Vec<ConfigField>,
    /// Catalog of selectable model/launcher-profile names, plus the
    /// "(default)" sentinel at index 0 meaning "no launcher_agent override".
    pub models: Vec<String>,
    /// Agent name -> its edited model, for agents whose model was changed in
    /// this session but not yet saved. Empty when nothing is pending.
    pub pending_models: std::collections::HashMap<String, String>,
    /// Cursor row within the currently focused list/form.
    pub cursor: usize,
    /// True once any config field has been edited since load.
    pub dirty: bool,
    /// True once any agent model has been changed but not yet saved.
    pub models_dirty: bool,
    /// Set when the user confirms Kill; the render loop acts on it and exits.
    pub kill_requested: bool,
    /// Set when the user asks to quit.
    pub should_quit: bool,
    /// Transient status message (e.g. "saved", "kill requested").
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub domain: String,
    pub running: bool,
    pub inbox: usize,
    pub model: String,
}

impl DiagnosticState {
    /// Build the editable field list from a Config. This is the single
    /// source of truth for the config<->fields mapping; `apply_to_config`
    /// is its exact inverse.
    pub fn fields_from_config(config: &Config) -> Vec<ConfigField> {
        vec![
            ConfigField {
                key: "squire.interval_s",
                label: "Squire tick interval (s)",
                value: FieldValue::Uint { value: config.squire.interval_s, min: 1, max: 3600, step: 1 },
            },
            ConfigField {
                key: "squire.max_concurrent",
                label: "Max concurrent agents",
                value: FieldValue::Uint { value: config.squire.max_concurrent as u64, min: 1, max: 64, step: 1 },
            },
            ConfigField {
                key: "squire.reply_timeout_s",
                label: "Reply-wait timeout (s)",
                value: FieldValue::OptUint {
                    value: config.squire.reply_timeout_s,
                    default_on: 3600,
                    min: 30,
                    max: 86_400,
                    step: 30,
                },
            },
            ConfigField {
                key: "claims.default_ttl_s",
                label: "Claim TTL (s)",
                value: FieldValue::Uint { value: config.claims.default_ttl_s, min: 30, max: 86_400, step: 30 },
            },
            ConfigField {
                key: "biplane.detect_thematic_deadlock",
                label: "Detect thematic deadlock",
                value: FieldValue::Bool(config.biplane.detect_thematic_deadlock),
            },
            ConfigField {
                key: "biplane.reanalyze_on_all_stop",
                label: "Reanalyze on all-stop",
                value: FieldValue::Bool(config.biplane.reanalyze_on_all_stop),
            },
            ConfigField {
                key: "ui.pane_navigation",
                label: "Pane navigation keys",
                value: FieldValue::Bool(config.ui.pane_navigation),
            },
            ConfigField {
                key: "ui.match_host_terminal",
                label: "Match host terminal keys",
                value: FieldValue::Bool(config.ui.match_host_terminal),
            },
        ]
    }

    /// Write the current field values back onto a Config. Inverse of
    /// `fields_from_config`. Unknown keys are ignored defensively.
    pub fn apply_to_config(&self, config: &mut Config) {
        for f in &self.fields {
            match (f.key, &f.value) {
                ("squire.interval_s", FieldValue::Uint { value, .. }) => config.squire.interval_s = *value,
                ("squire.max_concurrent", FieldValue::Uint { value, .. }) => config.squire.max_concurrent = *value as usize,
                ("squire.reply_timeout_s", FieldValue::OptUint { value, .. }) => config.squire.reply_timeout_s = *value,
                ("claims.default_ttl_s", FieldValue::Uint { value, .. }) => config.claims.default_ttl_s = *value,
                ("biplane.detect_thematic_deadlock", FieldValue::Bool(b)) => config.biplane.detect_thematic_deadlock = *b,
                ("biplane.reanalyze_on_all_stop", FieldValue::Bool(b)) => config.biplane.reanalyze_on_all_stop = *b,
                ("ui.pane_navigation", FieldValue::Bool(b)) => config.ui.pane_navigation = *b,
                ("ui.match_host_terminal", FieldValue::Bool(b)) => config.ui.match_host_terminal = *b,
                _ => {}
            }
        }
    }

    pub fn new(
        project: String,
        session_line: String,
        agents: Vec<AgentRow>,
        deadlock: Option<String>,
        config: &Config,
    ) -> Self {
        // Model catalog: "(default)" (no launcher_agent override) followed by
        // the configured launcher profile names, sorted for stable ordering.
        let mut model_names: Vec<String> = config.launcher.profiles.keys().cloned().collect();
        model_names.sort();
        let mut models = vec!["(default)".to_string()];
        models.extend(model_names);

        Self {
            tab: Tab::Overview,
            project,
            session_line,
            agents,
            deadlock,
            fields: Self::fields_from_config(config),
            models,
            pending_models: std::collections::HashMap::new(),
            cursor: 0,
            dirty: false,
            models_dirty: false,
            kill_requested: false,
            should_quit: false,
            status: None,
        }
    }

    /// Index of a model name in the catalog, defaulting to 0 ("(default)")
    /// for anything not found (e.g. a stale profile removed from config).
    fn model_index(&self, name: &str) -> usize {
        self.models.iter().position(|m| m == name).unwrap_or(0)
    }

    /// Cycle the focused agent's model to the next/previous catalog entry.
    /// Only meaningful on the Agents tab. Records the change in
    /// `pending_models` and updates the visible row, but does not persist
    /// (that happens on save).
    pub fn cycle_agent_model(&mut self, forward: bool) {
        if self.tab != Tab::Agents {
            return;
        }
        if self.models.is_empty() {
            return;
        }
        let Some(agent) = self.agents.get(self.cursor) else {
            return;
        };
        let cur = self.model_index(&agent.model);
        let n = self.models.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        let new_model = self.models[next].clone();
        let agent_name = agent.name.clone();
        self.agents[self.cursor].model = new_model.clone();
        self.pending_models.insert(agent_name, new_model);
        self.models_dirty = true;
    }

    /// The launcher_agent value to persist for a model display string:
    /// "(default)" maps to None (clear the override), anything else to Some.
    pub fn model_to_launcher_agent(model: &str) -> Option<&str> {
        if model == "(default)" {
            None
        } else {
            Some(model)
        }
    }

    pub fn mark_models_saved(&mut self) {
        self.pending_models.clear();
        self.models_dirty = false;
        self.status = Some("agent models saved".to_string());
    }

    /// Number of navigable rows in the currently focused tab.
    fn row_count(&self) -> usize {
        match self.tab {
            Tab::Overview => 0,
            Tab::Agents => self.agents.len(),
            Tab::Config => self.fields.len(),
        }
    }

    fn clamp_cursor(&mut self) {
        let n = self.row_count();
        if n == 0 {
            self.cursor = 0;
        } else if self.cursor >= n {
            self.cursor = n - 1;
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn cursor_down(&mut self) {
        let n = self.row_count();
        if n > 0 && self.cursor + 1 < n {
            self.cursor += 1;
        }
    }

    pub fn next_tab(&mut self) {
        self.tab = self.tab.next();
        self.cursor = 0;
    }

    pub fn prev_tab(&mut self) {
        self.tab = self.tab.prev();
        self.cursor = 0;
    }

    /// Left/right arrow behavior depends on the focused tab:
    /// - Config: adjust the focused field's value
    /// - Agents: cycle the focused agent's model
    /// - Overview: switch tabs (a common TUI convenience)
    pub fn adjust_left(&mut self) {
        match self.tab {
            Tab::Config => {
                self.clamp_cursor();
                if let Some(f) = self.fields.get_mut(self.cursor) {
                    f.adjust(false);
                    self.dirty = true;
                }
            }
            Tab::Agents => self.cycle_agent_model(false),
            Tab::Overview => self.prev_tab(),
        }
    }

    pub fn adjust_right(&mut self) {
        match self.tab {
            Tab::Config => {
                self.clamp_cursor();
                if let Some(f) = self.fields.get_mut(self.cursor) {
                    f.adjust(true);
                    self.dirty = true;
                }
            }
            Tab::Agents => self.cycle_agent_model(true),
            Tab::Overview => self.next_tab(),
        }
    }

    /// Space/Enter on a Config row toggles booleans / on-off fields.
    pub fn toggle_focused(&mut self) {
        if self.tab == Tab::Config {
            self.clamp_cursor();
            if let Some(f) = self.fields.get_mut(self.cursor) {
                f.toggle();
                self.dirty = true;
            }
        }
    }

    /// Currently-focused agent row, if the Agents tab is active.
    pub fn focused_agent(&self) -> Option<&AgentRow> {
        if self.tab == Tab::Agents {
            self.agents.get(self.cursor)
        } else {
            None
        }
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.status = Some("configuration saved".to_string());
    }

    pub fn request_kill(&mut self) {
        self.kill_requested = true;
        self.status = Some("kill requested".to_string());
    }
}

// ----------------------------------------------------------------------------
// Thin I/O shell: real terminal render + event loop. Excluded from unit tests;
// the logic above is what's tested.
// ----------------------------------------------------------------------------

/// Entry point for `trelane diagnostic`. Opens the session, gathers a live
/// snapshot, and runs the interactive loop. No-ops with a message if stdout is
/// not a TTY (e.g. piped), so it never wedges a non-interactive shell.
pub fn run(ctx: &crate::Context) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        println!("trelane diagnostic requires an interactive terminal (TTY).");
        return Ok(());
    }
    let mut state = gather_state(ctx)?;
    run_loop(ctx, &mut state)
}

fn gather_state(ctx: &crate::Context) -> Result<DiagnosticState> {
    use crate::{commands, splash, squire, store};

    let agent_names = store::list_agents(&ctx.conn)?;
    let mut running_count = 0;
    let mut rows = Vec::new();
    for name in &agent_names {
        let running = commands::is_running(&ctx.conn, name).unwrap_or(false);
        if running {
            running_count += 1;
        }
        let inbox = store::get_unprocessed_messages(&ctx.conn, name)
            .map(|m| m.len())
            .unwrap_or(0);
        let domain = store::get_domain(&ctx.conn, name)?;
        let (domain_desc, model) = match domain {
            Some(d) => (
                if d.writable.is_empty() { d.description.clone() } else { d.writable.join(", ") },
                d.launcher_agent.clone().unwrap_or_else(|| "(default)".to_string()),
            ),
            None => ("(unknown)".to_string(), "(default)".to_string()),
        };
        rows.push(AgentRow { name: name.clone(), domain: domain_desc, running, inbox, model });
    }

    let (_, cycle) = squire::wait_graph(&ctx.conn)?;
    let deadlock = cycle.map(|c| {
        let mut disp = c.clone();
        disp.push(c[0].clone());
        disp.join(" -> ")
    });

    let state_label = if deadlock.is_some() {
        "DEADLOCK".to_string()
    } else if running_count > 0 {
        format!("ACTIVE ({running_count} running)")
    } else {
        "IDLE".to_string()
    };
    let _ = splash::SessionState::Idle; // keep splash coupling explicit/optional

    let project = ctx
        .root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| ctx.root.display().to_string());
    let session_line = format!("{} agent(s) | {}", agent_names.len(), state_label);

    Ok(DiagnosticState::new(project, session_line, rows, deadlock, &ctx.config))
}

fn run_loop(ctx: &crate::Context, state: &mut DiagnosticState) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
    use crossterm::execute;
    use ratatui::prelude::*;
    use std::time::Duration;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let outcome = (|| -> Result<()> {
        loop {
            terminal.draw(|f| render(f, state))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
                        KeyCode::Tab => state.next_tab(),
                        KeyCode::BackTab => state.prev_tab(),
                        KeyCode::Up => state.cursor_up(),
                        KeyCode::Down => state.cursor_down(),
                        KeyCode::Left => state.adjust_left(),
                        KeyCode::Right => state.adjust_right(),
                        KeyCode::Char(' ') | KeyCode::Enter => state.toggle_focused(),
                        KeyCode::Char('s') => {
                            if state.dirty {
                                save_config(state)?;
                                state.mark_saved();
                            }
                            if state.models_dirty {
                                save_agent_models(ctx, state)?;
                                state.mark_models_saved();
                            }
                        }
                        KeyCode::Char('K') => confirm_and_kill(&mut terminal, state)?,
                        _ => {}
                    }
                }
            }
            if state.should_quit || state.kill_requested {
                break;
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome?;

    if state.kill_requested {
        let _ = ctx; // kill is process-global; handled by cmd_kill in lib.rs
        crate::run_kill_from_diagnostic()?;
    }
    Ok(())
}

fn save_config(state: &DiagnosticState) -> Result<()> {
    let path = crate::config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut config: Config = serde_json::from_str(&text).unwrap_or_default();
    state.apply_to_config(&mut config);
    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    Ok(())
}

/// Persist pending per-agent model changes. Each change updates only the
/// agent's `launcher_agent`, preserving its description, writable globs, and
/// forbidden globs (read back from the current domain row).
fn save_agent_models(ctx: &crate::Context, state: &DiagnosticState) -> Result<()> {
    use crate::{crypto, store};
    for (agent, model) in &state.pending_models {
        let Some(domain) = store::get_domain(&ctx.conn, agent)? else {
            continue; // agent vanished since load; skip defensively
        };
        let launcher = DiagnosticState::model_to_launcher_agent(model);
        store::upsert_agent(
            &ctx.conn,
            agent,
            &domain.description,
            &domain.writable,
            launcher,
            &domain.forbidden_write,
            &crypto::now_iso(),
        )?;
    }
    Ok(())
}

fn confirm_and_kill<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    state: &mut DiagnosticState,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    // Draw a confirm overlay, then block for a single y/n.
    terminal.draw(|f| render_kill_confirm(f))?;
    loop {
        if let Event::Key(key) = event::read()? {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    state.request_kill();
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => return Ok(()),
                _ => {}
            }
        }
    }
}

fn theme_color(rgb: (u8, u8, u8)) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2)
}

fn render(f: &mut ratatui::Frame, state: &DiagnosticState) {
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs};

    let accent = theme_color(THEME_TRELANE_ACCENT);
    let dim = theme_color(THEME_DIM);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(5),    // body
            Constraint::Length(3), // footer / status
        ])
        .split(f.area());

    let titles: Vec<Line> = Tab::ALL.iter().map(|t| Line::from(t.title())).collect();
    let tabs = Tabs::new(titles)
        .select(Tab::ALL.iter().position(|t| *t == state.tab).unwrap())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Trelane :: {} ", state.project))
                .border_style(Style::default().fg(accent)),
        )
        .highlight_style(Style::default().fg(accent).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, chunks[0]);

    match state.tab {
        Tab::Overview => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Session: ", Style::default().fg(dim)),
                    Span::raw(state.session_line.clone()),
                ]),
            ];
            match &state.deadlock {
                Some(cycle) => lines.push(Line::from(vec![
                    Span::styled("Deadlock: ", Style::default().fg(theme_color(THEME_WARN)).add_modifier(Modifier::BOLD)),
                    Span::raw(cycle.clone()),
                ])),
                None => lines.push(Line::from(vec![
                    Span::styled("Deadlock: ", Style::default().fg(dim)),
                    Span::styled("none", Style::default().fg(theme_color(THEME_OK))),
                ])),
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Tab: switch view   ↑↓: move   ←→/space: edit (Config)   s: save   K: kill   q: quit",
                Style::default().fg(dim),
            )));
            let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(accent)));
            f.render_widget(p, chunks[1]);
        }
        Tab::Agents => {
            let items: Vec<ListItem> = state
                .agents
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let focused = i == state.cursor;
                    let marker = if focused { "▶ " } else { "  " };
                    let run = if a.running {
                        Span::styled("●", Style::default().fg(theme_color(THEME_OK)))
                    } else {
                        Span::styled("○", Style::default().fg(dim))
                    };
                    let pending = state.pending_models.contains_key(&a.name);
                    // On the focused row, wrap the model in ‹ › to signal it's
                    // editable with left/right; mark unsaved edits with '*'.
                    let model_cell = if focused {
                        format!("model:‹{}›{} ", a.model, if pending { "*" } else { "" })
                    } else {
                        format!("model:{}{} ", a.model, if pending { "*" } else { "" })
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(marker),
                        run,
                        Span::raw(format!(" {:<14} ", a.name)),
                        Span::styled(format!("inbox:{:<3} ", a.inbox), Style::default().fg(dim)),
                        Span::styled(
                            format!("{model_cell:<20}"),
                            Style::default().fg(accent).add_modifier(if pending {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        ),
                        Span::styled(a.domain.clone(), Style::default().fg(dim)),
                    ]))
                })
                .collect();
            let title = if state.models_dirty {
                " Agents & Domains * (unsaved model changes) "
            } else {
                " Agents & Domains "
            };
            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(list, chunks[1]);
        }
        Tab::Config => {
            let items: Vec<ListItem> = state
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let marker = if i == state.cursor { "▶ " } else { "  " };
                    ListItem::new(Line::from(vec![
                        Span::raw(marker),
                        Span::styled(format!("{:<28}", field.label), Style::default()),
                        Span::styled(field.display_value(), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
                    ]))
                })
                .collect();
            let title = if state.dirty { " Config * (unsaved) " } else { " Config " };
            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(list, chunks[1]);
        }
    }

    let status = state.status.clone().unwrap_or_else(|| {
        match state.tab {
            Tab::Config => "←→ adjust   space toggle   s save".to_string(),
            Tab::Agents => "↑↓ select   ←→ change model   s save".to_string(),
            Tab::Overview => "Tab to switch views".to_string(),
        }
    });
    let footer = Paragraph::new(Line::from(Span::styled(status, Style::default().fg(dim))))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(dim)));
    f.render_widget(footer, chunks[2]);
}

fn render_kill_confirm(f: &mut ratatui::Frame) {
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    let area = centered_rect(50, 20, f.area());
    f.render_widget(Clear, area);
    let warn = theme_color(THEME_WARN);
    let p = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled("  Kill ALL Trelane sessions?", Style::default().fg(warn).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("  y = confirm    n/Esc = cancel"),
    ])
    .block(Block::default().borders(Borders::ALL).title(" Emergency Kill ").border_style(Style::default().fg(warn)));
    f.render_widget(p, area);
}

fn centered_rect(pct_x: u16, pct_y: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    use ratatui::prelude::*;
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Config;

    fn state_with_defaults() -> DiagnosticState {
        let config = Config::default();
        let agents = vec![
            AgentRow { name: "alpha".into(), domain: "src/a/**".into(), running: true, inbox: 2, model: "opencode".into() },
            AgentRow { name: "beta".into(), domain: "src/b/**".into(), running: false, inbox: 0, model: "claude-code".into() },
        ];
        DiagnosticState::new("demo".into(), "2 agents | ACTIVE".into(), agents, None, &config)
    }

    #[test]
    fn tab_cycling_wraps_both_directions() {
        let mut s = state_with_defaults();
        assert_eq!(s.tab, Tab::Overview);
        s.next_tab();
        assert_eq!(s.tab, Tab::Agents);
        s.next_tab();
        assert_eq!(s.tab, Tab::Config);
        s.next_tab();
        assert_eq!(s.tab, Tab::Overview);
        s.prev_tab();
        assert_eq!(s.tab, Tab::Config);
    }

    #[test]
    fn cursor_is_bounded_by_row_count() {
        let mut s = state_with_defaults();
        s.tab = Tab::Agents;
        s.cursor = 0;
        s.cursor_up(); // already at top
        assert_eq!(s.cursor, 0);
        s.cursor_down();
        assert_eq!(s.cursor, 1);
        s.cursor_down(); // only 2 agents -> clamped
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn overview_tab_has_no_navigable_rows() {
        let mut s = state_with_defaults();
        s.tab = Tab::Overview;
        s.cursor_down();
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn toggling_a_bool_field_flips_and_dirties() {
        let mut s = state_with_defaults();
        s.tab = Tab::Config;
        // find the detect_thematic_deadlock field (default true)
        let idx = s.fields.iter().position(|f| f.key == "biplane.detect_thematic_deadlock").unwrap();
        s.cursor = idx;
        assert!(!s.dirty);
        s.toggle_focused();
        assert!(s.dirty);
        match &s.fields[idx].value {
            FieldValue::Bool(b) => assert!(!*b),
            _ => panic!("expected bool"),
        }
    }

    #[test]
    fn uint_field_adjusts_and_clamps() {
        let mut s = state_with_defaults();
        s.tab = Tab::Config;
        let idx = s.fields.iter().position(|f| f.key == "squire.interval_s").unwrap();
        s.cursor = idx;
        // default is 20 (from Config::default); decrement toward min 1
        for _ in 0..30 {
            s.adjust_left();
        }
        match &s.fields[idx].value {
            FieldValue::Uint { value, min, .. } => assert_eq!(*value, *min),
            _ => panic!("expected uint"),
        }
    }

    #[test]
    fn opt_uint_toggles_off_and_on() {
        let mut s = state_with_defaults();
        s.tab = Tab::Config;
        let idx = s.fields.iter().position(|f| f.key == "squire.reply_timeout_s").unwrap();
        s.cursor = idx;
        // default from Config::default() is Some(3600)
        s.toggle_focused();
        assert!(matches!(s.fields[idx].value, FieldValue::OptUint { value: None, .. }));
        s.toggle_focused();
        assert!(matches!(s.fields[idx].value, FieldValue::OptUint { value: Some(_), .. }));
    }

    #[test]
    fn fields_roundtrip_through_config() {
        let mut config = Config::default();
        config.squire.interval_s = 42;
        config.squire.max_concurrent = 5;
        config.biplane.reanalyze_on_all_stop = true;
        config.claims.default_ttl_s = 1200;

        let state = DiagnosticState::new("x".into(), "".into(), vec![], None, &config);
        let mut out = Config::default();
        state.apply_to_config(&mut out);

        assert_eq!(out.squire.interval_s, 42);
        assert_eq!(out.squire.max_concurrent, 5);
        assert!(out.biplane.reanalyze_on_all_stop);
        assert_eq!(out.claims.default_ttl_s, 1200);
    }

    #[test]
    fn edits_reflected_back_into_config() {
        let mut s = state_with_defaults();
        s.tab = Tab::Config;
        let idx = s.fields.iter().position(|f| f.key == "squire.max_concurrent").unwrap();
        s.cursor = idx;
        s.adjust_right(); // +1 from default 2 -> 3
        let mut config = Config::default();
        s.apply_to_config(&mut config);
        assert_eq!(config.squire.max_concurrent, 3);
    }

    #[test]
    fn left_right_switch_tabs_from_overview_only() {
        let mut s = state_with_defaults();
        s.tab = Tab::Overview;
        s.adjust_right();
        assert_eq!(s.tab, Tab::Agents);
        // From Agents, left/right are repurposed (model cycling) and must NOT
        // change the tab; navigation back happens via Tab/BackTab.
        s.adjust_left();
        assert_eq!(s.tab, Tab::Agents);
        s.prev_tab();
        assert_eq!(s.tab, Tab::Overview);
        // And from Overview, left wraps to the last tab.
        s.adjust_left();
        assert_eq!(s.tab, Tab::Config);
    }

    #[test]
    fn focused_agent_only_on_agents_tab() {
        let mut s = state_with_defaults();
        assert!(s.focused_agent().is_none()); // overview
        s.tab = Tab::Agents;
        s.cursor = 1;
        assert_eq!(s.focused_agent().unwrap().name, "beta");
    }

    #[test]
    fn kill_and_save_set_expected_flags() {
        let mut s = state_with_defaults();
        s.request_kill();
        assert!(s.kill_requested);
        s.dirty = true;
        s.mark_saved();
        assert!(!s.dirty);
    }

    #[test]
    fn display_value_formats_each_kind() {
        let b = ConfigField { key: "k", label: "l", value: FieldValue::Bool(true) };
        assert_eq!(b.display_value(), "[x]");
        let u = ConfigField { key: "k", label: "l", value: FieldValue::Uint { value: 7, min: 0, max: 9, step: 1 } };
        assert_eq!(u.display_value(), "7");
        let o = ConfigField { key: "k", label: "l", value: FieldValue::OptUint { value: None, default_on: 1, min: 1, max: 9, step: 1 } };
        assert_eq!(o.display_value(), "off");
    }

    #[test]
    fn model_catalog_starts_with_default_then_sorted_profiles() {
        // Config::default() ships claude-code, copilot, opencode.
        let s = state_with_defaults();
        assert_eq!(s.models[0], "(default)");
        assert_eq!(&s.models[1..], &["claude-code", "copilot", "opencode"]);
    }

    #[test]
    fn cycling_model_forward_advances_and_marks_pending() {
        let mut s = state_with_defaults();
        s.tab = Tab::Agents;
        s.cursor = 0; // alpha, model "opencode"
        assert_eq!(s.agents[0].model, "opencode");
        s.cycle_agent_model(true); // opencode is last -> wraps to "(default)"
        assert_eq!(s.agents[0].model, "(default)");
        assert!(s.models_dirty);
        assert_eq!(s.pending_models.get("alpha").map(String::as_str), Some("(default)"));
    }

    #[test]
    fn cycling_model_backward_wraps() {
        let mut s = state_with_defaults();
        s.tab = Tab::Agents;
        s.cursor = 1; // beta, model "claude-code" (index 1)
        s.cycle_agent_model(false); // -> index 0 "(default)"
        assert_eq!(s.agents[1].model, "(default)");
        s.cycle_agent_model(false); // wraps to last "opencode"
        assert_eq!(s.agents[1].model, "opencode");
    }

    #[test]
    fn cycling_model_is_noop_off_agents_tab() {
        let mut s = state_with_defaults();
        s.tab = Tab::Config;
        s.cycle_agent_model(true);
        assert!(!s.models_dirty);
        assert!(s.pending_models.is_empty());
    }

    #[test]
    fn model_to_launcher_agent_maps_default_to_none() {
        assert_eq!(DiagnosticState::model_to_launcher_agent("(default)"), None);
        assert_eq!(DiagnosticState::model_to_launcher_agent("opencode"), Some("opencode"));
    }

    #[test]
    fn unknown_model_indexes_to_default() {
        let mut s = state_with_defaults();
        s.tab = Tab::Agents;
        s.cursor = 0;
        s.agents[0].model = "some-removed-profile".to_string();
        // cycling forward from an unknown model treats it as index 0 -> index 1
        s.cycle_agent_model(true);
        assert_eq!(s.agents[0].model, "claude-code");
    }

    #[test]
    fn mark_models_saved_clears_pending() {
        let mut s = state_with_defaults();
        s.tab = Tab::Agents;
        s.cursor = 0;
        s.cycle_agent_model(true);
        assert!(s.models_dirty);
        s.mark_models_saved();
        assert!(!s.models_dirty);
        assert!(s.pending_models.is_empty());
    }
}
