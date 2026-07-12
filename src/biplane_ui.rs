//! Interactive Biplane report/description editor (`trelane biplane --ui`).
//!
//! Biplane's job is to turn a loose but well-documented project into a
//! structured description with clear domains and tasks. This module is its
//! non-headless face: it loads (or scaffolds) a `ProjectDescription`, lets the
//! user review and rearrange the proposed domains, adjust per-domain agent
//! counts, include/exclude domains, and save the curated result back to
//! `.trelane/biplane-description.json` for Trelane to launch from.
//!
//! Architecture mirrors `diagnostic.rs`: all state and transitions live in
//! `BiplaneUiState` and are pure + unit-tested; the raw-mode render/event loop
//! is a thin shell gated behind a TTY check.
//!
//! Theme: amber accent (`THEME_BIPLANE_ACCENT`), deliberately distinct from
//! Trelane's teal so the two UIs are never confused at a glance -- both
//! palettes live in `crate::diagnostic` as a single source of truth.

use crate::biplane::{DomainSpec, ProjectDescription, validate_description};
use crate::error::Result;

/// A single editable row: a domain plus whether it's currently included.
#[derive(Debug, Clone)]
pub struct DomainRow {
    pub spec: DomainSpec,
    pub include: bool,
}

/// The full editor state. Every mutation goes through the pure methods below.
#[derive(Debug, Clone)]
pub struct BiplaneUiState {
    pub project_name: String,
    pub project_summary: String,
    pub rows: Vec<DomainRow>,
    /// Effective agent budget (from description.max_agents or a default).
    pub budget: usize,
    /// Cursor row within the domain list.
    pub cursor: usize,
    /// True once any edit has been made since load.
    pub dirty: bool,
    /// Last validation error, if a save attempt failed.
    pub last_error: Option<String>,
    pub should_quit: bool,
    pub save_requested: bool,
    pub status: Option<String>,
    /// Source of the description ("loaded from file" vs "scaffolded").
    pub source: String,
}

impl BiplaneUiState {
    pub fn from_description(desc: &ProjectDescription, source: impl Into<String>) -> Self {
        let rows = desc
            .domains
            .iter()
            .map(|d| DomainRow {
                spec: d.clone(),
                include: true,
            })
            .collect();
        let budget = desc.max_agents.unwrap_or(desc.domains.len().max(1)).max(1);
        Self {
            project_name: desc.name.clone(),
            project_summary: desc.description.clone(),
            rows,
            budget,
            cursor: 0,
            dirty: false,
            last_error: None,
            should_quit: false,
            save_requested: false,
            status: None,
            source: source.into(),
        }
    }

    /// Rebuild a ProjectDescription from the *included* rows, preserving the
    /// project name/summary/budget. Excluded domains are dropped, and their
    /// dangling `depends_on` references are pruned so the result validates.
    pub fn to_description(&self) -> ProjectDescription {
        use std::collections::HashSet;
        let included: HashSet<&str> = self
            .rows
            .iter()
            .filter(|r| r.include)
            .map(|r| r.spec.name.as_str())
            .collect();

        let domains: Vec<DomainSpec> = self
            .rows
            .iter()
            .filter(|r| r.include)
            .map(|r| {
                let mut spec = r.spec.clone();
                spec.depends_on
                    .retain(|dep| included.contains(dep.as_str()));
                spec
            })
            .collect();

        ProjectDescription {
            name: self.project_name.clone(),
            description: self.project_summary.clone(),
            domains,
            max_agents: Some(self.budget),
            default_model: None,
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn cursor_down(&mut self) {
        if !self.rows.is_empty() && self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    /// Toggle include/exclude on the focused domain.
    pub fn toggle_include(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.include = !row.include;
            self.dirty = true;
        }
    }

    /// Increase/decrease the focused domain's requested agent count (min 1).
    pub fn adjust_agents(&mut self, increase: bool) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            if increase {
                row.spec.agents = row.spec.agents.saturating_add(1);
            } else {
                row.spec.agents = row.spec.agents.saturating_sub(1).max(1);
            }
            self.dirty = true;
        }
    }

    /// Adjust the overall agent budget (min 1).
    pub fn adjust_budget(&mut self, increase: bool) {
        if increase {
            self.budget = self.budget.saturating_add(1);
        } else {
            self.budget = self.budget.saturating_sub(1).max(1);
        }
        self.dirty = true;
    }

    /// Move the focused domain up one position (reordering priority).
    pub fn move_up(&mut self) {
        if self.cursor > 0 && self.cursor < self.rows.len() {
            self.rows.swap(self.cursor, self.cursor - 1);
            self.cursor -= 1;
            self.dirty = true;
        }
    }

    /// Move the focused domain down one position.
    pub fn move_down(&mut self) {
        if !self.rows.is_empty() && self.cursor + 1 < self.rows.len() {
            self.rows.swap(self.cursor, self.cursor + 1);
            self.cursor += 1;
            self.dirty = true;
        }
    }

    /// Number of currently included domains.
    pub fn included_count(&self) -> usize {
        self.rows.iter().filter(|r| r.include).count()
    }

    /// Validate the current curated description. On success clears any error
    /// and returns the description; on failure records the message and returns
    /// None (so the caller can refuse to save).
    pub fn validated(&mut self) -> Option<ProjectDescription> {
        let desc = self.to_description();
        match validate_description(&desc) {
            Ok(()) => {
                self.last_error = None;
                Some(desc)
            }
            Err(e) => {
                self.last_error = Some(format!("{e:?}"));
                None
            }
        }
    }

    pub fn mark_saved(&mut self) {
        self.dirty = false;
        self.status = Some("description saved".to_string());
    }
}

// ----------------------------------------------------------------------------
// Thin I/O shell.
// ----------------------------------------------------------------------------

/// Entry point for `trelane biplane --ui`. Loads the stored description if one
/// exists, otherwise scaffolds from the project structure, then runs the
/// editor. No-ops with a message when stdout is not a TTY.
pub fn run(root: &std::path::Path) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        println!("trelane biplane --ui requires an interactive terminal (TTY).");
        return Ok(());
    }

    let desc_path = root.join(".trelane").join("biplane-description.json");
    let (desc, source) = if desc_path.exists() {
        (
            crate::biplane::load_project_description(&desc_path)?,
            format!("loaded from {}", desc_path.display()),
        )
    } else {
        (
            crate::biplane::scaffold_description_from_structure(root),
            "scaffolded from project source layout".to_string(),
        )
    };

    let mut state = BiplaneUiState::from_description(&desc, source);
    run_loop(root, &mut state)
}

fn save_description(root: &std::path::Path, desc: &ProjectDescription) -> Result<()> {
    let dir = root.join(".trelane");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("biplane-description.json");
    std::fs::write(&path, serde_json::to_string_pretty(desc)?)?;
    Ok(())
}

fn run_loop(root: &std::path::Path, state: &mut BiplaneUiState) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
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
                        KeyCode::Up => state.cursor_up(),
                        KeyCode::Down => state.cursor_down(),
                        KeyCode::Char(' ') | KeyCode::Enter => state.toggle_include(),
                        KeyCode::Left => state.adjust_agents(false),
                        KeyCode::Right => state.adjust_agents(true),
                        KeyCode::Char('[') => state.adjust_budget(false),
                        KeyCode::Char(']') => state.adjust_budget(true),
                        KeyCode::Char('K') => state.move_up(),
                        KeyCode::Char('J') => state.move_down(),
                        KeyCode::Char('s') => {
                            if let Some(desc) = state.validated() {
                                save_description(root, &desc)?;
                                state.mark_saved();
                            }
                        }
                        _ => {}
                    }
                }
            }
            if state.should_quit {
                break;
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome
}

fn tc(rgb: (u8, u8, u8)) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2)
}

fn render(f: &mut ratatui::Frame, state: &BiplaneUiState) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK, THEME_WARN};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Min(5),    // domain list
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    // Header
    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(
                "Biplane :: ",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::raw(state.project_name.clone()),
        ]),
        Line::from(vec![
            Span::styled("budget ", Style::default().fg(dim)),
            Span::styled(
                format!("{} agent(s)", state.budget),
                Style::default().fg(accent),
            ),
            Span::styled(
                format!(
                    "   included {}/{}",
                    state.included_count(),
                    state.rows.len()
                ),
                Style::default().fg(dim),
            ),
            Span::styled(format!("   ({})", state.source), Style::default().fg(dim)),
        ]),
    ];
    if let Some(err) = &state.last_error {
        header_lines.push(Line::from(vec![
            Span::styled(
                "invalid: ",
                Style::default()
                    .fg(tc(THEME_WARN))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(err.clone()),
        ]));
    }
    let title = if state.dirty {
        " Project * (unsaved) "
    } else {
        " Project "
    };
    let header = Paragraph::new(header_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(header, chunks[0]);

    // Domain list
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let marker = if i == state.cursor { "▶ " } else { "  " };
            let check = if row.include {
                Span::styled("[x]", Style::default().fg(tc(THEME_OK)))
            } else {
                Span::styled("[ ]", Style::default().fg(dim))
            };
            let deps = if row.spec.depends_on.is_empty() {
                "-".to_string()
            } else {
                row.spec.depends_on.join(",")
            };
            let name_style = if row.include {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim)
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                check,
                Span::styled(format!(" {:<16}", row.spec.name), name_style),
                Span::styled(
                    format!("agents:{:<3} ", row.spec.agents),
                    Style::default().fg(dim),
                ),
                Span::styled(
                    format!("work:{:<3} ", row.spec.planned_work.len()),
                    Style::default().fg(dim),
                ),
                Span::styled(format!("deps:{:<12} ", deps), Style::default().fg(dim)),
                Span::styled(row.spec.writable.join(","), Style::default().fg(dim)),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Domains ")
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(list, chunks[1]);

    // Footer
    let hint = state.status.clone().unwrap_or_else(|| {
        "↑↓ move  space include  ←→ agents  [ ] budget  K/J reorder  s save  q quit".to_string()
    });
    let footer = Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim)))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(dim)),
    );
    f.render_widget(footer, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::biplane::{DomainSpec, PlannedWork, ProjectDescription};

    fn domain(name: &str, deps: &[&str], agents: usize) -> DomainSpec {
        DomainSpec {
            name: name.to_string(),
            description: format!("owns {name}"),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: vec![],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            planned_work: vec![PlannedWork {
                subject: format!("build {name}"),
                body: String::new(),
                priority: "normal".to_string(),
            }],
            agents,
            model: None,
        }
    }

    fn desc() -> ProjectDescription {
        ProjectDescription {
            name: "demo".into(),
            description: "a demo".into(),
            domains: vec![
                domain("engine", &[], 1),
                domain("ui", &["engine"], 1),
                domain("api", &["engine"], 2),
            ],
            max_agents: Some(3),
            default_model: None,
        }
    }

    fn state() -> BiplaneUiState {
        BiplaneUiState::from_description(&desc(), "test")
    }

    #[test]
    fn builds_rows_all_included_by_default() {
        let s = state();
        assert_eq!(s.rows.len(), 3);
        assert!(s.rows.iter().all(|r| r.include));
        assert_eq!(s.included_count(), 3);
        assert_eq!(s.budget, 3);
    }

    #[test]
    fn cursor_bounds_hold() {
        let mut s = state();
        s.cursor_up();
        assert_eq!(s.cursor, 0);
        s.cursor_down();
        s.cursor_down();
        s.cursor_down(); // only 3 rows
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn toggle_include_excludes_and_dirties() {
        let mut s = state();
        s.cursor = 1; // ui
        s.toggle_include();
        assert!(!s.rows[1].include);
        assert!(s.dirty);
        assert_eq!(s.included_count(), 2);
    }

    #[test]
    fn adjust_agents_clamps_at_one() {
        let mut s = state();
        s.cursor = 0; // engine, agents 1
        s.adjust_agents(false);
        assert_eq!(s.rows[0].spec.agents, 1); // can't go below 1
        s.adjust_agents(true);
        assert_eq!(s.rows[0].spec.agents, 2);
    }

    #[test]
    fn adjust_budget_clamps_at_one() {
        let mut s = state();
        for _ in 0..10 {
            s.adjust_budget(false);
        }
        assert_eq!(s.budget, 1);
        s.adjust_budget(true);
        assert_eq!(s.budget, 2);
    }

    #[test]
    fn reorder_moves_domain_and_follows_cursor() {
        let mut s = state();
        s.cursor = 2; // api
        s.move_up();
        assert_eq!(s.rows[1].spec.name, "api");
        assert_eq!(s.cursor, 1);
        s.move_down();
        assert_eq!(s.rows[2].spec.name, "api");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn move_up_at_top_is_noop() {
        let mut s = state();
        s.cursor = 0;
        s.move_up();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.rows[0].spec.name, "engine");
    }

    #[test]
    fn to_description_prunes_dangling_dependencies() {
        let mut s = state();
        s.cursor = 0; // engine
        s.toggle_include(); // exclude engine, which ui and api depend on
        let d = s.to_description();
        let names: Vec<&str> = d.domains.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, vec!["ui", "api"]);
        // both had depends_on=[engine], now pruned
        assert!(d.domains.iter().all(|x| x.depends_on.is_empty()));
    }

    #[test]
    fn to_description_preserves_budget_and_agent_edits() {
        let mut s = state();
        s.cursor = 2; // api
        s.adjust_agents(true); // 2 -> 3
        s.adjust_budget(true); // 3 -> 4
        let d = s.to_description();
        assert_eq!(d.max_agents, Some(4));
        let api = d.domains.iter().find(|x| x.name == "api").unwrap();
        assert_eq!(api.agents, 3);
    }

    #[test]
    fn validated_succeeds_on_good_description() {
        let mut s = state();
        assert!(s.validated().is_some());
        assert!(s.last_error.is_none());
    }

    #[test]
    fn validated_fails_when_all_excluded() {
        let mut s = state();
        for i in 0..s.rows.len() {
            s.cursor = i;
            if s.rows[i].include {
                s.toggle_include();
            }
        }
        // zero domains -> validate_description rejects
        assert!(s.validated().is_none());
        assert!(s.last_error.is_some());
    }

    #[test]
    fn mark_saved_clears_dirty() {
        let mut s = state();
        s.toggle_include();
        assert!(s.dirty);
        s.mark_saved();
        assert!(!s.dirty);
    }
}
