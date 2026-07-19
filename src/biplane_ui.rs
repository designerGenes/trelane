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

use crate::biplane::{DomainSpec, PlannedWork, ProjectDescription, validate_description};
use crate::diagnostic::THEME_BIPLANE_ACCENT;
use crate::error::Result;

/// A single editable row: a domain plus whether it's currently included.
#[derive(Debug, Clone)]
pub struct DomainRow {
    pub spec: DomainSpec,
    pub include: bool,
}

/// The two "sub screens" the Excalidraw calls for inside the Biplane UI. These
/// are NOT tabs in the ratatui sense — they occupy the full content area and
/// are toggled with the `T` key. Kept as a plain enum so the render layer just
/// matches on it; adding a third view later is a one-line change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiplaneView {
    /// Domain view: the editable list of proposed domains (the default).
    Domains,
    /// Project view: full-project detail, itself switchable between the raw
    /// machine-readable report and the AI-written summary.
    Project,
}

impl BiplaneView {
    pub fn toggle(self) -> Self {
        match self {
            BiplaneView::Domains => BiplaneView::Project,
            BiplaneView::Project => BiplaneView::Domains,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BiplaneView::Domains => "Domains",
            BiplaneView::Project => "Project",
        }
    }
}

/// Within the Project view, which of the two content forms is showing. The
/// Excalidraw specifies the project view "can show either full text of the
/// Biplane report (JSON) OR an AI summary." This is the toggle between them,
/// driven by a separate key so it doesn't collide with the view toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectPane {
    /// The AI-written natural-language summary of the project (the default —
    /// it's the friendlier first thing to see).
    Summary,
    /// The full machine-readable report JSON.
    ReportJson,
}

impl ProjectPane {
    pub fn toggle(self) -> Self {
        match self {
            ProjectPane::Summary => ProjectPane::ReportJson,
            ProjectPane::ReportJson => ProjectPane::Summary,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProjectPane::Summary => "AI Summary",
            ProjectPane::ReportJson => "Report JSON",
        }
    }
}

/// The columns of a domain row, left to right. The column cursor lands on each
/// of these; `e` enters edit mode on the focused one, and the edit behavior is
/// determined by the column's kind (see `EditKind`). This is what makes
/// left/right *navigate* columns instead of directly mutating a value -- a
/// value only changes once its column is in edit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// Whether the domain is included in the plan (checkbox).
    Include,
    /// The domain's name (free text).
    Name,
    /// The AI model/launcher profile assigned to this domain (selector).
    Model,
    /// Requested agent count (numeric).
    Agents,
    /// Planned-work items (list — shown, not free-edited here).
    Work,
    /// Dependency domain names (list).
    Deps,
    /// Writable globs (list).
    Writable,
}

/// How a column's value is edited once its column is in edit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Boolean flip (space / up / down toggles).
    Toggle,
    /// Free text via a TextInput (type to edit).
    Text,
    /// Cycle through a fixed catalog (up/down or left/right steps entries).
    Selector,
    /// Number adjusted by up/down / +/- and set by digit keys.
    Numeric,
    /// A list of strings, shown for reference (read/scroll only in this view).
    List,
}

impl Column {
    /// Left-to-right order the column cursor walks.
    pub const ORDER: [Column; 7] = [
        Column::Include,
        Column::Name,
        Column::Model,
        Column::Agents,
        Column::Work,
        Column::Deps,
        Column::Writable,
    ];

    /// Fixed render width (in cells) for this column. Fixed widths are what
    /// keep the columns from bouncing as their contents change -- every cell
    /// and the header are padded/truncated to exactly these.
    pub fn width(self) -> usize {
        match self {
            Column::Include => 4,   // "[x]" + space
            Column::Name => 14,
            Column::Model => 20,
            Column::Agents => 7,
            Column::Work => 6,
            Column::Deps => 12,
            Column::Writable => 40,
        }
    }

    pub fn kind(self) -> EditKind {
        match self {
            Column::Include => EditKind::Toggle,
            Column::Name => EditKind::Text,
            Column::Model => EditKind::Selector,
            Column::Agents => EditKind::Numeric,
            Column::Work | Column::Deps | Column::Writable => EditKind::List,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Column::Include => "include",
            Column::Name => "name",
            Column::Model => "model",
            Column::Agents => "agents",
            Column::Work => "work",
            Column::Deps => "deps",
            Column::Writable => "writable",
        }
    }
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
    /// Which sub-screen is showing. Toggled with `T`.
    pub view: BiplaneView,
    /// Within the Project view, which content form. Toggled with `V`.
    pub project_pane: ProjectPane,
    /// The pretty-printed Biplane report JSON, shown in the Project view's
    /// ReportJson pane. None until an analysis has been attached; the render
    /// layer shows a friendly placeholder in that case.
    pub report_json: Option<String>,
    /// Scroll offset for the Project view's content (both panes can be long).
    pub project_scroll: u16,
    /// Whether the `?` help overlay is currently shown.
    pub show_help: bool,
    /// Index into Column::ORDER for the focused column (0 = Include).
    pub col_cursor: usize,
    /// True while the focused column is in edit mode. Left/right navigate
    /// columns when false; when true they (and other keys) mutate the value.
    pub editing: bool,
    /// Live text buffer while editing a Text column (e.g. Name). Committed to
    /// the row on exit; None when not editing a text column.
    pub text_edit: Option<crate::text_input::TextInput>,
    /// The catalog of assignable models for the Model selector: "(default)"
    /// plus the model ids from `opencode models` (or launcher profiles as a
    /// fallback).
    pub models: Vec<String>,
    /// Known free-model ids (mirror of `bench.free_models` from config). Used
    /// by the "show only free models" toggle in the Model overlay to filter the
    /// catalog down to entries that won't incur paid-model spend.
    pub free_models: Vec<String>,
    /// Live search query typed into the Model overlay's filter field. Empty
    /// means "show all"; otherwise entries are kept on a case-insensitive
    /// substring match. Reset every time the overlay opens.
    pub model_query: String,
    /// When true, the Model overlay hides any catalog entry that isn't in
    /// `free_models` (the "(default)" override-clearing entry is always kept).
    /// Toggled with `Tab` inside the overlay so it persists across opens.
    pub model_free_only: bool,
    /// When the Model overlay picker is open: (selected index, scroll offset).
    /// The `selected` index is into the FILTERED view (query + free-only),
    /// not the raw catalog -- `filtered_model_indices()` is the source of
    /// truth for the mapping. None when closed.
    pub model_overlay: Option<ModelOverlay>,
    /// When the inline list editor is open (work/deps/writable). None closed.
    pub list_editor: Option<ListEditor>,
    /// True while a row-delete confirmation is pending (the next y confirms,
    /// anything else cancels). Mirrors the monitor's kill_confirm pattern:
    /// destructive actions get an explicit on-screen y/n gate so a stray
    /// keypress can't silently drop a domain the user curated.
    pub delete_confirm_pending: bool,
}

/// State for the model-selection overlay: a paged, scrollable picker over the
/// (potentially long) model catalog. Return selects, Esc cancels.
#[derive(Debug, Clone)]
pub struct ModelOverlay {
    /// Index into the model catalog of the highlighted entry.
    pub selected: usize,
    /// Index of the first visible row (for paging through a long list).
    pub scroll: usize,
}

/// State for the inline list editor over a List column's items (work/deps/
/// writable). Items are edited as free-text lines: add, edit, remove.
#[derive(Debug, Clone)]
pub struct ListEditor {
    /// Which column's list is being edited.
    pub col: Column,
    /// Working copy of the items; committed back to the row on close.
    pub items: Vec<String>,
    /// Focused item index.
    pub cursor: usize,
    /// Active text buffer when adding/editing an item; None when just
    /// navigating.
    pub text_edit: Option<crate::text_input::TextInput>,
    /// True when the active text_edit is a NEW item being added (vs editing an
    /// existing one).
    pub adding: bool,
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
            view: BiplaneView::Domains,
            project_pane: ProjectPane::Summary,
            report_json: None,
            project_scroll: 0,
            show_help: false,
            col_cursor: 0,
            editing: false,
            text_edit: None,
            // Populated by with_models() from config; "(default)" alone until
            // then so the Model column always has at least the default entry.
            models: vec!["(default)".to_string()],
            // Populated by with_free_models() from config; empty until then so
            // the "free only" toggle has nothing to filter on.
            free_models: Vec::new(),
            // Empty search: every catalog entry is visible until the user types.
            model_query: String::new(),
            // Off by default so the full catalog is shown; user toggles with
            // Tab in the overlay when they want to avoid paid-model spend.
            model_free_only: false,
            model_overlay: None,
            list_editor: None,
            delete_confirm_pending: false,
        }
    }

    /// Attach the model catalog (config launcher profiles) for the Model
    /// column's selector. Chainable, mirroring with_report_json, so callers
    /// without a config still get a working editor (just the "(default)"
    /// entry). "(default)" is always first; profile names follow, sorted.
    pub fn with_models(mut self, profile_names: &[String]) -> Self {
        let mut names: Vec<String> = profile_names.to_vec();
        names.sort();
        let mut models = vec!["(default)".to_string()];
        models.extend(names);
        self.models = models;
        self
    }

    /// Attach the known free-model allowlist (from `bench.free_models`).
    /// Chainable, mirroring `with_models`, so callers without a config still
    /// get a working editor (the "free only" toggle just has nothing to hide).
    pub fn with_free_models(mut self, free: &[String]) -> Self {
        self.free_models = free.to_vec();
        self
    }

    /// The indices into `self.models` that pass the current overlay filter
    /// (search query + free-only toggle). The "(default)" entry at index 0 is
    /// always kept -- it clears the per-domain override rather than picking a
    /// paid model, so the user can always escape the picker. Other entries are
    /// kept on a case-insensitive substring match against the query, and (when
    /// `model_free_only` is on) only if they appear in `free_models`.
    pub fn filtered_model_indices(&self) -> Vec<usize> {
        let q = self.model_query.trim().to_lowercase();
        let mut out: Vec<usize> = Vec::new();
        for (i, name) in self.models.iter().enumerate() {
            if i == 0 {
                // "(default)" is always visible.
                out.push(i);
                continue;
            }
            if self.model_free_only && !self.free_models.iter().any(|f| f == name) {
                continue;
            }
            if !q.is_empty() && !name.to_lowercase().contains(&q) {
                continue;
            }
            out.push(i);
        }
        out
    }

    /// Attach a pretty-printed report JSON for the Project view. Kept separate
    /// from `from_description` so the state can be constructed without a report
    /// (the common case at first launch) and enriched later once analysis runs.
    pub fn with_report_json(mut self, json: impl Into<String>) -> Self {
        self.report_json = Some(json.into());
        self
    }

    /// Toggle between the Domain and Project sub-screens (`T`). Resets the
    /// project scroll so re-entering the view starts at the top.
    pub fn toggle_view(&mut self) {
        self.view = self.view.toggle();
        self.project_scroll = 0;
    }

    /// Toggle the Project view's content form between AI summary and report
    /// JSON (`V`). No-op unless the Project view is active, so the key is inert
    /// where it would be meaningless. Resets scroll on switch.
    pub fn toggle_project_pane(&mut self) {
        if self.view == BiplaneView::Project {
            self.project_pane = self.project_pane.toggle();
            self.project_scroll = 0;
        }
    }

    /// Scroll the Project view content. Only meaningful in the Project view;
    /// saturating so it never underflows past the top.
    pub fn project_scroll_down(&mut self) {
        if self.view == BiplaneView::Project {
            self.project_scroll = self.project_scroll.saturating_add(1);
        }
    }

    pub fn project_scroll_up(&mut self) {
        if self.view == BiplaneView::Project {
            self.project_scroll = self.project_scroll.saturating_sub(1);
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

    /// The column the cursor is currently on.
    pub fn focused_column(&self) -> Column {
        Column::ORDER[self.col_cursor.min(Column::ORDER.len() - 1)]
    }

    /// Move the column cursor left/right. No-op while editing (arrows are
    /// consumed by the value editor then). This is the fix for the reported
    /// bug: left/right navigate columns; they don't mutate a value.
    pub fn col_left(&mut self) {
        if self.editing {
            return;
        }
        if self.col_cursor > 0 {
            self.col_cursor -= 1;
        }
    }

    pub fn col_right(&mut self) {
        if self.editing {
            return;
        }
        if self.col_cursor + 1 < Column::ORDER.len() {
            self.col_cursor += 1;
        }
    }

    /// Toggle edit mode on the focused column. Model opens the overlay picker;
    /// List columns open the inline list editor; Toggle/Text/Numeric edit
    /// inline (entering a Text column seeds the buffer, leaving commits it).
    pub fn toggle_edit(&mut self) {
        let col = self.focused_column();
        if self.editing {
            if col.kind() == EditKind::Text {
                self.commit_text_edit();
            }
            self.editing = false;
            self.text_edit = None;
            return;
        }
        match col.kind() {
            EditKind::Selector => self.open_model_overlay(),
            EditKind::List => self.open_list_editor(col),
            EditKind::Text => {
                let cur = self.text_value_for(col);
                self.text_edit = Some(crate::text_input::TextInput::with_text(&cur));
                self.editing = true;
            }
            _ => {
                self.editing = true;
            }
        }
    }

    // ---------- model overlay picker ----------

    /// Open the model overlay, positioned on the row's current model. The
    /// search query is cleared each open (a fresh search per session) while the
    /// `model_free_only` toggle is preserved (it's a standing preference).
    /// `selected` is resolved against the FILTERED view so it points at the
    /// current model if that model is still visible, or falls back to
    /// "(default)" otherwise.
    pub fn open_model_overlay(&mut self) {
        self.model_query.clear();
        let filtered = self.filtered_model_indices();
        let cur = self.model_index();
        let selected = filtered
            .iter()
            .position(|&i| i == cur)
            .unwrap_or(0);
        self.model_overlay = Some(ModelOverlay {
            selected,
            scroll: 0,
        });
    }

    /// Move the overlay selection by `delta` rows within the FILTERED view
    /// (clamped), keeping a `visible`-row window scrolled so the selection
    /// stays on screen.
    pub fn model_overlay_move(&mut self, delta: isize, visible: usize) {
        let n = self.filtered_model_indices().len();
        if n == 0 {
            return;
        }
        if let Some(ov) = self.model_overlay.as_mut() {
            let cur = ov.selected as isize;
            let next = (cur + delta).clamp(0, n as isize - 1) as usize;
            ov.selected = next;
            // Keep the selection within [scroll, scroll+visible).
            if visible > 0 {
                if next < ov.scroll {
                    ov.scroll = next;
                } else if next >= ov.scroll + visible {
                    ov.scroll = next + 1 - visible;
                }
            }
        }
    }

    /// Commit the overlay selection to the focused row's model and close.
    /// Resolves the FILTERED-view selection back to a catalog entry, then maps
    /// "(default)" to `None` (clearing the per-domain override).
    pub fn model_overlay_commit(&mut self) {
        if let Some(ov) = self.model_overlay.take() {
            let filtered = self.filtered_model_indices();
            let picked = filtered.get(ov.selected).and_then(|&i| self.models.get(i).cloned());
            if let (Some(picked), Some(row)) = (picked, self.rows.get_mut(self.cursor)) {
                row.spec.model = if picked == "(default)" {
                    None
                } else {
                    Some(picked)
                };
                self.dirty = true;
            }
        }
    }

    pub fn model_overlay_cancel(&mut self) {
        self.model_overlay = None;
    }

    /// Append a printable character to the overlay's search query. Resets the
    /// selection/scroll to the top so the first filtered match is highlighted.
    pub fn model_overlay_type(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        self.model_query.push(c);
        if let Some(ov) = self.model_overlay.as_mut() {
            ov.selected = 0;
            ov.scroll = 0;
        }
    }

    /// Delete the last character of the overlay's search query. No-op when the
    /// query is already empty.
    pub fn model_overlay_backspace(&mut self) {
        if self.model_query.pop().is_some()
            && let Some(ov) = self.model_overlay.as_mut()
        {
            ov.selected = 0;
            ov.scroll = 0;
        }
    }

    /// Toggle the "show only free models" filter. The selection/scroll are
    /// reset so they stay in bounds of the new filtered view.
    pub fn model_overlay_toggle_free_only(&mut self) {
        self.model_free_only = !self.model_free_only;
        if let Some(ov) = self.model_overlay.as_mut() {
            ov.selected = 0;
            ov.scroll = 0;
        }
    }

    // ---------- inline list editor ----------

    /// Open the list editor over the focused row's list for `col`.
    pub fn open_list_editor(&mut self, col: Column) {
        let items = self
            .rows
            .get(self.cursor)
            .map(|r| match col {
                Column::Writable => r.spec.writable.clone(),
                Column::Deps => r.spec.depends_on.clone(),
                Column::Work => r
                    .spec
                    .planned_work
                    .iter()
                    .map(|w| w.subject.clone())
                    .collect(),
                _ => Vec::new(),
            })
            .unwrap_or_default();
        self.list_editor = Some(ListEditor {
            col,
            items,
            cursor: 0,
            text_edit: None,
            adding: false,
        });
    }

    pub fn list_editor_up(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && le.text_edit.is_none()
            && le.cursor > 0
        {
            le.cursor -= 1;
        }
    }

    pub fn list_editor_down(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && le.text_edit.is_none()
            && le.cursor + 1 < le.items.len()
        {
            le.cursor += 1;
        }
    }

    /// Begin adding a new item (empty text buffer appended at the end).
    pub fn list_editor_add(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && le.text_edit.is_none()
        {
            le.adding = true;
            le.text_edit = Some(crate::text_input::TextInput::new());
        }
    }

    /// Begin editing the focused item (buffer seeded from it).
    pub fn list_editor_edit(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && le.text_edit.is_none()
            && let Some(item) = le.items.get(le.cursor)
        {
            le.adding = false;
            le.text_edit = Some(crate::text_input::TextInput::with_text(item));
        }
    }

    /// Remove the focused item.
    pub fn list_editor_remove(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && le.text_edit.is_none()
            && le.cursor < le.items.len()
        {
            le.items.remove(le.cursor);
            if le.cursor > 0 && le.cursor >= le.items.len() {
                le.cursor = le.items.len().saturating_sub(1);
            }
        }
    }

    /// Commit the active item text buffer (add appends; edit replaces). Empty
    /// text is discarded.
    pub fn list_editor_commit_item(&mut self) {
        if let Some(le) = self.list_editor.as_mut()
            && let Some(buf) = le.text_edit.take()
        {
            let val = buf.value().trim().to_string();
            if !val.is_empty() {
                if le.adding {
                    le.items.push(val);
                    le.cursor = le.items.len() - 1;
                } else if let Some(slot) = le.items.get_mut(le.cursor) {
                    *slot = val;
                }
            }
            le.adding = false;
        }
    }

    /// Discard the active item text buffer without committing.
    pub fn list_editor_cancel_item(&mut self) {
        if let Some(le) = self.list_editor.as_mut() {
            le.text_edit = None;
            le.adding = false;
        }
    }

    /// Commit the whole edited list back to the row and close the editor.
    pub fn list_editor_commit(&mut self) {
        if let Some(le) = self.list_editor.take() {
            if let Some(row) = self.rows.get_mut(self.cursor) {
                match le.col {
                    Column::Writable => row.spec.writable = le.items,
                    Column::Deps => row.spec.depends_on = le.items,
                    Column::Work => {
                        // Preserve existing PlannedWork bodies where the subject
                        // still exists; new subjects get a default work item.
                        let existing = std::mem::take(&mut row.spec.planned_work);
                        row.spec.planned_work = le
                            .items
                            .into_iter()
                            .map(|subject| {
                                existing
                                    .iter()
                                    .find(|w| w.subject == subject)
                                    .cloned()
                                    .unwrap_or_else(|| PlannedWork {
                                        subject,
                                        ..Default::default()
                                    })
                            })
                            .collect();
                    }
                    _ => {}
                }
                self.dirty = true;
            }
        }
    }

    pub fn list_editor_cancel(&mut self) {
        self.list_editor = None;
    }

    /// Cancel edit mode without committing (Esc). A text edit is discarded.
    pub fn cancel_edit(&mut self) {
        self.editing = false;
        self.text_edit = None;
    }

    /// Current string value of a Text column, for seeding the edit buffer.
    fn text_value_for(&self, col: Column) -> String {
        match col {
            Column::Name => self
                .rows
                .get(self.cursor)
                .map(|r| r.spec.name.clone())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Write the live text buffer back to the focused row's field.
    fn commit_text_edit(&mut self) {
        let Some(buf) = self.text_edit.as_ref() else {
            return;
        };
        let value = buf.value();
        if self.focused_column() == Column::Name
            && let Some(row) = self.rows.get_mut(self.cursor)
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                row.spec.name = trimmed.to_string();
                self.dirty = true;
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

    /// Set the focused domain's agent count directly from a typed digit.
    pub fn set_agents_digit(&mut self, d: u32) {
        if let Some(row) = self.rows.get_mut(self.cursor) {
            // Single-digit set, clamped to at least 1. Simple and predictable;
            // multi-digit entry isn't needed for agent counts.
            row.spec.agents = (d as usize).max(1);
            self.dirty = true;
        }
    }

    /// Index of the focused row's current model within the catalog.
    fn model_index(&self) -> usize {
        let cur = self
            .rows
            .get(self.cursor)
            .and_then(|r| r.spec.model.clone())
            .unwrap_or_else(|| "(default)".to_string());
        self.models.iter().position(|m| *m == cur).unwrap_or(0)
    }

    /// Cycle the focused domain's assigned model to the next/previous catalog
    /// entry. "(default)" maps to None on the spec (clears the override).
    /// Superseded by the overlay picker for interactive use; retained for tests
    /// and quick programmatic cycling.
    #[allow(dead_code)]
    pub fn cycle_model(&mut self, forward: bool) {
        if self.models.is_empty() {
            return;
        }
        let n = self.models.len();
        let cur = self.model_index();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        let picked = self.models[next].clone();
        if let Some(row) = self.rows.get_mut(self.cursor) {
            row.spec.model = if picked == "(default)" {
                None
            } else {
                Some(picked)
            };
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

    /// Insert a new scaffolded domain row immediately AFTER the cursor and
    /// move the cursor onto it, so the user can immediately edit its name
    /// (press `e` on the Name column) and writable globs (press `e` on the
    /// Writable column). The new row is marked included and given a unique
    /// placeholder name + a default writable glob so it's close to valid;
    /// `validate_description` will still catch a forgotten name/glob at save
    /// time. Sets `dirty` — the user must press `s` to persist (the
    /// architecture already gates all writes behind `save_description`).
    pub fn add_row(&mut self) {
        let name = self.unique_scaffold_name();
        let spec = DomainSpec {
            name: name.clone(),
            description: String::new(),
            writable: vec![format!("src/{name}/**")],
            forbidden_write: Vec::new(),
            depends_on: Vec::new(),
            planned_work: Vec::new(),
            agents: 1,
            model: None,
        };
        let row = DomainRow {
            spec,
            include: true,
        };
        // Insert after the cursor (or at the end if the list is empty).
        let insert_at = if self.rows.is_empty() {
            0
        } else {
            self.cursor + 1
        };
        self.rows.insert(insert_at, row);
        self.cursor = insert_at;
        // Reset the column cursor to Include so the user starts at the
        // leftmost column of the new row, not whatever column the previous
        // row happened to be focused on.
        self.col_cursor = 0;
        self.dirty = true;
        self.status = Some(format!(
            "added '{}' — press e on the Name column to rename, s to save",
            name
        ));
    }

    /// Generate a unique scaffold name for a newly-added row: "new-domain",
    /// or "new-domain-2", "new-domain-3", ... if that's already taken.
    /// Uniqueness matters because `validate_description` rejects duplicate
    /// domain names, and a freshly-added row should be saveable without the
    /// user having to rename it first just to clear a collision.
    fn unique_scaffold_name(&self) -> String {
        let taken: std::collections::HashSet<&str> =
            self.rows.iter().map(|r| r.spec.name.as_str()).collect();
        if !taken.contains("new-domain") {
            return "new-domain".to_string();
        }
        let mut n = 2;
        loop {
            let candidate = format!("new-domain-{n}");
            if !taken.contains(candidate.as_str()) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Arm the delete-confirmation overlay for the focused row. The next
    /// keypress is captured by the run_loop's confirm intercept: `y`/`Y`
    /// calls `confirm_delete_row`, anything else calls `cancel_delete_row`.
    /// Does NOT remove the row yet — the on-screen confirmation is the
    /// whole point.
    pub fn request_delete_row(&mut self) {
        if self.cursor < self.rows.len() {
            self.delete_confirm_pending = true;
            self.status = None;
        }
    }

    /// Actually remove the focused row. Called by the confirm intercept
    /// when the user presses `y`. Clamps the cursor so it stays in bounds,
    /// sets `dirty`, and surfaces a status line noting which row was
    /// removed. The removal is NOT persisted to disk until the user presses
    /// `s` — same as every other mutation in this editor.
    pub fn confirm_delete_row(&mut self) {
        self.delete_confirm_pending = false;
        if self.cursor < self.rows.len() {
            let removed = self.rows.remove(self.cursor);
            if self.cursor > 0 && self.cursor >= self.rows.len() {
                self.cursor = self.rows.len().saturating_sub(1);
            }
            self.dirty = true;
            self.status = Some(format!("removed '{}' — press s to save", removed.spec.name));
        }
    }

    /// Cancel the pending delete without removing anything.
    pub fn cancel_delete_row(&mut self) {
        self.delete_confirm_pending = false;
        self.status = Some("delete cancelled".to_string());
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
    run_with_includes(root, &[])
}

/// Fetch the list of models opencode knows about by running `opencode models`
/// and taking each non-empty output line as a model id (e.g.
/// "openrouter/z-ai/glm-5.2"). Returns an empty Vec on any failure (opencode
/// missing, non-zero exit, timeout-ish hang avoided by the OS) so the caller
/// can fall back to the launcher profiles. Run ONCE at UI startup, before the
/// alternate screen is entered, so its output can't corrupt the TUI.
pub fn fetch_opencode_models() -> Vec<String> {
    use std::process::Command;
    let output = match Command::new("opencode").arg("models").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Like `run`, but the `G` (generate) action inside the UI also gathers
/// markdown from these extra include dirs (the `-i` folders), matching the CLI
/// gather flow. `run` passes an empty slice.
pub fn run_with_includes(root: &std::path::Path, includes: &[std::path::PathBuf]) -> Result<()> {
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

    // Populate the Model column's selector catalog. Prefer the live list from
    // `opencode models` (many models); fall back to the configured launcher
    // profiles if opencode isn't available. Fetched once here, before the
    // alternate screen, so the subprocess output can't corrupt the TUI.
    let mut model_names = fetch_opencode_models();
    let mut free_models: Vec<String> = Vec::new();
    if let Ok(config) = crate::load_config() {
        if model_names.is_empty() {
            model_names = config.launcher.profiles.keys().cloned().collect();
        }
        free_models = config.bench.free_models.clone();
    }
    state = state.with_models(&model_names).with_free_models(&free_models);
    // Attach the stored report JSON if an analysis has produced one, so the
    // Project view's Report pane shows real content. Best-effort: absence just
    // means the pane shows its placeholder.
    let report_path = root.join(".trelane").join("biplane-report.json");
    if let Ok(txt) = std::fs::read_to_string(&report_path) {
        // Re-pretty-print so the displayed JSON is readable regardless of how
        // it was written; fall back to the raw text if it doesn't parse.
        let pretty = serde_json::from_str::<serde_json::Value>(&txt)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(txt);
        state = state.with_report_json(pretty);
    }

    run_loop(root, includes, &mut state)
}

fn save_description(root: &std::path::Path, desc: &ProjectDescription) -> Result<()> {
    let dir = root.join(".trelane");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("biplane-description.json");
    std::fs::write(&path, serde_json::to_string_pretty(desc)?)?;
    Ok(())
}

fn run_loop(
    root: &std::path::Path,
    includes: &[std::path::PathBuf],
    state: &mut BiplaneUiState,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind};
    use crate::tui_session::TuiSession;
    use std::time::Duration;

    // TUI-006: the shared guard owns the raw-mode/alternate-screen ladder
    // and restores every completed stage in reverse order on Drop, so a
    // draw/poll error or a panic in the loop below can't strand the user's
    // terminal. stdout is boxed to match the guard's writer type.
    let mut session = TuiSession::enter()?;
    session.enter_alternate_screen(Box::new(std::io::stdout()))?;

    let outcome = (|| -> Result<()> {
        loop {
            // Short-lived per-statement borrows of the terminal: the loop
            // body calls `generate_via_model(&mut session, ...)` (suspend/
            // resume), which conflicts with a long-lived terminal borrow.
            session.terminal().unwrap().draw(|f| render(f, state))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // When the help overlay is open it captures input: only
                    // '?', Esc, or 'q' dismiss it; everything else is swallowed
                    // so the user can't accidentally edit behind the overlay.
                    if state.show_help {
                        match key.code {
                            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                                state.show_help = false;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // Model overlay picker: captures all input. Up/Down (and
                    // PageUp/PageDown) move the selection through the FILTERED
                    // catalog (search query + free-only toggle), Enter selects,
                    // Esc cancels. Typing a printable char refines the search;
                    // Backspace deletes; Tab toggles "free models only".
                    // `visible` is an estimate of the overlay's row window.
                    if state.model_overlay.is_some() {
                        let visible = 12usize;
                        match key.code {
                            KeyCode::Up => state.model_overlay_move(-1, visible),
                            KeyCode::Down => state.model_overlay_move(1, visible),
                            KeyCode::PageUp => {
                                state.model_overlay_move(-(visible as isize), visible)
                            }
                            KeyCode::PageDown => {
                                state.model_overlay_move(visible as isize, visible)
                            }
                            KeyCode::Enter => state.model_overlay_commit(),
                            KeyCode::Esc => state.model_overlay_cancel(),
                            KeyCode::Tab => state.model_overlay_toggle_free_only(),
                            KeyCode::Backspace => state.model_overlay_backspace(),
                            KeyCode::Char(c) => state.model_overlay_type(c),
                            _ => {}
                        }
                        continue;
                    }
                    // Inline list editor (work/deps/writable). Navigating items
                    // with Up/Down; a/e add/edit (opens a text buffer), d/Del
                    // remove; while a text buffer is open, type + Enter commits
                    // the item / Esc cancels it; Esc with no buffer commits the
                    // whole list and closes.
                    if let Some(le_editing) = state.list_editor.as_ref().map(|le| le.text_edit.is_some()) {
                        if le_editing {
                            match key.code {
                                KeyCode::Enter => state.list_editor_commit_item(),
                                KeyCode::Esc => state.list_editor_cancel_item(),
                                KeyCode::Backspace => {
                                    if let Some(le) = state.list_editor.as_mut()
                                        && let Some(b) = le.text_edit.as_mut()
                                    {
                                        b.backspace();
                                    }
                                }
                                KeyCode::Left => {
                                    if let Some(le) = state.list_editor.as_mut()
                                        && let Some(b) = le.text_edit.as_mut()
                                    {
                                        b.move_left();
                                    }
                                }
                                KeyCode::Right => {
                                    if let Some(le) = state.list_editor.as_mut()
                                        && let Some(b) = le.text_edit.as_mut()
                                    {
                                        b.move_right();
                                    }
                                }
                                KeyCode::Char(c) => {
                                    if let Some(le) = state.list_editor.as_mut()
                                        && let Some(b) = le.text_edit.as_mut()
                                    {
                                        b.insert(c);
                                    }
                                }
                                _ => {}
                            }
                        } else {
                            match key.code {
                                KeyCode::Up => state.list_editor_up(),
                                KeyCode::Down => state.list_editor_down(),
                                KeyCode::Char('a') => state.list_editor_add(),
                                KeyCode::Char('e') | KeyCode::Enter => state.list_editor_edit(),
                                KeyCode::Char('d') | KeyCode::Delete => state.list_editor_remove(),
                                KeyCode::Esc => state.list_editor_commit(),
                                _ => {}
                            }
                        }
                        continue;
                    }
                    // Edit-mode intercept: when a column is being edited, the
                    // value editor captures input BEFORE any global key, so
                    // typing into a text field (or adjusting a value) never
                    // triggers q/s/G/T/?. 'e' commits and exits; Esc cancels.
                    if state.editing && state.view == BiplaneView::Domains {
                        match state.focused_column().kind() {
                            EditKind::Text => match key.code {
                                // 'e' commits and exits (matched before the
                                // catch-all Char arm so it isn't typed).
                                KeyCode::Char('e') => state.toggle_edit(),
                                KeyCode::Esc => state.cancel_edit(),
                                KeyCode::Enter => state.toggle_edit(), // commit
                                KeyCode::Backspace => {
                                    if let Some(b) = state.text_edit.as_mut() {
                                        b.backspace();
                                    }
                                }
                                KeyCode::Left => {
                                    if let Some(b) = state.text_edit.as_mut() {
                                        b.move_left();
                                    }
                                }
                                KeyCode::Right => {
                                    if let Some(b) = state.text_edit.as_mut() {
                                        b.move_right();
                                    }
                                }
                                KeyCode::Char(c) => {
                                    if let Some(b) = state.text_edit.as_mut() {
                                        b.insert(c);
                                    }
                                }
                                _ => {}
                            },
                            EditKind::Toggle => match key.code {
                                KeyCode::Char('e') | KeyCode::Esc => state.toggle_edit(),
                                KeyCode::Char(' ')
                                | KeyCode::Enter
                                | KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::Left
                                | KeyCode::Right => state.toggle_include(),
                                _ => {}
                            },
                            EditKind::Numeric => match key.code {
                                KeyCode::Char('e') => state.toggle_edit(),
                                KeyCode::Esc => state.cancel_edit(),
                                KeyCode::Up | KeyCode::Char('+') => state.adjust_agents(true),
                                KeyCode::Down | KeyCode::Char('-') => state.adjust_agents(false),
                                KeyCode::Char(c) if c.is_ascii_digit() => {
                                    state.set_agents_digit(c.to_digit(10).unwrap_or(1));
                                }
                                _ => {}
                            },
                            // Selector (Model) and List columns don't use inline
                            // edit mode -- toggle_edit routes them to overlays,
                            // so state.editing is never true for them. These
                            // arms are unreachable but keep the match total.
                            EditKind::Selector | EditKind::List => {
                                if matches!(key.code, KeyCode::Char('e') | KeyCode::Esc) {
                                    state.cancel_edit();
                                }
                            }
                        }
                        continue;
                    }
                    // Row-delete confirmation intercept: when a delete is
                    // pending, the very next key is captured BEFORE any
                    // global or view-specific key, so 'q'/Tab/'s' can't
                    // slip past an unanswered "delete? y/n". Mirrors the
                    // monitor's kill_confirm_pending pattern.
                    if state.delete_confirm_pending {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => state.confirm_delete_row(),
                            _ => state.cancel_delete_row(),
                        }
                        continue; // key consumed by the confirmation
                    }
                    // Keys common to every view come first; view-specific keys
                    // are dispatched by the active sub-screen. This is the one
                    // place that knows which view is active, so the pure state
                    // methods stay view-agnostic and individually testable.
                    match key.code {
                        KeyCode::Char('?') => state.show_help = true,
                        KeyCode::Char('q') | KeyCode::Esc => state.should_quit = true,
                        KeyCode::Char('T') | KeyCode::Tab => state.toggle_view(),
                        KeyCode::Char('V') => state.toggle_project_pane(),
                        KeyCode::Char('G') | KeyCode::Char('g') => {
                            // AI analysis: gather markdown (root + include dirs)
                            // and submit to a model, replacing the current
                            // domains with the generated plan. The call takes
                            // seconds and prints, so it runs with the alternate
                            // screen suspended, then the TUI is restored.
                            match generate_via_model(&mut session, root, includes) {
                                Ok(new_state) => {
                                    let report = state.report_json.clone();
                                    // Preserve the standing free-only preference so
                                    // the user doesn't have to re-toggle it after
                                    // generating new domains.
                                    let free_only = state.model_free_only;
                                    *state = new_state;
                                    state.model_free_only = free_only;
                                    // Preserve any report already shown if the
                                    // regen didn't produce one.
                                    if state.report_json.is_none() {
                                        state.report_json = report;
                                    }
                                    state.status =
                                        Some("generated domains from AI analysis".to_string());
                                }
                                Err(e) => {
                                    state.last_error = Some(format!("generate failed: {e}"));
                                    state.status = Some(format!("generate failed: {e}"));
                                }
                            }
                            // A full redraw next frame clears any residue the
                            // suspended model output may have left. (resume()
                            // already cleared, but a second clear here is
                            // harmless and matches the old explicit intent.)
                            session.terminal().unwrap().clear()?;
                        }
                        KeyCode::Char('s') => {
                            if let Some(desc) = state.validated() {
                                save_description(root, &desc)?;
                                state.mark_saved();
                            }
                        }
                        other => match state.view {
                            BiplaneView::Domains => match other {
                                KeyCode::Up => state.cursor_up(),
                                KeyCode::Down => state.cursor_down(),
                                // Left/Right now NAVIGATE columns (the fix); a
                                // value only changes once its column is edited.
                                KeyCode::Left => state.col_left(),
                                KeyCode::Right => state.col_right(),
                                // 'e' enters edit mode on the focused column.
                                KeyCode::Char('e') => state.toggle_edit(),
                                // space is a convenience quick-toggle for the
                                // include checkbox regardless of focused column.
                                KeyCode::Char(' ') => state.toggle_include(),
                                KeyCode::Char('[') => state.adjust_budget(false),
                                KeyCode::Char(']') => state.adjust_budget(true),
                                KeyCode::Char('K') => state.move_up(),
                                KeyCode::Char('J') => state.move_down(),
                                // 'a' adds a new scaffolded domain row after
                                // the cursor and moves the cursor onto it so
                                // the user can immediately edit its name and
                                // writable globs. Not persisted until 's'.
                                KeyCode::Char('a') => state.add_row(),
                                // 'D' (shift-d) arms a delete confirmation
                                // overlay — the row is NOT removed until the
                                // user presses 'y'; any other key cancels.
                                // Uppercase to signal "destructive, gated"
                                // and to avoid overloading lowercase 'd'
                                // (which the list editor uses for item-level
                                // delete).
                                KeyCode::Char('D') => state.request_delete_row(),
                                _ => {}
                            },
                            BiplaneView::Project => match other {
                                KeyCode::Up => state.project_scroll_up(),
                                KeyCode::Down => state.project_scroll_down(),
                                _ => {}
                            },
                        },
                    }
                }
            }
            if state.should_quit {
                break;
            }
        }
        Ok(())
    })();

    // TUI-006: restore every stage in reverse order without short-circuiting;
    // the loop's outcome takes precedence over a cleanup error.
    let close_result = session.close();
    outcome?;
    close_result
}

/// Run AI analysis over the project (root + include dirs) and return a fresh
/// UI state built from the generated plan. The model call is a subprocess that
/// prints and takes seconds, so the alternate screen is LEFT for the duration
/// (raw mode disabled, cursor shown) and re-entered afterward -- otherwise the
/// model's stdout would corrupt the TUI. The generated description is persisted
/// so it survives a later reload, and its report JSON is attached for the
/// Project view. Any failure (model error, no network) propagates as an Err for
/// the caller to surface in the status line; the TUI is always restored first.
///
/// TUI-006: the leave/re-enter is the shared guard's suspend()/resume(), so
/// the stage flags track state across the pair and a panic or error in the
/// middle can't strand the terminal in a half-suspended state.
fn generate_via_model(
    session: &mut crate::tui_session::TuiSession,
    root: &std::path::Path,
    includes: &[std::path::PathBuf],
) -> Result<BiplaneUiState> {
    // Leave the alternate screen so the model subprocess can print to a normal
    // terminal without fighting the TUI's cells.
    session.suspend()?;
    println!();
    println!("[biplane] generating domains via AI analysis...");

    // Do the work, capturing the result so we can always restore the TUI
    // regardless of success or failure.
    let result = (|| -> Result<BiplaneUiState> {
        let model = crate::biplane::default_biplane_model();
        let max_agents = 3;
        let plan =
            crate::biplane::run_biplane_plan_from_sources(root, includes, &model, max_agents)?;
        let project_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        let desc = crate::biplane::plan_to_description(&plan, project_name, max_agents);

        // Persist so a later reload picks it up, matching the CLI flow.
        let desc_path = root.join(".trelane").join("biplane-description.json");
        if let Some(parent) = desc_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&desc_path, serde_json::to_string_pretty(&desc)?)?;

        let mut new_state = BiplaneUiState::from_description(&desc, "generated by AI analysis");
        let mut model_names = fetch_opencode_models();
        let mut free_models: Vec<String> = Vec::new();
        if let Ok(config) = crate::load_config() {
            if model_names.is_empty() {
                model_names = config.launcher.profiles.keys().cloned().collect();
            }
            free_models = config.bench.free_models.clone();
        }
        new_state = new_state
            .with_models(&model_names)
            .with_free_models(&free_models);
        if let Ok(json) = serde_json::to_string_pretty(&plan) {
            new_state = new_state.with_report_json(json);
        }
        Ok(new_state)
    })();

    // Re-enter the alternate screen no matter what happened above. resume()
    // re-enables raw mode, re-enters the alt screen, and clears (the normal
    // screen's content is untrusted after the subprocess printed).
    session.resume()?;

    result
}

fn tc(rgb: (u8, u8, u8)) -> ratatui::style::Color {
    ratatui::style::Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// Style for a column header: the focused column's label is highlighted
/// (bold + accent) so it's clear which column the cursor is on even before
/// looking at the row.
fn col_header_style(
    state: &BiplaneUiState,
    col: Column,
    dim: ratatui::style::Color,
) -> ratatui::style::Style {
    use ratatui::style::{Modifier, Style};
    if state.focused_column() == col {
        Style::default()
            .fg(tc(THEME_BIPLANE_ACCENT))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(dim).add_modifier(Modifier::BOLD)
    }
}

/// Fit a string to exactly `w` display cells: right-pad with spaces if short,
/// or truncate with a trailing ellipsis if long. This is what makes every
/// column a fixed width regardless of content, so nothing shifts as values
/// change. Counts by chars (adequate for the ASCII-ish content here).
fn fit(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len == w {
        s.to_string()
    } else if len < w {
        format!("{s}{}", " ".repeat(w - len))
    } else if w == 0 {
        String::new()
    } else if w == 1 {
        "…".to_string()
    } else {
        let keep: String = s.chars().take(w - 1).collect();
        format!("{keep}…")
    }
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

    // Domain list. Each cell is a bare value fitted to its column's fixed
    // width (no "agents:"/"model:" prefixes -- those move to the header row),
    // so columns never shift as content changes.
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let on_cursor_row = i == state.cursor;
            let marker = if on_cursor_row { "▶ " } else { "  " };

            // Style a column's span: the focused column of the cursor row is
            // highlighted -- reversed while editing, bold+underlined when just
            // focused.
            let col_style = |col: Column, base: Style| -> Style {
                if on_cursor_row && state.focused_column() == col {
                    if state.editing {
                        base.add_modifier(Modifier::REVERSED)
                    } else {
                        base.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    }
                } else {
                    base
                }
            };

            let check_base = if row.include {
                Style::default().fg(tc(THEME_OK))
            } else {
                Style::default().fg(dim)
            };
            let check_txt = if row.include { "[x]" } else { "[ ]" };

            // Name: show the live edit buffer when this row's Name is editing.
            let name_txt = if on_cursor_row
                && state.editing
                && state.focused_column() == Column::Name
            {
                state
                    .text_edit
                    .as_ref()
                    .map(|b| b.value())
                    .unwrap_or_else(|| row.spec.name.clone())
            } else {
                row.spec.name.clone()
            };
            let name_base = if row.include {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim)
            };

            let model_txt = row.spec.model.clone().unwrap_or_else(|| "(default)".to_string());
            let deps = if row.spec.depends_on.is_empty() {
                "-".to_string()
            } else {
                row.spec.depends_on.join(",")
            };
            let writable = if row.spec.writable.is_empty() {
                "-".to_string()
            } else {
                row.spec.writable.join(",")
            };

            ListItem::new(Line::from(vec![
                Span::raw(marker),
                Span::styled(
                    fit(check_txt, Column::Include.width()),
                    col_style(Column::Include, check_base),
                ),
                Span::styled(
                    fit(&name_txt, Column::Name.width()),
                    col_style(Column::Name, name_base),
                ),
                Span::styled(
                    fit(&model_txt, Column::Model.width()),
                    col_style(Column::Model, Style::default().fg(dim)),
                ),
                Span::styled(
                    fit(&row.spec.agents.to_string(), Column::Agents.width()),
                    col_style(Column::Agents, Style::default().fg(dim)),
                ),
                Span::styled(
                    fit(&row.spec.planned_work.len().to_string(), Column::Work.width()),
                    col_style(Column::Work, Style::default().fg(dim)),
                ),
                Span::styled(
                    fit(&deps, Column::Deps.width()),
                    col_style(Column::Deps, Style::default().fg(dim)),
                ),
                Span::styled(
                    fit(&writable, Column::Writable.width()),
                    col_style(Column::Writable, Style::default().fg(dim)),
                ),
            ]))
        })
        .collect();
    // Content area (chunks[1]) depends on the active sub-screen.
    match state.view {
        BiplaneView::Domains => {
            // A header row above the list, using the SAME fixed widths so the
            // labels sit exactly over their columns. The focused column's
            // header is highlighted so it's obvious which column is active.
            let header_row = Line::from(vec![
                Span::raw("  "), // aligns under the ▶ marker gutter
                Span::styled(
                    fit("inc", Column::Include.width()),
                    col_header_style(state, Column::Include, dim),
                ),
                Span::styled(
                    fit("name", Column::Name.width()),
                    col_header_style(state, Column::Name, dim),
                ),
                Span::styled(
                    fit("model", Column::Model.width()),
                    col_header_style(state, Column::Model, dim),
                ),
                Span::styled(
                    fit("agents", Column::Agents.width()),
                    col_header_style(state, Column::Agents, dim),
                ),
                Span::styled(
                    fit("work", Column::Work.width()),
                    col_header_style(state, Column::Work, dim),
                ),
                Span::styled(
                    fit("deps", Column::Deps.width()),
                    col_header_style(state, Column::Deps, dim),
                ),
                Span::styled(
                    fit("writable", Column::Writable.width()),
                    col_header_style(state, Column::Writable, dim),
                ),
            ]);
            // Split the domains area into a 1-line header and the list below.
            let dom_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(2), Constraint::Min(1)])
                .split(chunks[1]);
            let header_para = Paragraph::new(header_row).block(
                Block::default()
                    .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                    .title(" Domains ")
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(header_para, dom_chunks[0]);
            let list = List::new(items).block(
                Block::default()
                    .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                    .border_style(Style::default().fg(accent)),
            );
            f.render_widget(list, dom_chunks[1]);
        }
        BiplaneView::Project => {
            let (title, body): (&str, String) = match state.project_pane {
                ProjectPane::Summary => (
                    " Project — AI Summary  (V: report JSON) ",
                    if state.project_summary.trim().is_empty() {
                        "(no AI summary available — run analysis with G)".to_string()
                    } else {
                        state.project_summary.clone()
                    },
                ),
                ProjectPane::ReportJson => (
                    " Project — Report JSON  (V: AI summary) ",
                    state.report_json.clone().unwrap_or_else(|| {
                        "(no analysis report yet — run analysis with G)".to_string()
                    }),
                ),
            };
            let para = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(Style::default().fg(accent)),
                )
                .wrap(ratatui::widgets::Wrap { trim: false })
                .scroll((state.project_scroll, 0));
            f.render_widget(para, chunks[1]);
        }
    }

    // Footer — hint depends on the active view so keys shown are the live ones.
    let default_hint = match state.view {
        BiplaneView::Domains => {
            if state.delete_confirm_pending {
                "delete this domain?  y confirm   any other key cancel"
            } else if state.model_overlay.is_some() {
                "picking model — type to filter   Tab free-only   ↑↓ PgUp/PgDn move   Enter select   Esc cancel"
            } else if state.list_editor.is_some() {
                "list editor — ↑↓ move   a add   e edit   d delete   Esc save & close"
            } else if state.editing {
                "editing — type/adjust value   e/Enter commit   Esc cancel"
            } else {
                "↑↓ row  ←→ column  e edit  a add row  D delete row  [ ] budget  K/J reorder  G generate  T project  s save  ? help  q quit"
            }
        }
        BiplaneView::Project => "↑↓ scroll  V switch pane  G generate  T domains  s save  ? help  q quit",
    };
    let hint = state
        .status
        .clone()
        .unwrap_or_else(|| default_hint.to_string());
    let footer = Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(dim)))).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(dim)),
    );
    f.render_widget(footer, chunks[2]);

    // Help overlay: drawn last so it sits on top of everything.
    if state.show_help {
        render_help_overlay(f, accent, dim);
    }
    // Model picker overlay and list editor sit on top of the domain view.
    if state.model_overlay.is_some() {
        render_model_overlay(f, state, accent, dim);
    }
    if state.list_editor.is_some() {
        render_list_editor(f, state, accent, dim);
    }
    // Delete confirmation sits on top of everything else so it's the
    // unmissable focal point when armed.
    if state.delete_confirm_pending {
        render_delete_confirm(f, state, accent, dim);
    }
}

/// The delete-confirmation overlay: a small centered popup naming the
/// domain about to be removed and asking for an explicit y/n. Any key other
/// than y/Y cancels (the run_loop's confirm intercept handles this), so the
/// user can't accidentally confirm with a stray Enter or space.
fn render_delete_confirm(
    f: &mut ratatui::Frame,
    state: &BiplaneUiState,
    accent: ratatui::style::Color,
    dim: ratatui::style::Color,
) {
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(row) = state.rows.get(state.cursor) else {
        return;
    };
    let name = &row.spec.name;

    let area = f.area();
    let w = 52u16.min(area.width.saturating_sub(4));
    let h = 7u16;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    let warn = tc(crate::diagnostic::THEME_WARN);
    let lines = vec![
        Line::from(Span::styled(
            "Delete domain?",
            Style::default().fg(warn).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(name.clone(), Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            Span::styled(
                "  and all its settings will be removed from the plan.",
                Style::default().fg(dim),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  y", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" to confirm   ", Style::default().fg(dim)),
            Span::styled("any other key", Style::default().fg(dim)),
            Span::styled(" to cancel", Style::default().fg(dim)),
        ]),
    ];

    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Confirm Delete ")
            .border_style(Style::default().fg(warn)),
    );
    f.render_widget(para, popup);
}

/// A centered, bordered help overlay listing every key binding, grouped by
/// view. Drawn over a Clear so nothing behind it shows through.
fn render_help_overlay(
    f: &mut ratatui::Frame,
    accent: ratatui::style::Color,
    dim: ratatui::style::Color,
) {
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    // A key/description row helper for consistent alignment.
    fn key<'a>(k: &'a str, desc: &'a str) -> Line<'a> {
        Line::from(vec![
            Span::styled(format!("  {k:<12}"), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(desc),
        ])
    }

    let area = f.area();
    // Center a fixed-size box within the frame.
    let w = 66u16.min(area.width.saturating_sub(4));
    let h = 22u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    let lines = vec![
        Line::from(Span::styled(
            "Biplane — key bindings",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled("Domains view", Style::default().fg(accent))),
        key("↑ ↓", "move the row cursor"),
        key("← →", "move the COLUMN cursor (include/name/model/agents/...)"),
        key("e", "edit the focused column"),
        key("  toggle/name/agents", "edit inline (space, type, or ↑↓/digits)"),
        key("  model", "opens a picker — type to filter, Tab free-only, ↑↓/PgUp/PgDn, Enter, Esc"),
        key("  work/deps/writable", "opens a list editor — a add, e edit, d delete"),
        key("space", "quick-toggle the include checkbox"),
        key("[ ]", "decrease / increase the overall agent budget"),
        key("K / J", "reorder the focused domain up / down"),
        key("a", "add a new scaffolded domain row after the cursor"),
        key("D", "delete the focused domain row (asks y/n to confirm)"),
        Line::from(""),
        Line::from(Span::styled("Project view", Style::default().fg(accent))),
        key("↑ ↓", "scroll the content"),
        key("V", "switch between the summary and report-JSON panes"),
        Line::from(""),
        Line::from(Span::styled("Anywhere", Style::default().fg(accent))),
        key("T / Tab", "switch between Domains and Project views"),
        key("G", "generate domains via AI analysis (gathers markdown, calls a model)"),
        key("s", "save the description"),
        key("?", "toggle this help"),
        key("q / Esc", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  press ?, Esc, or q to close",
            Style::default().fg(dim),
        )),
    ];

    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Help ")
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(para, popup);
}

/// The model picker overlay: a centered, scrollable window over the model
/// catalog, filtered by the live search query and the "free models only"
/// toggle. Shows a fixed window of entries around the selection with a scroll
/// indicator, a search field at the top, and a free-only status line, so a
/// long list (many opencode models) stays navigable.
fn render_model_overlay(
    f: &mut ratatui::Frame,
    state: &BiplaneUiState,
    accent: ratatui::style::Color,
    dim: ratatui::style::Color,
) {
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(ov) = state.model_overlay.as_ref() else {
        return;
    };
    let area = f.area();
    let w = 60u16.min(area.width.saturating_sub(4));
    let h = 18u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    // Resolve the FILTERED view (indices into the raw catalog). The overlay's
    // selection/scroll live in this filtered coordinate space.
    let filtered = state.filtered_model_indices();
    let total = filtered.len();
    // visible rows: subtract top search field + free-only line + bottom hint
    // (3 fixed lines) and the 2 border rows.
    let visible = (h as usize).saturating_sub(5).max(1);
    let start = ov.scroll.min(total.saturating_sub(1));
    let end = (start + visible).min(total);

    // Top: the live search field, with a caret so the user can see input focus.
    let query_display = if state.model_query.is_empty() {
        "(type to filter)".to_string()
    } else {
        format!("{}▏", state.model_query)
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("  filter: ", Style::default().fg(dim)),
        Span::styled(
            query_display,
            if state.model_query.is_empty() {
                Style::default().fg(dim)
            } else {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            },
        ),
    ]));
    // Free-only toggle status: shows ON/OFF and the size of the allowlist so
    // the user knows whether the filter has teeth.
    let free_label = if state.model_free_only {
        "ON"
    } else {
        "OFF"
    };
    let free_style = if state.model_free_only {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(dim)
    };
    lines.push(Line::from(vec![
        Span::styled("  free-only: ", Style::default().fg(dim)),
        Span::styled(format!("{free_label} "), free_style),
        Span::styled(
            format!("(Tab to toggle, {} known free)", state.free_models.len()),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));

    let mut shown = 0;
    for (idx, &real) in filtered[start..end].iter().enumerate() {
        let real_idx = start + idx;
        let name = state.models.get(real).map(|s| s.as_str()).unwrap_or("");
        let selected = real_idx == ov.selected;
        // Mark free models with a leading sigil so the user can spot them at a
        // glance even when the free-only filter is off.
        let is_free = state.free_models.iter().any(|f| f == name);
        let tag = if real == 0 {
            " " // "(default)" isn't a model entry, no free/paid marking
        } else if is_free {
            "✓"
        } else {
            " "
        };
        let style = if selected {
            Style::default().fg(accent).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(dim)
        };
        let marker = if selected { "›" } else { " " };
        lines.push(Line::from(Span::styled(
            format!("{marker} {tag} {name}"),
            style,
        )));
        shown += 1;
    }
    if shown == 0 {
        lines.push(Line::from(Span::styled(
            "  (no matches — refine the filter or toggle free-only off)",
            Style::default().fg(dim),
        )));
    }

    lines.push(Line::from(Span::styled(
        format!(
            "  {}/{}   ↑↓ PgUp/PgDn move   Tab free-only   Enter select   Esc cancel",
            if total == 0 { 0 } else { ov.selected + 1 },
            total
        ),
        Style::default().fg(dim),
    )));

    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Select model ")
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(para, popup);
}

/// The inline list editor overlay: shows the items of a work/deps/writable
/// list with add/edit/remove, and a live text field while adding or editing an
/// item.
fn render_list_editor(
    f: &mut ratatui::Frame,
    state: &BiplaneUiState,
    accent: ratatui::style::Color,
    dim: ratatui::style::Color,
) {
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let Some(le) = state.list_editor.as_ref() else {
        return;
    };
    let area = f.area();
    let w = 70u16.min(area.width.saturating_sub(4));
    let h = 18u16.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    let mut lines: Vec<Line> = Vec::new();
    if le.items.is_empty() && le.text_edit.is_none() {
        lines.push(Line::from(Span::styled(
            "  (no items — press a to add)",
            Style::default().fg(dim),
        )));
    }
    for (idx, item) in le.items.iter().enumerate() {
        let focused = idx == le.cursor && le.text_edit.is_none();
        // If editing THIS existing item, show the live buffer instead.
        let text = if idx == le.cursor
            && !le.adding
            && let Some(b) = le.text_edit.as_ref()
        {
            format!("{}▏", b.value())
        } else {
            item.clone()
        };
        let style = if focused {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(dim)
        };
        let marker = if focused { "› " } else { "  " };
        lines.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
    }
    // A new item being added shows as a trailing live line.
    if le.adding
        && let Some(b) = le.text_edit.as_ref()
    {
        lines.push(Line::from(Span::styled(
            format!("› {}▏", b.value()),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    let hint = if le.text_edit.is_some() {
        "  type   Enter save item   Esc cancel item"
    } else {
        "  ↑↓ move   a add   e edit   d delete   Esc save & close"
    };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(dim))));

    f.render_widget(Clear, popup);
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Edit {} ", le.col.label()))
            .border_style(Style::default().fg(accent)),
    );
    f.render_widget(para, popup);
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
                ..Default::default()
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
    fn help_overlay_defaults_off() {
        // The `?` help overlay starts hidden; the input loop flips show_help.
        let s = state();
        assert!(!s.show_help);
    }

    // ---------------- column navigation + edit mode ----------------

    #[test]
    fn column_cursor_starts_on_include_and_navigates() {
        let mut s = state();
        assert_eq!(s.focused_column(), Column::Include);
        s.col_right();
        assert_eq!(s.focused_column(), Column::Name);
        s.col_right();
        assert_eq!(s.focused_column(), Column::Model);
        s.col_left();
        assert_eq!(s.focused_column(), Column::Name);
    }

    #[test]
    fn column_cursor_clamps_at_ends() {
        let mut s = state();
        s.col_left(); // already at 0
        assert_eq!(s.focused_column(), Column::Include);
        for _ in 0..20 {
            s.col_right();
        }
        assert_eq!(s.focused_column(), Column::Writable); // last column
    }

    #[test]
    fn left_right_do_not_change_value_when_not_editing() {
        // The reported bug: left/right must NOT change the agents count.
        let mut s = state();
        // Move column cursor to Agents.
        while s.focused_column() != Column::Agents {
            s.col_right();
        }
        let before = s.rows[s.cursor].spec.agents;
        s.col_left();
        s.col_right();
        assert_eq!(
            s.rows[s.cursor].spec.agents, before,
            "navigating columns must not mutate the agent count"
        );
    }

    #[test]
    fn col_nav_is_inert_while_editing() {
        let mut s = state();
        while s.focused_column() != Column::Agents {
            s.col_right();
        }
        s.toggle_edit();
        assert!(s.editing);
        let col = s.focused_column();
        s.col_left();
        s.col_right();
        assert_eq!(s.focused_column(), col, "column cursor frozen while editing");
    }

    #[test]
    fn edit_mode_agents_numeric() {
        let mut s = state();
        while s.focused_column() != Column::Agents {
            s.col_right();
        }
        s.toggle_edit();
        let start = s.rows[s.cursor].spec.agents;
        s.adjust_agents(true);
        assert_eq!(s.rows[s.cursor].spec.agents, start + 1);
        s.set_agents_digit(5);
        assert_eq!(s.rows[s.cursor].spec.agents, 5);
        s.set_agents_digit(0); // clamps to at least 1
        assert_eq!(s.rows[s.cursor].spec.agents, 1);
    }

    #[test]
    fn edit_mode_model_selector_cycles_and_clears() {
        let mut s = state().with_models(&["glm".to_string(), "opencode".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // Catalog: ["(default)", "glm", "opencode"]. Start at (default) -> None.
        assert!(s.rows[s.cursor].spec.model.is_none());
        s.cycle_model(true);
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("glm"));
        s.cycle_model(true);
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("opencode"));
        s.cycle_model(true); // wraps to "(default)" -> None
        assert!(s.rows[s.cursor].spec.model.is_none());
    }

    #[test]
    fn edit_mode_name_text_commits_on_exit() {
        let mut s = state();
        while s.focused_column() != Column::Name {
            s.col_right();
        }
        s.toggle_edit();
        // Simulate typing a new name.
        if let Some(b) = s.text_edit.as_mut() {
            *b = crate::text_input::TextInput::with_text("renamed");
        }
        s.toggle_edit(); // commit
        assert!(!s.editing);
        assert_eq!(s.rows[s.cursor].spec.name, "renamed");
    }

    #[test]
    fn edit_mode_name_cancel_discards() {
        let mut s = state();
        let original = s.rows[s.cursor].spec.name.clone();
        while s.focused_column() != Column::Name {
            s.col_right();
        }
        s.toggle_edit();
        if let Some(b) = s.text_edit.as_mut() {
            *b = crate::text_input::TextInput::with_text("throwaway");
        }
        s.cancel_edit();
        assert!(!s.editing);
        assert_eq!(s.rows[s.cursor].spec.name, original, "cancel discards edit");
    }

    #[test]
    fn list_columns_are_not_inline_editable() {
        // Landing on a List column and pressing 'e' opens the LIST EDITOR now
        // (not inline editing) -- so state.editing stays false but the editor
        // is open.
        let mut s = state();
        while s.focused_column() != Column::Writable {
            s.col_right();
        }
        s.toggle_edit();
        assert!(!s.editing, "list columns don't use inline edit mode");
        assert!(s.list_editor.is_some(), "list editor opens instead");
    }

    // ---------------- model overlay picker ----------------

    #[test]
    fn model_overlay_opens_on_current_and_commits() {
        let mut s = state().with_models(&["glm".to_string(), "opencode".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        assert!(s.model_overlay.is_some(), "e opens the overlay on Model");
        // Catalog: ["(default)","glm","opencode"], starts at (default)=index 0.
        s.model_overlay_move(1, 10);
        s.model_overlay_move(1, 10); // -> "opencode"
        s.model_overlay_commit();
        assert!(s.model_overlay.is_none(), "commit closes the overlay");
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("opencode"));
    }

    #[test]
    fn model_overlay_cancel_leaves_value_unchanged() {
        let mut s = state().with_models(&["glm".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        s.model_overlay_move(1, 10); // would pick glm
        s.model_overlay_cancel();
        assert!(s.model_overlay.is_none());
        assert!(s.rows[s.cursor].spec.model.is_none(), "cancel keeps default");
    }

    #[test]
    fn model_overlay_move_clamps_and_scrolls() {
        let many: Vec<String> = (0..30).map(|i| format!("m{i}")).collect();
        let mut s = state().with_models(&many);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // Move way down; selection clamps to last, scroll follows.
        s.model_overlay_move(1000, 10);
        let ov = s.model_overlay.as_ref().unwrap();
        assert_eq!(ov.selected, s.models.len() - 1);
        assert!(ov.scroll <= ov.selected);
        assert!(ov.selected < ov.scroll + 10, "selection within window");
    }

    #[test]
    fn model_overlay_search_filters_and_keeps_default() {
        // Catalog: ["(default)", "openrouter/glm", "openrouter/llama", "anthropic/claude"]
        let mut s = state().with_models(&[
            "openrouter/glm".to_string(),
            "openrouter/llama".to_string(),
            "anthropic/claude".to_string(),
        ]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // No query: all four entries visible (default + 3 models).
        assert_eq!(s.filtered_model_indices().len(), 4);
        // Type "llama": only "(default)" and "openrouter/llama" remain
        // ("(default)" is always kept so the user can clear the override).
        s.model_overlay_type('l');
        s.model_overlay_type('l');
        s.model_overlay_type('a');
        s.model_overlay_type('m');
        s.model_overlay_type('a');
        let filtered = s.filtered_model_indices();
        assert_eq!(filtered.len(), 2, "query keeps default + matching model");
        assert_eq!(filtered[0], 0, "default always first");
        assert_eq!(s.models[filtered[1]], "openrouter/llama");
        // Selection reset to 0 when the query changed.
        assert_eq!(s.model_overlay.as_ref().unwrap().selected, 0);
        // Committing picks the match.
        s.model_overlay_move(1, 10);
        s.model_overlay_commit();
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("openrouter/llama"));
    }

    #[test]
    fn model_overlay_search_is_case_insensitive() {
        let mut s = state().with_models(&["OpenRouter/GLM".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        s.model_overlay_type('g');
        s.model_overlay_type('l');
        s.model_overlay_type('m');
        assert_eq!(s.filtered_model_indices().len(), 2); // default + match
        s.model_overlay_move(1, 10);
        s.model_overlay_commit();
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("OpenRouter/GLM"));
    }

    #[test]
    fn model_overlay_backspace_shortens_query() {
        let mut s = state().with_models(&["alpha".to_string(), "zeta".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // Catalog: ["(default)", "alpha", "zeta"]. Query "z" matches only
        // "zeta" (plus the always-kept default) -> 2 visible.
        s.model_overlay_type('z');
        assert_eq!(s.filtered_model_indices().len(), 2);
        s.model_overlay_backspace(); // query empty again -> all visible
        assert_eq!(s.filtered_model_indices().len(), 3);
    }

    #[test]
    fn model_overlay_free_only_toggle_hides_paid() {
        // Catalog: ["(default)", "free-a", "paid-b"]; free_models = ["free-a"].
        let mut s = state()
            .with_models(&["free-a".to_string(), "paid-b".to_string()])
            .with_free_models(&["free-a".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // Off: everything visible.
        assert_eq!(s.filtered_model_indices().len(), 3);
        s.model_overlay_toggle_free_only();
        // On: "(default)" still shown, "paid-b" hidden, "free-a" kept.
        let filtered = s.filtered_model_indices();
        assert_eq!(filtered.len(), 2, "free-only hides paid models, keeps default");
        assert_eq!(filtered[0], 0);
        assert_eq!(s.models[filtered[1]], "free-a");
        // Selection reset to 0 on the toggle.
        assert_eq!(s.model_overlay.as_ref().unwrap().selected, 0);
        // Select "free-a" and commit.
        s.model_overlay_move(1, 10);
        s.model_overlay_commit();
        assert_eq!(s.rows[s.cursor].spec.model.as_deref(), Some("free-a"));
    }

    #[test]
    fn model_overlay_open_clears_query_but_preserves_free_only() {
        let mut s = state()
            .with_models(&["free-a".to_string(), "paid-b".to_string()])
            .with_free_models(&["free-a".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        s.model_overlay_toggle_free_only(); // ON
        s.model_overlay_type('x'); // no matches
        s.model_overlay_cancel();
        // Reopen: query should be cleared, but free-only preference persists.
        s.toggle_edit();
        assert_eq!(s.model_query, "", "query cleared on reopen");
        assert!(s.model_free_only, "free-only preference persists");
        // With free-only on and empty query, "(default)" + "free-a" visible.
        assert_eq!(s.filtered_model_indices().len(), 2);
    }

    #[test]
    fn model_overlay_open_resolves_current_model_in_filtered_view() {
        // Row already has a free model assigned; with free_only on, the overlay
        // should still open on that model (it's visible in the filtered view).
        let mut s = state()
            .with_models(&["free-a".to_string(), "paid-b".to_string()])
            .with_free_models(&["free-a".to_string()]);
        s.model_free_only = true;
        s.rows[s.cursor].spec.model = Some("free-a".to_string());
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // filtered = [0="(default)", 1="free-a"]; "free-a" is at filtered idx 1.
        assert_eq!(s.filtered_model_indices(), vec![0, 1]);
        assert_eq!(s.model_overlay.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn model_overlay_no_matches_still_commits_default() {
        let mut s = state().with_models(&["alpha".to_string()]);
        while s.focused_column() != Column::Model {
            s.col_right();
        }
        s.toggle_edit();
        // Type a query that matches nothing (except "(default)" which is always
        // kept). Selection stays at 0 -> "(default)" -> clears the override.
        s.model_overlay_type('z');
        assert_eq!(s.filtered_model_indices(), vec![0]);
        s.model_overlay_commit();
        assert!(s.rows[s.cursor].spec.model.is_none(), "default clears override");
    }

    // ---------------- inline list editor ----------------

    #[test]
    fn list_editor_add_edit_remove_commit() {
        let mut s = state();
        while s.focused_column() != Column::Writable {
            s.col_right();
        }
        let before = s.rows[s.cursor].spec.writable.clone();
        s.toggle_edit(); // opens list editor with existing writable globs
        // Add a new item.
        s.list_editor_add();
        if let Some(le) = s.list_editor.as_mut() {
            *le.text_edit.as_mut().unwrap() =
                crate::text_input::TextInput::with_text("newdir/**");
        }
        s.list_editor_commit_item();
        // Commit the whole list back.
        s.list_editor_commit();
        assert!(s.list_editor.is_none());
        assert_eq!(
            s.rows[s.cursor].spec.writable.len(),
            before.len() + 1,
            "added one writable glob"
        );
        assert!(s.rows[s.cursor].spec.writable.contains(&"newdir/**".to_string()));
    }

    #[test]
    fn list_editor_remove_item() {
        let mut s = state();
        while s.focused_column() != Column::Writable {
            s.col_right();
        }
        s.toggle_edit();
        let n = s.list_editor.as_ref().unwrap().items.len();
        if n > 0 {
            s.list_editor_remove();
            assert_eq!(s.list_editor.as_ref().unwrap().items.len(), n - 1);
        }
    }

    #[test]
    fn list_editor_edits_deps_from_domain_column() {
        let mut s = state();
        while s.focused_column() != Column::Deps {
            s.col_right();
        }
        s.toggle_edit();
        assert!(s.list_editor.is_some());
        assert_eq!(s.list_editor.as_ref().unwrap().col, Column::Deps);
    }

    #[test]
    fn fit_pads_and_truncates() {
        assert_eq!(fit("ab", 5), "ab   ");
        assert_eq!(fit("abcde", 5), "abcde");
        assert_eq!(fit("abcdef", 5), "abcd…");
        assert_eq!(fit("", 3), "   ");
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

    // ------------------------------------------------------ view/pane tests

    #[test]
    fn defaults_to_domain_view_and_summary_pane() {
        let s = state();
        assert_eq!(s.view, BiplaneView::Domains);
        assert_eq!(s.project_pane, ProjectPane::Summary);
    }

    #[test]
    fn toggle_view_flips_and_resets_scroll() {
        let mut s = state();
        s.view = BiplaneView::Project;
        s.project_scroll = 5;
        s.toggle_view();
        assert_eq!(s.view, BiplaneView::Domains);
        assert_eq!(s.project_scroll, 0, "scroll resets on view switch");
        s.toggle_view();
        assert_eq!(s.view, BiplaneView::Project);
    }

    #[test]
    fn project_pane_toggle_is_inert_outside_project_view() {
        let mut s = state();
        assert_eq!(s.view, BiplaneView::Domains);
        s.toggle_project_pane();
        assert_eq!(
            s.project_pane,
            ProjectPane::Summary,
            "V does nothing in Domains view"
        );
    }

    #[test]
    fn project_pane_toggles_inside_project_view() {
        let mut s = state();
        s.toggle_view(); // -> Project
        s.toggle_project_pane();
        assert_eq!(s.project_pane, ProjectPane::ReportJson);
        s.toggle_project_pane();
        assert_eq!(s.project_pane, ProjectPane::Summary);
    }

    #[test]
    fn project_scroll_only_moves_in_project_view_and_saturates() {
        let mut s = state();
        // In Domains view, scroll is inert.
        s.project_scroll_down();
        assert_eq!(s.project_scroll, 0);
        // In Project view it moves, and never underflows past 0.
        s.toggle_view();
        s.project_scroll_up();
        assert_eq!(s.project_scroll, 0, "saturates at top");
        s.project_scroll_down();
        s.project_scroll_down();
        assert_eq!(s.project_scroll, 2);
        s.project_scroll_up();
        assert_eq!(s.project_scroll, 1);
    }

    #[test]
    fn with_report_json_attaches_content() {
        let s = state().with_report_json("{\"k\":1}");
        assert_eq!(s.report_json.as_deref(), Some("{\"k\":1}"));
    }

    #[test]
    fn report_json_absent_by_default() {
        assert!(state().report_json.is_none());
    }

    // ---------------- add / remove row ----------------

    #[test]
    fn add_row_inserts_after_cursor_and_dirties() {
        let mut s = state(); // 3 rows: engine, ui, api
        s.cursor = 1; // ui
        s.add_row();
        // New row is at index 2 (after the cursor), cursor moved to it.
        assert_eq!(s.rows.len(), 4);
        assert_eq!(s.cursor, 2);
        assert!(s.dirty);
        // The new row is included by default and has a scaffolded name.
        assert!(s.rows[2].include);
        assert_eq!(s.rows[2].spec.name, "new-domain");
        assert_eq!(s.rows[2].spec.agents, 1);
        assert!(!s.rows[2].spec.writable.is_empty(), "scaffolded with a default glob");
    }

    #[test]
    fn add_row_generates_unique_name() {
        let mut s = state(); // cursor=0, rows: [engine, ui, api]
        s.add_row(); // inserts at 1, cursor -> 1
        assert_eq!(s.rows[1].spec.name, "new-domain");
        s.cursor = 3; // now on "api" (pushed to index 3)
        s.add_row(); // inserts at 4, cursor -> 4
        assert_eq!(s.rows[4].spec.name, "new-domain-2");
        // Adding again from a different position still avoids collision.
        s.cursor = 0;
        s.add_row(); // inserts at 1, cursor -> 1
        assert_eq!(s.rows[1].spec.name, "new-domain-3");
    }

    #[test]
    fn add_row_to_empty_list_works() {
        let mut s = state();
        s.rows.clear();
        s.cursor = 0;
        s.add_row();
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.cursor, 0);
        assert!(s.dirty);
    }

    #[test]
    fn add_row_resets_column_cursor() {
        let mut s = state();
        // Move column cursor to the rightmost column.
        for _ in 0..10 {
            s.col_right();
        }
        assert_ne!(s.col_cursor, 0);
        s.add_row();
        assert_eq!(s.col_cursor, 0, "new row starts at the Include column");
    }

    #[test]
    fn delete_confirm_arms_and_is_cancelled_by_non_y() {
        let mut s = state(); // 3 rows
        s.cursor = 0; // engine
        s.request_delete_row();
        assert!(s.delete_confirm_pending);
        // Simulate any non-y key: cancel.
        s.cancel_delete_row();
        assert!(!s.delete_confirm_pending);
        // Row was NOT removed.
        assert_eq!(s.rows.len(), 3);
        assert!(!s.dirty, "cancel doesn't mutate, so dirty stays false");
    }

    #[test]
    fn delete_confirm_removes_row_on_y_and_clamps_cursor() {
        let mut s = state(); // 3 rows: engine, ui, api
        s.cursor = 2; // api (last)
        s.request_delete_row();
        assert!(s.delete_confirm_pending);
        s.confirm_delete_row();
        assert!(!s.delete_confirm_pending);
        assert_eq!(s.rows.len(), 2, "api was removed");
        // Cursor clamped to the new last row.
        assert_eq!(s.cursor, 1);
        assert!(s.dirty);
        assert_eq!(s.rows[0].spec.name, "engine");
        assert_eq!(s.rows[1].spec.name, "ui");
    }

    #[test]
    fn delete_confirm_deleting_only_row_leaves_empty_list() {
        let mut s = state();
        s.rows.clear();
        s.add_row();
        assert_eq!(s.rows.len(), 1);
        s.request_delete_row();
        s.confirm_delete_row();
        assert!(s.rows.is_empty());
        assert_eq!(s.cursor, 0);
        assert!(s.dirty);
    }

    #[test]
    fn delete_confirm_status_names_the_removed_domain() {
        let mut s = state();
        s.cursor = 0; // engine
        s.request_delete_row();
        s.confirm_delete_row();
        assert!(s.status.as_deref().unwrap().contains("engine"));
    }

    #[test]
    fn add_then_delete_leaves_dirty_and_no_disk_write() {
        // The architecture gates ALL writes behind save_description (called
        // only on 's'). Adding then deleting a row leaves dirty=true (the
        // delete is itself a mutation), but no file was ever written. This
        // test verifies the state transitions; the actual file-write gate
        // is exercised by the save path's existing tests.
        let mut s = state();
        let original_len = s.rows.len();
        s.add_row();
        assert!(s.dirty);
        // Move to the new row and delete it.
        s.request_delete_row();
        s.confirm_delete_row();
        assert!(s.dirty, "dirty stays true after add+delete");
        assert_eq!(s.rows.len(), original_len, "net row count unchanged");
    }

    #[test]
    fn delete_confirm_does_not_fire_on_empty_list() {
        let mut s = state();
        s.rows.clear();
        s.cursor = 0;
        s.request_delete_row();
        // No row to delete -> confirm should NOT arm (no-op).
        assert!(!s.delete_confirm_pending);
    }
}
