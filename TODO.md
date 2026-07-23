# TODO

## Ideas

- TUI: log search (vim-style `/`).
- TUI: aggregated all-services log view — the MCP equivalent is available through
  `follow_all_logs` and `get_logs service="*"`.
- Attach v2: remote PTY input and resize with an explicit last-writer-wins policy for concurrent
  clients. Keep the enforcement boundary clear: MCP exposes no service-input tool.
- Attach v2: an explicit remote session-shutdown key with confirmation; detach must remain the
  default `q` behavior.
