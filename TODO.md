# TODO

## Ideas

- TUI: log search (vim-style `/`).
- TUI: aggregated all-services log view — the MCP equivalent is available through
  `follow_all_logs` and `get_logs service="*"`.
- TUI: lifecycle events timeline pane. Agents already get the timeline through
  `get_service_events`; the TUI ignores `Events` changes entirely, so a human cannot see
  dependency gating, restart backoff, or dynamic-service transitions the way an agent can.
- Attach v2: remote PTY input and resize with an explicit last-writer-wins policy for concurrent
  clients. Keep the enforcement boundary clear: MCP exposes no service-input tool.
- Attach v2: an explicit remote session-shutdown key with confirmation; detach must remain the
  default `q` behavior.
- Control protocol: paginate service rosters and retained-run indexes instead of returning
  `LimitExceeded` when either response exceeds one frame.
- Windows: add suspended PTY child spawning upstream in `portable-pty`, then assign each child to
  its kill-on-close job before resuming it.
