## TODO

- smoothly handle transitions between desired and actual state.

- switch between services and log view sections? 
    - for command help and key shortcuts, space for switching?

- we should have a fully independent state for the TUI with functions
    - this way, there is full decoupling and event-driven design

- implement desired vs actual state machine in the start service as a loop
- use a proper TTY for color detection, using crate such as portable-pty...
- allow showing the health checks
- fix the scrollbar
- shellexpand
- interpolate variable values with env variables
- parse dotfiles
- vim style search

- DONE: support colors
- DONE: figure out a debounced refreshing...
- DONE: start the processes
- DONE: use yaml spanned
- DONE: add logging to log file
