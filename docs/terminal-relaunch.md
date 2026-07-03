# Terminal Relaunch Research

Trelane has two relaunch modes:

1. **Headless launch**: `trelane pump` runs a configured launcher template. This is implemented and should remain the default.
2. **GUI/attached session wake**: Trelane injects a command into an already-open terminal/agent session. This requires terminal-specific adapters and OS permissions.

## Supported Direction

The first production adapter should be an abstraction over terminal-specific `send text` operations:

```text
target = terminal/session selector
payload = command text + newline
adapter = ghostty | iterm2 | terminal.app | kitty | wezterm | tmux | custom
```

The pump should not directly know terminal APIs. It should emit a wake request, and an adapter should deliver the text to the target session.

## Core Policy

For every supported terminal app, the preferred production setup is:

1. Run the agent inside `tmux`.
2. Keep the terminal app only as the visual host.
3. Point Trelane at the tmux target, not the terminal window itself.

This gives Trelane a deterministic target regardless of whether the visible host
is Ghostty, iTerm2, WezTerm, kitty, or Terminal.app. Native terminal adapters
remain useful, but they should be treated as fallback delivery mechanisms rather
than the primary orchestration layer.

## macOS Terminal.app

Apple documents Terminal as AppleScript-scriptable and runnable via `osascript`.

Useful direction:

```applescript
tell application "Terminal"
  do script "trelane inbox agent --json" in selected tab of front window
end tell
```

Limitations:

- Selecting the correct tab reliably needs tab/window metadata or title conventions.
- macOS Automation permissions are required.
- It is better for opening a new command in a tab than for safely typing into an arbitrary running TUI.
- If the visible terminal contains multiple tabs, panes, or splits, `tmux` inside the terminal is still the recommended targeting layer.

## iTerm2

iTerm2 exposes AppleScript objects for windows, tabs, and sessions. Its session object supports `write text`, optionally without newline.

Useful direction:

```applescript
tell application "iTerm2"
  tell current session of current window
    write text "trelane inbox agent --json"
  end tell
end tell
```

Useful selectors:

- Window name
- Session name
- Session unique id
- TTY

Limitations:

- AppleScript support is documented but marked deprecated in favor of iTerm2's Python API.
- Requires Automation permissions.
- Correct target selection should use a recorded session id, not only window title.
- Even with better session objects, tmux inside iTerm2 is still the more deterministic way to target the correct running agent surface.

## kitty

kitty has first-class remote control through `kitty @`. It supports matching windows/tabs and sending text.

Useful direction:

```bash
kitty @ send-text --match 'title:Trelane:agent-name' 'trelane inbox agent --json\n'
```

Limitations:

- Remote control must be enabled/configured.
- The safest selector is a title or window id recorded during attach.
- If the agent is already hosted inside tmux within kitty, target tmux directly instead of the kitty window.

## WezTerm

WezTerm exposes `wezterm cli send-text` and has mux/window/pane concepts. It can target panes through CLI selectors.

Useful direction:

```bash
wezterm cli send-text --pane-id <pane-id> 'trelane inbox agent --json'
```

Limitations:

- Requires discovering and storing the pane id.
- The CLI is best when paired with WezTerm's mux metadata.
- If WezTerm is only hosting a tmux session, Trelane should still prefer the tmux adapter over the WezTerm adapter.

## Ghostty

Ghostty does not currently expose a CLI remote-control surface in this repo's
environment, so the practical adapter path on macOS is GUI scripting.

Useful direction:

```applescript
tell application "Ghostty" to activate
tell application "System Events"
  tell process "Ghostty"
    keystroke "trelane inbox agent --json"
    key code 36
  end tell
end tell
```

Useful selectors:

- `frontmost` for the active Ghostty window
- A recorded window-title substring, used to focus a matching window before typing

Notes:

- Ghostty windows can contain multiple splits.
- The current Ghostty fallback only types into the currently focused split.
- It does not have a split-specific selector in Trelane today.

Limitations:

- Requires macOS Accessibility permissions for `System Events`.
- Window targeting is less precise than pane-aware terminals like tmux or WezTerm.
- Safe automation depends on consistent window naming.
- For split-heavy Ghostty layouts, tmux inside Ghostty is strongly preferred.

## tmux

tmux is the most portable terminal-adjacent option when sessions run inside tmux.

Useful direction:

```bash
tmux send-keys -t trelane:agent-name 'trelane inbox agent --json' Enter
```

Limitations:

- Only works if agents are inside tmux.
- Requires target naming conventions.

## Recommendation

Recommended order of operations:

1. `tmux` inside any supported terminal app.
2. Terminal-native pane/session targeting where a terminal offers a reliable API.
3. GUI scripting fallback for terminals that do not expose deterministic remote control.

Adapter quality, as currently understood:

1. `tmux`: simplest, cross-terminal, easiest to test in CI.
2. `wezterm`: strong when directly targeting real WezTerm panes.
3. `kitty`: strong when remote control is enabled and windows are named well.
4. `iterm2`: good GUI fallback with real session objects.
5. `ghostty`: workable macOS fallback, but currently window/focus based.
6. `terminal.app`: broad macOS fallback, but less precise than the others.

Trelane now implements attached-session relaunch through stored launch targets and
adapter delivery. Headless launch remains the default, while GUI relaunch is best
effort and depends on terminal-specific permissions and targeting fidelity. For
reliable multi-pane or multi-split orchestration, tmux is the intended control plane.
