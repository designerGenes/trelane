# Terminal Relaunch Research

Trelane has two relaunch modes:

1. **Headless launch**: `trelane pump` runs a configured launcher template. This is implemented and should remain the default.
2. **GUI/attached session wake**: Trelane injects a command into an already-open terminal/agent session. This requires terminal-specific adapters and OS permissions.

## Supported Direction

The first production adapter should be an abstraction over terminal-specific `send text` operations:

```text
target = terminal/session selector
payload = command text + newline
adapter = iterm2 | terminal.app | kitty | wezterm | tmux | custom
```

The pump should not directly know terminal APIs. It should emit a wake request, and an adapter should deliver the text to the target session.

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

## kitty

kitty has first-class remote control through `kitty @`. It supports matching windows/tabs and sending text.

Useful direction:

```bash
kitty @ send-text --match 'title:Trelane:agent-name' 'trelane inbox agent --json\n'
```

Limitations:

- Remote control must be enabled/configured.
- The safest selector is a title or window id recorded during attach.

## WezTerm

WezTerm exposes `wezterm cli send-text` and has mux/window/pane concepts. It can target panes through CLI selectors.

Useful direction:

```bash
wezterm cli send-text --pane-id <pane-id> 'trelane inbox agent --json'
```

Limitations:

- Requires discovering and storing the pane id.
- The CLI is best when paired with WezTerm's mux metadata.

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

Implement adapters in this order:

1. `tmux`: simplest, cross-terminal, easiest to test in CI.
2. `iterm2`: strongest macOS GUI fit via session `write text`.
3. `wezterm`: solid CLI/mux model.
4. `kitty`: strong remote-control model, but requires explicit user config.
5. `terminal.app`: fallback macOS support, less precise targeting.

For now, Trelane implements attachment by writing durable project instructions into `AGENTS.md` and recording enabled/disabled session agents. GUI relaunch should be added as adapter work after target identity storage is designed.
