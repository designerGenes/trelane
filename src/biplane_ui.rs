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

/// How many agents the AI detection pass may propose. Deliberately generous
/// and independent of the session budget: a budget of 1 must not force a
/// single-domain analysis (the user trims afterwards in the editor).
pub const AI_DETECT_MAX_AGENTS: usize = 8;

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

/// A navigable column within a domain row. Left/right move the column cursor;
/// `e`/Enter act on the focused column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Agents,
    Work,
    Priority,
    Deps,
    Model,
    Writable,
}

impl Column {
    /// Left-to-right order, matching the on-screen cell layout.
    pub const ORDER: [Column; 7] = [
        Column::Name,
        Column::Agents,
        Column::Work,
        Column::Priority,
        Column::Deps,
        Column::Model,
        Column::Writable,
    ];

    pub fn index(&self) -> usize {
        Column::ORDER.iter().position(|c| c == self).unwrap()
    }

    pub fn label(&self) -> &'static str {
        match self {
            Column::Name => "name",
            Column::Agents => "agents",
            Column::Work => "tasks",
            Column::Priority => "priority",
            Column::Deps => "deps",
            Column::Model => "model",
            Column::Writable => "writable",
        }
    }

    /// Column header shown in the domain table.
    pub fn title(&self) -> &'static str {
        match self {
            Column::Name => "NAME",
            Column::Agents => "AGENTS",
            Column::Work => "TASKS",
            Column::Priority => "PRIO",
            Column::Deps => "DEPS",
            Column::Model => "MODEL",
            Column::Writable => "WRITABLE",
        }
    }
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
    /// Edit the focused domain's agent count (numeric entry).
    Agents,
    /// Edit the focused domain's comma-separated dependencies.
    Deps,
    /// Type a new source-folder path to add to the scan list.
    NewSource,
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
    /// Focused column within the current row (for left/right navigation and
    /// column-aware editing).
    pub col_cursor: Column,
    /// When true, the full-screen help overlay is shown.
    pub show_help: bool,
    /// When true, the full-detail report view for the focused domain is shown.
    pub show_detail: bool,
    /// Explicit list of folders the AI analysis will scan for domains. The
    /// project root is always scanned implicitly; these are additional sources
    /// (e.g. a features folder or a related repo). Replaces the old automatic
    /// safe-pocket detection.
    pub scan_sources: Vec<String>,
    /// When true, the source-folder editor overlay is shown.
    pub editing_sources: bool,
    /// Cursor within the source-folder editor.
    pub source_cursor: usize,
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
            col_cursor: Column::Name,
            show_help: false,
            show_detail: false,
            scan_sources: Vec::new(),
            editing_sources: false,
            source_cursor: 0,
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

    /// Replace the editor's contents with a freshly generated description
    /// (e.g. from an in-UI AI analysis). Rows and budget are rebuilt, cursors
    /// reset, any in-progress edit or overlay closed, and the state is marked
    /// dirty so the user consciously saves (or quits to discard). The fetched
    /// model catalog is preserved so the selector keeps working.
    pub fn replace_from_description(
        &mut self,
        desc: &ProjectDescription,
        source: impl Into<String>,
    ) {
        self.project_name = desc.name.clone();
        self.project_summary = desc.description.clone();
        self.rows = desc
            .domains
            .iter()
            .map(|d| DomainRow { spec: d.clone(), include: true })
            .collect();
        self.budget = desc.max_agents.unwrap_or(desc.domains.len().max(1)).max(1);
        self.cursor = 0;
        self.col_cursor = Column::Name;
        self.editing = None;
        self.model_selector_open = false;
        self.reviewing_suggestions = false;
        self.show_help = false;
        self.show_detail = false;
        self.last_error = None;
        self.dirty = true;
        self.source = source.into();
        self.status = Some(format!("analysis complete: {} domain(s)", self.rows.len()));
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

    /// Move the column cursor one cell left/right within the current row,
    /// clamped to the ends (no wrap, so users can feel the edges).
    pub fn col_left(&mut self) {
        let i = self.col_cursor.index();
        if i > 0 {
            self.col_cursor = Column::ORDER[i - 1];
        }
    }

    pub fn col_right(&mut self) {
        let i = self.col_cursor.index();
        if i + 1 < Column::ORDER.len() {
            self.col_cursor = Column::ORDER[i + 1];
        }
    }

    /// Activate the focused column: text columns open the edit buffer, numeric
    /// columns cycle/increment, and the model column signals that the model
    /// selector should open (returned as true so the caller can open it, since
    /// the selector needs the launcher-model list gathered by the I/O layer).
    pub fn activate_focused_column(&mut self) -> bool {
        match self.col_cursor {
            Column::Name => {
                self.begin_rename();
                false
            }
            Column::Agents => {
                self.begin_edit_agents();
                false
            }
            Column::Work => {
                // Edit the first work item, creating one if the domain has none
                // so the column is never a dead end.
                let has_work = self
                    .rows
                    .get(self.cursor)
                    .map(|r| !r.spec.planned_work.is_empty())
                    .unwrap_or(false);
                if has_work {
                    self.begin_edit_work(0);
                } else {
                    self.add_work_item();
                }
                false
            }
            Column::Priority => {
                self.cycle_work_priority(true);
                false
            }
            Column::Deps => {
                self.begin_edit_deps();
                false
            }
            Column::Model => true, // caller opens the model selector
            Column::Writable => {
                self.begin_edit_writable();
                false
            }
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

    /// Begin numeric entry for the focused domain's agent count.
    pub fn begin_edit_agents(&mut self) {
        if let Some(row) = self.rows.get(self.cursor) {
            self.editing = Some(ActiveEdit {
                input: crate::text_input::TextInput::with_text(&row.spec.agents.to_string()),
                target: EditTarget::Agents,
            });
            self.last_error = None;
        }
    }

    /// Begin editing the focused domain's dependencies as a comma list.
    pub fn begin_edit_deps(&mut self) {
        if let Some(row) = self.rows.get(self.cursor) {
            let seed = row.spec.depends_on.join(", ");
            self.editing = Some(ActiveEdit {
                input: crate::text_input::TextInput::with_text(&seed),
                target: EditTarget::Deps,
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
            EditTarget::Agents => self.commit_agents(),
            EditTarget::Deps => self.commit_deps(),
            EditTarget::NewSource => self.commit_new_source(),
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

    /// Commit the numeric agent-count edit. Rejects anything that isn't a
    /// positive integer (keeping the editor open so the user can fix it).
    fn commit_agents(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let text = edit.input.value().trim().to_string();
        let parsed: Option<usize> = text.parse().ok().filter(|n| *n >= 1);
        let Some(n) = parsed else {
            self.last_error = Some(format!("'{text}' is not a valid agent count (must be 1+)"));
            return false;
        };
        match self.rows.get_mut(self.cursor) {
            Some(row) => {
                if row.spec.agents == n {
                    self.editing = None;
                    self.last_error = None;
                    return false;
                }
                row.spec.agents = n;
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

    /// Commit the dependencies edit. Every entry must name another existing
    /// domain (self-dependencies and unknown names are rejected with the
    /// editor kept open). An empty list clears the dependencies.
    fn commit_deps(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let deps: Vec<String> = edit
            .input
            .value()
            .split(',')
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect();

        let self_name = match self.rows.get(self.cursor) {
            Some(r) => r.spec.name.clone(),
            None => {
                self.editing = None;
                return false;
            }
        };
        for d in &deps {
            if *d == self_name {
                self.last_error = Some(format!("'{d}' cannot depend on itself"));
                return false;
            }
            if !self.rows.iter().any(|r| r.spec.name == *d) {
                self.last_error = Some(format!("unknown domain in deps: '{d}'"));
                return false;
            }
        }

        let row = self.rows.get_mut(self.cursor).expect("checked above");
        if row.spec.depends_on == deps {
            self.editing = None;
            self.last_error = None;
            return false;
        }
        row.spec.depends_on = deps;
        self.editing = None;
        self.last_error = None;
        self.dirty = true;
        true
    }

    // -- Scan-source management (folders the AI analysis reads) --

    /// Open the source-folder editor overlay.
    pub fn open_source_editor(&mut self) {
        self.editing_sources = true;
        self.source_cursor = 0;
        self.last_error = None;
    }

    pub fn close_source_editor(&mut self) {
        self.editing_sources = false;
    }

    pub fn source_up(&mut self) {
        if self.source_cursor > 0 {
            self.source_cursor -= 1;
        }
    }

    pub fn source_down(&mut self) {
        if !self.scan_sources.is_empty() && self.source_cursor + 1 < self.scan_sources.len() {
            self.source_cursor += 1;
        }
    }

    /// Begin typing a new source-folder path.
    pub fn begin_add_source(&mut self) {
        self.editing = Some(ActiveEdit {
            input: crate::text_input::TextInput::new(),
            target: EditTarget::NewSource,
        });
        self.last_error = None;
    }

    /// Commit a typed source path: trims it, rejects empty/duplicate, appends
    /// to the scan list, and focuses it. Existence is not required at edit
    /// time (the analysis reports missing folders), but empty is rejected.
    fn commit_new_source(&mut self) -> bool {
        let Some(edit) = self.editing.as_ref() else {
            return false;
        };
        let path = edit.input.value().trim().to_string();
        if path.is_empty() {
            self.last_error = Some("source path must not be empty".to_string());
            return false;
        }
        if self.scan_sources.contains(&path) {
            self.last_error = Some(format!("'{path}' is already a source"));
            return false;
        }
        self.scan_sources.push(path);
        self.source_cursor = self.scan_sources.len() - 1;
        self.editing = None;
        self.last_error = None;
        true
    }

    /// Remove the focused source folder.
    pub fn remove_source(&mut self) -> bool {
        if self.scan_sources.is_empty() {
            return false;
        }
        self.scan_sources.remove(self.source_cursor);
        if self.source_cursor >= self.scan_sources.len() && self.source_cursor > 0 {
            self.source_cursor -= 1;
        }
        true
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
                // Transient status messages (e.g. "analysis complete") live
                // until the next keypress, then the keyboard hints return.
                state.status = None;
                // Help overlay: any key closes it.
                if state.show_help {
                    state.show_help = false;
                    continue;
                }
                // Detail view: browse domains with the arrows, anything else closes.
                if state.show_detail {
                    match key.code {
                        KeyCode::Up => state.cursor_up(),
                        KeyCode::Down => state.cursor_down(),
                        _ => state.show_detail = false,
                    }
                    continue;
                }
                // Source-folder editor overlay.
                if state.editing_sources {
                    if state.is_editing() {
                        // Typing a new path.
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
                        KeyCode::Esc | KeyCode::Char('q') => state.close_source_editor(),
                        KeyCode::Up => state.source_up(),
                        KeyCode::Down => state.source_down(),
                        KeyCode::Char('a') => state.begin_add_source(),
                        KeyCode::Char('d') => {
                            state.remove_source();
                        }
                        _ => {}
                    }
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
                    KeyCode::Char('?') => state.show_help = true,
                    KeyCode::Char('v') => {
                        if !state.rows.is_empty() {
                            state.show_detail = true;
                        }
                    }
                    KeyCode::Char('S') => state.open_source_editor(),
                    KeyCode::Up => state.cursor_up(),
                    KeyCode::Down => state.cursor_down(),
                    KeyCode::Left => state.col_left(),
                    KeyCode::Right => state.col_right(),
                    KeyCode::Char(' ') => state.toggle_include(),
                    KeyCode::Enter | KeyCode::Char('e') => {
                        if state.activate_focused_column() {
                            state.open_model_selector();
                        }
                    }
                    KeyCode::Char('[') => state.adjust_budget(false),
                    KeyCode::Char(']') => state.adjust_budget(true),
                    KeyCode::Char('K') => state.move_up(),
                    KeyCode::Char('J') => state.move_down(),
                    KeyCode::Char('w') => {
                        // Explicit work-edit: create an item first if none exist
                        // so the key always does something visible.
                        let has_work = state
                            .rows
                            .get(state.cursor)
                            .map(|r| !r.spec.planned_work.is_empty())
                            .unwrap_or(false);
                        if has_work {
                            state.begin_edit_work(0);
                        } else {
                            state.add_work_item();
                        }
                    }
                    KeyCode::Char('g') => state.begin_edit_writable(),
                    KeyCode::Char('n') => {
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
                    KeyCode::Char('G') => {
                        // AI analysis is a long, blocking model call: drop out of
                        // raw mode / alt screen so the planner's progress output is
                        // visible, then restore the TUI and swap in the results.
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        let model = crate::biplane::default_biplane_model();
                        println!("[biplane] Analyzing project with '{model}' (this reads the safe-pocket features folder)...");
                        // Detection must never be capped by the current session
                        // budget (a budget of 1 would force a single-domain
                        // plan); give the planner room and let the user trim.
                        let detect_cap = AI_DETECT_MAX_AGENTS;
                        let sources: Vec<std::path::PathBuf> =
                            state.scan_sources.iter().map(std::path::PathBuf::from).collect();
                        let analysis = crate::biplane::run_biplane_plan_from_sources(
                            root, &sources, &model, detect_cap,
                        );
                        enable_raw_mode()?;
                        execute!(terminal.backend_mut(), EnterAlternateScreen)?;
                        terminal.clear()?;
                        match analysis {
                            Ok(plan) => {
                                let name = state.project_name.clone();
                                // Budget follows what was actually detected,
                                // not the pre-analysis UI state.
                                let detected = plan.agents.len().max(1);
                                let desc =
                                    crate::biplane::plan_to_description(&plan, &name, detected);
                                state.replace_from_description(
                                    &desc,
                                    "AI-detected from project features",
                                );
                            }
                            Err(e) => {
                                state.last_error = Some(format!("AI analysis failed: {e}"));
                            }
                        }
                    }
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
    use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // header (title, budget, AI hint, optional error)
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
    // The AI-analysis entry point stays visible at all times; when the current
    // description is only a structural scaffold (no curation yet), emphasize it.
    let ai_hint_style = if state.source.starts_with("scaffolded") {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(dim)
    };
    let src_note = if state.scan_sources.is_empty() {
        "G: analyze with AI (scans this project)   S: add source folders to scan".to_string()
    } else {
        format!(
            "G: analyze with AI (scans this project + {} source folder(s))   S: edit sources",
            state.scan_sources.len()
        )
    };
    header_lines.push(Line::from(Span::styled(src_note, ai_hint_style)));
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

    // Domain table: titled columns with fixed widths so every row aligns.
    // Cell values are plain ("1", not "agents:1") because the header names
    // the column. The focused cell is REVERSED; a cell being edited shows the
    // live buffer with a caret in the warn color.
    let target = state.editing.as_ref().map(|e| e.target.clone());
    let warn_bold = Style::default()
        .fg(tc(THEME_WARN))
        .add_modifier(Modifier::BOLD);

    let header_row = Row::new(
        std::iter::once(Cell::from("INCL"))
            .chain(Column::ORDER.iter().map(|c| Cell::from(c.title())))
            .collect::<Vec<Cell>>(),
    )
    .style(
        Style::default()
            .fg(accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    );

    let table_rows: Vec<Row> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let is_focus_row = i == state.cursor;
            // Live edit buffer belongs in exactly one cell of the focused row.
            let editing_cell = |col: Column| -> Option<String> {
                if !is_focus_row {
                    return None;
                }
                let hit = match (&target, col) {
                    (Some(EditTarget::DomainName), Column::Name) => true,
                    (Some(EditTarget::WorkSubject { .. }), Column::Work) => true,
                    (Some(EditTarget::Writable), Column::Writable) => true,
                    (Some(EditTarget::Agents), Column::Agents) => true,
                    (Some(EditTarget::Deps), Column::Deps) => true,
                    _ => false,
                };
                hit.then(|| state.edit_input().unwrap().render_with_caret())
            };
            let cell = |col: Column, text: String, base: Style| -> Cell {
                match editing_cell(col) {
                    Some(buf) => Cell::from(buf).style(warn_bold),
                    None => {
                        let style = if is_focus_row
                            && !state.is_editing()
                            && state.col_cursor == col
                        {
                            base.add_modifier(Modifier::REVERSED)
                        } else {
                            base
                        };
                        Cell::from(text).style(style)
                    }
                }
            };

            let name_style = if row.include {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim)
            };
            let dim_style = Style::default().fg(dim);
            let incl = if row.include {
                Cell::from("[x]").style(Style::default().fg(tc(THEME_OK)))
            } else {
                Cell::from("[ ]").style(dim_style)
            };
            let deps_text = if row.spec.depends_on.is_empty() {
                "-".to_string()
            } else {
                row.spec.depends_on.join(",")
            };
            let prio_text = row
                .spec
                .planned_work
                .first()
                .map(|w| w.priority.clone())
                .unwrap_or_else(|| "-".to_string());
            let model_text = row.spec.model.clone().unwrap_or_else(|| "(default)".to_string());
            let model_style = if row.spec.model.is_some() {
                Style::default().fg(accent)
            } else {
                dim_style
            };

            Row::new(vec![
                incl,
                cell(Column::Name, row.spec.name.clone(), name_style),
                cell(Column::Agents, row.spec.agents.to_string(), dim_style),
                cell(Column::Work, row.spec.planned_work.len().to_string(), dim_style),
                cell(Column::Priority, prio_text, dim_style),
                cell(Column::Deps, deps_text, dim_style),
                cell(Column::Model, model_text, model_style),
                cell(Column::Writable, row.spec.writable.join(","), dim_style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(4),  // INCL
        Constraint::Length(18), // NAME
        Constraint::Length(6),  // AGENTS
        Constraint::Length(5),  // TASKS
        Constraint::Length(8),  // PRIO
        Constraint::Length(16), // DEPS
        Constraint::Length(22), // MODEL
        Constraint::Min(12),    // WRITABLE
    ];
    let table = Table::new(table_rows, widths)
        .header(header_row)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD))
        .highlight_symbol("▶")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Domains ")
                .border_style(Style::default().fg(accent)),
        );
    let mut table_state = TableState::default();
    if !state.rows.is_empty() {
        table_state.select(Some(state.cursor.min(state.rows.len() - 1)));
    }
    f.render_stateful_widget(table, chunks[1], &mut table_state);

    // Model selector popup
    if state.model_selector_open {
        render_model_selector(f, state);
    }

    // Reconciliation review overlay
    if state.reviewing_suggestions {
        render_review_overlay(f, state);
    }

    // Source-folder editor.
    if state.editing_sources {
        render_source_overlay(f, state);
    }

    // Domain detail / report view.
    if state.show_detail {
        render_detail_overlay(f, state);
    }

    // Help overlay (drawn last so it sits on top of everything).
    if state.show_help {
        render_help_overlay(f);
    }

    // Footer
    let hint = state.status.clone().unwrap_or_else(|| {
        if state.editing_sources {
            if state.is_editing() {
                "type a folder path  Enter add  Esc cancel".to_string()
            } else {
                "↑↓ select  a add folder  d remove  Esc/q close".to_string()
            }
        } else if state.show_detail {
            "↑↓ browse domains  any other key: back to the table".to_string()
        } else if state.reviewing_suggestions {
            "↑↓ select  y/Enter accept  n reject  Esc close review".to_string()
        } else if state.model_selector_open {
            "↑↓ select model  Enter apply  f free-only  a apply-to-all  c clear  Esc close".to_string()
        } else if state.editing.is_some() {
            "typing… Enter save  Esc cancel  ←→ move caret  Backspace delete".to_string()
        } else {
            let base = "↑↓ row  ←→ column  Enter/e edit  v details  n/d domain  G analyze  S sources  s save  ? help  q quit";
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
        .map(|m| {
            let free_tag = if m.is_free {
                Span::styled(" [FREE]", Style::default().fg(tc(THEME_OK)))
            } else {
                Span::raw("")
            };
            ListItem::new(Line::from(vec![
                Span::styled(&m.id, Style::default().fg(accent)),
                free_tag,
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(accent)),
        )
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().fg(accent).add_modifier(Modifier::BOLD));
    // A ListState seeded with the cursor makes ratatui scroll the viewport to
    // keep the selected model visible even when the list runs off-screen.
    let mut list_state = ratatui::widgets::ListState::default();
    if !filtered.is_empty() {
        list_state.select(Some(state.model_cursor.min(filtered.len() - 1)));
    }
    f.render_stateful_widget(list, area, &mut list_state);

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

/// Source-folder editor: the explicit list of folders the AI analysis scans,
/// plus the always-scanned project root. Add paths (`a`), remove (`d`).
fn render_source_overlay(f: &mut ratatui::Frame, state: &BiplaneUiState) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);
    let area = centered_rect_pct(78, 60, f.area());
    f.render_widget(Clear, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(area);

    // The project root is always scanned; shown as context, not editable.
    let root_note = Paragraph::new(vec![
        Line::from(Span::styled(
            "  Folders scanned by AI analysis (project root is always included):",
            Style::default().fg(dim),
        )),
        Line::from(Span::styled(
            format!("  • {} (project root)", state.project_name),
            Style::default().fg(tc(THEME_OK)),
        )),
    ]);
    f.render_widget(root_note, rows[0]);

    let mut items: Vec<ListItem> = state
        .scan_sources
        .iter()
        .enumerate()
        .map(|(i, src)| {
            let marker = if i == state.source_cursor && !state.is_editing() {
                "▶ "
            } else {
                "  "
            };
            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(src.clone(), Style::default().fg(accent)),
            ]))
        })
        .collect();

    // Live add-buffer as a trailing entry when typing a new path.
    if state.is_editing() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled("+ ", Style::default().fg(tc(THEME_OK))),
            Span::styled(
                state.edit_input().unwrap().render_with_caret(),
                Style::default()
                    .fg(tc(crate::diagnostic::THEME_WARN))
                    .add_modifier(Modifier::BOLD),
            ),
        ])));
    } else if state.scan_sources.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "  (no extra folders — press 'a' to add one)",
            Style::default().fg(dim),
        ))));
    }

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Scan Sources ")
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(list, rows[1]);
}

/// Full-content view of the focused domain: everything the Biplane report
/// says about it, logically laid out — identity and ownership facts up top,
/// then every planned task with its full (wrapped) body. `↑↓` browse domains
/// without leaving the view.
fn render_detail_overlay(f: &mut ratatui::Frame, state: &BiplaneUiState) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM, THEME_OK};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);
    let Some(row) = state.rows.get(state.cursor) else {
        return;
    };
    let d = &row.spec;

    let area = centered_rect_pct(84, 88, f.area());
    f.render_widget(Clear, area);

    let label = |k: &str, v: String| -> Line {
        Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().fg(dim)),
            Span::styled(v, Style::default().fg(accent)),
        ])
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("  {}", d.name),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "    ({} of {}, {})",
                    state.cursor + 1,
                    state.rows.len(),
                    if row.include { "included" } else { "excluded" }
                ),
                Style::default().fg(dim),
            ),
        ]),
        Line::from(""),
    ];
    if !d.description.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", d.description),
            Style::default().fg(tc(THEME_OK)),
        )));
        lines.push(Line::from(""));
    }
    lines.push(label("agents", d.agents.to_string()));
    lines.push(label(
        "model",
        d.model.clone().unwrap_or_else(|| "(default)".to_string()),
    ));
    lines.push(label(
        "deps",
        if d.depends_on.is_empty() {
            "-".to_string()
        } else {
            d.depends_on.join(", ")
        },
    ));
    lines.push(label("writable", d.writable.join(", ")));
    if !d.forbidden_write.is_empty() {
        lines.push(label("forbidden", d.forbidden_write.join(", ")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("  Planned tasks ({})", d.planned_work.len()),
        Style::default()
            .fg(accent)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    if d.planned_work.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (none — press A in the table to add one)",
            Style::default().fg(dim),
        )));
    }
    for (i, w) in d.planned_work.iter().enumerate() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {}. {}", i + 1, w.subject),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("   [{}]", w.priority), Style::default().fg(dim)),
        ]));
        if !w.body.is_empty() {
            for body_line in w.body.lines() {
                lines.push(Line::from(Span::styled(
                    format!("     {body_line}"),
                    Style::default().fg(dim),
                )));
            }
        }
    }

    let title = format!(" Biplane Report — {} :: {} ", state.project_name, d.name);
    let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(para, area);
}

/// Full-screen help overlay listing every control, opened with `?`.
fn render_help_overlay(f: &mut ratatui::Frame) {
    use crate::diagnostic::{THEME_BIPLANE_ACCENT, THEME_DIM};
    use ratatui::prelude::*;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let accent = tc(THEME_BIPLANE_ACCENT);
    let dim = tc(THEME_DIM);
    let area = centered_rect_pct(80, 84, f.area());
    f.render_widget(Clear, area);

    let key = |k: &str, desc: &str| -> Line {
        Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            Span::styled(desc.to_string(), Style::default().fg(dim)),
        ])
    };
    let header = |t: &str| -> Line {
        Line::from(Span::styled(
            format!(" {t}"),
            Style::default().fg(accent).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ))
    };

    let lines = vec![
        header("Navigation"),
        key("↑ / ↓", "move between domain rows"),
        key("← / →", "move the column cursor across the focused row"),
        key("Enter / e", "edit the focused column"),
        key("v", "view the full Biplane report for the focused domain"),
        Line::from(""),
        header("Columns"),
        key("NAME", "the domain (an agent's area of responsibility)"),
        key("AGENTS", "how many agents work this domain — Enter to type a number"),
        key("TASKS", "how many planned tasks the domain has (w edits the first)"),
        key("PRIO", "priority of the domain's first task (p/P cycles)"),
        key("DEPS", "domains this one depends on — Enter to edit the list"),
        key("MODEL", "the model its agents run — Enter opens the selector"),
        key("WRITABLE", "glob patterns the domain may write to"),
        Line::from(""),
        header("Row editing"),
        key("space", "include / exclude the focused domain"),
        key("e", "rename the focused domain (Name column)"),
        key("w", "edit first task (creates one if none)"),
        key("g", "edit writable globs"),
        key("p / P", "cycle task priority up / down"),

        Line::from(""),
        header("Structure"),
        key("n", "add a new domain"),
        key("d", "remove the focused domain"),
        key("A", "add a task to the focused domain"),
        key("D", "remove the last task from the focused domain"),
        key("K / J", "reorder the focused domain up / down"),
        key("[ / ]", "decrease / increase the agent budget"),
        Line::from(""),
        header("Models & review"),
        key("G", "analyze with AI to detect domains (scans the source folders)"),
        key("S", "edit the list of source folders the AI analysis scans"),
        key("m", "open the model selector for the focused domain"),
        key("M", "set all included domains to the first free model"),
        key("r", "reopen the reconciliation review (if suggestions pending)"),
        Line::from(""),
        header("Session"),
        key("s", "validate and save the description"),
        key("?", "show this help"),
        key("q / Esc", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(accent),
        )),
    ];

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Biplane — Help ")
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(para, area);
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
    fn column_cursor_moves_and_clamps() {
        let mut s = state();
        assert_eq!(s.col_cursor, Column::Name);
        s.col_left(); // already at leftmost
        assert_eq!(s.col_cursor, Column::Name);
        s.col_right();
        assert_eq!(s.col_cursor, Column::Agents);
        // walk to the far right
        for _ in 0..10 {
            s.col_right();
        }
        assert_eq!(s.col_cursor, Column::Writable);
        s.col_right(); // clamps at rightmost
        assert_eq!(s.col_cursor, Column::Writable);
    }

    #[test]
    fn activate_name_column_opens_rename() {
        let mut s = state();
        s.cursor = 0;
        s.col_cursor = Column::Name;
        let opens_model = s.activate_focused_column();
        assert!(!opens_model);
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "engine");
    }

    #[test]
    fn activate_agents_column_opens_numeric_entry() {
        let mut s = state();
        s.cursor = 0; // engine, agents 1
        s.col_cursor = Column::Agents;
        s.activate_focused_column();
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "1");
    }

    #[test]
    fn commit_agents_parses_typed_number() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_agents();
        s.editing.as_mut().unwrap().input.clear();
        for c in "4".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[0].spec.agents, 4);
        assert!(s.dirty);
    }

    #[test]
    fn commit_agents_rejects_junk_and_zero() {
        let mut s = state();
        s.cursor = 0;
        s.begin_edit_agents();
        s.editing.as_mut().unwrap().input.clear();
        for c in "zero".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_edit());
        assert!(s.is_editing()); // stays open to fix
        assert!(s.last_error.is_some());
        s.editing.as_mut().unwrap().input.clear();
        s.edit_insert('0');
        assert!(!s.commit_edit());
        assert_eq!(s.rows[0].spec.agents, 1); // unchanged
    }

    #[test]
    fn activate_deps_column_opens_editor() {
        let mut s = state();
        s.cursor = 1; // ui, deps [engine]
        s.col_cursor = Column::Deps;
        s.activate_focused_column();
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "engine");
    }

    #[test]
    fn commit_deps_accepts_known_domains_and_clears_on_empty() {
        let mut s = state();
        s.cursor = 0; // engine, no deps
        s.begin_edit_deps();
        s.editing.as_mut().unwrap().input.clear();
        for c in "ui, api".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.rows[0].spec.depends_on, vec!["ui", "api"]);
        // now clear them
        s.begin_edit_deps();
        s.editing.as_mut().unwrap().input.clear();
        assert!(s.commit_edit());
        assert!(s.rows[0].spec.depends_on.is_empty());
    }

    #[test]
    fn commit_deps_rejects_unknown_and_self() {
        let mut s = state();
        s.cursor = 0; // engine
        s.begin_edit_deps();
        s.editing.as_mut().unwrap().input.clear();
        for c in "ghost".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_edit());
        assert!(s.last_error.as_ref().unwrap().contains("unknown"));
        s.editing.as_mut().unwrap().input.clear();
        for c in "engine".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_edit());
        assert!(s.last_error.as_ref().unwrap().contains("itself"));
    }

    #[test]
    fn deps_column_is_navigable() {
        let mut s = state();
        // walk right from Name and confirm Deps is reachable
        s.col_cursor = Column::Name;
        for _ in 0..4 {
            s.col_right();
        }
        assert_eq!(s.col_cursor, Column::Deps);
    }

    #[test]
    fn detail_view_flag_requires_rows() {
        let mut s = state();
        s.show_detail = true;
        assert!(s.show_detail);
        // replace_from_description closes it
        let d = desc();
        s.replace_from_description(&d, "x");
        assert!(!s.show_detail);
    }

    #[test]
    fn add_source_appends_and_focuses() {
        let mut s = state();
        s.open_source_editor();
        s.begin_add_source();
        s.editing.as_mut().unwrap().input.clear();
        for c in "/tmp/features".chars() {
            s.edit_insert(c);
        }
        assert!(s.commit_edit());
        assert_eq!(s.scan_sources, vec!["/tmp/features"]);
        assert_eq!(s.source_cursor, 0);
        assert!(!s.is_editing());
    }

    #[test]
    fn add_source_rejects_empty_and_duplicate() {
        let mut s = state();
        s.scan_sources = vec!["/a".to_string()];
        // empty
        s.begin_add_source();
        assert!(!s.commit_edit());
        assert!(s.last_error.is_some());
        // duplicate
        s.begin_add_source();
        for c in "/a".chars() {
            s.edit_insert(c);
        }
        assert!(!s.commit_edit());
        assert!(s.last_error.as_ref().unwrap().contains("already"));
        assert_eq!(s.scan_sources.len(), 1);
    }

    #[test]
    fn remove_source_holds_cursor_bounds() {
        let mut s = state();
        s.scan_sources = vec!["/a".into(), "/b".into(), "/c".into()];
        s.source_cursor = 2; // /c
        assert!(s.remove_source());
        assert_eq!(s.scan_sources, vec!["/a", "/b"]);
        assert_eq!(s.source_cursor, 1);
        assert!(s.remove_source()); // removes /b
        assert!(s.remove_source()); // removes /a
        assert!(!s.remove_source()); // empty
        assert_eq!(s.source_cursor, 0);
    }

    #[test]
    fn source_editor_open_close() {
        let mut s = state();
        assert!(!s.editing_sources);
        s.open_source_editor();
        assert!(s.editing_sources);
        s.close_source_editor();
        assert!(!s.editing_sources);
    }

    #[test]
    fn activate_priority_column_cycles() {
        let mut s = state();
        s.cursor = 0; // engine, priority "normal"
        s.col_cursor = Column::Priority;
        s.activate_focused_column();
        assert_eq!(s.rows[0].spec.planned_work[0].priority, "high");
    }

    #[test]
    fn activate_model_column_signals_selector() {
        let mut s = state();
        s.cursor = 0;
        s.col_cursor = Column::Model;
        assert!(s.activate_focused_column()); // caller should open the selector
    }

    #[test]
    fn activate_work_column_creates_item_when_none() {
        let mut s = state();
        s.cursor = 0;
        s.rows[0].spec.planned_work.clear();
        s.col_cursor = Column::Work;
        s.activate_focused_column();
        // work column is never a dead end: an item is created and opened
        assert_eq!(s.rows[0].spec.planned_work.len(), 1);
        assert!(s.is_editing());
    }

    #[test]
    fn activate_writable_column_opens_glob_edit() {
        let mut s = state();
        s.cursor = 0;
        s.col_cursor = Column::Writable;
        s.activate_focused_column();
        assert!(s.is_editing());
        assert_eq!(s.edit_input().unwrap().value(), "src/engine/**");
    }

    #[test]
    fn help_flag_toggles() {
        let mut s = state();
        assert!(!s.show_help);
        s.show_help = true;
        assert!(s.show_help);
    }

    #[test]
    fn replace_from_description_swaps_contents_and_dirties() {
        let mut s = state();
        // put the state into a messy mid-interaction condition
        s.cursor = 2;
        s.col_cursor = Column::Model;
        s.begin_rename();
        s.show_help = true;
        s.models = vec![crate::biplane_ui::ModelEntry {
            id: "test/model".into(),
            is_free: true,
        }];

        let new_desc = ProjectDescription {
            name: "detected".into(),
            description: "ai output".into(),
            domains: vec![domain("cache", &[], 2), domain("telemetry", &[], 1)],
            max_agents: Some(5),
            default_model: None,
        };
        s.replace_from_description(&new_desc, "AI-detected from project features");

        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.rows[0].spec.name, "cache");
        assert!(s.rows.iter().all(|r| r.include));
        assert_eq!(s.budget, 5);
        assert_eq!(s.cursor, 0);
        assert_eq!(s.col_cursor, Column::Name);
        assert!(!s.is_editing());
        assert!(!s.show_help);
        assert!(s.dirty);
        assert_eq!(s.source, "AI-detected from project features");
        // fetched model catalog survives so the selector keeps working
        assert_eq!(s.models.len(), 1);
    }

    #[test]
    fn replace_from_description_budget_defaults_to_domain_count() {
        let mut s = state();
        let new_desc = ProjectDescription {
            name: "d".into(),
            description: String::new(),
            domains: vec![domain("a", &[], 1), domain("b", &[], 1)],
            max_agents: None,
            default_model: None,
        };
        s.replace_from_description(&new_desc, "test");
        assert_eq!(s.budget, 2);
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
