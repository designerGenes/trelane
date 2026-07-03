use crate::error::Result;
use std::process::Command;

pub fn set_session_status_bar(session: &str, project: &str, active: bool) -> Result<()> {
    let status_text = format!(
        "Trelane | {project} | {}",
        if active { "ACTIVE" } else { "IDLE" }
    );
    let bg_color = if active { "red" } else { "green" };

    Command::new("tmux")
        .args(["set-option", "-t", session, "status-top", "on"])
        .status()?;

    Command::new("tmux")
        .args([
            "set-option",
            "-t",
            session,
            "status-top-style",
            &format!("bg={bg_color},fg=white,bold"),
        ])
        .status()?;

    Command::new("tmux")
        .args([
            "set-option",
            "-t",
            session,
            "status-top-format",
            &format!(" [{status_text}] "),
        ])
        .status()?;

    Ok(())
}

pub fn send_splash_to_pane(pane_id: &str, agent: &str, reason: &str, root: &str) -> Result<()> {
    let splash = format!(
        "clear && printf '\\n\\n{}\\n  Agent   : {}\\n  Reason  : {}\\n  Project : {}\\n  Status  : launching...\\n\\n' && sleep 1",
        crate::logo::LOGO_SMALL.replace('\'', "'\"'\"'"),
        agent.replace('\'', "'\"'\"'"),
        reason.replace('\'', "'\"'\"'"),
        root.replace('\'', "'\"'\"'"),
    );

    Command::new("tmux")
        .args(["send-keys", "-t", pane_id, &splash, "Enter"])
        .status()?;

    Ok(())
}

pub fn bind_diagnostic_toggle(session: &str) -> Result<()> {
    let diagnostic_script = format!(
        "trelane --root \"$(cat /tmp/trelane-{}-root 2>/dev/null || echo $HOME)\" status",
        session
    );

    Command::new("tmux")
        .args([
            "bind-key",
            "-t",
            session,
            "F2",
            "split-window",
            "-v",
            "-p",
            "40",
            &diagnostic_script,
        ])
        .status()?;

    Command::new("tmux")
        .args([
            "bind-key",
            "-t",
            session,
            "F3",
            "split-window",
            "-v",
            "-p",
            "40",
            "trelane --root \"$(cat /tmp/trelane-{}-root 2>/dev/null || echo $HOME)\" inbox \"$(tmux display-message -p -t '#{pane_id}' -F '#{pane_title}' 2>/dev/null || echo frontend)\" --json",
        ])
        .status()?;

    Ok(())
}
