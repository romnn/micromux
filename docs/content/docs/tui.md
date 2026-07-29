---
title: The terminal UI
weight: 5
---

# The terminal UI

Running `micromux` with no subcommand opens the TUI for the current project. It's the primary way to watch and steer a stack.

{{< figure src="images/overview.png" alt="The micromux TUI: a service sidebar beside a log pane" caption="The service sidebar (left) and the selected service's live logs (right)." >}}

## Layout

- **Sidebar** — every service, one row each, showing its lifecycle state: pending, blocked, starting, running, healthy, unhealthy, exited, or disabled. The selected row drives the panes on the right.
- **Log pane** — the selected service's live output, annotated with the lifecycle transitions that stall it. ANSI color and interactive/redrawing output are supported.
- **Healthcheck pane** — toggled with `H`, it shows the selected service's latest probe: the command, its exit status, and its output.

## Keybindings

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | Move the selection |
| `r` | Restart the selected service |
| `R` | Restart all services |
| `d` | Disable / enable the selected service |
| `a` | Enter PTY **input mode** — send keystrokes to the service (exit with `Alt+Esc`) |
| `Tab` | Move focus between panes |
| `H` | Toggle the healthcheck pane |
| `w` | Toggle log wrapping |
| `t` | Toggle follow-tail (stick to the newest logs) |
| `q` / `Esc` | Quit |

Restart, disable, and enable all go through the [control plane]({{< relref "agent-control/control-plane.md" >}}), so they respect dependency gating and restart policy.

## Sending input to a service

Some processes want input — a REPL, a prompt, a dev server waiting on a keypress. Press `a` to enter **input mode**: keystrokes are forwarded to the selected service's PTY until you leave input mode with `Alt+Esc`.

## Disabling on the fly

Press `d` to disable the selected service: micromux stops it and its row turns gray, while its captured logs remain for inspection. Press `d` again to re-enable and start it.

Those preserved logs would otherwise look frozen, so micromux marks each transition in the log with a blue rule — `=== service disable requested ===`, `=== service enable requested ===`, `=== waiting for postgres to become healthy ===`, `=== automatic restart scheduled after 250 ms ===`. Whatever the log is doing, the last line says why. The same rules appear in `micromux ctl logs` and in the retained run files; the MCP log tools strip their color along with the rest of the ANSI.

{{< figure src="images/disable.png" alt="A disabled service, stopped with its row grayed out" caption="A disabled service — stopped, grayed out, its logs preserved." >}}

## Attach to a running session

When a session is running **headless** — started by an agent via `micromux serve`, or by `start_session` over [MCP]({{< relref "agent-control/_index.md" >}}) — open the same TUI against it without launching a second supervisor:

```bash
micromux attach                          # the session for the config resolved from the cwd
micromux attach --config ./micromux.yaml
micromux attach --session name:my-project
```

A selector may be a bare session name or `name:`, `pid:`, or `hash:`. More than one attach client can observe and operate the same session at once.

An attached TUI follows service status, logs, and healthchecks, and its lifecycle keys (`r`, `R`, `d`) still restart, enable, and disable services. In v1 it does **not** forward service PTY input or terminal-resize events.

> [!NOTE]
> Pressing `q` or `Ctrl-C` in an attached client only **detaches** it — it never stops the session or its services. To stop a headless session explicitly, use `micromux ctl stop` or the MCP `stop_session` tool.
