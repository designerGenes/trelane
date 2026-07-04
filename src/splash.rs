use crate::error::{Result, TrelaneError};
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

/// Set a session-scoped top status bar reading `Trelane | <project> | ACTIVE`
/// (red while active, green while idle).
///
/// Note: these are real tmux options -- `status`, `status-position`,
/// `status-style`, `status-left`, `status-left-length`. An earlier version
/// used `status-top`/`status-top-style`/`status-top-format`, which are not
/// tmux options at all: tmux rejected each `set-option` with "unknown option"
/// and the non-zero exit was swallowed, so the status bar silently never
/// appeared. Failures now surface via `tmux()`.
pub fn set_session_status_bar(session: &str, project: &str, active: bool) -> Result<()> {
    let status_text = format!(
        "Trelane | {project} | {}",
        if active { "ACTIVE" } else { "IDLE" }
    );
    let bg_color = if active { "red" } else { "green" };

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
        &["set-option", "-t", session, "status-left-length", "60"],
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

/// Bind a key that pops a diagnostic split showing `trelane status` for the
/// session's project. Binds into tmux's `root` key table (via `-n`) so it works
/// without a prefix.
///
/// `bind-key` has no `-t <session>` flag (that was invalid syntax that tmux
/// rejected and the error was swallowed); key bindings are global to the tmux
/// server, keyed by table. The project root is read at trigger time from the
/// per-session marker file written by `provision_interactive_tmux_layout`.
pub fn bind_diagnostic_toggle(session: &str) -> Result<()> {
    let root_marker = format!("/tmp/trelane-{session}-root");
    let status_cmd = format!(
        "trelane --root \"$(cat {root_marker} 2>/dev/null || echo $HOME)\" status; \
         echo; echo '[press any key to close]'; read -n 1",
    );
    tmux(
        "bind-key F2 (diagnostics)",
        &[
            "bind-key",
            "-n",
            "F2",
            "split-window",
            "-v",
            "-l",
            "40%",
            &status_cmd,
        ],
    )?;

    // F3: inbox for the pane under the cursor. `#{pane_title}` is a tmux format
    // that tmux itself expands when the binding fires (it is intentionally not
    // substituted here in Rust), so the diagnostic targets whichever agent pane
    // the user is focused on.
    let inbox_cmd = format!(
        "trelane --root \"$(cat {root_marker} 2>/dev/null || echo $HOME)\" \
         inbox \"#{{pane_title}}\" --json; echo; echo '[press any key to close]'; read -n 1",
    );
    tmux(
        "bind-key F3 (inbox)",
        &[
            "bind-key",
            "-n",
            "F3",
            "split-window",
            "-v",
            "-l",
            "40%",
            &inbox_cmd,
        ],
    )?;
    Ok(())
}
