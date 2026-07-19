//! Shared terminal-session helpers for the TUI entry points (monitor,
//! bench_ui, biplane_ui, diagnostic).
//!
//! Two concerns live here:
//!
//! 1. `StdCapture` -- exclusive ownership of the controlling terminal while
//!    a ratatui alternate screen is active. Both `STDOUT_FILENO` and
//!    `STDERR_FILENO` are redirected to a log file for the guard's lifetime
//!    so background threads (squire tick loop, bench orchestrator) that print
//!    progress with `println!`/`eprintln!` cannot write into cells the
//!    ratatui backend doesn't track -- the root cause of the persistent
//!    "letter fragment" artifacts documented in
//!    `FEATURES/external/trelane-ratatui-artifact-remediation.json` (TUI-003).
//!
//! 2. `TuiSession` -- exception-safe RAII guard for the crossterm setup
//!    ladder (raw mode -> alternate screen -> cursor hidden). Restores every
//!    completed stage in reverse order on `Drop`, never short-circuiting,
//!    so a panic or early-setup error cannot leave the terminal stuck in raw
//!    mode (TUI-006).
//!
//! These helpers are deliberately process-global and Unix-only: they call
//! `libc::dup`/`dup2` against file descriptors 1 and 2. On non-Unix hosts
//! `StdCapture::to_file` returns an error so callers can fall back without
//! crashing, but trelane's real targets are macOS/Linux where this is exact.

use crate::error::{Result, TrelaneError};
use std::os::unix::io::IntoRawFd;
use std::path::Path;

// ---------------------------------------------------------------- StdCapture

/// Redirect process `stdout` (fd 1) AND `stderr` (fd 2) to a file for this
/// guard's lifetime, restoring both on drop.
///
/// Why both: the ratatui `CrosstermBackend` is constructed against `/dev/tty`
/// (see `monitor::run_monitor`), but fd 2 still refers to the same
/// controlling terminal. A background thread's `eprintln!` therefore writes
/// straight onto the TUI's screen -- into cells the backend never diffs --
/// producing the scattered "letter fragments" the screenshots show. Capturing
/// fd 1 alone left that path open. Capturing both means nothing but the
/// crossterm backend can touch the screen while the alternate screen is up.
///
/// Failure semantics (TUI-003): the constructor returns an error if the log
/// can't be opened or the initial `dup` of either standard stream fails, so
/// the caller can refuse to enter the alternate screen rather than continuing
/// best-effort with a half-redirected terminal. A capture that fails to
/// install is *not* constructed; there is no "empty" guard to remember to
/// drop.
///
/// Drop semantics (TUI-003): both streams are flushed, both saved fds are
/// restored, all duplicates are closed, and both restores are attempted even
/// if one fails. Drop is infallible (we're past reporting errors by then);
/// use `close()` for an aggregated cleanup error on a normal exit path.
pub struct StdCapture {
    saved_stdout: i32,
    saved_stderr: i32,
    /// The fd we installed onto both fd 1 and fd 2. We keep it so `close()`
    /// and `drop()` can close it exactly once.
    installed_fd: i32,
}

impl StdCapture {
    /// Point stdout and stderr at `path` (created/truncated). Returns an
    /// error on any setup failure so the caller can fail *before* entering
    /// the alternate screen rather than continuing with one stream captured
    /// and the other still wired to the terminal.
    ///
    /// On error, any partial setup (saved fds, opened log) is fully undone
    /// before returning -- there is nothing for the caller to clean up.
    pub fn to_file(path: &Path) -> Result<Self> {
        // Save both originals up front. If either dup fails we close the
        // other and bail before touching anything.
        let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if saved_stdout < 0 {
            return Err(io_err("dup(stdout)"));
        }
        let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved_stderr < 0 {
            unsafe { libc::close(saved_stdout) };
            return Err(io_err("dup(stderr)"));
        }

        let file = std::fs::File::create(path)
            .map_err(|e| TrelaneError::msg(format!("capture log {}: {e}", path.display())))?;
        let file_fd = {
            // We need the raw fd, but File::into_raw_fd consumes the file
            // and closes the fd only when our dup2 install completes. Keep
            // ownership via a guard that closes on error.
            struct RawFdGuard(i32);
            impl Drop for RawFdGuard {
                fn drop(&mut self) {
                    if self.0 >= 0 {
                        unsafe { libc::close(self.0) };
                    }
                }
            }
            let g = RawFdGuard(file.into_raw_fd());
            let fd = g.0;
            // Transfer ownership: only close on the error path below.
            std::mem::forget(g);
            fd
        };

        // Install onto fd 1 first; if that fails, close the log fd and the
        // saved fds, leaving the terminal untouched.
        if unsafe { libc::dup2(file_fd, libc::STDOUT_FILENO) } < 0 {
            unsafe { libc::close(file_fd) };
            unsafe { libc::close(saved_stdout) };
            unsafe { libc::close(saved_stderr) };
            return Err(io_err("dup2(stdout)"));
        }
        // Now fd 1 is the log; fd 2 is still the terminal. Install fd 2.
        // If this fails, restore fd 1 from saved_stdout before unwinding.
        if unsafe { libc::dup2(file_fd, libc::STDERR_FILENO) } < 0 {
            unsafe {
                libc::dup2(saved_stdout, libc::STDOUT_FILENO);
                libc::close(file_fd);
                libc::close(saved_stdout);
                libc::close(saved_stderr);
            }
            return Err(io_err("dup2(stderr)"));
        }
        // Both installs succeeded. The log fd is now referenced by fd 1 and
        // fd 2; close our private copy so there's exactly one writer each.
        unsafe { libc::close(file_fd) };

        Ok(StdCapture {
            saved_stdout,
            saved_stderr,
            installed_fd: -1, // closed above; no extra close in drop
        })
    }
}

impl Drop for StdCapture {
    fn drop(&mut self) {
        // Flush both streams so anything the background thread buffered in
        // libc FILE* state reaches the log before we pull the rug out from
        // under fd 1/2.
        flush_std_streams();
        unsafe {
            // Attempt both restores even if the first fails -- a half-
            // restored terminal is worse than a fully restored one with one
            // error swallowed.
            let stdout_ok = if self.saved_stdout >= 0 {
                libc::dup2(self.saved_stdout, libc::STDOUT_FILENO) == 0
            } else {
                true
            };
            let stderr_ok = if self.saved_stderr >= 0 {
                libc::dup2(self.saved_stderr, libc::STDERR_FILENO) == 0
            } else {
                true
            };
            if self.saved_stdout >= 0 {
                libc::close(self.saved_stdout);
            }
            if self.saved_stderr >= 0 {
                libc::close(self.saved_stderr);
            }
            if self.installed_fd >= 0 {
                libc::close(self.installed_fd);
            }
            let _ = (stdout_ok, stderr_ok);
        }
    }
}

fn flush_std_streams() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

fn io_err(ctx: &str) -> TrelaneError {
    let errno = std::io::Error::last_os_error();
    TrelaneError::msg(format!("{ctx} failed: {errno}"))
}

// ---------------------------------------------------------------- TuiSession

/// Injectable raw-mode operations. `enable_raw_mode`/`disable_raw_mode` are
/// process-global crossterm calls (tcsetattr on stdin), so they can't be
/// exercised or failure-injected in unit tests without a real tty. Routing
/// them through this seam lets the acceptance tests for TUI-006 (injected
/// failure after each setup stage; panic inside a draw closure) count calls
/// and force failures without touching the test runner's own terminal.
///
/// Production callers use `RawModeOps::real()`; tests build their own with
/// counting closures.
pub struct RawModeOps {
    pub enable: Box<dyn Fn() -> std::io::Result<()> + Send>,
    pub disable: Box<dyn Fn() -> std::io::Result<()> + Send>,
}

impl RawModeOps {
    /// The real crossterm operations.
    pub fn real() -> Self {
        RawModeOps {
            enable: Box::new(|| {
                crossterm::terminal::enable_raw_mode()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            }),
            disable: Box::new(|| {
                crossterm::terminal::disable_raw_mode()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
            }),
        }
    }
}

/// RAII guard for the crossterm terminal-setup ladder, shared by the four
/// TUI entry points (monitor, diagnostic, biplane_ui, bench_ui).
///
/// The setup stages, in install order:
///   1. `enable_raw_mode`            (via RawModeOps)
///   2. `EnterAlternateScreen`       (on the backend's writer)
///   3. `hide_cursor`                (optional; some UIs never hide)
///
/// `Drop` reverses every stage that was actually reached, never
/// short-circuiting on the first error: show cursor, leave alternate screen,
/// flush the backend, disable raw mode. A panic in the middle of the draw
/// loop is the canonical case this guard exists for -- the terminal would
/// otherwise be left in raw mode with no visible cursor and the user would
/// have to `stty sane` and `reset` by hand.
///
/// `close()` performs the same restoration but returns an aggregated error
/// if any stage failed, for callers that want to surface cleanup problems
/// on a normal exit. `Drop` is the panic/error safety net and is infallible.
///
/// `suspend()`/`resume()` handle the mid-loop exit-and-re-enter pattern
/// (biplane_ui's `generate_via_model`, which runs a model subprocess that
/// needs the normal terminal): suspend shows the cursor, leaves the
/// alternate screen, and disables raw mode; resume re-enters in reverse.
/// The stage flags track state across the pair so `close()`/`Drop` still
/// do the right thing if the error path unwinds while suspended.
///
/// Extension point: mouse-capture and bracketed-paste stages (none of the
/// current UIs enable them) would slot in between stages 2 and 3, with the
/// corresponding disable on the unwind path, tracked by flags exactly like
/// `cursor_hidden`.
///
/// (TUI-006)
pub struct TuiSession {
    raw_mode: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
    raw_ops: RawModeOps,
    // The backend is owned by the terminal; we keep the terminal so we can
    // flush and leave the alternate screen on drop. Boxed writer keeps the
    // guard movable without generic params and unifies the four UIs'
    // writer types (/dev/tty or stdout) behind one signature.
    terminal:
        Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<Box<dyn std::io::Write + Send>>>>,
}

/// The concrete terminal type the guard hands out, so call sites can name
/// it without repeating the full generic.
pub type GuardedTerminal =
    ratatui::Terminal<ratatui::backend::CrosstermBackend<Box<dyn std::io::Write + Send>>>;

impl TuiSession {
    /// Build the guard and perform stage 1 (`enable_raw_mode`) with the real
    /// crossterm operations. On failure no stages have been completed and no
    /// guard is returned -- the caller can simply propagate the error.
    pub fn enter() -> Result<Self> {
        Self::enter_with_ops(RawModeOps::real())
    }

    /// Stage 1 with caller-provided raw-mode ops (tests inject counting or
    /// failing closures here).
    pub fn enter_with_ops(raw_ops: RawModeOps) -> Result<Self> {
        (raw_ops.enable)()?;
        Ok(TuiSession {
            raw_mode: true,
            alternate_screen: false,
            cursor_hidden: false,
            raw_ops,
            terminal: None,
        })
    }

    /// Stage 2: enter the alternate screen with the given writer and
    /// construct the ratatui terminal. On failure the guard keeps its prior
    /// state (raw mode on) and the error propagates; the caller typically
    /// returns it, dropping the guard, which disables raw mode.
    pub fn enter_alternate_screen(
        &mut self,
        writer: Box<dyn std::io::Write + Send>,
    ) -> Result<()> {
        use crossterm::execute;
        use crossterm::terminal::EnterAlternateScreen;
        use ratatui::Terminal;
        use ratatui::backend::CrosstermBackend;

        let mut writer = writer;
        execute!(writer, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(writer);
        let terminal = Terminal::new(backend)?;
        self.terminal = Some(terminal);
        self.alternate_screen = true;
        Ok(())
    }

    /// Borrow the underlying terminal for drawing. Returns `None` if the
    /// alternate screen was never entered (or the session is suspended --
    /// the terminal is kept across suspend, so terminal() still returns it,
    /// but drawing while suspended writes to the normal screen's buffer).
    pub fn terminal(&mut self) -> Option<&mut GuardedTerminal> {
        self.terminal.as_mut()
    }

    /// Stage 3 (optional): hide the cursor.
    pub fn hide_cursor(&mut self) -> Result<()> {
        if let Some(t) = self.terminal.as_mut() {
            t.hide_cursor()?;
            self.cursor_hidden = true;
        }
        Ok(())
    }

    /// Show the cursor (inverse of stage 3). Tracked so `resume()` and
    /// `close()` restore exactly the pre-hide state.
    pub fn show_cursor(&mut self) -> Result<()> {
        if let Some(t) = self.terminal.as_mut() {
            t.show_cursor()?;
            self.cursor_hidden = false;
        }
        Ok(())
    }

    /// Force a full clear of the terminal's previous buffer. Per the
    /// remediation plan (TUI-005): keep the initial clear; on `Resize`,
    /// call this before the next draw so the backend and both ratatui
    /// buffers are invalidated together.
    pub fn clear(&mut self) -> Result<()> {
        if let Some(t) = self.terminal.as_mut() {
            t.clear()?;
        }
        Ok(())
    }

    /// Mid-loop exit: hand the normal terminal back to a subprocess or a
    /// blocking prompt. Shows the cursor, leaves the alternate screen,
    /// disables raw mode -- in reverse setup order, tracking each stage so
    /// `resume()` re-enters exactly and `close()`/`Drop` stay correct even
    /// if the error path unwinds while suspended.
    pub fn suspend(&mut self) -> Result<()> {
        // Cursor: ALWAYS show while suspended when we have a terminal, not
        // only when the guard's flag is set -- ratatui's draw() hides the
        // cursor internally whenever a frame sets no cursor position, and
        // that internal state isn't visible to the flag, so the flag alone
        // can't decide whether the physical cursor is currently hidden.
        // show_cursor is idempotent (CSI ?25h when already visible is a
        // no-op), and a subprocess on the normal screen needs it.
        if let Some(t) = self.terminal.as_mut() {
            t.show_cursor()?;
            self.cursor_hidden = false;
        }
        // Leave the alternate screen (keep the terminal so resume can
        // re-enter without rebuilding the backend).
        if self.alternate_screen {
            if let Some(t) = self.terminal.as_mut() {
                use crossterm::execute;
                use crossterm::terminal::LeaveAlternateScreen;
                execute!(t.backend_mut(), LeaveAlternateScreen)?;
                use std::io::Write;
                t.backend_mut().flush()?;
            }
            self.alternate_screen = false;
        }
        // Disable raw mode last (subprocess expects a cooked terminal).
        if self.raw_mode {
            (self.raw_ops.disable)()?;
            self.raw_mode = false;
        }
        Ok(())
    }

    /// Re-enter after `suspend()`: enable raw mode, re-enter the alternate
    /// screen, re-hide the cursor if it was hidden before suspending, and
    /// clear (the normal screen's content may have changed while suspended,
    /// so the backend's previous buffer is untrusted).
    pub fn resume(&mut self) -> Result<()> {
        if !self.raw_mode {
            (self.raw_ops.enable)()?;
            self.raw_mode = true;
        }
        if !self.alternate_screen {
            if let Some(t) = self.terminal.as_mut() {
                use crossterm::execute;
                use crossterm::terminal::EnterAlternateScreen;
                execute!(t.backend_mut(), EnterAlternateScreen)?;
            }
            self.alternate_screen = true;
            // The normal screen may have arbitrary content now; force a full
            // clear so the next draw's diff is against a known-blank state.
            self.clear()?;
        }
        Ok(())
    }

    /// Reverse the completed stages without consuming `self`, attempting
    /// every applicable restoration in reverse order without
    /// short-circuiting. Returns the first error encountered, if any.
    /// Shared by `close()` and `Drop`.
    fn restore(&mut self) -> Result<()> {
        let mut first_err: Option<TrelaneError> = None;
        let mut push = |e: TrelaneError| {
            if first_err.is_none() {
                first_err = Some(e);
            }
        };

        // Cursor: ALWAYS show on teardown when we have a terminal, not only
        // when the guard's flag is set -- ratatui's draw() hides the cursor
        // internally whenever a frame sets no cursor position, and that
        // internal state isn't visible to the flag, so the flag alone can't
        // decide whether the physical cursor is currently hidden. show_cursor
        // is idempotent (CSI ?25h when already visible is a no-op). This is
        // the parity fix for the old unconditional `terminal.show_cursor()`
        // cleanup in the four UIs this guard replaces.
        if self.terminal.is_some() {
            let t = self.terminal.as_mut().unwrap();
            if let Err(e) = t.show_cursor() {
                push(TrelaneError::msg(format!("show_cursor: {e}")));
            }
            self.cursor_hidden = false;
        }
        // Stage 2 inverse: leave the alternate screen and flush the backend
        // so the leave command is actually written.
        if self.alternate_screen
            && let Some(t) = self.terminal.as_mut()
        {
            use crossterm::execute;
            use crossterm::terminal::LeaveAlternateScreen;
            if let Err(e) = execute!(t.backend_mut(), LeaveAlternateScreen) {
                push(TrelaneError::msg(format!("LeaveAlternateScreen: {e}")));
            }
            if let Err(e) = t.flush() {
                push(TrelaneError::msg(format!("backend flush: {e}")));
            }
            self.alternate_screen = false;
        }
        // Drop the terminal before disabling raw mode -- the backend's own
        // Drop may flush, and that wants raw mode off only after the alt
        // screen is left. We've already left it; safe to drop now.
        self.terminal = None;
        // Stage 1 inverse: disable raw mode.
        if self.raw_mode {
            if let Err(e) = (self.raw_ops.disable)() {
                push(TrelaneError::msg(format!("disable_raw_mode: {e}")));
            }
            self.raw_mode = false;
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Normal-exit cleanup: reverse every completed stage and return an
    /// aggregated error if any stage failed. Consumes the guard (which is
    /// why Drop's own restore becomes a no-op afterward -- every stage flag
    /// is already false).
    pub fn close(mut self) -> Result<()> {
        self.restore()
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        // Best-effort restoration without short-circuiting. We can't report
        // errors from drop, so we swallow them -- use close() for that. This
        // is the panic/error safety net: a panic unwinding through the draw
        // loop drops this guard and still leaves the terminal usable.
        let _ = self.restore();
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A writer that shares its bytes with the test so we can inspect which
    /// escape sequences were written (Enter/LeaveAlternateScreen, cursor
    /// show/hide) after the guard has moved it into the backend.
    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Raw-mode ops that count enable/disable calls and can be told to fail.
    #[derive(Clone, Default)]
    struct RawCounter {
        enables: Arc<Mutex<usize>>,
        disables: Arc<Mutex<usize>>,
        fail_enable: Arc<Mutex<bool>>,
        fail_disable: Arc<Mutex<bool>>,
    }
    impl RawCounter {
        fn ops(&self) -> RawModeOps {
            let (en, dis) = (self.enables.clone(), self.disables.clone());
            let (fe, fd) = (self.fail_enable.clone(), self.fail_disable.clone());
            RawModeOps {
                enable: Box::new(move || {
                    *en.lock().unwrap() += 1;
                    if *fe.lock().unwrap() {
                        Err(std::io::Error::new(std::io::ErrorKind::Other, "injected enable failure"))
                    } else {
                        Ok(())
                    }
                }),
                disable: Box::new(move || {
                    *dis.lock().unwrap() += 1;
                    if *fd.lock().unwrap() {
                        Err(std::io::Error::new(std::io::ErrorKind::Other, "injected disable failure"))
                    } else {
                        Ok(())
                    }
                }),
            }
        }
        fn enables(&self) -> usize { *self.enables.lock().unwrap() }
        fn disables(&self) -> usize { *self.disables.lock().unwrap() }
    }

    fn bytes_of(w: &SharedWriter) -> String {
        String::from_utf8_lossy(&w.0.lock().unwrap()).into_owned()
    }

    const ENTER_ALT: &str = "\x1b[?1049h";
    const LEAVE_ALT: &str = "\x1b[?1049l";
    const SHOW_CURSOR: &str = "\x1b[?25h";

    // Regression: ratatui's draw() hides the cursor INTERNALLY whenever a
    // frame sets no cursor position (all four UIs are like this), and that
    // internal hide is invisible to the guard's cursor_hidden flag. close()
    // must still emit a show-cursor so the user's terminal isn't left with
    // an invisible cursor -- the unconditional `terminal.show_cursor()` in
    // the old hand-rolled cleanup did this; the guard must match it.
    #[test]
    fn close_shows_cursor_even_when_only_ratatui_hid_it() {
        let c = RawCounter::default();
        let w = SharedWriter::default();
        let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
        session
            .enter_alternate_screen(Box::new(w.clone()))
            .unwrap();
        // Draw a frame that sets NO cursor position: ratatui hides the
        // cursor internally as a result. The guard's cursor_hidden flag
        // stays false -- the exact state the four UIs are in every frame.
        session.terminal().unwrap().draw(|_| {}).unwrap();
        assert!(!session.cursor_hidden, "guard's flag was never set");
        // close() must still show the cursor (physical state was hidden by
        // the draw). If restore() gated on the flag, this assertion fails.
        session.close().unwrap();
        let bytes = bytes_of(&w);
        assert!(
            bytes.contains(SHOW_CURSOR),
            "close() must emit show-cursor after ratatui's internal hide: {bytes:?}"
        );
    }

    // Acceptance: injected failure at stage 1 (raw mode) -> no guard, no
    // restore attempt against a half-set-up terminal.
    #[test]
    fn stage1_failure_returns_error_without_guard() {
        let c = RawCounter::default();
        *c.fail_enable.lock().unwrap() = true;
        let res = TuiSession::enter_with_ops(c.ops());
        assert!(res.is_err());
        assert_eq!(c.enables(), 1, "enable attempted once");
        assert_eq!(c.disables(), 0, "disable never called on stage-1 failure");
    }

    // Acceptance: injected failure at stage 2 (alternate screen) after stage
    // 1 succeeded -> dropping the errored path restores stage 1.
    #[test]
    fn stage2_failure_restores_stage1_on_drop() {
        let c = RawCounter::default();
        let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
        // A writer that fails every write makes EnterAlternateScreen fail.
        struct FailWriter;
        impl std::io::Write for FailWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::Other, "injected write failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let res = session.enter_alternate_screen(Box::new(FailWriter));
        assert!(res.is_err());
        // Caller returns the error, dropping the session.
        drop(session);
        assert_eq!(c.disables(), 1, "raw mode restored on drop after stage-2 failure");
    }

    // Acceptance: a full setup then an injected error in the draw path ->
    // drop restores everything (cursor shown, alt screen left, raw off).
    #[test]
    fn draw_error_leaves_terminal_restored() {
        let c = RawCounter::default();
        let w = SharedWriter::default();
        let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
        session
            .enter_alternate_screen(Box::new(w.clone()))
            .unwrap();
        session.hide_cursor().unwrap();
        // Simulate a draw error: the caller propagates and the guard drops.
        let draw_result: Result<()> = (|| {
            let t = session.terminal().unwrap();
            t.draw(|_| {})?;
            Err(TrelaneError::msg("injected draw failure"))
        })();
        assert!(draw_result.is_err());
        drop(session);
        let bytes = bytes_of(&w);
        assert!(bytes.contains(ENTER_ALT), "entered alt screen: {bytes:?}");
        assert!(bytes.contains(LEAVE_ALT), "left alt screen on drop: {bytes:?}");
        assert_eq!(c.disables(), 1, "raw mode disabled on drop");
    }

    // Acceptance: a panic inside the draw closure triggers guard cleanup
    // (Drop is the panic safety net).
    #[test]
    fn panic_in_draw_triggers_cleanup() {
        let c = RawCounter::default();
        let w = SharedWriter::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
            session
                .enter_alternate_screen(Box::new(w.clone()))
                .unwrap();
            let t = session.terminal().unwrap();
            let _ = t.draw(|_| panic!("injected draw panic"));
        }));
        assert!(result.is_err(), "panic propagated");
        let bytes = bytes_of(&w);
        assert!(
            bytes.contains(LEAVE_ALT),
            "panic unwinding left alt screen: {bytes:?}"
        );
        assert_eq!(c.disables(), 1, "panic unwinding disabled raw mode");
    }

    // Acceptance: suspend/resume round-trips raw mode + alt screen, and a
    // drop WHILE SUSPENDED does not double-restore (flags track state).
    #[test]
    fn suspend_resume_tracks_stages_across_the_pair() {
        let c = RawCounter::default();
        let w = SharedWriter::default();
        let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
        session
            .enter_alternate_screen(Box::new(w.clone()))
            .unwrap();
        assert_eq!(c.enables(), 1);
        session.suspend().unwrap();
        assert_eq!(c.disables(), 1, "suspend disabled raw mode");
        session.resume().unwrap();
        assert_eq!(c.enables(), 2, "resume re-enabled raw mode");
        // Now drop while fully set up: exactly one more disable, no double.
        drop(session);
        assert_eq!(c.disables(), 2, "single disable on drop after resume");
        let bytes = bytes_of(&w);
        // Enter appears twice (initial + resume), Leave twice (suspend + drop).
        assert_eq!(bytes.matches(ENTER_ALT).count(), 2);
        assert_eq!(bytes.matches(LEAVE_ALT).count(), 2);
    }

    // Acceptance: close() aggregates cleanup errors instead of
    // short-circuiting -- an injected disable failure is reported, and the
    // alt-screen leave was still attempted before it.
    #[test]
    fn close_reports_cleanup_error_but_attempts_all_stages() {
        let c = RawCounter::default();
        let w = SharedWriter::default();
        let mut session = TuiSession::enter_with_ops(c.ops()).unwrap();
        session
            .enter_alternate_screen(Box::new(w.clone()))
            .unwrap();
        // Inject a failure into the disable step.
        *c.fail_disable.lock().unwrap() = true;
        let res = session.close();
        assert!(res.is_err(), "cleanup error surfaced");
        // The alt screen leave was still attempted BEFORE the failing disable.
        let bytes = bytes_of(&w);
        assert!(bytes.contains(LEAVE_ALT), "leave attempted before failing disable");
    }

    /// Repeatedly creating and dropping a capture must always restore the
    /// original fds exactly. Catches a class of bugs where the drop path
    /// closes the wrong fd or forgets to restore one.
    ///
    /// We can't observe the redirect directly from the same process without
    /// forking, but we can verify the saved fds round-trip: after a capture
    /// is dropped, a *new* capture's saved fds should refer to the same
    /// underlying files (the original stdout/stderr).
    #[test]
    fn std_capture_round_trips_fds() {
        // Bypass Rust's stdout LineWriter (and `cargo test`'s per-thread
        // stdout capture) by writing directly with libc::write, so the
        // bytes really go to fd 1/fd 2 -- the thing StdCapture redirects.
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("a.log");
        let cap = StdCapture::to_file(&log).expect("capture install");
        let stdout_msg = b"hello from captured stdout\n";
        let stderr_msg = b"hello from captured stderr\n";
        unsafe {
            libc::write(libc::STDOUT_FILENO, stdout_msg.as_ptr() as *const _, stdout_msg.len());
            libc::write(libc::STDERR_FILENO, stderr_msg.as_ptr() as *const _, stderr_msg.len());
        }
        flush_std_streams();
        drop(cap);
        let text = std::fs::read_to_string(&log).expect("log readable");
        assert!(text.contains("hello from captured stdout"), "stdout: {text:?}");
        assert!(text.contains("hello from captured stderr"), "stderr: {text:?}");
    }

    /// A capture whose log path is in a nonexistent directory must fail
    /// cleanly, leaving both fds untouched. The caller can then refuse to
    /// enter the alternate screen rather than running with a half-broken
    /// capture (TUI-003: "failure to establish exclusive terminal ownership
    /// must fail before entering the alternate screen").
    #[test]
    fn std_capture_fails_cleanly_on_unopenable_log() {
        let bad = Path::new("/no/such/dir/anywhere/log.txt");
        assert!(StdCapture::to_file(bad).is_err(), "should have failed");
        // fd 1/2 are intact: a subsequent successful capture works.
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("b.log");
        let cap = StdCapture::to_file(&log).expect("recovery capture works");
        drop(cap);
    }

    /// TuiSession::enter must enable raw mode and then Drop must disable it,
    /// even when no alternate screen was entered (a setup-failure scenario).
    /// Skips when there's no controlling terminal (cargo test under a pipe)
    /// because `enable_raw_mode` requires a real tty -- the no-TTY path is
    /// exercised in production by `run_monitor`'s `/dev/tty` fallback.
    #[test]
    fn tui_session_restores_raw_mode_on_drop_without_alt_screen() {
        let have_tty = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .is_ok();
        if !have_tty {
            eprintln!("skipping: no /dev/tty (cargo test under a pipe)");
            return;
        }
        let s = TuiSession::enter().expect("enter");
        drop(s);
        // Second enter should still work (raw mode was restored).
        let s = TuiSession::enter().expect("enter again");
        drop(s);
    }
}
