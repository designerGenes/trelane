<!-- BEGIN TRELANE -->
## Trelane Coordination

This project is attached to a Trelane session. Before each substantial action, check Trelane state and use the protocol instead of waiting on other agents.

- Project root: `/Users/jadennation/DEV/01_active_projects/trelane`
- Enabled agents/models: (none)
- Disabled agents/models: (none)

Rules for agents:

1. Start by running `trelane status` and `trelane inbox <your-agent-id> --json`.
2. Never wait while running. If blocked, send a message with `trelane send`, then park the task with `trelane park`.
3. Stay in your domain. Use `trelane claim <your-agent-id> <path>` before editing contested or cross-domain files.
4. Run `trelane audit <your-agent-id>` before exiting, then `trelane done <your-agent-id>`.
5. If woken for a deadlock, proceed with a documented assumption, notify the counterpart, and unpark the task.

Useful commands:

```bash
trelane status
trelane inbox <agent> --json
trelane send --from <agent> --to <agent> --type question --subject "..." --body "..."
trelane park <agent> --wait-reply <msg-id> --waiting-on <agent> --resume-hint "..."
trelane claim <agent> <path>
trelane audit <agent>
trelane done <agent>
```
<!-- END TRELANE -->
