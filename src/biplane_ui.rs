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
    /// Edit the comma-separated writable globs of the focused domain.
    Writable,
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
    /// Emergent domains surfaced by a reconciliation report, awaiting the
    /// user's accept/reject decision. Empty when there is nothing to review.
    pub pending_suggestions: Vec<DomainSpec>,
    /// Cursor within the pending-suggestions review overlay.
    pub suggestion_cursor: usize,
    /// True when the reconciliation review overlay is open.
    pub reviewing_suggestions: bool,
    /// Human-readable stalled-domain notices (read-only) from the last report.
    pub stalled_notices: Vec<String>,
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
            pending_suggestions: Vec::new(),
            suggestion_cursor: 0,
            reviewing_suggestions: false,
            stalled_notices: Vec::new(),
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

    /// Valid planned-work priority levels, low→high. Mirrors the set accepted
    /// by `biplane::normalize_urgency`.
    pub const PRIORITIES: [&'static str; 4] = ["low", "normal", "high", "critical"];

    /// Cycle the focused domain's first planned-work item priority to the next
    /// (or previous) level. No-op if the domain has no planned work.
    pub fn cycle_work_priority(&mut self, forward: bool) {
        let Some(row) = self.rows.get_mut(self.cursor) else {
            return;
        };
        let Some(work) = row.spec.planned_work.first_mut() else {
            self.last_error = Some("this domain has no planned work".to_string());
            return;
        };
        let cur = Self::PRIORITIES
            .iter()
            .position(|p| *p == work.priority)
            .unwrap_or(1); // default to "normal" if unrecognized
        let n = Self::PRIORITIES.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        work.priority = Self::PRIORITIES[next].to_string();
        self.last_error = None;
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

    // -- Reconciliation: accept/reject emergent domains, view stalled ones --

    /// Ingest a reconciliation report: queue its emergent domains as pending
    /// suggestions for accept/reject, and record human-readable notices for
    /// any stalled domains (including cycle members when present). Opens the
    /// review overlay when there is at least one emergent suggestion.
    pub fn ingest_reconciliation(&mut self, report: &crate::biplane::ReconciliationReport) {
        // Only queue emergent domains whose names aren't already present, so a
        // report applied twice doesn't create duplicates.
        let existing: std::collections::HashSet<String> =
            self.rows.iter().map(|r| r.spec.name.clone()).collect();
        self.pending_suggestions = report
            .emergent_domains
            .iter()
            .filter(|d| !existing.contains(&d.name))
            .cloned()
            .collect();
        self.suggestion_cursor = 0;
        self.reviewing_suggestions = !self.pending_suggestions.is_empty();

        self.stalled_notices = report
            .stalled_domains
            .iter()
            .map(|s| match &s.blocked_by_cycle {
                Some(cycle) => format!("{}: blocked by cycle [{}]", s.domain, cycle.join(" → ")),
                None => format!("{}: {}", s.domain, s.evidence),
            })
            .collect();
    }

    /// Number of emergent suggestions still awaiting a decision.
    pub fn pending_count(&self) -> usize {
        self.pending_suggestions.len()
    }

    fn clamp_suggestion_cursor(&mut self) {
        if self.pending_suggestions.is_empty() {
            self.suggestion_cursor = 0;
            self.reviewing_suggestions = false;
        } else if self.suggestion_cursor >= self.pending_suggestions.len() {
            self.suggestion_cursor = self.pending_suggestions.len() - 1;
        }
    }

    pub fn suggestion_up(&mut self) {
        if self.suggestion_cursor > 0 {
            self.suggestion_cursor -= 1;
        }
    }

    pub fn suggestion_down(&mut self) {
        if self.suggestion_cursor + 1 < self.pending_suggestions.len() {
            self.suggestion_cursor += 1;
        }
    }

    /// Accept the focused suggestion: append it as an included domain and
    /// remove it from the pending queue. Returns the accepted domain name.
    pub fn accept_suggestion(&mut self) -> Option<String> {
        if self.suggestion_cursor >= self.pending_suggestions.len() {
            return None;
        }
        let spec = self.pending_suggestions.remove(self.suggestion_cursor);
        let name = spec.name.clone();
        self.rows.push(DomainRow { spec, include: true });
        self.dirty = true;
        self.clamp_suggestion_cursor();
        Some(name)
    }

    /// Reject the focused suggestion: drop it from the pending queue without
    /// adding it. Returns the rejected domain name.
    pub fn reject_suggestion(&mut self) -> Option<String> {
        if self.suggestion_cursor >= self.pending_suggestions.len() {
            return None;
        }
        let name = self.pending_suggestions.remove(self.suggestion_cursor).name;
        self.clamp_suggestion_cursor();
        Some(name)
    }

    /// Close the review overlay, leaving any undecided suggestions queued.
    pub fn close_review(&mut self) {
        self.reviewing_suggestions = false;
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

    /// Begin editing the focused domain's writable globs, seeded with the
    /// current globs joined by ", ".
    pub fn begin_edit_writable(&mut self) {
        if let Some(row) = self.rows.get(self.cursor) {
            let seed = row.spec.writable.join(", ");
            self.editing = Some(ActiveEdit {
                input: crate::text_input::TextInput::with_text(&seed),
                target: EditTarget::Writable,
            });
            self.last_error = None;
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
            EditTarget::Writable => self.commit_writable(),
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

    /// Commit the in-progress writable-globs edit. Parses the comma-separated
    /// buffer into globs; rejects an empty set (keeping the editor open).
    fn commit_writable(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let globs: Vec<String> = edit
            .input
            .value()
            .split(',')
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect();
        if globs.is_empty() {
            self.last_error = Some("a domain needs at least one writable glob".to_string());
            return false;
        }
        match self.rows.get_mut(self.cursor) {
            Some(row) => {
                if row.spec.writable == globs {
                    self.editing = None;
                    self.last_error = None;
                    return false;
                }
                row.spec.writable = globs;
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

    // -- Structural edits: add/remove domains and planned-work items --

    /// Add a new domain with a unique placeholder name and a single writable
    /// glob, inserted after the cursor, and focus it. Returns the new name.
    pub fn add_domain(&mut self) -> String {
        let mut n = self.rows.len() + 1;
        let mut name = format!("domain-{n}");
        while self.rows.iter().any(|r| r.spec.name == name) {
            n += 1;
            name = format!("domain-{n}");
        }
        let spec = DomainSpec {
            name: name.clone(),
            description: String::new(),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: vec![],
            depends_on: vec![],
            planned_work: vec![],
            agents: 1,
            model: None,
        };
        let insert_at = if self.rows.is_empty() {
            0
        } else {
            (self.cursor + 1).min(self.rows.len())
        };
        self.rows.insert(insert_at, DomainRow { spec, include: true });
        self.cursor = insert_at;
        self.dirty = true;
        name
    }

    /// Remove the focused domain, pruning any other domain's dependency edges
    /// that referenced it. No-op if there are no rows. Returns true if removed.
    pub fn remove_domain(&mut self) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let removed = self.rows.remove(self.cursor).spec.name;
        for row in self.rows.iter_mut() {
            row.spec.depends_on.retain(|d| d != &removed);
        }
        if self.cursor >= self.rows.len() && self.cursor > 0 {
            self.cursor -= 1;
        }
        self.dirty = true;
        true
    }

    /// Append a new planned-work item to the focused domain with a placeholder
    /// subject, and immediately open it for editing. Returns the new index.
    pub fn add_work_item(&mut self) -> Option<usize> {
        let idx = {
            let row = self.rows.get_mut(self.cursor)?;
            row.spec.planned_work.push(crate::biplane::PlannedWork {
                subject: "new task".to_string(),
                body: String::new(),
                priority: "normal".to_string(),
            });
            row.spec.planned_work.len() - 1
        };
        self.dirty = true;
        self.begin_edit_work(idx);
        Some(idx)
    }

    /// Remove planned-work item `index` from the focused domain. Returns true
    /// if an item was removed.
    pub fn remove_work_item(&mut self, index: usize) -> bool {
        match self.rows.get_mut(self.cursor) {
            Some(row) if index < row.spec.planned_work.len() => {
                row.spec.planned_work.remove(index);
                self.dirty = true;
                true
            }
            _ => false,
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

    // Best-effort: if a live session DB is present, run a reconciliation scan
    // and surface any emergent/stalled domains for accept/reject review. This
    // is purely additive — failure (e.g. no session yet) leaves the editor in
    // its normal pre-session state rather than aborting.
    if let Ok(ctx) = crate::Context::open(Some(root)) {
        if let Ok(report) = crate::biplane::reconcile_against_reality(&ctx, &desc) {
            state.ingest_reconciliation(&report);
        }
    }
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
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(key) = event::read()?
            {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Reconciliation review overlay: keys route here when open.
                if state.reviewing_suggestions {
                    match key.code {
                        KeyCode::Esc => state.close_review(),
                        KeyCode::Up => state.suggestion_up(),
                        KeyCode::Down => state.suggestion_down(),
                        KeyCode::Char('y') | KeyCode::Enter => {
                            state.accept_suggestion();
                        }
                        KeyCode::Char('n') => {
                            state.reject_suggestion();
                        }
                        _ => {}
                    }
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
                    KeyCode::Char('g') => state.begin_edit_writable(),
                    KeyCode::Char('a') => {
                        state.add_domain();
                    }
                    KeyCode::Char('d') => {
                        state.remove_domain();
                    }
                    KeyCode::Char('A') => {
                        state.add_work_item();
                    }
                    KeyCode::Char('D') => {
                        // Remove the last planned-work item of the focused domain.
                        if let Some(row) = state.rows.get(state.cursor) {
                            let n = row.spec.planned_work.len();
                            if n > 0 {
                                state.remove_work_item(n - 1);
                            }
                        }
                    }
                    KeyCode::Char('p') => state.cycle_work_priority(true),
                    KeyCode::Char('P') => state.cycle_work_priority(false),
                    KeyCode::Char('r') => {
                        // Reopen the review overlay if suggestions remain.
                        if state.pending_count() > 0 {
                            state.reviewing_suggestions = true;
                        }
                    }
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
            let target = state.editing.as_ref().map(|e| &e.target);
            let editing_name =
                i == state.cursor && matches!(target, Some(EditTarget::DomainName));
            let editing_work =
                i == state.cursor && matches!(target, Some(EditTarget::WorkSubject { .. }));
            let editing_writable =
                i == state.cursor && matches!(target, Some(EditTarget::Writable));
            let warn_bold = Style::default().fg(tc(THEME_WARN)).add_modifier(Modifier::BOLD);

            let name_cell = if editing_name {
                format!(" {:<16}", state.edit_input().unwrap().render_with_caret())
            } else {
                format!(" {:<16}", row.spec.name)
            };
            let name_span = if editing_name {
                Span::styled(name_cell, warn_bold)
            } else {
                Span::styled(name_cell, name_style)
            };
            let work_cell = if editing_work {
                format!("work:{} ", state.edit_input().unwrap().render_with_caret())
            } else {
                let prio = row
                    .spec
                    .planned_work
                    .first()
                    .map(|w| w.priority.as_str())
                    .unwrap_or("-");
                format!("work:{:<2}[{}] ", row.spec.planned_work.len(), prio)
            };
            let work_span = if editing_work {
                Span::styled(work_cell, warn_bold)
            } else {
                Span::styled(work_cell, Style::default().fg(dim))
            };
            let writable_str = if editing_writable {
                state.edit_input().unwrap().render_with_caret()
            } else {
                row.spec.writable.join(",")
            };
            let writable_span = if editing_writable {
                Span::styled(writable_str, warn_bold)
            } else {
                Span::styled(writable_str, Style::default().fg(dim))
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
                writable_span,
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

    // Reconciliation review overlay
    if state.reviewing_suggestions {
        render_review_overlay(f, state);
    }

    // Footer
    let hint = state.status.clone().unwrap_or_else(|| {
        if state.reviewing_suggestions {
            "↑↓ select  y/Enter accept  n reject  Esc close review".to_string()
        } else if state.model_selector_open {
            "↑↓ select model  Enter apply  f free-only  a apply-to-all  c clear  Esc close".to_string()
        } else if state.editing.is_some() {
            "typing… Enter save  Esc cancel  ←→ move caret  Backspace delete".to_string()
        } else {
            let base = "↑↓ move  spc incl  ←→ agents  [ ] budget  K/J reorder  e name  w work  g globs  p prio  a/d domain  A/D task  m model  s save  q quit";
            if state.pending_count() > 0 {
                format!("{} · r review ({})", base, state.pending_count())
            } else {
                base.to_string()
            }
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

/// Reconciliation review overlay: pending emergent-domain suggestions to
/// accept/reject, plus a read-only list of stalled-domain notices.
fn render_review_overlay(f: &mut ratatui::Frame, state: &BiplaneUiState) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK, THEME_WARN};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);

    let area = centered_rect_pct(70, 70, f.area());
    f.render_widget(Clear, area);

    // Split: suggestions list on top, stalled notices below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(6)])
        .split(area);

    let items: Vec<ListItem> = state
        .pending_suggestions
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let marker = if i == state.suggestion_cursor {
                "▶ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    format!("{:<16}", d.name),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("writable: {}", d.writable.join(",")),
                    Style::default().fg(dim),
                ),
            ]))
        })
        .collect();
    let title = format!(" Emergent Domains — review ({}) ", state.pending_suggestions.len());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(tc(THEME_OK))),
    );
    f.render_widget(list, rows[0]);

    // Stalled notices (read-only).
    let stalled_lines: Vec<Line> = if state.stalled_notices.is_empty() {
        vec![Line::from(Span::styled(
            "  no stalled domains",
            Style::default().fg(dim),
        ))]
    } else {
        state
            .stalled_notices
            .iter()
            .map(|n| Line::from(Span::styled(format!("  {n}"), Style::default().fg(tc(THEME_WARN)))))
            .collect()
    };
    let stalled = Paragraph::new(stalled_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Stalled (read-only) ")
            .border_style(Style::default().fg(tc(THEME_WARN))),
    );
    f.render_widget(stalled, rows[1]);
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

    // --- planned-work subject editing ---

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
        assert!(s.is_editing());
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

    // --- writable-glob editing ---

    #[test]
    fn begin_edit_writable_seeds_joined_globs() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_writable();
        assert_eq!(s.edit_input().unwrap().value(), "src/engine/**");
    }

    #[test]
    fn commit_writable_parses_comma_list() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_writable();
        s.editing.as_mut().unwrap().input.clear();
        for c in "src/a/**, src/b/**".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[0].spec.writable, vec!["src/a/**", "src/b/**"]);
        assert!(s.dirty);
    }

    #[test]
    fn commit_writable_rejects_empty_set() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_writable();
        s.editing.as_mut().unwrap().input.clear();
        for c in " , , ".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_edit());
        assert!(s.is_editing());
        assert!(s.last_error.is_some());
        assert_eq!(s.rows[0].spec.writable, vec!["src/engine/**"]); // unchanged
    }

    // --- add / remove domain ---

    #[test]
    fn add_domain_inserts_unique_and_focuses() {
        let mut s = state();
        s.cursor = 0;
        let before = s.rows.len();
        let name = s.add_domain();
        assert_eq!(s.rows.len(), before + 1);
        // inserted right after the cursor, and cursor follows it
        assert_eq!(s.rows[1].spec.name, name);
        assert_eq!(s.cursor, 1);
        assert!(s.dirty);
        // unique + at least one writable glob so it validates
        assert!(!name.is_empty());
        assert!(!s.rows[1].spec.writable.is_empty());
    }

    #[test]
    fn add_domain_then_validate_ok() {
        let mut s = state();
        s.add_domain();
        assert!(s.validated().is_some());
    }

    #[test]
    fn remove_domain_prunes_dependents_and_bounds_cursor() {
        let mut s = state();
        s.cursor = 0; // engine, depended on by ui and api
        assert!(s.remove_domain());
        assert!(s.rows.iter().all(|r| r.spec.name != "engine"));
        assert!(s.rows.iter().all(|r| !r.spec.depends_on.contains(&"engine".to_string())));
        assert!(s.cursor < s.rows.len());
    }

    #[test]
    fn remove_domain_at_end_moves_cursor_back() {
        let mut s = state();
        s.cursor = s.rows.len() - 1; // api
        s.remove_domain();
        assert_eq!(s.cursor, s.rows.len() - 1);
    }

    // --- add / remove planned-work item ---

    #[test]
    fn add_work_item_appends_and_opens_editor() {
        let mut s = state();
        s.cursor = 0;
        let before = s.rows[0].spec.planned_work.len();
        let idx = s.add_work_item().unwrap();
        assert_eq!(s.rows[0].spec.planned_work.len(), before + 1);
        assert_eq!(idx, before);
        // editor is open on the new item, seeded with its placeholder
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "new task");
    }

    #[test]
    fn remove_work_item_deletes() {
        let mut s = state();
        s.cursor = 0;
        assert!(s.remove_work_item(0));
        assert!(s.rows[0].spec.planned_work.is_empty());
        assert!(!s.remove_work_item(0)); // nothing left
    }

    #[test]
    fn commit_edit_dispatches_by_target() {
        // rename, work, and writable all route through the one entry point.
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

    // --- planned-work priority cycling ---

    #[test]
    fn cycle_work_priority_advances_and_wraps() {
        let mut s = state();
        s.cursor = 0; // engine, planned_work[0] priority "normal"
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "normal");
        s.cycle_work_priority(true); // normal -> high
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "high");
        s.cycle_work_priority(true); // high -> critical
        s.cycle_work_priority(true); // critical -> low (wrap)
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "low");
        assert!(s.dirty);
    }

    #[test]
    fn cycle_work_priority_backward() {
        let mut s = state();
        s.cursor = 0;
        s.cycle_work_priority(false); // normal -> low
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "low");
        s.cycle_work_priority(false); // low -> critical (wrap)
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "critical");
    }

    #[test]
    fn cycle_work_priority_no_work_sets_error() {
        let mut s = state();
        s.cursor = 0;
        s.rows[0].spec.planned_work.clear();
        s.cycle_work_priority(true);
        assert!(s.last_error.is_some());
    }

    // --- reconciliation accept/reject ---

    fn report_with(emergent: &[&str], stalled: &[(&str, &str)]) -> crate::biplane::ReconciliationReport {
        crate::biplane::ReconciliationReport {
            emergent_domains: emergent
                .iter()
                .map(|n| domain(n, &[], 1))
                .collect(),
            stalled_domains: stalled
                .iter()
                .map(|(d, e)| crate::biplane::StalledDomain {
                    domain: d.to_string(),
                    evidence: e.to_string(),
                    blocked_by_cycle: None,
                })
                .collect(),
            healthy_domains: vec![],
        }
    }

    #[test]
    fn ingest_queues_emergent_and_opens_review() {
        let mut s = state();
        let report = report_with(&["cache", "telemetry"], &[]);
        s.ingest_reconciliation(&report);
        assert_eq!(s.pending_count(), 2);
        assert!(s.reviewing_suggestions);
    }

    #[test]
    fn ingest_skips_already_present_domains() {
        let mut s = state();
        // "engine" already exists in the fixture; only "cache" is new.
        let report = report_with(&["engine", "cache"], &[]);
        s.ingest_reconciliation(&report);
        assert_eq!(s.pending_count(), 1);
        assert_eq!(s.pending_suggestions[0].name, "cache");
    }

    #[test]
    fn accept_suggestion_adds_domain() {
        let mut s = state();
        let before = s.rows.len();
        s.ingest_reconciliation(&report_with(&["cache"], &[]));
        let name = s.accept_suggestion().unwrap();
        assert_eq!(name, "cache");
        assert_eq!(s.rows.len(), before + 1);
        assert!(s.rows.iter().any(|r| r.spec.name == "cache"));
        assert_eq!(s.pending_count(), 0);
        // queue empty -> review auto-closes
        assert!(!s.reviewing_suggestions);
        assert!(s.dirty);
    }

    #[test]
    fn reject_suggestion_drops_without_adding() {
        let mut s = state();
        let before = s.rows.len();
        s.ingest_reconciliation(&report_with(&["cache"], &[]));
        let name = s.reject_suggestion().unwrap();
        assert_eq!(name, "cache");
        assert_eq!(s.rows.len(), before); // not added
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn accepted_emergent_domain_validates() {
        let mut s = state();
        s.ingest_reconciliation(&report_with(&["cache"], &[]));
        s.accept_suggestion();
        assert!(s.validated().is_some());
    }

    #[test]
    fn ingest_records_stalled_notices() {
        let mut s = state();
        s.ingest_reconciliation(&report_with(&[], &[("ui", "no commits in 2h")]));
        assert_eq!(s.stalled_notices.len(), 1);
        assert!(s.stalled_notices[0].contains("ui"));
        // no emergent -> review not opened
        assert!(!s.reviewing_suggestions);
    }

    #[test]
    fn stalled_cycle_notice_lists_members() {
        let mut s = state();
        let mut report = report_with(&[], &[]);
        report.stalled_domains.push(crate::biplane::StalledDomain {
            domain: "a".into(),
            evidence: "waiting".into(),
            blocked_by_cycle: Some(vec!["a".into(), "b".into()]),
        });
        s.ingest_reconciliation(&report);
        assert!(s.stalled_notices[0].contains("cycle"));
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
}
