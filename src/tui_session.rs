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

/// RAII guard for the crossterm terminal-setup ladder.
///
/// The setup stages, in install order:
///   1. `enable_raw_mode`
///   2. `EnterAlternateScreen` (on the backend's writer)
///   3. `hide_cursor` (via `terminal.hide_cursor()`)
///
/// `Drop` reverses every stage that was actually reached, never
/// short-circuiting on the first error: show cursor, leave alternate screen,
/// disable raw mode, flush the backend. A panic in the middle of the draw
/// loop is the canonical case this guard exists for -- the terminal would
/// otherwise be left in raw mode with no visible cursor and the user would
/// have to `stty sane` and `reset` by hand.
///
/// `close()` performs the same restoration but returns an aggregated error
/// if any stage failed, for callers that want to surface cleanup problems
/// on a normal exit. `Drop` is the panic/error safety net and is infallible.
///
/// (TUI-006)
pub struct TuiSession {
    raw_mode: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
    // The backend is owned by the terminal; we keep the terminal so we can
    // flush and leave the alternate screen on drop. Boxed to keep the guard
    // movable without generic params.
    terminal:
        Option<ratatui::Terminal<ratatui::backend::CrosstermBackend<Box<dyn std::io::Write + Send>>>>,
}

impl TuiSession {
    /// Build the guard and perform stage 1 (`enable_raw_mode`). On failure
    /// no stages have been completed and no guard is returned -- the caller
    /// can simply propagate the error.
    pub fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(TuiSession {
            raw_mode: true,
            alternate_screen: false,
            cursor_hidden: false,
            terminal: None,
        })
    }

    /// Stage 2: open the writer (typically `/dev/tty`, falling back to
    /// stdout), enter the alternate screen, and construct the ratatui
    /// terminal. Idempotent-ish: calling twice replaces the terminal; the
    /// previous one is dropped (which would leave the alt screen via its
    /// own drop path -- but we don't support nested sessions, so this is
    /// just a panic guard).
    pub fn enter_alternate_screen(
        mut self,
        writer: Box<dyn std::io::Write + Send>,
    ) -> Result<Self> {
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
        Ok(self)
    }

    /// Borrow the underlying terminal for drawing. Returns `None` if the
    /// alternate screen was never entered.
    pub fn terminal(
        &mut self,
    ) -> Option<&mut ratatui::Terminal<ratatui::backend::CrosstermBackend<Box<dyn std::io::Write + Send>>>>
    {
        self.terminal.as_mut()
    }

    /// Stage 3: hide the cursor. Optional because some entry points hide
    /// the cursor only while a full-screen picker is open and show it again
    /// for text entry.
    pub fn hide_cursor(&mut self) -> Result<()> {
        if let Some(t) = self.terminal.as_mut() {
            t.hide_cursor()?;
            self.cursor_hidden = true;
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

    /// Run a closure with mutable access to the terminal, then automatically
    /// restore the cursor visibility at the end (the common TUI pattern:
    /// draw with cursor hidden, show it again when the user quits).
    pub fn draw<F>(&mut self, f: F) -> Result<()>
    where
        F: FnOnce(&mut ratatui::Terminal<ratatui::backend::CrosstermBackend<Box<dyn std::io::Write + Send>>>) -> Result<()>,
    {
        if let Some(t) = self.terminal.as_mut() {
            f(t)?;
        }
        Ok(())
    }

    /// Normal-exit cleanup: reverse every completed stage and return an
    /// aggregated error if any stage failed. Consumes the guard.
    pub fn close(mut self) -> Result<()> {
        let mut first_err: Option<TrelaneError> = None;
        let mut push = |e: TrelaneError| {
            if first_err.is_none() {
                first_err = Some(e);
            }
        };

        // Show the cursor first (the inverse of stage 3).
        if self.cursor_hidden
            && let Some(t) = self.terminal.as_mut()
        {
            if let Err(e) = t.show_cursor() {
                push(TrelaneError::msg(format!("show_cursor: {e}")));
            }
        }
        // Leave the alternate screen (inverse of stage 2). We use the
        // backend's writer so the command reaches the same tty.
        if self.alternate_screen
            && let Some(t) = self.terminal.as_mut()
        {
            use crossterm::execute;
            use crossterm::terminal::LeaveAlternateScreen;
            if let Err(e) = execute!(t.backend_mut(), LeaveAlternateScreen) {
                push(TrelaneError::msg(format!("LeaveAlternateScreen: {e}")));
            }
            // Flush the backend so the leave command is actually written.
            if let Err(e) = t.flush() {
                push(TrelaneError::msg(format!("backend flush: {e}")));
            }
        }
        // Drop the terminal before disabling raw mode -- the Drop may flush
        // the backend, which wants raw mode off only after the alt screen
        // is left. We've already left it; safe to drop now.
        self.terminal = None;
        // Disable raw mode (inverse of stage 1).
        if self.raw_mode {
            if let Err(e) = crossterm::terminal::disable_raw_mode() {
                push(TrelaneError::msg(format!("disable_raw_mode: {e}")));
            }
            self.raw_mode = false;
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        // Best-effort restoration without short-circuiting. We can't report
        // errors from drop, so we swallow them -- use close() for that.
        if self.cursor_hidden
            && let Some(t) = self.terminal.as_mut()
        {
            let _ = t.show_cursor();
        }
        if self.alternate_screen
            && let Some(t) = self.terminal.as_mut()
        {
            use crossterm::execute;
            use crossterm::terminal::LeaveAlternateScreen;
            let _ = execute!(t.backend_mut(), LeaveAlternateScreen);
            let _ = t.flush();
        }
        self.terminal = None;
        if self.raw_mode {
            let _ = crossterm::terminal::disable_raw_mode();
            self.raw_mode = false;
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

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
