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
/// - grey   = IDLE: no agents running, no wait-cycle; the squire is watching
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
/// This is intentionally cheap and idempotent -- the squire calls it on every
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

/// Path of the per-session marker file that toggles verbose squire output.
/// Present = verbose on. The squire checks it every tick, so the toggle takes
/// effect without restarting anything.
pub fn verbose_marker_path(session: &str) -> String {
    format!("/tmp/trelane-{session}-verbose")
}

/// Whether verbose squire output is enabled for this session. `TRELANE_VERBOSE=1`
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
    // tmux message line. The squire re-reads the marker every tick.
    let marker = verbose_marker_path(session);
    let toggle_cmd = format!(
        "if [ -f {marker} ]; then rm -f {marker}; tmux display-message 'trelane: verbose OFF'; \
         else touch {marker}; tmux display-message 'trelane: verbose ON'; fi"
    );
    tmux(
        "bind-key verbose toggle",
        &[
            "bind-key",
            "-n",
            &ui.keys.verbose_toggle,
            "run-shell",
            &toggle_cmd,
        ],
    )?;

    // Pane navigation. When `match_host_terminal` is set, mirror the host
    // terminal's own pane-navigation shortcuts where tmux can receive them;
    // otherwise (and as the fallback) bind Alt+arrows, which every terminal
    // forwards. The binding is tmux-level either way, so it is consistent
    // across emulators.
    if ui.pane_navigation {
        let (bindings, note) = resolve_pane_nav_bindings(ui);
        if let Some(note) = note {
            eprintln!("  pane-nav: {note}");
        }
        for (key, dir) in &bindings {
            tmux(
                "bind-key pane-nav",
                &["bind-key", "-n", key, "select-pane", dir],
            )?;
        }
    }
    Ok(())
}

/// A terminal emulator we can recognize from the environment. Used to decide
/// how to bind tmux pane navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostTerminal {
    Ghostty,
    Kitty,
    WezTerm,
    ITerm2,
    AppleTerminal,
    Unknown,
}

impl HostTerminal {
    fn label(&self) -> &'static str {
        match self {
            HostTerminal::Ghostty => "Ghostty",
            HostTerminal::Kitty => "kitty",
            HostTerminal::WezTerm => "WezTerm",
            HostTerminal::ITerm2 => "iTerm2",
            HostTerminal::AppleTerminal => "Apple Terminal",
            HostTerminal::Unknown => "an unrecognized terminal",
        }
    }
}

fn detect_host_terminal() -> HostTerminal {
    detect_terminal_from(|k| std::env::var(k).ok())
}

/// Pure terminal classification from an environment lookup, so it can be tested
/// without touching the real process environment.
fn detect_terminal_from<F: Fn(&str) -> Option<String>>(get: F) -> HostTerminal {
    let term_program = get("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    if get("GHOSTTY_RESOURCES_DIR").is_some() || term_program == "ghostty" {
        return HostTerminal::Ghostty;
    }
    if get("KITTY_WINDOW_ID").is_some() {
        return HostTerminal::Kitty;
    }
    if get("WEZTERM_PANE").is_some() || get("WEZTERM_EXECUTABLE").is_some() {
        return HostTerminal::WezTerm;
    }
    if term_program == "iterm.app" || get("ITERM_SESSION_ID").is_some() {
        return HostTerminal::ITerm2;
    }
    if term_program == "apple_terminal" {
        return HostTerminal::AppleTerminal;
    }
    HostTerminal::Unknown
}

struct PaneNavParse {
    /// (tmux key, select-pane direction flag) pairs tmux can actually receive.
    forwardable: Vec<(String, &'static str)>,
    /// Count of `goto_split` bindings we could not forward (cmd/super-based, or
    /// a non-arrow key).
    unforwardable: usize,
}

/// Parse a Ghostty config, extracting `goto_split` pane-navigation bindings and
/// translating the ones tmux can receive into tmux key syntax. Cmd/Super-based
/// bindings are counted as unforwardable (macOS intercepts them before the
/// terminal, so they never reach tmux).
fn parse_ghostty_pane_nav(config: &str) -> PaneNavParse {
    let mut forwardable = Vec::new();
    let mut unforwardable = 0;

    for raw in config.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // keybind = <trigger>=<action>
        let rest = match line.strip_prefix("keybind") {
            Some(r) => r.trim_start(),
            None => continue,
        };
        let rest = match rest.strip_prefix('=') {
            Some(r) => r.trim(),
            None => continue,
        };
        // The trigger contains no '=', so the first '=' separates it from the
        // action (e.g. "alt+left=goto_split:left").
        let (trigger, action) = match rest.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        let dir = match action.trim().strip_prefix("goto_split:").map(str::trim) {
            Some("left") => "-L",
            Some("right") => "-R",
            Some("up") => "-U",
            Some("down") => "-D",
            _ => continue, // not a directional pane-navigation binding
        };
        match ghostty_trigger_to_tmux(trigger.trim()) {
            Some(key) => forwardable.push((key, dir)),
            None => unforwardable += 1,
        }
    }
    PaneNavParse {
        forwardable,
        unforwardable,
    }
}

/// Translate a Ghostty trigger (e.g. "alt+left", "ctrl+shift+up") into tmux key
/// syntax (e.g. "M-Left", "C-S-Up"). Returns None if the trigger uses a
/// non-forwardable modifier (cmd/super), targets a non-arrow key, or carries no
/// modifier (binding a bare arrow would clobber ordinary cursor movement).
fn ghostty_trigger_to_tmux(trigger: &str) -> Option<String> {
    let mut ctrl = false;
    let mut meta = false;
    let mut shift = false;
    let mut base: Option<&'static str> = None;

    for part in trigger.split('+') {
        match part.trim().to_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "opt" | "option" => meta = true,
            "shift" => shift = true,
            "cmd" | "command" | "super" => return None, // macOS keeps these from tmux
            "left" => base = Some("Left"),
            "right" => base = Some("Right"),
            "up" => base = Some("Up"),
            "down" => base = Some("Down"),
            _ => return None, // unknown / non-arrow key
        }
    }

    let base = base?;
    let mut key = String::new();
    if ctrl {
        key.push_str("C-");
    }
    if meta {
        key.push_str("M-");
    }
    if shift {
        key.push_str("S-");
    }
    if key.is_empty() {
        return None; // a bare arrow would eat normal cursor keys
    }
    key.push_str(base);
    Some(key)
}

fn default_pane_nav() -> Vec<(String, &'static str)> {
    vec![
        ("M-Left".to_string(), "-L"),
        ("M-Right".to_string(), "-R"),
        ("M-Up".to_string(), "-U"),
        ("M-Down".to_string(), "-D"),
    ]
}

fn read_ghostty_config() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        candidates.push(std::path::PathBuf::from(xdg).join("ghostty").join("config"));
    }
    candidates.push(
        std::path::PathBuf::from(&home)
            .join(".config")
            .join("ghostty")
            .join("config"),
    );
    candidates.push(
        std::path::PathBuf::from(&home)
            .join("Library")
            .join("Application Support")
            .join("com.mitchellh.ghostty")
            .join("config"),
    );
    for c in candidates {
        if let Ok(text) = std::fs::read_to_string(&c) {
            return Some(text);
        }
    }
    None
}

/// Decide which tmux pane-navigation bindings to install, plus an optional note
/// explaining the choice. Falls back to Alt+arrows whenever the host terminal's
/// own bindings can't be matched.
fn resolve_pane_nav_bindings(ui: &UiConfig) -> (Vec<(String, &'static str)>, Option<String>) {
    if !ui.match_host_terminal {
        return (default_pane_nav(), None);
    }

    let term = detect_host_terminal();
    match term {
        HostTerminal::Ghostty => {
            if let Some(cfg) = read_ghostty_config() {
                let parsed = parse_ghostty_pane_nav(&cfg);
                if !parsed.forwardable.is_empty() {
                    let note = if parsed.unforwardable > 0 {
                        format!(
                            "matched {} Ghostty pane-nav binding(s) from config; {} cmd/super binding(s) can't reach tmux and were skipped.",
                            parsed.forwardable.len(),
                            parsed.unforwardable
                        )
                    } else {
                        format!(
                            "matched {} Ghostty pane-nav binding(s) from config.",
                            parsed.forwardable.len()
                        )
                    };
                    return (parsed.forwardable, Some(note));
                }
            }
            (
                default_pane_nav(),
                Some(
                    "detected Ghostty; its pane-navigation shortcuts use cmd (which macOS keeps from tmux), so Alt+arrows are bound instead."
                        .to_string(),
                ),
            )
        }
        other => (
            default_pane_nav(),
            Some(format!(
                "detected {}; using Alt+arrow pane navigation (universally forwarded to tmux).",
                other.label()
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn detect_ghostty_from_resources_dir() {
        let get = lookup(&[("GHOSTTY_RESOURCES_DIR", "/opt/ghostty")]);
        assert_eq!(detect_terminal_from(get), HostTerminal::Ghostty);
    }

    #[test]
    fn detect_ghostty_from_term_program() {
        let get = lookup(&[("TERM_PROGRAM", "ghostty")]);
        assert_eq!(detect_terminal_from(get), HostTerminal::Ghostty);
    }

    #[test]
    fn detect_kitty_and_wezterm_and_iterm() {
        assert_eq!(
            detect_terminal_from(lookup(&[("KITTY_WINDOW_ID", "1")])),
            HostTerminal::Kitty
        );
        assert_eq!(
            detect_terminal_from(lookup(&[("WEZTERM_PANE", "0")])),
            HostTerminal::WezTerm
        );
        assert_eq!(
            detect_terminal_from(lookup(&[("TERM_PROGRAM", "iTerm.app")])),
            HostTerminal::ITerm2
        );
    }

    #[test]
    fn detect_unknown_when_nothing_matches() {
        assert_eq!(detect_terminal_from(lookup(&[])), HostTerminal::Unknown);
    }

    #[test]
    fn ghostty_alt_arrows_are_forwardable() {
        let cfg = "\
# my ghostty config
keybind = alt+left=goto_split:left
keybind = alt+right=goto_split:right
keybind = alt+up=goto_split:up
keybind = alt+down=goto_split:down
";
        let parsed = parse_ghostty_pane_nav(cfg);
        assert_eq!(parsed.unforwardable, 0);
        assert_eq!(parsed.forwardable.len(), 4);
        assert!(parsed.forwardable.contains(&("M-Left".to_string(), "-L")));
    }

    #[test]
    fn ghostty_cmd_arrows_are_unforwardable() {
        let cfg = "\
keybind = cmd+opt+left=goto_split:left
keybind = cmd+opt+right=goto_split:right
";
        let parsed = parse_ghostty_pane_nav(cfg);
        assert!(parsed.forwardable.is_empty());
        assert_eq!(parsed.unforwardable, 2);
    }

    #[test]
    fn ghostty_ctrl_shift_combo_translates() {
        let cfg = "keybind = ctrl+shift+up=goto_split:up\n";
        let parsed = parse_ghostty_pane_nav(cfg);
        assert_eq!(parsed.forwardable, vec![("C-S-Up".to_string(), "-U")]);
    }

    #[test]
    fn non_pane_nav_keybinds_are_ignored() {
        let cfg = "keybind = ctrl+a=new_split:right\nkeybind = cmd+c=copy_to_clipboard\n";
        let parsed = parse_ghostty_pane_nav(cfg);
        assert!(parsed.forwardable.is_empty());
        assert_eq!(parsed.unforwardable, 0);
    }
}
