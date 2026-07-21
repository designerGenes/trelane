//! Live bench TUI: a full-screen ratatui view that tails `bench-events.jsonl`
//! while the bench orchestrator runs on a background thread. Keeps the user
//! informed of every message and tick in real time -- the stated reason for
//! not wanting headless runs was being uninformed, and this is the answer.
//!
//! Layout:
//!   +------------------------------------------+---------------------------+
//!   | bench status (model, runs, elapsed)      | message stream            |
//!   | (left, ~40%)                             | (right, ~60%, auto-scroll)|
//!   +------------------------------------------+---------------------------+
//!   | footer: events seen, quit hint                                       |
//!   +----------------------------------------------------------------------+
//!
//! The TUI is a pure file reader: it tails bench-events.jsonl by tracking the
//! read position. The orchestrator (bench::run_bench on a background thread)
//! writes to that file. The two never share memory -- the file is the
//! interface. This means the TUI can be skipped (--no-ui) and the events file
//! is still a complete record; and a crash in the TUI cannot affect the run.

use crate::error::{Result, TrelaneError};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Redirect process stdout (fd 1) to a file for as long as this guard lives,
/// restoring the original on drop.
///
/// Why this exists: the orchestrator reuses `testing::run_testing`, which was
/// written as a CLI runner and prints ~30 progress lines to stdout (`[testing]
/// step 1: ...`, `waking engine`, `launched ... pid=`). Under the TUI those
/// prints land on the SAME terminal the alternate screen owns, in raw mode, so
/// newlines don't carriage-return -- producing the cascading staircase of
/// corrupted text. Rather than convert all 30 print sites (they're still the
/// right behavior for a non-TUI run), we capture fd 1 into a `bench.log` file
/// for the TUI's lifetime. The progress isn't lost -- it's in the file, and
/// the TUI already renders the same information from the events stream -- it
/// just stops fighting the screen.
///
/// fd-level (not `print!`-level) redirect is required because the prints
/// happen on a background thread and inside `run_testing`, which we don't
/// thread a writer through; swapping the fd catches every write regardless of
/// where it originates.
struct StdoutCapture {
    saved_fd: i32,
}

impl StdoutCapture {
    /// Point stdout at `path` (created/truncated). On any failure, returns a
    /// guard that restores nothing -- capture is best-effort; a bench that
    /// can't open its log should still run (just with the old corruption),
    /// not abort.
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

/// Run the bench TUI in the foreground while the orchestrator runs on a
/// background thread. The orchestrator is `orchestrator()` -- a closure that
/// runs `bench::run_bench` (or equivalent) and writes to `events_path`.
/// Returns when the user quits (q/Esc) or the orchestrator finishes.
pub fn run_with_tui<F>(
    events_path: &std::path::Path,
    scenario_name: &str,
    model: &str,
    max_turns: u32,
    runs: u32,
    orchestrator: F,
) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = stop.clone();

    // Capture stdout (fd 1) into bench.log beside the events file, for the
    // whole run. The orchestrator reuses run_testing, which prints ~30
    // progress lines to stdout; the TUI draws to /dev/tty (see run_loop) so it
    // is immune to this redirect. Result: progress is preserved in a file the
    // user can read, and it no longer corrupts the screen. Best-effort -- if
    // the log can't be opened, the guard restores nothing and the run
    // proceeds (with the old behavior) rather than aborting.
    let capture = events_path
        .parent()
        .map(|dir| StdoutCapture::to_file(&dir.join("bench.log")));

    // Spawn the orchestrator on a background thread. It writes to
    // bench-events.jsonl (tailed by the TUI); its stdout goes to bench.log.
    let handle = std::thread::spawn(move || -> Result<()> {
        let result = orchestrator();
        // Signal the TUI to stop regardless of outcome.
        stop_for_thread.store(true, Ordering::Relaxed);
        result
    });

    // Run the TUI on the main thread.
    let tui_result = run_loop(events_path, scenario_name, model, max_turns, runs, &stop);

    // If the TUI exited via user quit before the orchestrator finished, we
    // still join the thread to avoid leaking it. The orchestrator cannot be
    // interrupted mid-slice (a free-model subprocess is already running), but
    // it will stop spawning new slices after the current one finishes.
    let orch_result = handle
        .join()
        .map_err(|_| TrelaneError::msg("bench orchestrator thread panicked"))?;

    // Restore the real stdout before the final summary prints.
    drop(capture);
    if let Some(dir) = events_path.parent() {
        eprintln!(
            "[bench] orchestrator progress log: {}",
            dir.join("bench.log").display()
        );
    }

    tui_result?;
    orch_result
}

fn run_loop(
    events_path: &std::path::Path,
    scenario_name: &str,
    model: &str,
    max_turns: u32,
    runs: u32,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    use crossterm::event::{self, Event, KeyCode};
    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };

    enable_raw_mode()?;
    // Draw to /dev/tty directly rather than std::io::stdout(). This decouples
    // the TUI from fd 1, so the orchestrator's captured stdout (redirected to
    // bench.log by run_with_tui) can't corrupt the screen and, conversely, the
    // redirect can't steal the TUI's output. Falls back to stdout if /dev/tty
    // is unavailable (e.g. not a real terminal), which is the old behavior.
    let mut tty: Box<dyn std::io::Write + Send> =
        match std::fs::OpenOptions::new().write(true).open("/dev/tty") {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(std::io::stdout()),
        };
    execute!(tty, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(tty);
    let mut terminal = Terminal::new(backend)?;

    let start = Instant::now();
    let mut events: Vec<BenchEventView> = Vec::new();
    let mut last_tick_count = 0u32;
    let mut last_launched = 0usize;
    let mut last_running = 0usize;
    let mut file_pos: u64 = 0;
    let orchestrator_finished = false;

    let outcome = (|| -> Result<()> {
        loop {
            // Tail the events file: read everything new since last position.
            let new_events = read_new_events(events_path, &mut file_pos)?;
            events.extend(new_events);

            // Update tick summary from the latest tick event.
            for e in &events {
                if e.kind == "tick" {
                    if let Some(tick) = e.data.get("tick").and_then(|v| v.as_u64()) {
                        last_tick_count = tick as u32;
                    }
                    if let Some(launched) = e.data.get("launched").and_then(|v| v.as_u64()) {
                        last_launched = launched as usize;
                    }
                    if let Some(running) = e.data.get("running").and_then(|v| v.as_u64()) {
                        last_running = running as usize;
                    }
                }
            }

            terminal.draw(|f| {
                render(
                    f,
                    scenario_name,
                    model,
                    max_turns,
                    runs,
                    start,
                    last_tick_count,
                    last_launched,
                    last_running,
                    &events,
                    orchestrator_finished,
                );
            })?;

            // Poll for keyboard input with a short timeout so the loop can
            // also check for new events and the stop flag.
            if event::poll(Duration::from_millis(200))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        _ => {}
                    }
                }
            }

            if stop.load(Ordering::Relaxed) {
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

/// A simplified view of a bench event for rendering. Parsed from the JSONL
/// file; fields that fail to parse are shown as raw strings.
#[derive(Debug, Clone)]
struct BenchEventView {
    ts: String,
    kind: String,
    from: Option<String>,
    to: Option<String>,
    msg_type: Option<String>,
    subject: Option<String>,
    tick: Option<u64>,
    launched: Option<u64>,
    running: Option<u64>,
    data: serde_json::Value,
}

impl BenchEventView {
    fn from_json(obj: serde_json::Value) -> Self {
        let ts = obj
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let kind = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
        Self {
            ts,
            kind: kind.clone(),
            from: data.get("from").and_then(|v| v.as_str()).map(String::from),
            to: data.get("to").and_then(|v| v.as_str()).map(String::from),
            msg_type: data.get("type").and_then(|v| v.as_str()).map(String::from),
            subject: data
                .get("subject")
                .and_then(|v| v.as_str())
                .map(String::from),
            tick: data.get("tick").and_then(|v| v.as_u64()),
            launched: data.get("launched").and_then(|v| v.as_u64()),
            running: data.get("running").and_then(|v| v.as_u64()),
            data,
        }
    }

    /// One-line summary for the message stream pane, with a short timestamp.
    fn summary(&self) -> String {
        // Show only the time portion (HH:MM:SS) of the ISO timestamp to keep
        // lines narrow in the message pane.
        let short_ts = self
            .ts
            .split('T')
            .nth(1)
            .and_then(|s| s.split('.').next())
            .unwrap_or(&self.ts);
        match self.kind.as_str() {
            "message_sent" => {
                let from = self.from.as_deref().unwrap_or("?");
                let to = self.to.as_deref().unwrap_or("?");
                let msg_type = self.msg_type.as_deref().unwrap_or("?");
                let subject = self.subject.as_deref().unwrap_or("");
                format!("{short_ts} {from} -> {to} [{msg_type}] {subject}")
            }
            "tick" => {
                let tick = self.tick.unwrap_or(0);
                let launched = self.launched.unwrap_or(0);
                let running = self.running.unwrap_or(0);
                format!("{short_ts} --- tick {tick}: launched={launched}, running={running} ---")
            }
            other => format!("{short_ts} {other}: {}", self.data),
        }
    }
}

/// Read new lines from the events file since the last read position.
fn read_new_events(path: &std::path::Path, pos: &mut u64) -> Result<Vec<BenchEventView>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut file = std::fs::OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(*pos))?;
    let reader = BufReader::new(&mut file);
    let mut events = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
            events.push(BenchEventView::from_json(obj));
        }
    }
    *pos = file.stream_position()?;
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut ratatui::Frame,
    scenario_name: &str,
    model: &str,
    max_turns: u32,
    runs: u32,
    start: Instant,
    tick_count: u32,
    launched: usize,
    running: usize,
    events: &[BenchEventView],
    orchestrator_finished: bool,
) {
    let accent = Color::Rgb(0x2d, 0xd4, 0xbf); // THEME_TRELANE_ACCENT (teal)
    let dim = Color::Rgb(0x6b, 0x72, 0x80); // THEME_DIM
    let warn = Color::Rgb(0xef, 0x44, 0x44); // THEME_WARN
    let ok = Color::Rgb(0x22, 0xc5, 0x5e); // THEME_OK

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // body (left + right)
            Constraint::Length(3), // footer
        ])
        .split(f.area());

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40), // status
            Constraint::Percentage(60), // message stream
        ])
        .split(chunks[0]);

    // Left pane: bench status
    let elapsed = start.elapsed();
    let status_text = vec![
        Line::from(vec![
            Span::styled("Scenario: ", Style::default().fg(dim)),
            Span::raw(scenario_name),
        ]),
        Line::from(vec![
            Span::styled("Model:     ", Style::default().fg(dim)),
            Span::raw(model),
        ]),
        Line::from(vec![
            Span::styled("Max turns: ", Style::default().fg(dim)),
            Span::raw(max_turns.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Runs:       ", Style::default().fg(dim)),
            Span::raw(format!("{runs}")),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Elapsed:   ", Style::default().fg(dim)),
            Span::raw(format!("{:.1}s", elapsed.as_secs_f64())),
        ]),
        Line::from(vec![
            Span::styled("Last tick: ", Style::default().fg(dim)),
            Span::raw(tick_count.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Launched:  ", Style::default().fg(dim)),
            Span::raw(launched.to_string()),
        ]),
        Line::from(vec![
            Span::styled("Running:   ", Style::default().fg(dim)),
            Span::styled(
                running.to_string(),
                if running > 0 {
                    Style::default().fg(warn)
                } else {
                    Style::default().fg(ok)
                },
            ),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            if orchestrator_finished {
                "FINISHED"
            } else {
                "RUNNING"
            },
            Style::default()
                .fg(if orchestrator_finished { ok } else { accent })
                .add_modifier(Modifier::BOLD),
        )]),
    ];
    let status_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", scenario_name))
        .border_style(Style::default().fg(accent));
    let status = Paragraph::new(status_text).block(status_block);
    f.render_widget(status, body[0]);

    // Right pane: message stream (newest at bottom, show last N that fit)
    let msg_items: Vec<ListItem> = events
        .iter()
        .map(|e| {
            let style = match e.kind.as_str() {
                "tick" => Style::default().fg(dim),
                "message_sent" => Style::default(),
                _ => Style::default().fg(accent),
            };
            ListItem::new(Line::from(vec![Span::styled(e.summary(), style)]))
        })
        .collect();
    let msg_block = Block::default()
        .borders(Borders::ALL)
        .title(" Message stream ")
        .border_style(Style::default().fg(accent));
    let msg_list = List::new(msg_items).block(msg_block);
    f.render_widget(msg_list, body[1]);

    // Footer
    let footer_text = format!(" {} event(s) | q to quit ", events.len());
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(dim),
    )]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_event_view_parses_message_sent() {
        let json = serde_json::json!({
            "ts": "2026-07-18T18:00:00Z",
            "kind": "message_sent",
            "data": {
                "id": "msg-1",
                "from": "alpha",
                "to": "beta",
                "type": "question",
                "subject": "what's the schema?"
            }
        });
        let view = BenchEventView::from_json(json);
        assert_eq!(view.kind, "message_sent");
        assert_eq!(view.from.as_deref(), Some("alpha"));
        assert_eq!(view.to.as_deref(), Some("beta"));
        assert_eq!(view.msg_type.as_deref(), Some("question"));
        assert_eq!(view.subject.as_deref(), Some("what's the schema?"));
        assert_eq!(
            view.summary(),
            "18:00:00Z alpha -> beta [question] what's the schema?"
        );
    }

    #[test]
    fn bench_event_view_parses_tick() {
        let json = serde_json::json!({
            "ts": "2026-07-18T18:00:01Z",
            "kind": "tick",
            "data": { "tick": 3, "launched": 2, "running": 1 }
        });
        let view = BenchEventView::from_json(json);
        assert_eq!(view.kind, "tick");
        assert_eq!(view.tick, Some(3));
        assert_eq!(view.launched, Some(2));
        assert_eq!(view.running, Some(1));
        assert_eq!(
            view.summary(),
            "18:00:01Z --- tick 3: launched=2, running=1 ---"
        );
    }

    #[test]
    fn read_new_events_returns_empty_when_file_missing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nonexistent.jsonl");
        let mut pos = 0u64;
        let events = read_new_events(&path, &mut pos).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn read_new_events_reads_only_new_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bench-events.jsonl");
        std::fs::write(&path, "").unwrap();
        let mut pos = 0u64;

        // Write first event.
        let event1 = serde_json::json!({
            "ts": "t1", "kind": "message_sent",
            "data": { "from": "a", "to": "b", "type": "info", "subject": "first" }
        });
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{}", event1).unwrap();
        }
        let events = read_new_events(&path, &mut pos).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subject.as_deref(), Some("first"));

        // Write second event. First must NOT be re-read.
        let event2 = serde_json::json!({
            "ts": "t2", "kind": "tick",
            "data": { "tick": 1, "launched": 0, "running": 0 }
        });
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{}", event2).unwrap();
        }
        let events = read_new_events(&path, &mut pos).unwrap();
        assert_eq!(events.len(), 1, "only the new event");
        assert_eq!(events[0].kind, "tick");
    }
}
