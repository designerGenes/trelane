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
/// (TUI-001) This is a complete ANSI/VT state machine, not the partial
/// ad-hoc parser that shipped before. It removes, per the remediation plan's
/// boundary invariant:
///   * CSI sequences (`ESC [ <params/intermediates> <final byte in
///     0x40..=0x7E>`), e.g. `ESC [ 2 ~`, `ESC [ 1 @`, `ESC [ 32 m`.
///   * OSC sequences (`ESC ] ...`), terminated by either BEL (`0x07`) or ST
///     (`ESC \`) -- both terminators are fully consumed, not left as
///     visible debris.
///   * DCS / SOS / PM / APC string sequences (`ESC P`, `ESC X`, `ESC ^`,
///     `ESC _`), each terminated by ST (`ESC \`).
///   * Two-byte ESC sequences (`ESC <any other byte>`) -- cursor saves,
///     index/next-line, charset shifts, etc. Both bytes are dropped.
///   * C0 controls (`U+0000..U+001F`), DEL (`U+007F`), and C1 controls
///     (`U+0080..U+009F`) -- every remaining `char::is_control()` byte --
///     are replaced with a single space so spacing is preserved without
///     ever emitting a byte that can move a cursor.
///
/// The output is guaranteed to contain only printable Unicode plus ordinary
/// spaces: `sanitize(s).chars().all(|c| !c.is_control() && c != '\u{1b}')`.
fn sanitize(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\u{1b}' {
            i = consume_escape(&chars, i, &mut out);
            continue;
        }
        if c.is_control() {
            // C0 (incl. CR, LF, BEL), DEL, and C1 (U+0080..U+009F) all become
            // a single space. char::is_control() covers all three ranges.
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Advance past one escape sequence starting at `chars[start]` (which is
/// `ESC`). Returns the index of the next character to process. Any
/// recognized sequence appends nothing to `out`; unrecognized forms drop
/// only the ESC byte so real content after it is not silently swallowed.
fn consume_escape(chars: &[char], start: usize, out: &mut String) -> usize {
    let mut i = start;
    // ESC alone at end of input: drop it.
    if i + 1 >= chars.len() {
        return i + 1;
    }
    let next = chars[i + 1];
    i += 2; // past ESC and the introducer
    match next {
        // CSI: ESC [ <parameter/intermediate bytes> <final byte 0x40..0x7E>.
        '[' => {
            while i < chars.len() {
                let b = chars[i];
                i += 1;
                let code = b as u32;
                if (0x40..=0x7E).contains(&code) {
                    break; // final byte consumed
                }
                if b == '\u{1b}' {
                    // Embedded ESC: a new sequence is starting before the
                    // CSI got its final byte. Back up so the outer loop
                    // handles the ESC.
                    i -= 1;
                    break;
                }
                // Parameter (0x30..0x3F) and intermediate (0x20..0x2F)
                // bytes are consumed silently. Anything else (e.g. an
                // unexpected control) is consumed and the loop continues
                // until a final byte or end of input.
            }
        }
        // OSC: ESC ] ... terminated by BEL (0x07) or ST (ESC \).
        ']' => {
            while i < chars.len() {
                let b = chars[i];
                if b == '\u{7}' {
                    i += 1; // consume BEL terminator
                    break;
                }
                if b == '\u{1b}' {
                    // Either ST (ESC \) or an embedded ESC starting a new
                    // sequence.
                    if i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 2; // consume ST
                        break;
                    }
                    // ESC alone: leave it for the outer loop.
                    break;
                }
                i += 1;
            }
        }
        // DCS / SOS / PM / APC: terminated ONLY by ST (ESC \). Unlike OSC,
        // BEL does NOT terminate these.
        'P' | 'X' | '^' | '_' => {
            while i < chars.len() {
                let b = chars[i];
                if b == '\u{1b}' {
                    if i + 1 < chars.len() && chars[i + 1] == '\\' {
                        i += 2; // consume ST
                        break;
                    }
                    // ESC alone: leave it for the outer loop.
                    break;
                }
                i += 1;
            }
        }
        // Any other byte after ESC is a two-byte escape sequence
        // (ESC =, ESC >, ESC D, ESC M, ESC E, ESC 7, ESC 8, ESC c, ...).
        // Both bytes are already consumed by the initial `i += 2` above;
        // nothing is appended.
        _ => {}
    }
    // `out` is unchanged -- escape sequences produce no visible output.
    let _ = out;
    i
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

/// Clip a string to at most `width` TERMINAL COLUMNS, appending an ellipsis
/// when it was cut. Truncation is by extended grapheme cluster and measures
/// display width via `unicode-width`/`unicode-segmentation`, so:
///
/// - CJK and full-width glyphs count as 2 columns (not 1 char) -- the old
///   `chars().count()` code under-counted these and let a row of Japanese
///   text overflow the pane.
/// - Combining-mark clusters (e + ́ ) are kept together (the combining mark
///   has width 0 but belongs to the previous cluster).
/// - ZWJ emoji sequences (family, flag) are kept together (width 2).
/// - The result's `UnicodeWidthStr::width` is guaranteed `<= max_width`.
///
/// This is what keeps the agent feed to exactly ONE terminal row per event:
/// a body longer than the pane can't wrap onto extra rows, which is the root
/// cause of the leftover-fragment artifacts (a long line wraps to several
/// rows, then when the feed scrolls, the vacated wrap-rows aren't reliably
/// cleared). One row per event makes rendered-rows == event-count, so the
/// height-based windowing below is exact.
///
/// (TUI-004: this replaced the old `chars().count()` version, which measured
/// Unicode scalar values rather than terminal columns.)
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    if max_width == 0 {
        return String::new();
    }
    // Fast path: the whole string already fits.
    if UnicodeWidthStr::width(s) <= max_width {
        return s.to_string();
    }
    // The ellipsis (…) is a single grapheme with display width 1 (it's
    // U+2026 in NFC). Reserve its width when we're going to clip.
    let ellipsis_width = 1usize;
    if max_width <= ellipsis_width {
        // Caller asked for a width so small we can't fit content + ellipsis.
        // The old code returned just "…" for width==1; preserve that shape.
        return "…".to_string();
    }
    let content_budget = max_width - ellipsis_width;

    // Walk extended grapheme clusters, accumulating until the next cluster
    // would exceed the content budget. The width of each cluster is the
    // width of its rendered form -- combining marks contribute 0, ZWJ
    // sequences contribute the rendered glyph's width (usually 2 for emoji,
    // occasionally 1 or 3 for some).
    let mut out: String = String::with_capacity(s.len());
    let mut used: usize = 0;
    for cluster in s.graphemes(true) {
        // A cluster's display width is the sum of its chars' widths.
        // unicode-width's UnicodeWidthStr::width(cluster) does exactly this.
        let cluster_width = UnicodeWidthStr::width(cluster);
        // Width-0 clusters (a lone combining mark, a ZWJ) never cause us to
        // stop -- but they can still be appended if we have budget left.
        if cluster_width == 0 {
            if used <= content_budget {
                out.push_str(cluster);
            }
            // else: dropping a trailing zero-width cluster after we've
            // already filled the budget is the right thing; appending it
            // would leave a dangling combiner.
            continue;
        }
        if used + cluster_width > content_budget {
            // This cluster doesn't fit; stop and append the ellipsis.
            break;
        }
        out.push_str(cluster);
        used += cluster_width;
    }
    out.push('…');

    // Guarantee: the result's display width is <= max_width. The content
    // is <= content_budget, and the ellipsis adds 1, so total <= max_width.
    // This holds even when the last cluster is a zero-width trailing
    // combiner (we never append beyond content_budget).
    debug_assert!(
        UnicodeWidthStr::width(out.as_str()) <= max_width,
        "truncate_to_width overflow: result width {} > max_width {} for {:?}",
        UnicodeWidthStr::width(out.as_str()),
        max_width,
        out
    );
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
    /// Bytes read from the log tail that didn't end in a complete LF record
    /// yet. Carried across polls so a multibyte UTF-8 codepoint or a
    /// partially-written JSON line that straddles two polls is reconstructed
    /// instead of dropped or decoded as invalid. (TUI-007.)
    pub pending: Vec<u8>,
    /// The most recent poll error (file vanished, permission denied, etc.),
    /// surfaced as a one-line sanitized state line so a transient problem is
    /// visible instead of silently swallowed. Cleared on the next successful
    /// poll. (TUI-007.)
    pub last_poll_error: Option<String>,
}

impl AgentFeed {
    /// Register that a (possibly new) run log was selected. A changed name
    /// resets the cursor AND the pending byte buffer -- a fresh wake means a
    /// fresh file, so any half-record from the previous file must not bleed
    /// into the new one. Events are kept: the feed spans wakes, which is
    /// exactly what "why did it sleep and what happened when it woke" needs.
    pub fn select_log(&mut self, name: Option<String>) {
        if name != self.log_name {
            self.log_name = name;
            self.pos = 0;
            self.pending.clear();
            self.last_poll_error = None;
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
///
/// (TUI-007.) This is byte-oriented, not string-oriented:
/// - The appended tail is read as bytes into a per-feed `pending` buffer that
///   survives across polls. A multibyte UTF-8 codepoint split across two
///   reads is reconstructed instead of failing the whole poll.
/// - Complete records are split on BYTE LF (`\n`) so a `\r` inside a line
///   never breaks the boundary. Each complete record is decoded with
///   `String::from_utf8_lossy` so invalid bytes become `U+FFFD` replacement
///   glyphs instead of stopping future feed updates.
/// - If the file length becomes smaller than `feed.pos`, the file was
///   truncated or rotated; the cursor and pending buffer are reset so we
///   re-tail from the new start.
/// - Any poll error (file vanished, permission denied) is stored in
///   `feed.last_poll_error` as a one-line sanitized state instead of being
///   silently swallowed. The next successful poll clears it.
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

    let bytes_read = match read_log_tail(&path, feed) {
        Ok(n) => n,
        Err(e) => {
            // Surface the error as a sanitized one-liner so the user can see
            // the feed stopped, then return Ok so the monitor keeps running.
            // `e` is sanitized before storage because poll errors can carry
            // paths with arbitrary characters (no control bytes from real
            // io::Error, but the invariant holds regardless).
            feed.last_poll_error = Some(sanitize(&format!("feed read error: {e}")));
            return Ok(());
        }
    };
    if bytes_read == 0 {
        return Ok(());
    }

    // Split the pending buffer on byte LF. Every complete record (including
    // the trailing LF) is decoded with from_utf8_lossy and parsed; the final
    // incomplete record (no LF yet) stays in `pending` for the next poll.
    let mut events = Vec::new();
    let mut start = 0usize;
    let pending = std::mem::take(&mut feed.pending);
    // We need to scan the full pending buffer (old + new bytes), so put it
    // back and work on a local.
    let mut buf = pending;
    while start < buf.len() {
        // Find the next byte LF from `start`.
        match buf[start..].iter().position(|&b| b == b'\n') {
            Some(rel) => {
                let end = start + rel; // exclusive: the LF is at `end`
                let record = &buf[start..end];
                // Decode with lossy: invalid bytes become U+FFFD, never fail.
                let line = String::from_utf8_lossy(record).into_owned();
                // parse_line already sanitizes control bytes; from_utf8_lossy
                // already replaced any invalid UTF-8 sequences with U+FFFD,
                // which is a printable replacement glyph.
                events.extend(parse_line(&line));
                start = end + 1; // skip past the LF
            }
            None => {
                // No more complete records; the rest is the new pending tail.
                break;
            }
        }
    }
    // Keep the unconsumed tail. `start` is the byte offset of the first
    // incomplete record; everything from there forward waits for the next
    // poll's appended bytes.
    feed.pending = buf.split_off(start);
    feed.push_events(events);
    // Successful poll: clear any previous error.
    if feed.last_poll_error.is_some() {
        feed.last_poll_error = None;
    }
    Ok(())
}

/// Read the appended tail of `path` since `feed.pos` into `feed.pending`.
/// Returns the number of bytes read this poll (0 if nothing new). Handles
/// truncation/rotation: if the file length is smaller than `feed.pos`,
/// reset both `pos` and `pending` before reading from the start.
///
/// Separated from `poll_agent_feed` so the I/O shape is testable on its own
/// (the unit tests below exercise truncation recovery and partial-record
/// reconstruction without standing up a full Context).
fn read_log_tail(path: &std::path::Path, feed: &mut AgentFeed) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < feed.pos {
        // Truncation or rotation: the file shrank below our cursor. Reset
        // the cursor and the pending buffer so we re-tail from the new
        // start; the alternative (seeking to a now-invalid offset) would
        // either error or read garbage.
        feed.pos = 0;
        feed.pending.clear();
    }
    if len <= feed.pos {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(feed.pos))?;
    let mut chunk = Vec::new();
    let n = file.read_to_end(&mut chunk)?;
    feed.pos += n as u64;
    feed.pending.extend_from_slice(&chunk);
    Ok(n)
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
    use crate::tui_session::TuiSession;

    let agents = store::list_agents(&ctx.conn)?;
    let mut state = MonitorState::new(&agents);
    poll_statuses(ctx, &mut state);

    // Draw to /dev/tty directly, not std::io::stdout(). When the monitor
    // co-runs with a background squire tick-loop (the default `trelane`
    // session), that loop prints progress to stdout; drawing to /dev/tty keeps
    // the TUI immune to it (stdout is captured to a session log by the caller).
    // Falls back to stdout when /dev/tty is unavailable (not a real terminal).
    let tty: Box<dyn std::io::Write + Send> =
        match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stdout()),
        };
    // TUI-006: the TuiSession guard owns the raw-mode/alternate-screen
    // ladder and restores every completed stage in reverse order on Drop,
    // so a panic or an error anywhere in the loop below can't strand the
    // user's terminal in raw mode with the cursor hidden.
    let mut session = TuiSession::enter()?;
    session.enter_alternate_screen(tty)?;
    // Force a full clear before the first draw. Some terminals/multiplexers
    // don't guarantee a blank alternate screen on entry, and ratatui only
    // diffs against its OWN prior frame -- so without this, a cell that no
    // widget happens to touch this frame (a quiet gap in the layout) can go
    // on showing whatever was there before this session started.
    session.clear()?;

    let outcome = (|| -> Result<()> {
        let terminal = session.terminal().unwrap();
        let mut last_poll = std::time::Instant::now() - std::time::Duration::from_secs(10);
        // TUI-005: a full terminal.clear() is requested on Resize (the
        // backend and both ratatui buffers must be invalidated together --
        // old-geometry cells otherwise linger) and on Ctrl-L (manual
        // recovery after out-of-band terminal corruption, e.g. a stray
        // background write or a pager glitch). It is NOT done per-frame or
        // per-tab-switch; the per-frame Clear widget that used to live in
        // render() was a misleading mitigation that can't repair backend/
        // terminal desync after an out-of-band write. See the plan's
        // TUI-005 "do_not" list: no clearing every frame, no sleeps, and
        // input draining alone is not the guarantee.
        let mut needs_clear = false;
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
            loop {
                if !event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
                // TUI-005: Resize is not a Key event -- it must be handled
                // here in the drain loop, and it invalidates both ratatui
                // buffers (the backend's previous-frame diff state) via a
                // full clear before the next draw. Without this, cells from
                // the old geometry can survive into the new frame. Ctrl-L
                // is the manual-recovery binding for out-of-band corruption.
                // Both classifications live in the pure `requests_full_clear`
                // / `is_ctrl_l` helpers above so they're unit-testable.
                let ev = event::read()?;
                if requests_full_clear(&ev) {
                    needs_clear = true;
                    continue; // redraw with clear happens below this iteration
                }
                let Event::Key(key) = ev else {
                    continue; // mouse/focus/paste: ignored
                };
                {
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
                } // end Event::Key handling block
                if state.should_quit {
                    return Ok(());
                }
            }

            // TUI-005: honor a full-clear request (Resize or Ctrl-L) BEFORE
            // the draw, so the backend's previous-buffer state and the
            // physical terminal are invalidated together. A tab switch
            // deliberately does NOT request a clear: the frame's own content
            // covers the whole pane, and clearing on every switch would be
            // exactly the per-frame clearing the plan forbids.
            if needs_clear {
                terminal.clear()?;
                needs_clear = false;
            }
            terminal.draw(|f| render(f, &state))?;

            // Block briefly for the next event so the loop doesn't busy-spin
            // when idle. Deliberately not consumed here: if an event arrives
            // during this wait, it stays queued and the drain loop above
            // picks it up on the next iteration.
            event::poll(std::time::Duration::from_millis(120))?;
        }
    })();

    // TUI-006: close() restores cursor, leaves the alternate screen, and
    // disables raw mode in reverse order, attempting every stage even if one
    // fails (the plan's TUI-006 invariant: a failure in disable_raw_mode
    // must not short-circuit LeaveAlternateScreen or show_cursor). The
    // loop's own outcome takes precedence over a cleanup error; on a clean
    // loop exit, a cleanup error is surfaced instead of swallowed.
    let close_result = session.close();
    outcome?;
    close_result
}

/// Redirect process stdout (fd 1) AND stderr (fd 2) to a file for this
/// guard's lifetime, restoring both on drop. Used so a background squire
/// tick-loop's output lands in a session log instead of corrupting the
/// monitor's screen (the monitor draws to /dev/tty, so it's unaffected by
/// this redirect). See `tui_session::StdCapture` for the full rationale
/// (TUI-003: exclusive terminal ownership).
///
/// This re-export exists so the call site stays readable; the actual
/// implementation is shared with `bench_ui` so both entry points get the
/// same flush-both, restore-both, fail-before-alt-screen semantics.
pub(crate) use crate::tui_session::StdCapture;

// ---------------------------------------------------- TUI-005 event helpers
//
// The full-clear decision is factored out of run_monitor's event loop into
// these pure functions so it's unit-testable without a PTY: the loop calls
// `requests_full_clear(event)` and, when true, calls `terminal.clear()`
// before the next draw. See TUI-005 in the remediation plan.

/// True when this key event is Ctrl-L. Most terminals deliver it as
/// `Char('l')` with the CONTROL modifier; a few deliver the raw form-feed
/// byte (0x0C), which crossterm reports as `Char('\x0c')` with no modifier.
/// Handle both so the binding works across kitty/iTerm/Terminal.app/
/// alacritty equally.
pub fn is_ctrl_l(key: &crossterm::event::KeyEvent) -> bool {
    (key.code == crossterm::event::KeyCode::Char('l')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL))
        || key.code == crossterm::event::KeyCode::Char('\u{c}')
}

/// True when this event requires a full `terminal.clear()` before the next
/// draw, per TUI-005:
/// - `Event::Resize`: the backend's previous-buffer state refers to the old
///   geometry, so it and both ratatui buffers must be invalidated together
///   or old-geometry cells linger.
/// - Ctrl-L: manual recovery after out-of-band terminal corruption (a stray
///   background write, a pager glitch). Draws immediately after the clear.
///
/// Deliberately NOT requested on tab switches: the in-frame Clear widget in
/// render() makes every tab's frame specify every cell of the content area,
/// so a switch is exact in one frame without a full-terminal clear (which
/// would be exactly the per-frame clearing the plan forbids).
pub fn requests_full_clear(ev: &crossterm::event::Event) -> bool {
    match ev {
        crossterm::event::Event::Resize(_, _) => true,
        crossterm::event::Event::Key(key) => is_ctrl_l(key),
        _ => false,
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

    // Capture the squire thread's stdout AND stderr to a session log for
    // the UI's lifetime. The monitor is on /dev/tty, so this can't blank
    // its screen -- and capturing stderr too stops the squire's eprintln!
    // wake/skip lines from bleeding onto the TUI.
    //
    // TUI-003: capture setup must succeed BEFORE run_monitor enters the
    // alternate screen. If the log can't be opened or either fd can't be
    // dup'd we return the error here rather than continuing with a
    // half-redirected terminal -- a partially-captured terminal leaves
    // the background squire thread's writes able to corrupt the screen,
    // which is exactly the bug this guard exists to prevent.
    let log_path = ctx.trelane_dir().join("session.log");
    let capture = StdCapture::to_file(&log_path)?;

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

    // Clear the content area inside the frame. Purpose and limits, precisely:
    // ratatui widgets only write the cells they own -- a Paragraph with N
    // lines leaves rows N..H untouched in the buffer -- so switching from a
    // dense agent tab to a sparse Trelane tab without this would leave the
    // old tab's rows lingering (a frame-geometry bug, visible as stale
    // rows). The in-frame Clear makes THIS frame specify every cell of the
    // area, so the diff is exact and tab switches are correct in one frame
    // with no timing delay (acceptance test 3 of TUI-005).
    //
    // What this does NOT do (and never did): repair the physical terminal
    // after an OUT-OF-BAND write. A logical Clear participates in the frame
    // diff, so if another writer (a background thread's println, or an
    // embedded escape sequence in event text) changed the physical terminal
    // behind ratatui's buffer model, diff draws are not guaranteed to repair
    // cells ratatui believes are already correct. Those cases are handled
    // elsewhere: TUI-001 (sanitize all log-derived text), TUI-002
    // (structured launcher output), TUI-003 (exclusive terminal ownership
    // via stdout+stderr capture), and the deterministic full terminal.clear()
    // calls in run_monitor (initial entry, Resize, Ctrl-L).
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
                // TUI-001 boundary assertion: the body of every rendered
                // event must be free of ESC and other control bytes. This
                // catches a regression where a new AgentEvent variant or a
                // new parser path forgets to route through `sanitize` at
                // ingestion time. Cheap (the slice is already truncated)
                // and never trips in practice -- when it does, it means a
                // bug, not a runtime condition, so `debug_assert!` is the
                // right level.
                debug_assert!(
                    !ev.body().contains('\u{1b}')
                        && ev.body().chars().all(|c| !c.is_control()),
                    "rendered event body still contains control bytes: {:?}",
                    ev.body()
                );
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
            // TUI-007: surface the most recent poll error (file vanished,
            // permission denied, etc.) as a single sanitized line so the
            // user can see the feed stopped, instead of the monitor looking
            // frozen with no explanation. The error text was already
            // sanitized at storage time, but truncate_to_width is the final
            // render-boundary check that bounds it to one row.
            if let Some(err) = feed.and_then(|fd| fd.last_poll_error.as_ref()) {
                lines.push(Line::from(Span::styled(
                    truncate_to_width(&format!("[feed] {}", err), inner_width),
                    Style::default().fg(theme_color(THEME_WARN)),
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
            "Tab switch  d feed  ↑↓ row  ←→ adjust  space toggle  s save  p/r pause/resume  k kill  ^L redraw  q quit"
        }
        (ViewMode::Diagnostic, false) => "Tab switch  d back to feed  ^L redraw  q quit",
        (ViewMode::Normal, _) => {
            "Tab/←→ switch  0-9 jump  d diagnostic  ↑↓ scroll  End/f follow  ^L redraw  q quit"
        }
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

    // -------- TUI-001: extended coverage from the remediation plan --------

    #[test]
    fn sanitize_strips_csi_with_tilde_final() {
        // ESC [ 2 ~ is the Insert key; the plan explicitly names it as a case
        // the old parser mishandled (the `~` was left as visible debris
        // because the loop only stopped on `is_ascii_alphabetic()`).
        assert_eq!(sanitize("\x1b[2~"), "");
        assert_eq!(sanitize("a\x1b[2~b"), "ab");
    }

    #[test]
    fn sanitize_strips_csi_with_at_sign_final() {
        // ESC [ 1 @ is the "insert character" CSI; `@` is 0x40, the low end
        // of the final-byte range 0x40..=0x7E, which the old `is_ascii_alphabetic`
        // test missed.
        assert_eq!(sanitize("\x1b[1@"), "");
        assert_eq!(sanitize("a\x1b[1@b"), "ab");
    }

    #[test]
    fn sanitize_strips_osc_terminated_by_st() {
        // ESC ] ... ESC \  -- the String Terminator variant. The plan
        // explicitly requires both the ESC and the backslash to be consumed.
        assert_eq!(sanitize("\x1b]0;title\x1b\\visible"), "visible");
        // No leftover backslash anywhere in the output.
        let out = sanitize("\x1b]11;rgb:00/00/00\x1b\\text");
        assert!(!out.contains('\\'), "backslash survived: {out:?}");
        assert_eq!(out, "text");
    }

    #[test]
    fn sanitize_strips_dcs_string() {
        // DCS (ESC P) is terminated only by ST (ESC \), never by BEL.
        assert_eq!(sanitize("\x1bP1$qtbel-stays-as-data\x07\x1b\\after"), "after");
    }

    #[test]
    fn sanitize_strips_apc_string() {
        // APC (ESC _) is used by tmux for pass-through sequences.
        assert_eq!(sanitize("\x1b_tmux-passthrough\x1b\\after"), "after");
    }

    #[test]
    fn sanitize_strips_sos_and_pm_strings() {
        // SOS (ESC X) and PM (ESC ^) share the DCS/APC termination rule.
        assert_eq!(sanitize("\x1bXfoo\x1b\\after"), "after");
        assert_eq!(sanitize("\x1b^bar\x1b\\after"), "after");
    }

    #[test]
    fn sanitize_strips_two_byte_esc_sequences() {
        // ESC 7 / ESC 8 (save/restore cursor), ESC M (reverse index), ESC c
        // (full reset), ESC = / ESC > (keypad mode). All two-byte ESC <x>
        // forms must drop both bytes.
        assert_eq!(sanitize("\x1b7AB"), "AB");
        assert_eq!(sanitize("A\x1b8B"), "AB");
        assert_eq!(sanitize("A\x1bMB"), "AB");
        assert_eq!(sanitize("\x1bc"), "");
        assert_eq!(sanitize("\x1b=x"), "x");
        assert_eq!(sanitize("\x1b>x"), "x");
    }

    #[test]
    fn sanitize_replaces_c1_controls_with_space() {
        // C1 controls (U+0080..U+009F) are invisible in many editors but
        // some terminals still honor them as control bytes. The plan
        // requires they be replaced with a space, not dropped.
        let poisoned = "a\u{0085}b\u{0099}c\u{0084}d";
        let out = sanitize(poisoned);
        assert_eq!(out, "a b c d");
    }

    #[test]
    fn sanitize_preserves_emoji_and_zwj_sequences() {
        // Emoji and ZWJ sequences are not control characters -- they must
        // survive intact (related to TUI-004's grapheme handling).
        let s = "wave 👋 and family 👨‍👩‍👧 done";
        assert_eq!(sanitize(s), s);
    }

    #[test]
    fn sanitize_json_text_field_with_escaped_controls_is_safe() {
        // The plan's acceptance test: a JSON text field containing escaped
        // U+001B and U+000D is safe AFTER parse_line. parse_line decodes
        // JSON unescaping (so `"\u001b"` becomes a real ESC byte) and then
        // sanitizes; the resulting Text event body must have no ESC and no
        // bare CR. The opencode stream-json "text" event carries the
        // string under /part/text -- see part_text.
        let line = r#"{"type":"text","part":{"type":"text","text":"hi\u001b[31mred\rthere"}}"#;
        let evs = parse_line(line);
        assert_eq!(evs.len(), 1);
        let body = evs[0].body();
        assert!(!body.contains('\u{1b}'), "ESC survived: {body:?}");
        assert!(!body.contains('\r'), "bare CR survived: {body:?}");
        assert!(body.contains("red"));
        assert!(body.contains("there"));
    }

    #[test]
    fn sanitize_fuzzed_output_has_no_control_bytes_or_esc() {
        // The plan's invariant: for any input, sanitized output contains
        // no ESC and no control characters. Run a quick deterministic fuzz
        // over mixed-byte junk.
        let cases = [
            "\x1b[1;2;3mtext\x1b[0m\x1b]2;title\x07after",
            "a\x1bb\x1bc\x1bd",
            "\x1bPp\x1b\\\x1b]q\x07r",
            "\r\r\r\x1b[H\x1b[2J\r",
            "\u{0000}\u{0001}\u{007f}\u{0080}\u{009f}",
            "mixed\x1b[?25l\x1b[?1006h\x1b[?1049hanimation",
            "\x1b[38;2;255;0;0mred\x1b[39m",
        ];
        for case in cases {
            let out = sanitize(case);
            for c in out.chars() {
                assert!(
                    !c.is_control() && c != '\u{1b}',
                    "control byte {:#x} survived in {case:?} -> {out:?}",
                    c as u32
                );
            }
        }
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
        // TUI-004: the invariant is DISPLAY WIDTH (terminal columns), not
        // char count. ASCII happens to have width==chars, so this stays
        // equivalent for the ASCII case but uses the real measure.
        use unicode_width::UnicodeWidthStr;
        let long = "a".repeat(500);
        for w in [0, 1, 2, 10, 80, 200] {
            let out = truncate_to_width(&long, w);
            assert!(
                UnicodeWidthStr::width(out.as_str()) <= w.max(0),
                "width {w} exceeded by {:?} (display width {})",
                out,
                UnicodeWidthStr::width(out.as_str())
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
        // Cutting a run of multibyte chars must land on a grapheme cluster
        // boundary (not in the middle of a combining mark or a ZWJ sequence)
        // and must NOT exceed the requested terminal-column width.
        let s = "日本語のテキストです"; // 10 CJK glyphs, 3 bytes each, 2 cols each
        let out = truncate_to_width(s, 4);
        // CJK glyphs are width 2, so width=4 fits exactly one glyph + ellipsis.
        // The old code returned 4 chars (4 glyphs = 8 columns) -- a width
        // overflow the screenshots showed as wrap-row artifacts.
        use unicode_width::UnicodeWidthStr;
        assert!(
            UnicodeWidthStr::width(out.as_str()) <= 4,
            "CJK truncation overflowed width 4: {:?} (width {})",
            out,
            UnicodeWidthStr::width(out.as_str())
        );
        assert!(out.ends_with('…'));
    }

    // -------- TUI-004: display-column acceptance tests --------

    #[test]
    fn truncate_cjk_never_exceeds_requested_width() {
        use unicode_width::UnicodeWidthStr;
        // Japanese, CJK, and full-width text: every glyph is 2 columns.
        // A body that fits exactly at width N (even N) stays intact; odd
        // Ns clip to N-1 cols of content + 1 ellipsis.
        let s = "日本語テスト";
        for w in 0..20 {
            let out = truncate_to_width(s, w);
            let actual = UnicodeWidthStr::width(out.as_str());
            assert!(
                actual <= w,
                "width {w}: result {out:?} has display width {actual}"
            );
        }
        // Width 4 = 1 CJK glyph (2 cols) + ellipsis (1 col) = 3 cols total.
        assert_eq!(UnicodeWidthStr::width(truncate_to_width(s, 4).as_str()), 3);
        // Width 5 = 2 CJK glyphs (4 cols) + ellipsis (1 col) = 5 cols.
        assert_eq!(UnicodeWidthStr::width(truncate_to_width(s, 5).as_str()), 5);
    }

    #[test]
    fn truncate_keeps_combining_mark_clusters_together() {
        // "e\u{301}" is one grapheme (é represented as e + combining acute).
        // Truncation must NOT split it -- a trailing combining mark without
        // its base would render as a stray diacritic on the wrong glyph or
        // as a tofu box.
        let s = "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}"; // 5 é clusters
        let out = truncate_to_width(s, 4);
        // Each cluster is 1 column wide, so width 4 = 3 clusters + ellipsis.
        assert!(out.ends_with('…'));
        // The result must not contain a stray combining mark at the start
        // of a cluster (i.e. the truncation point is on a grapheme boundary).
        assert!(
            !out.starts_with('\u{301}'),
            "split a combining mark from its base: {out:?}"
        );
        use unicode_width::UnicodeWidthStr;
        assert!(UnicodeWidthStr::width(out.as_str()) <= 4);
    }

    #[test]
    fn truncate_keeps_zwj_emoji_sequences_together() {
        // The family emoji is a ZWJ sequence: man + ZWJ + woman + ZWJ + girl.
        // It's a single grapheme of width 2. Truncation must NOT split it
        // (a stray ZWJ or a half-family would render as garbage).
        let family = "👨\u{200d}👩\u{200d}👧";
        let s = format!("aa{family}bb"); // 2 ASCII + emoji (width 2) + 2 ASCII = 6 cols
        let out = truncate_to_width(&s, 4);
        use unicode_width::UnicodeWidthStr;
        assert!(
            UnicodeWidthStr::width(out.as_str()) <= 4,
            "ZWJ overflow: {out:?}"
        );
        // The ZWJ sequence is either entirely present or entirely absent;
        // never the leading man alone (which would have width 2 but a
        // dangling ZWJ waiting for the next codepoint).
        assert!(
            !out.contains('\u{200d}') || out.contains(family),
            "split a ZWJ emoji sequence: {out:?}"
        );
    }

    #[test]
    fn truncate_width_zero_and_one_remain_safe() {
        assert_eq!(truncate_to_width("anything", 0), "");
        assert_eq!(truncate_to_width("anything", 1), "…");
        // CJK at width 1: the glyph (width 2) doesn't fit, so just the ellipsis.
        assert_eq!(truncate_to_width("日本", 1), "…");
    }

    #[test]
    fn truncate_preserves_short_strings() {
        // ASCII and CJK short strings that fit are returned verbatim.
        assert_eq!(truncate_to_width("hello", 20), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("日本", 4), "日本");
    }

    #[test]
    fn truncate_long_ascii_uses_ellipsis_and_fits() {
        use unicode_width::UnicodeWidthStr;
        let long = "a".repeat(500);
        for w in [0, 1, 2, 10, 80, 200] {
            let out = truncate_to_width(&long, w);
            let actual = UnicodeWidthStr::width(out.as_str());
            assert!(actual <= w, "width {w}: actual {actual}");
            if w >= 2 {
                assert!(out.ends_with('…'));
            }
        }
    }

    // ---------------- TUI-005: full-clear decision + frame correctness --------
    //
    // These tests cover the plan's TUI-005 acceptance criteria using
    // ratatui's TestBackend instead of a PTY. The full PTY end-to-end
    // scenario (real monitor in a pseudo-terminal, 100 tab switches, two
    // resizes, one Ctrl-L) is the plan's regression_test_plan; the unit
    // tests here lock in the DECISION logic (when do we request a full
    // clear) and the FRAME correctness (the rendered buffer has no stale
    // cells and no control bytes), which is what the PTY scenario asserts
    // at the byte level.

    #[test]
    fn ctrl_l_is_detected_both_modi() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // The common form: Char('l') + CONTROL.
        let k1 = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(is_ctrl_l(&k1), "Char('l') + CONTROL");
        // The raw form-feed byte form (some terminals send 0x0C directly).
        let k2 = KeyEvent::new(KeyCode::Char('\u{c}'), KeyModifiers::NONE);
        assert!(is_ctrl_l(&k2), "raw form-feed Char('\\x0c')");
        // Plain 'l' without CONTROL is NOT a redraw request.
        let k3 = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(!is_ctrl_l(&k3));
        // Control-something-else is NOT.
        let k4 = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(!is_ctrl_l(&k4));
    }

    #[test]
    fn resize_and_ctrl_l_request_full_clear_others_do_not() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        // Resize always requests a clear.
        assert!(requests_full_clear(&Event::Resize(80, 24)));
        // Ctrl-L requests a clear.
        assert!(requests_full_clear(&Event::Key(KeyEvent::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL
        ))));
        assert!(requests_full_clear(&Event::Key(KeyEvent::new(
            KeyCode::Char('\u{c}'),
            KeyModifiers::NONE
        ))));
        // A Tab key (tab switch) does NOT request a clear: the in-frame
        // Clear widget makes tab switches exact without a full-terminal
        // clear. This is the "do not clear every frame / every switch"
        // invariant from the plan.
        assert!(!requests_full_clear(&Event::Key(KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE
        ))));
        // A quit key does NOT.
        assert!(!requests_full_clear(&Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        ))));
        // Mouse/focus events do NOT.
        assert!(!requests_full_clear(&Event::FocusGained));
    }

    #[test]
    fn render_switches_from_dense_agent_tab_to_sparse_trelane_tab_without_stale_cells(
    ) {
        // Acceptance test 3 (normal tab switching correct without a timing
        // delay): render a dense agent tab (many events), then switch to
        // the sparse Trelane tab, and assert EVERY cell in the content
        // rectangle holds the second frame's content -- i.e. no stale rows
        // from the dense tab survive in the buffer. The in-frame Clear
        // widget is what makes this exact; this test would fail without it.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let width: u16 = 60;
        let height: u16 = 20;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        // Build state with one agent whose feed is dense.
        let agents = vec!["alpha".to_string()];
        let mut state = MonitorState::new(&agents);
        let feed = state.feed_mut("alpha");
        feed.status_line = "running -- working".to_string();
        // Fill the feed with more events than the pane can show, so the
        // dense tab truly fills the content area.
        for i in 0..50 {
            feed.push_events(vec![AgentEvent::Text(format!("dense line {i}"))]);
        }
        // Render the dense agent tab (tab 1).
        state.jump_to(1);
        terminal.draw(|f| render(f, &state)).unwrap();

        // Switch to the sparse Trelane tab (tab 0) and render again.
        state.jump_to(0);
        terminal.draw(|f| render(f, &state)).unwrap();

        // Inspect the buffer: no cell anywhere may still contain the dense
        // tab's "dense line N" text. The buffer's cell symbols are joined
        // row by row; a stale dense row would show up as that exact text.
        let buf = terminal.backend().buffer();
        let mut all_text = String::new();
        for row in 0..height {
            for col in 0..width {
                all_text.push_str(buf[(col, row)].symbol());
            }
            all_text.push('\n');
        }
        assert!(
            !all_text.contains("dense line"),
            "stale dense-tab row survived the switch to the sparse Trelane tab:\n{all_text}"
        );
        // And the sparse tab's expected content IS present.
        assert!(
            all_text.contains("Trelane Monitor"),
            "Trelane tab chrome missing after switch:\n{all_text}"
        );
    }

    #[test]
    fn render_poisoned_raw_event_emits_no_control_bytes() {
        // The ratatui_test_backend part of the plan's regression plan:
        // render poisoned Raw events and assert no Cell symbol contains ESC
        // or control characters. parse_line sanitizes at ingestion, so a
        // poisoned line's stored event body is already clean; this test
        // verifies the full pipeline end-to-end at the buffer level.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = vec!["alpha".to_string()];
        let mut state = MonitorState::new(&agents);
        // Feed a poisoned line through the PUBLIC parse path (the one the
        // real feed uses), then push the resulting events.
        let poisoned = "> build \x1b[36m\u{b7}\x1b[0m nvidia/nemotron\rHits\rHull\r\x07done";
        let evs = parse_line(poisoned);
        let feed = state.feed_mut("alpha");
        feed.push_events(evs);
        state.jump_to(1);
        terminal.draw(|f| render(f, &state)).unwrap();

        let buf = terminal.backend().buffer();
        for row in 0..24u16 {
            for col in 0..80u16 {
                let sym = buf[(col, row)].symbol();
                for c in sym.chars() {
                    assert!(
                        !c.is_control() && c != '\u{1b}',
                        "control byte {:#x} in cell ({col},{row}) symbol {sym:?}",
                        c as u32
                    );
                }
            }
        }
    }

    #[test]
    fn render_every_event_occupies_exactly_one_row_at_several_widths() {
        // The plan's "render at several widths and assert no event occupies
        // more than one logical row." Each feed line is one row by
        // construction (truncate_to_width + no .wrap()); this asserts the
        // invariant at the buffer level for a long event at several widths.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        for (w, h) in [(40u16, 15u16), (80, 24), (120, 30)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            let agents = vec!["alpha".to_string()];
            let mut state = MonitorState::new(&agents);
            let feed = state.feed_mut("alpha");
            // One very long event + a few short ones.
            feed.push_events(vec![
                AgentEvent::Text("x".repeat(500)),
                AgentEvent::Text("short".to_string()),
            ]);
            state.jump_to(1);
            terminal.draw(|f| render(f, &state)).unwrap();

            // Count rows in the buffer that contain the long-event body.
            // The long body is truncated to ONE row (with an ellipsis), so
            // the run of 'x' characters appears on exactly one row; a wrap
            // would put 'x' runs on 2+ rows.
            let buf = terminal.backend().buffer();
            let mut rows_with_x_run = 0;
            for row in 0..h {
                let mut line = String::new();
                for col in 0..w {
                    line.push_str(buf[(col, row)].symbol());
                }
                // A run of many x's marks the long event's row.
                if line.matches('x').count() > 10 {
                    rows_with_x_run += 1;
                }
            }
            assert_eq!(
                rows_with_x_run, 1,
                "width {w}: long event wrapped onto {rows_with_x_run} rows"
            );
        }
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

    // ---------------- TUI-007: byte-oriented feed ingestion ----------------
    //
    // The old read_to_string path failed the whole poll on invalid or
    // temporarily-incomplete UTF-8, and silently discarded every polling
    // error. These tests exercise the new byte-buffer + from_utf8_lossy
    // + truncation-recovery + error-surfacing path. They drive read_log_tail
    // and poll_agent_feed directly against a temp file so the I/O shape is
    // covered without standing up a full Context.

    fn write_and_sync(path: &std::path::Path, bytes: &[u8]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    fn append_and_sync(path: &std::path::Path, bytes: &[u8]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
    }

    #[test]
    fn read_log_tail_reads_appended_bytes_and_advances_pos() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.log");
        write_and_sync(&log, b"line one\nline two\n");
        let mut feed = AgentFeed::default();
        let n = read_log_tail(&log, &mut feed).unwrap();
        assert_eq!(n, 18, "read both lines");
        assert_eq!(feed.pos, 18);
        // Both complete records are in pending; no LF-terminated record is
        // ever dropped or split.
        assert_eq!(feed.pending.iter().filter(|&&b| b == b'\n').count(), 2);
    }

    #[test]
    fn read_log_tail_reconstructs_split_utf8_codepoint_across_polls() {
        // The Japanese Hiragana 'hiragana A' あ is U+3042, encoded as the
        // 3 bytes E3 81 82. Split it after the second byte; the first poll
        // must NOT fail, and the second poll must complete the codepoint so
        // from_utf8_lossy decodes it correctly (no U+FFFD).
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.log");
        write_and_sync(&log, b"");
        let mut feed = AgentFeed::default();

        // First poll: write the first 2 bytes of あ + an LF that closes a
        // partial record. The record is INCOMPLETE (the codepoint is split),
        // so it must stay in `pending` -- no event emitted yet.
        append_and_sync(&log, b"x\xe3\x81\n");
        let n = read_log_tail(&log, &mut feed).unwrap();
        assert_eq!(n, 4);
        // The byte LF at offset 3 closes a record whose bytes are
        // 'x', 0xE3, 0x81. Those bytes don't form valid UTF-8 (0xE3 0x81 is
        // a 2-byte prefix of a 3-byte codepoint), so from_utf8_lossy would
        // replace them with U+FFFD. That's the correct behavior per the
        // spec: "Invalid UTF-8 bytes become replacement glyphs and cannot
        // stop future feed updates." But the record IS LF-terminated, so
        // it's consumed (and the partial codepoint at the END of the buffer
        // is what stays pending). To exercise the cross-poll reconstruction
        // cleanly, we instead test a record that straddles polls without an
        // LF in the middle.
        // Reset and run the real reconstruction case.
        feed = AgentFeed::default();
        write_and_sync(&log, b"");
        // Write 'a' + the first 2 bytes of あ, NO LF.
        append_and_sync(&log, b"a\xe3\x81");
        read_log_tail(&log, &mut feed).unwrap();
        // No complete record yet (no LF), so nothing should have been
        // consumed and pending holds the partial bytes.
        assert_eq!(feed.pending, b"a\xe3\x81");
        assert_eq!(feed.pos, 3);

        // Second poll: append the final byte of あ + LF.
        append_and_sync(&log, b"\x82\n");
        read_log_tail(&log, &mut feed).unwrap();
        // Now the record 'aあ\n' is complete and pending is drained.
        assert_eq!(feed.pending, b"a\xe3\x81\x82\n");
        // Simulate the parse step poll_agent_feed does: split on byte LF.
        let mut events = Vec::new();
        let mut buf = std::mem::take(&mut feed.pending);
        let mut start = 0;
        while start < buf.len() {
            match buf[start..].iter().position(|&b| b == b'\n') {
                Some(rel) => {
                    let end = start + rel;
                    let line = String::from_utf8_lossy(&buf[start..end]).into_owned();
                    events.extend(parse_line(&line));
                    start = end + 1;
                }
                None => break,
            }
        }
        feed.pending = buf.split_off(start);
        // The reconstructed string is 'aあ' (the U+3042 codepoint came back
        // together), NOT 'a\u{FFFD}'.
        assert_eq!(events.len(), 1);
        match &events[0] {
            AgentEvent::Raw(s) => assert_eq!(s, "aあ", "codepoint reconstructed across polls"),
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn read_log_tail_invalid_utf8_becomes_replacement_glyphs_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.log");
        // Two lone continuation bytes (invalid as UTF-8 start bytes) + LF.
        write_and_sync(&log, b"\xff\xfe\n");
        let mut feed = AgentFeed::default();
        // The read itself must succeed -- invalid UTF-8 is a decode-time
        // concern, handled by from_utf8_lossy in the parser step, not an
        // I/O error.
        let n = read_log_tail(&log, &mut feed).unwrap();
        assert_eq!(n, 3);
        assert!(!feed.pending.is_empty());
    }

    #[test]
    fn read_log_tail_resets_on_truncation() {
        // Simulate log rotation: the file shrank below our cursor. The
        // reader must reset pos to 0 and clear pending so we re-tail from
        // the new start, instead of seeking to a now-invalid offset.
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("run.log");
        // Write 100 bytes and consume them.
        write_and_sync(&log, &b"x".repeat(100));
        let mut feed = AgentFeed::default();
        feed.pos = 100;
        feed.pending = b"leftover".to_vec();
        // Truncate the file to 30 bytes (rotation).
        write_and_sync(&log, &b"y".repeat(30));
        let n = read_log_tail(&log, &mut feed).unwrap();
        assert_eq!(feed.pos, 30, "pos reset to file length");
        assert_eq!(n, 30, "read the new content from start");
        assert!(
            !feed.pending.contains(&b'x'),
            "pending was cleared of pre-truncation bytes"
        );
        assert!(feed.pending.iter().all(|&b| b == b'y'));
    }

    #[test]
    fn select_log_clears_pending_and_error() {
        // Switching to a fresh run log must drop any partial bytes and any
        // error state from the previous file, so they don't bleed into the
        // new feed.
        let mut feed = AgentFeed::default();
        feed.select_log(Some("run-a.log".to_string()));
        feed.pending = b"partial".to_vec();
        feed.last_poll_error = Some("old error".to_string());
        feed.pos = 99;
        feed.select_log(Some("run-b.log".to_string()));
        assert!(feed.pending.is_empty(), "pending cleared on log switch");
        assert!(
            feed.last_poll_error.is_none(),
            "error cleared on log switch"
        );
        assert_eq!(feed.pos, 0, "pos reset on log switch");
        // Same log name: no reset (cursor and pending untouched).
        feed.pending = b"more".to_vec();
        feed.pos = 7;
        feed.select_log(Some("run-b.log".to_string()));
        assert_eq!(feed.pending, b"more");
        assert_eq!(feed.pos, 7);
    }

    #[test]
    fn poll_agent_feed_records_error_when_log_unreadable() {
        // When the log file can't be opened (it doesn't exist), the poll
        // must store a sanitized error in last_poll_error and return Ok so
        // the monitor keeps running. The error text must contain no control
        // bytes (the sanitize invariant holds even for io::Error strings).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        let ctx = Context {
            root: root.clone(),
            conn,
            config: crate::models::Config::default(),
        };
        // Register an agent so poll_agent_feed's log_dir resolution runs.
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();

        let mut feed = AgentFeed::default();
        // No log file exists yet; the feed should not error out at the
        // `poll_agent_feed` level.
        feed.select_log(Some("run-nope.log".to_string()));
        // poll_agent_feed needs a log_name; force one that won't be found.
        let result = poll_agent_feed(&ctx, "alpha", &mut feed);
        assert!(result.is_ok(), "poll returned Ok despite missing log");
        // The feed has no log_name (newest_run_log returned None for the
        // empty dir), so last_poll_error stays None -- the "no log yet"
        // case is normal, not an error.
        assert!(feed.last_poll_error.is_none());
    }

    #[test]
    fn poll_agent_feed_parses_complete_lines_from_byte_buffer() {
        // End-to-end: write two JSON events to a log, poll, and assert
        // both are parsed. This exercises the byte-LF split + the
        // from_utf8_lossy decode + parse_line path together.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        let ctx = Context {
            root: root.clone(),
            conn,
            config: crate::models::Config::default(),
        };
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        let log_dir = ctx.trelane_dir().join("agents").join("alpha").join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("run-r-20260719T000000Z-zz.log");
        write_and_sync(
            &log_path,
            b"{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"hello\"}}\n\
              {\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"world\"}}\n",
        );

        let mut feed = AgentFeed::default();
        poll_agent_feed(&ctx, "alpha", &mut feed).unwrap();
        assert_eq!(feed.events.len(), 2, "both complete records parsed");
        match (&feed.events[0], &feed.events[1]) {
            (AgentEvent::Text(a), AgentEvent::Text(b)) => {
                assert_eq!(a, "hello");
                assert_eq!(b, "world");
            }
            other => panic!("expected two Text events, got {other:?}"),
        }
        assert!(feed.pending.is_empty(), "no partial record left");
        assert!(feed.last_poll_error.is_none());
    }

    #[test]
    fn poll_agent_feed_handles_partial_trailing_record_across_polls() {
        // Write a complete record + the start of a second (no LF), poll,
        // then append the rest + LF and poll again. The first poll emits
        // one event and keeps the partial bytes; the second poll emits the
        // second event and drains pending.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        let ctx = Context {
            root: root.clone(),
            conn,
            config: crate::models::Config::default(),
        };
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        let log_dir = ctx.trelane_dir().join("agents").join("alpha").join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("run-r-20260719T000000Z-aa.log");
        write_and_sync(
            &log_path,
            b"{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"first\"}}\n\
              {\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"se",
        );

        let mut feed = AgentFeed::default();
        poll_agent_feed(&ctx, "alpha", &mut feed).unwrap();
        assert_eq!(feed.events.len(), 1, "only the complete record parsed");
        assert!(!feed.pending.is_empty(), "partial record kept for next poll");
        // The partial bytes are the start of the second JSON object.
        assert!(feed.pending.starts_with(b"{\"type\":\"text\""));

        // Append the rest of the second record + LF.
        append_and_sync(&log_path, b"cond\"}}\n");
        poll_agent_feed(&ctx, "alpha", &mut feed).unwrap();
        assert_eq!(feed.events.len(), 2, "second record parsed after completion");
        assert!(feed.pending.is_empty(), "pending drained after second poll");
        match &feed.events[1] {
            AgentEvent::Text(s) => assert_eq!(s, "second"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn poll_agent_feed_recovers_from_log_truncation() {
        // Write two records, poll both, then truncate the file to a single
        // fresh record and poll again. The cursor must reset and the new
        // record must be parsed without the old pending bytes leaking in.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let db_path = root.join(".trelane").join("trelane.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let conn = crate::db::open(&db_path).unwrap();
        let ctx = Context {
            root: root.clone(),
            conn,
            config: crate::models::Config::default(),
        };
        crate::commands::cmd_add_agent(
            &ctx,
            "alpha",
            &["src/**".to_string()],
            &[],
            None,
            None,
        )
        .unwrap();
        let log_dir = ctx.trelane_dir().join("agents").join("alpha").join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("run-r-20260719T000000Z-bb.log");
        write_and_sync(
            &log_path,
            b"{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"old1\"}}\n\
              {\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"old2\"}}\n",
        );
        let mut feed = AgentFeed::default();
        poll_agent_feed(&ctx, "alpha", &mut feed).unwrap();
        assert_eq!(feed.events.len(), 2);

        // Truncate + write a single fresh record (rotation).
        write_and_sync(
            &log_path,
            b"{\"type\":\"text\",\"part\":{\"type\":\"text\",\"text\":\"fresh\"}}\n",
        );
        // Force-feed a stale pending to prove it gets cleared on truncation.
        feed.pending = b"stale partial".to_vec();
        poll_agent_feed(&ctx, "alpha", &mut feed).unwrap();
        // The fresh record is parsed; the stale pending is gone (not
        // concatenated onto the fresh content).
        let last = feed.events.last().unwrap();
        match last {
            AgentEvent::Text(s) => assert_eq!(s, "fresh", "stale pending leaked into fresh content"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert!(feed.pending.is_empty());
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
