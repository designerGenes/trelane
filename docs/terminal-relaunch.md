# Terminal Relaunch Research

Trelane has two relaunch modes:

1. **Headless launch**: `trelane pump` runs a configured launcher template. This is implemented and remains the default for non-interactive use.
2. **tmux-managed interactive wake**: Trelane injects a command into a tmux session/pane that it can create and target deterministically.

## Supported Direction

The primary supported adapter is now tmux itself:

```text
target = terminal/session selector
payload = command text + newline
adapter = tmux
```

The pump should not need terminal-app APIs in the primary path. It should emit a wake request, and tmux should deliver the text to the target session.

## Core Policy

Trelane is tmux-first across the project:

1. Run interactive agents inside tmux.
2. Let Trelane target tmux sessions/panes directly.
3. Treat any GUI terminal app only as an optional viewport onto those tmux sessions.

This gives Trelane deterministic control over creation, naming, wakeup, and observation of the swarm.

## tmux

tmux is the supported interactive control plane.

Useful direction:

```bash
tmux send-keys -t trelane:agent-name 'trelane inbox agent --json' Enter
```

Limitations:

- Users need tmux installed.
- Long-running interactive runs depend on the configured launcher commands exiting cleanly or being managed by the human/operator.

## Recommendation

Recommended order of operations:

1. Use tmux directly.
2. Let Trelane create and target tmux sessions.
3. If you want a GUI, attach your preferred terminal app to those tmux sessions as a viewer, not as the orchestration surface.

Trelane now treats tmux as the intended and supported interactive orchestration surface. Headless launch remains the default for non-interactive runs.
