# TODO

## Known limitations

- **PTY log-reader thread can leak.** Each service's PTY output is read by a dedicated
  std thread that exits only on EOF of the master. If a service spawns a grandchild/daemon
  that inherits the slave and outlives the direct child, EOF never arrives and the reader
  thread (plus its dup'd fd) lingers until process exit. A cancellable read loop
  (non-blocking fd + `poll` against the terminate/shutdown tokens) would fix this.
- **Interactive snapshots use a lossy channel.** Alt-screen frames are delivered with
  `try_send` using an Append/ReplaceLast protocol. A dropped frame under heavy load can
  briefly desync the live snapshot line. Tagging snapshots with a per-run generation id (or
  delivering them over the reliable path) would make this robust.
- **Word-wrap height is approximated.** Wrapped line height is computed with a character-based
  estimate, so the last row or two of very long word-wrapped lines may only be reachable via
  follow-tail (`t` / `G`) rather than line-by-line scrolling.

## Ideas

- Log search (vim-style `/`).
- Aggregated "all services" log view.
