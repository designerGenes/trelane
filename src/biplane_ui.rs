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
use std::process::Command;

/// A model entry from `opencode models`, with a free-model flag.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub is_free: bool,
}

/// Fetch the available model list from `opencode models`.
pub fn fetch_opencode_models() -> Vec<ModelEntry> {
    let output = Command::new("opencode").arg("models").output();
    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| ModelEntry {
            id: l.trim().to_string(),
            is_free: l.contains(":free") || l.contains("-free"),
        })
        .collect()
}

/// A single editable row: a domain plus whether it's currently included.
#[derive(Debug, Clone)]
pub struct DomainRow {
    pub spec: DomainSpec,
    pub include: bool,
}

/// What an in-progress text edit will be written to on commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// Rename the focused domain.
    DomainName,
    /// Edit the subject of planned-work item `index` on the focused domain.
    WorkSubject { index: usize },
    /// Edit writable glob `index` on the focused domain.
    Writable { index: usize },
}

/// An active text edit: the buffer plus where it commits.
#[derive(Debug, Clone)]
pub struct ActiveEdit {
    pub input: crate::text_input::TextInput,
    pub target: EditTarget,
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
    /// When Some, a text edit is in progress; keys route into the buffer until
    /// commit (Enter) or cancel (Esc). `target` records what is being edited.
    pub editing: Option<ActiveEdit>,
    /// Available models from `opencode models`.
    pub models: Vec<ModelEntry>,
    /// When true, the model selector popup is open for the focused domain.
    pub model_selector_open: bool,
    /// Cursor within the model selector popup.
    pub model_cursor: usize,
    /// When true, the model selector only shows free models.
    pub free_only: bool,
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
            editing: None,
            models: Vec::new(),
            model_selector_open: false,
            model_cursor: 0,
            free_only: false,
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

    /// True when a text-edit is in progress (keys should route to the buffer).
    pub fn is_editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The active edit's input buffer, if any (for the render layer).
    pub fn edit_input(&self) -> Option<&crate::text_input::TextInput> {
        self.editing.as_ref().map(|e| &e.input)
    }

    /// Begin editing the focused domain's name, seeding the buffer with its
    /// current value. No-op if there are no rows.
    pub fn begin_rename(&mut self) {
        if let Some(row) = self.rows.get(self.cursor) {
            self.editing = Some(ActiveEdit {
                input: crate::text_input::TextInput::with_text(&row.spec.name),
                target: EditTarget::DomainName,
            });
            self.last_error = None;
        }
    }

    /// Begin editing planned-work item `index` on the focused domain. Sets an
    /// error (and does not enter edit mode) if the domain has no such item.
    pub fn begin_edit_work(&mut self, index: usize) {
        if let Some(row) = self.rows.get(self.cursor) {
            if let Some(work) = row.spec.planned_work.get(index) {
                self.editing = Some(ActiveEdit {
                    input: crate::text_input::TextInput::with_text(&work.subject),
                    target: EditTarget::WorkSubject { index },
                });
                self.last_error = None;
            } else {
                self.last_error = Some("this domain has no planned work to edit".to_string());
            }
        }
    }

    /// Cancel an in-progress edit, discarding the buffer.
    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    /// Commit the in-progress edit to its target. Returns true if a change was
    /// applied. Keeps the editor open on validation failure so the user can fix
    /// the input.
    pub fn commit_edit(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        match edit.target.clone() {
            EditTarget::DomainName => self.commit_rename(),
            EditTarget::WorkSubject { index } => self.commit_work_subject(index),
            EditTarget::Writable { index } => self.commit_writable(index),
        }
    }

    /// Commit the in-progress rename to the focused domain. Rejects empty or
    /// duplicate names (setting `last_error` and keeping edit mode open so the
    /// user can fix it). On success, rewrites any other domain's `depends_on`
    /// entries that referenced the old name, so dependencies stay intact.
    /// Returns true if the rename was applied.
    pub fn commit_rename(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let new_name = edit.input.value().trim().to_string();

        if new_name.is_empty() {
            self.last_error = Some("domain name must not be empty".to_string());
            return false;
        }
        let old_name = match self.rows.get(self.cursor) {
            Some(r) => r.spec.name.clone(),
            None => {
                self.editing = None;
                return false;
            }
        };
        if new_name == old_name {
            // No change; just close the editor cleanly.
            self.editing = None;
            self.last_error = None;
            return false;
        }
        // Reject a name that collides with another domain.
        if self
            .rows
            .iter()
            .enumerate()
            .any(|(i, r)| i != self.cursor && r.spec.name == new_name)
        {
            self.last_error = Some(format!("a domain named '{new_name}' already exists"));
            return false;
        }

        // Apply: rename the focused domain and rewrite dependents' edges.
        self.rows[self.cursor].spec.name = new_name.clone();
        for (i, row) in self.rows.iter_mut().enumerate() {
            if i == self.cursor {
                continue;
            }
            for dep in row.spec.depends_on.iter_mut() {
                if *dep == old_name {
                    *dep = new_name.clone();
                }
            }
        }
        self.editing = None;
        self.last_error = None;
        self.dirty = true;
        true
    }

    /// Commit the in-progress planned-work subject edit. Rejects an empty
    /// subject (keeping the editor open). A no-op if unchanged.
    fn commit_work_subject(&mut self, index: usize) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let new_subject = edit.input.value().trim().to_string();
        if new_subject.is_empty() {
            self.last_error = Some("work subject must not be empty".to_string());
            return false;
        }
        match self
            .rows
            .get_mut(self.cursor)
            .and_then(|r| r.spec.planned_work.get_mut(index))
        {
            Some(work) => {
                if work.subject == new_subject {
                    self.editing = None;
                    self.last_error = None;
                    return false;
                }
                work.subject = new_subject;
                self.editing = None;
                self.last_error = None;
                self.dirty = true;
                true
            }
            None => {
                self.editing = None;
                false
            }
        }
    }

    /// Feed a character into the active edit buffer (no-op if not editing).
    pub fn edit_insert(&mut self, c: char) {
        if let Some(edit) = self.editing.as_mut() {
            edit.input.insert(c);
        }
    }

    /// Backspace in the active edit buffer.
    pub fn edit_backspace(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            edit.input.backspace();
        }
    }

    // -- Writable glob editing --

    /// Begin editing writable glob `index` on the focused domain.
    pub fn begin_edit_writable(&mut self, index: usize) {
        if let Some(row) = self.rows.get(self.cursor)
            && let Some(glob) = row.spec.writable.get(index)
        {
            self.editing = Some(ActiveEdit {
                input: crate::text_input::TextInput::with_text(glob),
                target: EditTarget::Writable { index },
            });
            self.last_error = None;
        }
    }

    /// Commit the in-progress writable glob edit. Rejects empty globs.
    fn commit_writable(&mut self, index: usize) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let new_glob = edit.input.value().trim().to_string();
        if new_glob.is_empty() {
            self.last_error = Some("writable glob must not be empty".to_string());
            return false;
        }
        match self
            .rows
            .get_mut(self.cursor)
            .and_then(|r| r.spec.writable.get_mut(index))
        {
            Some(glob) => {
                if *glob == new_glob {
                    self.editing = None;
                    self.last_error = None;
                    return false;
                }
                *glob = new_glob;
                self.editing = None;
                self.last_error = None;
                self.dirty = true;
                true
            }
            None => {
                self.editing = None;
                false
            }
        }
    }

    // -- Domain add/remove --

    /// Add a new empty domain at the end of the list and focus it.
    pub fn add_domain(&mut self) {
        let mut n = 1;
        loop {
            let name = format!("domain-{n}");
            if !self.rows.iter().any(|r| r.spec.name == name) {
                self.rows.push(DomainRow {
                    spec: DomainSpec {
                        name,
                        description: String::new(),
                        writable: vec!["src/**".to_string()],
                        forbidden_write: vec![],
                        depends_on: vec![],
                        planned_work: vec![],
                        agents: 1,
                        model: None,
                    },
                    include: true,
                });
                self.cursor = self.rows.len() - 1;
                self.dirty = true;
                self.status = Some("domain added -- press 'e' to rename".to_string());
                return;
            }
            n += 1;
        }
    }

    /// Remove the focused domain. Does nothing if there are no rows.
    pub fn remove_domain(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let removed_name = self.rows[self.cursor].spec.name.clone();
        self.rows.remove(self.cursor);
        if self.cursor >= self.rows.len() && !self.rows.is_empty() {
            self.cursor = self.rows.len() - 1;
        }
        // Prune dangling depends_on references.
        for row in &mut self.rows {
            row.spec.depends_on.retain(|d| *d != removed_name);
        }
        self.dirty = true;
        self.status = Some(format!("domain '{removed_name}' removed"));
    }

    // -- Planned-work add/remove --

    /// Add a new planned-work item to the focused domain.
    pub fn add_work(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.spec.planned_work.push(crate::biplane::PlannedWork {
                subject: "new task".to_string(),
                body: String::new(),
                priority: "normal".to_string(),
            });
            self.dirty = true;
            self.status = Some("work item added -- press 'w' to edit".to_string());
        }
    }

    /// Remove planned-work item `index` from the focused domain.
    pub fn remove_work(&mut self, index: usize) {
        if let Some(row) = self.rows.get_mut(self.cursor)
            && index < row.spec.planned_work.len()
        {
            row.spec.planned_work.remove(index);
            self.dirty = true;
            self.status = Some(format!("work item {index} removed"));
        }
    }

    /// Move the edit caret left/right (no-op if not editing).
    pub fn edit_caret_left(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            edit.input.move_left();
        }
    }

    pub fn edit_caret_right(&mut self) {
        if let Some(edit) = self.editing.as_mut() {
            edit.input.move_right();
        }
    }

    // -- Model selector --

    /// Open the model selector popup for the focused domain.
    pub fn open_model_selector(&mut self) {
        if self.rows.is_empty() || self.cursor >= self.rows.len() {
            return;
        }
        if self.models.is_empty() {
            self.models = fetch_opencode_models();
        }
        self.model_selector_open = true;
        self.model_cursor = 0;
        // If the domain already has a model, try to position the cursor on it.
        if let Some(row) = self.rows.get(self.cursor)
            && let Some(ref model) = row.spec.model
            && let Some(idx) = self.filtered_models().iter().position(|m| m.id == *model)
        {
            self.model_cursor = idx;
        }
    }

    /// Close the model selector without applying.
    pub fn close_model_selector(&mut self) {
        self.model_selector_open = false;
    }

    /// Toggle the free-only filter in the model selector.
    pub fn toggle_free_only(&mut self) {
        self.free_only = !self.free_only;
        self.model_cursor = 0;
    }

    /// Move the model selector cursor up.
    pub fn model_cursor_up(&mut self) {
        if self.model_cursor > 0 {
            self.model_cursor -= 1;
        }
    }

    /// Move the model selector cursor down.
    pub fn model_cursor_down(&mut self) {
        let len = self.filtered_models().len();
        if len > 0 && self.model_cursor + 1 < len {
            self.model_cursor += 1;
        }
    }

    /// Apply the selected model to the focused domain and close the popup.
    pub fn apply_model(&mut self) {
        let selected_id: Option<String> = self
            .filtered_models()
            .get(self.model_cursor)
            .map(|m| m.id.clone());
        if let Some(id) = selected_id
            && let Some(row) = self.rows.get_mut(self.cursor)
        {
            row.spec.model = Some(id.clone());
            self.dirty = true;
            self.status = Some(format!("model set to {id} for {}", row.spec.name));
        }
        self.model_selector_open = false;
    }

    /// Clear the focused domain's model (revert to project default).
    pub fn clear_model(&mut self) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.spec.model = None;
            self.dirty = true;
            self.status = Some(format!("model cleared for {}", row.spec.name));
        }
        self.model_selector_open = false;
    }

    /// Set the same model on all included domains.
    pub fn set_model_for_all(&mut self, model_id: &str) {
        for row in &mut self.rows {
            if row.include {
                row.spec.model = Some(model_id.to_string());
            }
        }
        self.dirty = true;
        self.status = Some(format!("model set to {model_id} for all agents"));
    }

    /// Set the same model on all included domains using the currently
    /// selected model in the popup.  Convenience: press 'a' in the popup.
    pub fn apply_model_to_all(&mut self) {
        let selected_id: Option<String> = self
            .filtered_models()
            .get(self.model_cursor)
            .map(|m| m.id.clone());
        if let Some(id) = selected_id {
            self.set_model_for_all(&id);
        }
        self.model_selector_open = false;
    }

    /// The list of models currently visible (respecting free_only filter).
    pub fn filtered_models(&self) -> Vec<&ModelEntry> {
        if self.free_only {
            self.models.iter().filter(|m| m.is_free).collect()
        } else {
            self.models.iter().collect()
        }
    }
}

// ----------------------------------------------------------------------------
// Thin I/O shell.
// ----------------------------------------------------------------------------

/// Entry point for `trelane biplane --ui`. Loads the stored description if one
/// exists, otherwise scaffolds from the project structure, then runs the
/// editor. No-ops with a message when stdout is not a TTY.
///
/// For empty projects (no source files), falls through to a "describe your
/// project" prompt that uses an LLM to generate the initial description.
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
        let scaffolded = crate::biplane::scaffold_description_from_structure(root);
        if scaffolded.domains.is_empty() {
            // Empty project: ask the user to describe it, then use an LLM
            // to generate the domain split.
            return run_empty_project_flow(root);
        }
        (
            scaffolded,
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

/// Flow for empty projects: show a text input asking the user to describe
/// their project, then use an LLM to generate a domain split from that
/// description.  The result is loaded into the normal Biplane UI for
/// review/editing before the user decides whether to begin work.
fn run_empty_project_flow(root: &std::path::Path) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::prelude::*;
    #[allow(unused_imports)]
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    use std::time::Duration;

    // Phase 1: collect the project description from the user.
    let mut description_input = crate::text_input::TextInput::new();
    let mut max_agents_input = String::from("3");
    let mut focused_field = 0usize; // 0 = description, 1 = max_agents
    let mut phase = EmptyProjectPhase::Input;
    let mut status_msg = String::new();
    let mut generated_desc: Option<ProjectDescription> = None;

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let outcome = (|| -> Result<()> {
        loop {
            terminal.draw(|f| {
                render_empty_project_input(
                    f,
                    &description_input,
                    &max_agents_input,
                    focused_field,
                    &phase,
                    &status_msg,
                )
            })?;

            match &phase {
                EmptyProjectPhase::Input => {
                    if event::poll(Duration::from_millis(250))?
                        && let Event::Key(key) = event::read()?
                        && key.kind == KeyEventKind::Press
                    {
                        match key.code {
                            KeyCode::Tab => focused_field = 1 - focused_field,
                            KeyCode::Enter if focused_field == 1 => {
                                // Generate
                                let desc_text = description_input.value().trim().to_string();
                                if desc_text.is_empty() {
                                    status_msg = "Please describe your project first.".into();
                                    continue;
                                }
                                let max_agents: usize =
                                    max_agents_input.trim().parse().unwrap_or(3).clamp(1, 10);
                                phase = EmptyProjectPhase::Generating;
                                status_msg =
                                    format!("Analyzing with LLM (max {} agents)...", max_agents);
                                terminal.draw(|f| {
                                    render_empty_project_input(
                                        f,
                                        &description_input,
                                        &max_agents_input,
                                        focused_field,
                                        &phase,
                                        &status_msg,
                                    )
                                })?;

                                // Run the LLM planner.
                                let plan = crate::biplane::run_biplane_plan_from_description(
                                    root, &desc_text, max_agents,
                                );
                                match plan {
                                    Ok(desc) => {
                                        generated_desc = Some(desc);
                                        phase = EmptyProjectPhase::Review;
                                        status_msg =
                                            "Plan generated. Review and edit below.".into();
                                    }
                                    Err(e) => {
                                        status_msg = format!("Failed: {e}");
                                        phase = EmptyProjectPhase::Input;
                                    }
                                }
                            }
                            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                            KeyCode::Backspace => {
                                if focused_field == 0 {
                                    description_input.backspace();
                                } else {
                                    max_agents_input.pop();
                                }
                            }
                            KeyCode::Left => {
                                if focused_field == 0 {
                                    description_input.move_left();
                                }
                            }
                            KeyCode::Right => {
                                if focused_field == 0 {
                                    description_input.move_right();
                                }
                            }
                            KeyCode::Char(c) => {
                                if focused_field == 0 {
                                    description_input.insert(c);
                                } else if c.is_ascii_digit() {
                                    max_agents_input.push(c);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                EmptyProjectPhase::Generating => {
                    // Handled inline above; just keep the loop alive.
                    std::thread::sleep(Duration::from_millis(50));
                }
                EmptyProjectPhase::Review => {
                    // Drop out of the alt screen and hand off to the normal
                    // Biplane UI editor with the generated description.
                    if let Some(desc) = generated_desc.take() {
                        // Save it so the editor can load from file on future runs.
                        save_description(root, &desc)?;
                        // Exit this loop; the caller will start the editor.
                        return Ok(());
                    }
                    return Ok(());
                }
            }
        }
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    outcome?;

    // If we got a generated description, launch the normal editor.
    let desc_path = root.join(".trelane").join("biplane-description.json");
    if desc_path.exists() {
        let desc = crate::biplane::load_project_description(&desc_path)?;
        let mut state =
            BiplaneUiState::from_description(&desc, "generated from project description");
        run_loop(root, &mut state)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyProjectPhase {
    Input,
    Generating,
    Review,
}

fn render_empty_project_input(
    f: &mut ratatui::Frame,
    desc_input: &crate::text_input::TextInput,
    max_agents_input: &str,
    focused: usize,
    phase: &EmptyProjectPhase,
    status: &str,
) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK, THEME_WARN};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // header
            Constraint::Min(8),    // description input
            Constraint::Length(3), // max agents
            Constraint::Length(3), // status / hints
        ])
        .split(f.area());

    // Header
    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            "Biplane :: New Project",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "This project is empty. Describe what you want to build and Biplane\nwill propose a domain split using an LLM.",
            Style::default().fg(dim),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(accent)));
    f.render_widget(header, chunks[0]);

    // Description input
    let desc_focused = focused == 0 && *phase == EmptyProjectPhase::Input;
    let desc_display = if desc_focused {
        desc_input.render_with_caret()
    } else {
        desc_input.value().to_string()
    };
    let desc_style = if desc_focused {
        Style::default().fg(accent)
    } else {
        Style::default().fg(dim)
    };
    let desc_title = if desc_focused {
        " Project Description (editing) "
    } else {
        " Project Description "
    };
    let desc_para = Paragraph::new(desc_display)
        .style(desc_style)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(desc_title)
                .border_style(Style::default().fg(accent)),
        );
    f.render_widget(desc_para, chunks[1]);

    // Max agents input
    let ma_focused = focused == 1 && *phase == EmptyProjectPhase::Input;
    let ma_style = if ma_focused {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(dim)
    };
    let ma_title = if ma_focused {
        " Max Agents (editing) "
    } else {
        " Max Agents "
    };
    let ma_para = Paragraph::new(format!("{}  (press Enter to generate)", max_agents_input))
        .style(ma_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(ma_title)
                .border_style(Style::default().fg(accent)),
        );
    f.render_widget(ma_para, chunks[2]);

    // Status / hints
    let status_color = if status.starts_with("Failed") {
        tc(THEME_WARN)
    } else if status.starts_with("Plan generated") {
        tc(THEME_OK)
    } else {
        dim
    };
    let hint = if *phase == EmptyProjectPhase::Generating {
        status.to_string()
    } else {
        format!(
            "{}  |  Tab: switch field  Enter: generate  Esc: quit",
            status
        )
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(status_color),
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(dim)),
    );
    f.render_widget(footer, chunks[3]);
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
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
            {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Model selector popup: keys route here when open.
                if state.model_selector_open {
                    match key.code {
                        KeyCode::Esc => state.close_model_selector(),
                        KeyCode::Up => state.model_cursor_up(),
                        KeyCode::Down => state.model_cursor_down(),
                        KeyCode::Enter => state.apply_model(),
                        KeyCode::Char('f') => state.toggle_free_only(),
                        KeyCode::Char('a') => state.apply_model_to_all(),
                        KeyCode::Char('c') => state.clear_model(),
                        _ => {}
                    }
                    continue;
                }
                // Edit mode: keys flow into the rename buffer.
                if state.is_editing() {
                    match key.code {
                        KeyCode::Enter => {
                            state.commit_edit();
                        }
                        KeyCode::Esc => state.cancel_edit(),
                        KeyCode::Backspace => state.edit_backspace(),
                        KeyCode::Left => state.edit_caret_left(),
                        KeyCode::Right => state.edit_caret_right(),
                        KeyCode::Char(c) => state.edit_insert(c),
                        _ => {}
                    }
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
                    KeyCode::Char('e') => state.begin_rename(),
                    KeyCode::Char('w') => state.begin_edit_work(0),
                    KeyCode::Char('W') => state.begin_edit_writable(0),
                    KeyCode::Char('a') => state.add_domain(),
                    KeyCode::Char('D') => state.remove_domain(),
                    KeyCode::Char('+') => state.add_work(),
                    KeyCode::Char('-') => state.remove_work(0),
                    KeyCode::Char('m') => state.open_model_selector(),
                    KeyCode::Char('M') => {
                        // Quick: set all included domains to the first free model
                        let models = fetch_opencode_models();
                        if let Some(free) = models.iter().find(|m| m.is_free) {
                            state.set_model_for_all(&free.id);
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some(desc) = state.validated() {
                            save_description(root, &desc)?;
                            state.mark_saved();
                        }
                    }
                    _ => {}
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
            // When editing the focused row, show the live buffer with a caret
            // in the cell matching the edit target.
            let editing_name = i == state.cursor
                && matches!(
                    state.editing.as_ref().map(|e| &e.target),
                    Some(EditTarget::DomainName)
                );
            let editing_work = i == state.cursor
                && matches!(
                    state.editing.as_ref().map(|e| &e.target),
                    Some(EditTarget::WorkSubject { .. })
                );
            let name_cell = if editing_name {
                format!(" {:<16}", state.edit_input().unwrap().render_with_caret())
            } else {
                format!(" {:<16}", row.spec.name)
            };
            let name_span = if editing_name {
                Span::styled(
                    name_cell,
                    Style::default()
                        .fg(tc(THEME_WARN))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(name_cell, name_style)
            };
            let work_cell = if editing_work {
                format!("work:{} ", state.edit_input().unwrap().render_with_caret())
            } else {
                format!("work:{:<3} ", row.spec.planned_work.len())
            };
            let work_span = if editing_work {
                Span::styled(
                    work_cell,
                    Style::default()
                        .fg(tc(THEME_WARN))
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(work_cell, Style::default().fg(dim))
            };
            let model_str = row.spec.model.as_deref().unwrap_or("(default)");
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                check,
                name_span,
                Span::styled(
                    format!("agents:{:<3} ", row.spec.agents),
                    Style::default().fg(dim),
                ),
                work_span,
                Span::styled(format!("deps:{:<12} ", deps), Style::default().fg(dim)),
                Span::styled(
                    format!("model:{} ", model_str),
                    Style::default().fg(if row.spec.model.is_some() {
                        accent
                    } else {
                        dim
                    }),
                ),
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

    // Model selector popup
    if state.model_selector_open {
        render_model_selector(f, state);
    }

    // Footer
    let hint = state.status.clone().unwrap_or_else(|| {
        if state.model_selector_open {
            "↑↓ select model  Enter apply  f free-only  a apply-to-all  c clear  Esc close".to_string()
        } else if state.editing.is_some() {
            "typing… Enter save  Esc cancel  ←→ move caret  Backspace delete".to_string()
        } else {
            "↑↓ move  space include  ←→ agents  [ ] budget  K/J reorder  e rename  w work  W glob  a add-domain  D del-domain  + add-work  - del-work  m model  s save  q quit"
                .to_string()
        }
    });
    let footer = Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim)))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(dim)),
    );
    f.render_widget(footer, chunks[2]);
}

fn render_model_selector(f: &mut ratatui::Frame, state: &BiplaneUiState) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);

    let area = centered_rect_pct(60, 60, f.area());
    f.render_widget(Clear, area);

    let filtered = state.filtered_models();
    let title = if state.free_only {
        " Select Model (free only) "
    } else {
        " Select Model (all) "
    };

    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let marker = if i == state.model_cursor {
                "▶ "
            } else {
                "  "
            };
            let free_tag = if m.is_free {
                Span::styled(" [FREE]", Style::default().fg(tc(THEME_OK)))
            } else {
                Span::raw("")
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(&m.id, Style::default().fg(accent)),
                free_tag,
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(list, area);

    // Hint line at the bottom of the popup
    let hint_area = Rect {
        x: area.x,
        y: area.bottom().saturating_sub(1),
        width: area.width,
        height: 1,
    };
    let hint = format!(
        "  {} models  |  f: free-only({})  a: all  c: clear  Esc: close",
        filtered.len(),
        if state.free_only { "ON" } else { "off" }
    );
    let hint_para = Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim))));
    f.render_widget(hint_para, hint_area);
}

fn centered_rect_pct(pct_x: u16, pct_y: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
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

    #[test]
    fn begin_rename_seeds_buffer_with_current_name() {
        let mut s = state();
        s.cursor = 1; // ui
        s.begin_rename();
        assert!(s.is_editing());
        assert_eq!(s.editing.as_ref().unwrap().input.value(), "ui");
    }

    #[test]
    fn commit_rename_applies_and_rewires_dependents() {
        let mut s = state();
        s.cursor = 0; // engine; ui and api both depend on it
        s.begin_rename();
        // clear buffer and type a new name
        s.editing.as_mut().unwrap().input.clear();
        for c in "core".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_rename());
        assert_eq!(s.rows[0].spec.name, "core");
        // dependents rewired
        let ui = s.rows.iter().find(|r| r.spec.name == "ui").unwrap();
        let api = s.rows.iter().find(|r| r.spec.name == "api").unwrap();
        assert!(ui.spec.depends_on.contains(&"core".to_string()));
        assert!(api.spec.depends_on.contains(&"core".to_string()));
        assert!(!s.is_editing());
        assert!(s.dirty);
    }

    #[test]
    fn commit_rename_rejects_empty_name() {
        let mut s = state();
        s.cursor = 0;
        s.begin_rename();
        s.editing.as_mut().unwrap().input.clear();
        assert!(!s.commit_rename());
        assert!(s.is_editing()); // stays open to fix
        assert!(s.last_error.is_some());
        assert_eq!(s.rows[0].spec.name, "engine"); // unchanged
    }

    #[test]
    fn commit_rename_rejects_duplicate_name() {
        let mut s = state();
        s.cursor = 0; // engine
        s.begin_rename();
        s.editing.as_mut().unwrap().input.clear();
        for c in "ui".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_rename());
        assert!(s.is_editing());
        assert!(s.last_error.as_ref().unwrap().contains("already exists"));
        assert_eq!(s.rows[0].spec.name, "engine");
    }

    #[test]
    fn commit_rename_to_same_name_closes_without_dirtying() {
        let mut s = state();
        s.cursor = 0;
        s.begin_rename(); // buffer already "engine"
        assert!(!s.commit_rename()); // no change
        assert!(!s.is_editing());
        assert!(!s.dirty);
    }

    #[test]
    fn cancel_edit_discards_buffer() {
        let mut s = state();
        s.cursor = 0;
        s.begin_rename();
        s.edit_insert('X');
        s.cancel_edit();
        assert!(!s.is_editing());
        assert_eq!(s.rows[0].spec.name, "engine"); // unchanged
    }

    #[test]
    fn begin_edit_work_seeds_subject() {
        let mut s = state();
        s.cursor = 0; // engine, planned_work[0] = "build engine"
        s.begin_edit_work(0);
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "build engine");
    }

    #[test]
    fn commit_work_subject_applies_and_dirties() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_work(0);
        s.editing.as_mut().unwrap().input.clear();
        for c in "wire the turn loop".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[0].spec.planned_work[0].subject, "wire the turn loop");
        assert!(!s.is_editing());
        assert!(s.dirty);
    }

    #[test]
    fn commit_work_subject_rejects_empty() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_work(0);
        s.editing.as_mut().unwrap().input.clear();
        assert!(!s.commit_edit());
        assert!(s.is_editing()); // stays open to fix
        assert!(s.last_error.is_some());
        assert_eq!(s.rows[0].spec.planned_work[0].subject, "build engine");
    }

    #[test]
    fn begin_edit_work_without_work_sets_error() {
        let mut s = state();
        s.cursor = 0;
        s.rows[0].spec.planned_work.clear();
        s.begin_edit_work(0);
        assert!(!s.is_editing());
        assert!(s.last_error.is_some());
    }

    #[test]
    fn commit_edit_dispatches_rename_via_generic_entry() {
        let mut s = state();
        s.cursor = 1; // ui
        s.begin_rename();
        s.editing.as_mut().unwrap().input.clear();
        for c in "frontend".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[1].spec.name, "frontend");
    }

    #[test]
    fn work_edit_does_not_touch_domain_name() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_work(0);
        s.editing.as_mut().unwrap().input.clear();
        s.edit_insert('x');
        s.commit_edit();
        assert_eq!(s.rows[0].spec.name, "engine"); // name untouched
    }

    #[test]
    fn model_selector_opens_and_closes() {
        let mut s = state();
        assert!(!s.model_selector_open);
        s.open_model_selector();
        assert!(s.model_selector_open);
        s.close_model_selector();
        assert!(!s.model_selector_open);
    }

    #[test]
    fn apply_model_sets_domain_model() {
        let mut s = state();
        s.cursor = 0;
        s.models = vec![
            ModelEntry {
                id: "opencode/big-pickle".into(),
                is_free: false,
            },
            ModelEntry {
                id: "opencode/deepseek-v4-flash-free".into(),
                is_free: true,
            },
        ];
        s.open_model_selector();
        s.model_cursor = 1;
        s.apply_model();
        assert!(!s.model_selector_open);
        assert_eq!(
            s.rows[0].spec.model.as_deref(),
            Some("opencode/deepseek-v4-flash-free")
        );
        assert!(s.dirty);
    }

    #[test]
    fn free_only_filter_hides_paid() {
        let mut s = state();
        s.models = vec![
            ModelEntry {
                id: "paid-model".into(),
                is_free: false,
            },
            ModelEntry {
                id: "free-model:free".into(),
                is_free: true,
            },
        ];
        assert_eq!(s.filtered_models().len(), 2);
        s.toggle_free_only();
        assert_eq!(s.filtered_models().len(), 1);
        assert_eq!(s.filtered_models()[0].id, "free-model:free");
    }

    #[test]
    fn set_model_for_all_updates_included_only() {
        let mut s = state();
        s.cursor = 1; // ui
        s.toggle_include(); // exclude ui
        s.set_model_for_all("test-model");
        assert_eq!(s.rows[0].spec.model.as_deref(), Some("test-model")); // engine
        assert_eq!(s.rows[1].spec.model, None); // ui excluded
        assert_eq!(s.rows[2].spec.model.as_deref(), Some("test-model")); // api
        assert!(s.dirty);
    }

    #[test]
    fn clear_model_removes_assignment() {
        let mut s = state();
        s.rows[0].spec.model = Some("test".into());
        s.cursor = 0;
        s.clear_model();
        assert_eq!(s.rows[0].spec.model, None);
        assert!(s.dirty);
    }

    #[test]
    fn model_cursor_is_bounded() {
        let mut s = state();
        s.models = vec![
            ModelEntry {
                id: "a".into(),
                is_free: false,
            },
            ModelEntry {
                id: "b".into(),
                is_free: false,
            },
        ];
        s.open_model_selector();
        s.model_cursor_down();
        assert_eq!(s.model_cursor, 1);
        s.model_cursor_down(); // at end, should not advance
        assert_eq!(s.model_cursor, 1);
        s.model_cursor_up();
        assert_eq!(s.model_cursor, 0);
        s.model_cursor_up(); // at top
        assert_eq!(s.model_cursor, 0);
    }

    #[test]
    fn begin_edit_writable_seeds_buffer() {
        let mut s = state();
        s.cursor = 0; // engine, writable ["src/engine/**"]
        s.begin_edit_writable(0);
        assert!(s.is_editing());
        assert_eq!(s.editing.as_ref().unwrap().input.value(), "src/engine/**");
    }

    #[test]
    fn commit_writable_updates_glob() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_writable(0);
        s.editing.as_mut().unwrap().input.clear();
        for c in "src/core/**".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[0].spec.writable[0], "src/core/**");
        assert!(s.dirty);
        assert!(!s.is_editing());
    }

    #[test]
    fn commit_writable_rejects_empty() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_writable(0);
        s.editing.as_mut().unwrap().input.clear();
        assert!(!s.commit_edit());
        assert!(s.is_editing()); // stays open
        assert!(s.last_error.is_some());
    }

    #[test]
    fn add_domain_appends_and_focuses() {
        let mut s = state();
        let initial_len = s.rows.len();
        s.add_domain();
        assert_eq!(s.rows.len(), initial_len + 1);
        assert_eq!(s.cursor, initial_len); // focused on new domain
        assert!(s.dirty);
        assert!(s.rows[s.cursor].spec.name.starts_with("domain-"));
    }

    #[test]
    fn add_domain_avoids_name_collision() {
        let mut s = state();
        // Add a domain named "domain-1" manually
        s.rows.push(DomainRow {
            spec: DomainSpec {
                name: "domain-1".into(),
                description: "".into(),
                writable: vec!["x/**".into()],
                forbidden_write: vec![],
                depends_on: vec![],
                planned_work: vec![],
                agents: 1,
                model: None,
            },
            include: true,
        });
        s.add_domain();
        // Should skip "domain-1" and use "domain-2"
        let last = s.rows.last().unwrap();
        assert_eq!(last.spec.name, "domain-2");
    }

    #[test]
    fn remove_domain_deletes_and_prunes_deps() {
        let mut s = state();
        // engine is at index 0; ui and api depend on it
        s.cursor = 0;
        s.remove_domain();
        // engine should be gone
        assert!(!s.rows.iter().any(|r| r.spec.name == "engine"));
        // ui and api should have empty depends_on (pruned)
        assert!(s.rows.iter().all(|r| r.spec.depends_on.is_empty()));
        assert!(s.dirty);
    }

    #[test]
    fn remove_domain_adjusts_cursor() {
        let mut s = state();
        s.cursor = 2; // api (last)
        s.remove_domain();
        // cursor should clamp to last remaining row
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn add_work_appends_to_focused_domain() {
        let mut s = state();
        s.cursor = 0; // engine has 1 work item
        let initial_work = s.rows[0].spec.planned_work.len();
        s.add_work();
        assert_eq!(s.rows[0].spec.planned_work.len(), initial_work + 1);
        assert_eq!(
            s.rows[0].spec.planned_work.last().unwrap().subject,
            "new task"
        );
        assert!(s.dirty);
    }

    #[test]
    fn remove_work_deletes_at_index() {
        let mut s = state();
        s.cursor = 0;
        s.add_work(); // now 2 items
        let len_before = s.rows[0].spec.planned_work.len();
        s.remove_work(0);
        assert_eq!(s.rows[0].spec.planned_work.len(), len_before - 1);
        assert!(s.dirty);
    }

    #[test]
    fn remove_work_out_of_bounds_is_noop() {
        let mut s = state();
        s.cursor = 0;
        let len = s.rows[0].spec.planned_work.len();
        s.remove_work(99);
        assert_eq!(s.rows[0].spec.planned_work.len(), len);
        assert!(!s.dirty);
    }
}
