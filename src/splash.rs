use crate::error::{Result, TrelaneError};
use crate::models::UiConfig;
use std::process::Command;

/// Run a tmux command and turn a non-zero exit into a real error instead of
/// silently discarding it. `label` is used in the error message so a failure
/// points at which piece of the interactive layout broke.
fn tmux(label: &str, args: &[&str]) -> Result<()> {
    let status = Command::new("tmux").args(args).status()?;
    if !status.success() {
        return Err(TrelaneError::msg(format!(
            "tmux {label} failed (args: {})",
            args.join(" ")
        )));
    }
    Ok(())
}

/// Overall state of a Trelane session, as shown in the tmux status bar.
///
/// Colour semantics (changed in 0.3.0 -- previously red meant "active"):
/// - green  = ACTIVE: at least one agent is running; work is happening
/// - grey   = IDLE: no agents running, no wait-cycle; the prop is watching
/// - red    = DEADLOCK: a wait-cycle exists in the parked-task ledger
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Active { running: usize },
    Idle,
    Deadlock { cycle: String },
}

impl SessionState {
    fn label(&self) -> String {
        match self {
            SessionState::Active { running } => format!("ACTIVE ({running} running)"),
            SessionState::Idle => "IDLE".to_string(),
            SessionState::Deadlock { cycle } => format!("DEADLOCK {cycle}"),
        }
    }

    fn bg_color(&self) -> &'static str {
        match self {
            SessionState::Active { .. } => "green",
            SessionState::Idle => "colour240",
            SessionState::Deadlock { .. } => "red",
        }
    }
}

/// Set (or refresh) the session-scoped top status bar:
/// `Trelane | <project> | ACTIVE (2 running)`.
///
/// This is intentionally cheap and idempotent -- the prop calls it on every
/// watch tick so the bar tracks the real session state instead of whatever
/// it was set to at bootstrap. An earlier version set the bar exactly once
/// (and only from the testing bootstrap), which is why it never updated.
///
/// Note: these are real tmux options -- `status`, `status-position`,
/// `status-style`, `status-left`, `status-left-length`. An earlier version
/// used `status-top`/`status-top-style`/`status-top-format`, which are not
/// tmux options at all: tmux rejected each `set-option` with "unknown option"
/// and the non-zero exit was swallowed, so the status bar silently never
/// appeared. Failures now surface via `tmux()`.
pub fn set_session_status(session: &str, project: &str, state: &SessionState) -> Result<()> {
    let status_text = format!("Trelane | {project} | {}", state.label());
    let bg_color = state.bg_color();

    tmux("status on", &["set-option", "-t", session, "status", "on"])?;
    tmux(
        "status-position top",
        &["set-option", "-t", session, "status-position", "top"],
    )?;
    tmux(
        "status-style",
        &[
            "set-option",
            "-t",
            session,
            "status-style",
            &format!("bg={bg_color},fg=white,bold"),
        ],
    )?;
    tmux(
        "status-left-length",
        &["set-option", "-t", session, "status-left-length", "100"],
    )?;
    tmux(
        "status-left",
        &[
            "set-option",
            "-t",
            session,
            "status-left",
            &format!(" [{status_text}] "),
        ],
    )?;
    Ok(())
}

/// Send a one-shot Trelane splash into a specific tmux pane.
pub fn send_splash_to_pane(pane_id: &str, agent: &str, reason: &str, root: &str) -> Result<()> {
    // Use echo (not printf) so backslashes in the ASCII logo are printed
    // literally instead of being interpreted as escape sequences.
    let logo = crate::logo::LOGO_SMALL.replace('\'', "'\"'\"'");
    let agent_q = agent.replace('\'', "'\"'\"'");
    let reason_q = reason.replace('\'', "'\"'\"'");
    let root_q = root.replace('\'', "'\"'\"'");

    let splash = format!(
        "clear && echo '' && echo '' && echo '{logo}' && echo '  Agent   : {agent_q}' && echo '  Reason  : {reason_q}' && echo '  Project : {root_q}' && echo '  Status  : launching...' && echo '' && sleep 1"
    );

    tmux(
        "send-keys splash",
        &["send-keys", "-t", pane_id, &splash, "Enter"],
    )
}

/// Path of the per-session marker file that toggles verbose prop output.
/// Present = verbose on. The prop checks it every tick, so the toggle takes
/// effect without restarting anything.
pub fn verbose_marker_path(session: &str) -> String {
    format!("/tmp/trelane-{session}-verbose")
}

/// Whether verbose prop output is enabled for this session. `TRELANE_VERBOSE=1`
/// forces it on regardless of the marker (useful outside tmux).
pub fn verbose_enabled(session: Option<&str>) -> bool {
    if std::env::var("TRELANE_VERBOSE").ok().as_deref() == Some("1") {
        return true;
    }
    match session {
        Some(s) => std::path::Path::new(&verbose_marker_path(s)).exists(),
        None => false,
    }
}

/// Bind the configured session keys and pane-navigation shortcuts.
///
/// `bind-key` has no `-t <session>` flag (that was invalid syntax that tmux
/// rejected and the error was swallowed); key bindings are global to the tmux
/// server, keyed by table. We bind into the `root` table (via `-n`) so they
/// work without a prefix. The project root is read at trigger time from the
/// per-session marker file written by the launch script /
/// `provision_interactive_tmux_layout`.
pub fn setup_session_ui(session: &str, ui: &UiConfig) -> Result<()> {
    let root_marker = format!("/tmp/trelane-{session}-root");

    // Diagnostics split: full-session `trelane status`.
    let status_cmd = format!(
        "trelane --root \"$(cat {root_marker} 2>/dev/null || echo $HOME)\" status; \
         echo; echo '[press any key to close]'; read -n 1",
    );
    tmux(
        "bind-key diagnostics",
        &[
            "bind-key",
            "-n",
            &ui.keys.diagnostics,
            "split-window",
            "-v",
            "-l",
            "40%",
            &status_cmd,
        ],
    )?;

    // Inbox split for the pane under the cursor. `#{pane_title}` is a tmux
    // format that tmux itself expands when the binding fires (it is
    // intentionally not substituted here in Rust), so the diagnostic targets
    // whichever agent pane the user is focused on.
    let inbox_cmd = format!(
        "trelane --root \"$(cat {root_marker} 2>/dev/null || echo $HOME)\" \
         inbox \"#{{pane_title}}\" --json; echo; echo '[press any key to close]'; read -n 1",
    );
    tmux(
        "bind-key inbox",
        &[
            "bind-key",
            "-n",
            &ui.keys.inbox,
            "split-window",
            "-v",
            "-l",
            "40%",
            &inbox_cmd,
        ],
    )?;

    // Verbose toggle: flip the marker file and flash a confirmation in the
    // tmux message line. The prop re-reads the marker every tick.
    let marker = verbose_marker_path(session);
    let toggle_cmd = format!(
        "if [ -f {marker} ]; then rm -f {marker}; tmux display-message 'trelane: verbose OFF'; \
         else touch {marker}; tmux display-message 'trelane: verbose ON'; fi"
    );
    tmux(
        "bind-key verbose toggle",
        &["bind-key", "-n", &ui.keys.verbose_toggle, "run-shell", &toggle_cmd],
    )?;

    // Pane navigation: Alt+arrows move focus between frames. tmux-level, so
    // it behaves identically in Ghostty, iTerm2, Terminal.app, kitty, ...
    if ui.pane_navigation {
        for (key, dir) in [
            ("M-Left", "-L"),
            ("M-Right", "-R"),
            ("M-Up", "-U"),
            ("M-Down", "-D"),
        ] {
            tmux(
                "bind-key pane-nav",
                &["bind-key", "-n", key, "select-pane", dir],
            )?;
        }
    }
    Ok(())
}
