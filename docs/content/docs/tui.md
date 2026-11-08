---
title: The terminal UI
weight: 5
---

# The terminal UI

Running `micromux` with no subcommand opens the TUI for the current project. It's the primary way to watch and steer a stack.

{{< figure src="images/overview.png" alt="The micromux TUI: a service sidebar beside a log pane" caption="The service sidebar (left) and the selected service's live logs (right)." >}}

## Layout

- **Sidebar** — every service, one row each, showing its lifecycle state: pending, blocked, starting, running, healthy, unhealthy, exited, or disabled. A [job]({{< relref "configuration/services.md" >}}#jobs) shows **COMPLETED** once it exited with status 0, grayed out like a disabled service, **FAILED** with its exit code in red, **RETRYING** between attempts, and **IDLE** while a manual job waits to be run. The selected row drives the panes on the right.
- **Log pane** — the selected service's live output, annotated with the lifecycle transitions that stall it. ANSI color and interactive/redrawing output are supported. `A` switches it to [every service's logs](#all-services-logs) in time order.
- **Healthcheck pane** — toggled with `H`, it shows the selected service's latest probe: the command, its exit status, and its output.

The pane with the focus, which the movement keys act on, has a thick frame in an accent color; `Tab` or a click moves it. The thickness tells it apart even where the terminal shows no color.

## Keybindings

The header's right side lists the view keys, which change only what the panes show, with their current state. With [routes]({{< relref "configuration/sessions.md" >}}#hostnames), the Logs pane's title lists the browser URLs of the selected service, the routes whose port is in its `ports` list, as many as fit, then a count of the rest; the header says so in red when the shared proxy does not serve them right now. The footer lists the keys that move around and act on services. Both wrap onto extra rows when the terminal is too narrow.

| Key | Action |
|---|---|
| `j` / `k`, `↓` / `↑` | Move the selection |
| `r` | Restart the selected service, or run the selected job again |
| `R` | Restart all services |
| `d` | Disable / enable the selected service |
| `a` | Enter PTY **input mode** — send keystrokes to the service (exit with `Ctrl+]`) |
| `Tab` | Move focus between panes |
| `H` | Toggle the healthcheck pane |
| `A` | Toggle every service's logs in one pane, in time order (see [All services' logs](#all-services-logs)) |
| `w` | Toggle log wrapping |
| `t` | Toggle follow-tail (stick to the newest logs) |
| `/` | Search the logs the log pane shows (see [Searching logs](#searching-logs)) |
| `n` / `N` | Move to the next older / newer search match |
| `L` | Pick the selected service's log level threshold (`ALL`, `DEBUG`, `INFO`, `WARN`, `ERROR`) |
| `T` | Toggle timestamps on the selected service's structured JSON log lines |
| `F` | Toggle the selected service's fields that `logs.fields` hides |
| `i` | Show everything about the selected service: URLs, ports by variable, command, directory, dependencies, restart and stop policy, the last healthcheck, the current run, and the last cleanup (close with `i` or `Esc`) |
| `u` | Show the session: its domain, the proxy port, every route with its port and service, and the allocated ports (close with `u` or `Esc`) |
| `?` | Show every key binding (close with `Esc`) |
| `m` | Toggle mouse capture (see [Mouse](#mouse)) |
| `Ctrl-Z` | Suspend to the shell; `fg` brings the TUI back |
| `y` / `Enter` | Copy the mouse selection; so do a right-click and the terminal's copy shortcut (see [Copying](#copying)) |
| `Esc` | Clear the mouse selection, or else the search |
| `q` | Quit. `Ctrl-C` never quits, so a stray one cannot stop the session; the footer only says to press `q`. To stop micromux from outside, send it `SIGINT` or `SIGTERM` |

{{< figure src="images/service-details.png" alt="The service details popup over the log pane, listing the api service's state, URL, ports, command, directory, dependencies, restart and stop policy, last healthcheck, and current run" caption="`i` describes the selected service in full; `u` shows the session's routes and ports the same way." >}}

`Ctrl-Z` gives the terminal back and stops micromux as the shell would stop any foreground job, together with whatever started it in the same job, such as `cargo run` or a task runner. Until you resume it with `fg`, micromux supervises nothing: the services keep running, but their output piles up unread until they block on it, healthchecks and restart policies pause, agents talking to the session get no answer, and after about 15 seconds the shared proxy drops the session's routes, so its `*.localhost` URLs stop answering until micromux continues.

Restart, disable, and enable all go through the [control plane]({{< relref "agent-control/control-plane.md" >}}), so they respect dependency gating and restart policy.

## Searching logs

`/` opens a prompt over the footer's last row: type a query and press `Enter` to search the logs the log pane shows, the selected service's or [every service's](#all-services-logs), or `Esc` to cancel; `Backspace` edits it. The query matches literal text, the way the pane shows it, and ignores case unless it has an uppercase letter, as vim's smartcase does: `error` finds `ERROR` too, `Error` finds only `Error`.

The newest lines are at the bottom, so a search goes back in time, like vim's `?`: `Enter` moves to the newest match at or above the bottom of the view, `n` to the next older match and `N` to the next newer one, wrapping around at either end. A match off screen scrolls to the middle of the pane, and follow-tail stops, so the match stays put while new lines arrive; `t` follows the tail again. Every match on screen is highlighted, the current one in the focus color, and the Logs pane's top-right corner shows the query with the current match's place, such as `/error 3/17`, or `no match`.

The search stays while the log moves on: new lines are searched as they arrive, and the current match stays on its line until that line is dropped from the log. Selecting another service searches its logs for the same query, with no match current until `n` or `N` moves to one. `Esc` ends the search and its highlighting, once any mouse selection is cleared. In PTY input mode `/`, `n` and `N` go to the service like any other key. Without wrapping (`w`), a match past the pane's right edge is not visible.

## All services' logs

`A` shows the logs of every service in the log pane, interleaved in time order, so a request can be followed from one service to the next; `A` again shows the selected service's logs. Each line starts with the name of its service, in a color of its own and padded to the longest name, so the log text lines up; a name longer than 16 columns is cut short. The header shows `All:ON` while the pane shows them.

- **Order.** Lines are ordered by when micromux captured them, one clock for every service, even for lines that carry no timestamp of their own. Lines captured in the same millisecond go in the order of their services in the sidebar, and each service's lines keep their own order. The MCP tool `follow_all_logs` also keeps each service's order, but goes by a JSON record's own timestamp where it has one and breaks ties by service name.
- **Filters.** Each service's lines look as they do in its own pane: its level threshold (`L`), timestamps (`T`) and hidden fields (`F`) apply here too. Those keys still act on the selected service.
- **Live screens.** A program that redraws its screen in place, such as a progress bar or a full-screen dashboard, shows as one block of lines, each tagged, that moves to the time of its latest redraw.
- **Everything else works as in a single service's pane:** scrolling and follow-tail, wrapping, search with `/`, `n` and `N`, which also matches the service names, and selecting and copying with the mouse, which copies the names too. A view off the tail stays on its lines while services drop their oldest ones. Switching between the views follows the newest lines.
- **Input.** `a` still types into the selected service; the pane shows that service's logs while PTY input mode lasts, so you see what you type, and every service's logs again once `Ctrl+]` leaves it.

Only the lines that the services still keep in memory show, as in their own panes, so memory stays within each service's [log retention]({{< relref "configuration/logs.md" >}}).

## Mouse

With mouse capture on, which is the default, the mouse works as in most terminal apps:

- **Click** a service to select it, or any pane to focus it, as `Tab` would.
- **Wheel** scrolls the pane under the pointer, without moving the focus: each notch moves the service list's selection by one service, and the log and healthcheck panes by three rows, also where the terminal sends a burst of events for one notch. A high-resolution wheel or a touchpad moves at most a step every 15 milliseconds. Scrolling up leaves follow-tail, as the keys do, but only when the view moves; a pane whose text fits, such as a full-screen program's live screen, keeps following. Scrolling back to the bottom follows the tail again. While a popup is open, the wheel scrolls the popup.
- **Drag** selects text inside the pane where the drag started, never across panes, so a selection in the logs never picks up the sidebar. Dragging past the top or bottom of the pane scrolls it, and past its right edge selects to the end of the line, even where a line runs on beyond the pane. A double click selects a word, a triple click the whole line, even when it wraps over several rows or runs past the pane. `Esc` clears the selection.
- **The selection stays on its text** until you clear it or start another: while new lines arrive and the pane scrolls, while a busy log drops its oldest lines, and while a live screen or a popup redraws. A resize keeps it too, unless wrapping lays the text out anew.
- **Selecting only highlights.** Press `y` or `Enter`, right-click anywhere, or click `y Copy` in the footer, which takes the mouse toggle's place while text is selected, to copy the selection to the clipboard. The terminal's own copy shortcut can copy too, as [Copying](#copying) describes. A right-click copies when the button comes up and changes nothing else: not the selection, the focus or the selected service; without a selection it does nothing. To copy as soon as a drag ends instead, turn on [`ui.copy_on_select`](#settings).
- **Copying** goes to the clipboard, and to the primary selection where there is one, through the terminal's OSC 52 support, which also works over ssh. The footer says what was copied. Some terminals ignore OSC 52, such as macOS Terminal and GNOME Terminal and other VTE-based terminals, so outside ssh micromux also hands the text to `pbcopy` on macOS, or to `wl-copy`, `xclip` or `xsel` on Linux, whichever is installed. Where neither works, use the terminal's own selection, described below.
- **Click** a key hint in the header or footer to run its key, such as `H` to show the healthcheck pane or `?` for help. It acts when the button comes up on the hint it went down on, so sliding off cancels it. The `q` hint does nothing, so a stray click never ends the session, and neither do hints that name two keys, like `↑/↓`.
- **Click** a URL in the Logs pane's title to open it in your browser.
- **Click** outside an open popup to close it, as `Esc` does. In the level picker, click the threshold you want.

In PTY input mode, neither clicks nor the wheel change which service receives your keystrokes, a right-click copies nothing, and `y`, `Enter`, `Ctrl-C` and the copy shortcuts go to the service, as the bytes a terminal without the kitty keyboard protocol sends for them; leave the mode with `Ctrl+]` to copy.

{{< figure src="images/mouse-selection.png" alt="Four request lines of the api service's logs highlighted by a mouse drag, with the footer offering y to copy them" caption="A drag selects lines inside the logs pane only, and y copies them." >}}

### Copying

Besides `y`, `Enter`, a right-click and the footer's `y Copy`, micromux copies the selection on the shortcuts terminals copy with: `Ctrl+Shift+C` and `Ctrl+Insert`, as on Linux and Windows, and `Super+C`, which is `Cmd+C` on macOS. A plain `Ctrl-C` never copies.

Two things stand between these shortcuts and micromux:

- **The terminal consumes its copy shortcut**, as long as it is bound, even when it has nothing selected, which under mouse capture it never has. To let it through when the terminal has no selection of its own, mark the binding `performable:` in Ghostty:

  ```
  keybind = performable:ctrl+shift+c=copy_to_clipboard:mixed
  keybind = performable:super+c=copy_to_clipboard:mixed
  ```

  In WezTerm, a Lua `action_callback` can copy when `window:get_selection_text_for_pane(pane)` returns text and otherwise pass the key on with `window:perform_action(act.SendKey { key = 'c', mods = 'CTRL|SHIFT' }, pane)`. Where a terminal cannot pass the shortcut on, `Ctrl+Insert`, which few terminals bind, a right-click and `y` still copy.
- **The kitty keyboard protocol tells `Ctrl+Shift+C` from `Ctrl-C`.** Without it, terminals send both as the same byte, and `Super` never reaches programs at all. micromux turns the protocol on wherever the terminal supports it, as Ghostty, kitty, WezTerm, foot and Alacritty do, and turns it off again whenever it lets go of the terminal. In a terminal without it, a copy shortcut passed on to programs arrives as `Ctrl-C`; that is the terminal's doing, not micromux's, and micromux then only says to press `q` to quit. tmux has no kitty protocol either, but it can report `Ctrl+Shift+C` apart in its own way: with

  ```
  set -s extended-keys on
  set -s extended-keys-format csi-u
  set -as terminal-features '*:extkeys'
  ```

  in `~/.tmux.conf`, micromux asks tmux for these reports (xterm's modifyOtherKeys) and resets the request whenever it lets go of the terminal. The last line has tmux ask the terminal around it to send `Ctrl+Shift+C` apart in turn, which tmux only does for terminals it knows to support it. tmux's default `extended-keys-format xterm` sends a form micromux cannot read, so there micromux asks for nothing, and `Ctrl+Shift+C` arrives as `Ctrl-C`; `Ctrl+Insert`, a right-click and `y` still copy. The terminal around tmux needs its copy shortcut passed on too, as above.

To see what micromux receives, start it with `MICROMUX_DEBUG_KEYS=1`: its log, `~/.cache/micromux/micromux.log.<date>` on Linux, `~/Library/Caches/com.romnn.micromux/micromux.log.<date>` on macOS, or the file `--log-file` names, then records whether the terminal has the protocol, what tmux said, and every key as micromux reads it; in PTY input mode it leaves out which character was typed, so a password typed into a service stays out of the log.

A service in PTY input mode never asked for the protocol, so micromux turns it, and modifyOtherKeys in tmux, off while the mode lasts, so `Ctrl` chords reach the service as the terminal's own bytes on any keyboard layout. As in any terminal without the protocol, `Alt+O` and `Alt+[` start an escape sequence and `Alt+Esc` arrives as a plain `Esc` there. `Ctrl+]`, which every terminal sends apart, leaves the mode, and the protocol comes back on. Outside PTY input mode one limit remains: on a non-Latin keyboard layout, such as Russian or Greek, the protocol reports `Ctrl` chords as the layout's letter, so `Ctrl-C` and `Ctrl-Z` do not reach micromux as such; switch to a Latin layout for them.

### Using the terminal's own mouse handling

Mouse capture keeps the terminal from seeing the mouse. To use your terminal's own selection or link clicking for a moment, hold its bypass modifier while you click or drag: **Shift** in most terminals, including GNOME Terminal, Konsole, kitty, Alacritty, WezTerm, foot, Ghostty and Windows Terminal, and **Option** in iTerm2. macOS Terminal switches mouse reporting off and on with **View › Allow Mouse Reporting**. Inside tmux with `mouse on`, Shift also bypasses tmux.

Press `m` to turn capture off or on while the TUI runs; the footer shows `Mouse:ON` or `Mouse:OFF`. With capture off, the terminal behaves exactly as without micromux: micromux sends it no mouse sequences, and nothing in the TUI reacts to the mouse.

### Settings

Two settings shape the mouse:

| Setting | Flag and environment variable | Default | Effect |
|---|---|---|---|
| `ui.mouse` | `--mouse on\|off`, `MICROMUX_MOUSE` | on | Capture the mouse. |
| `ui.copy_on_select` | `--copy-on-select on\|off`, `MICROMUX_COPY_ON_SELECT` | off | Copy a selection as soon as the drag ends, without waiting for `y`. |

For each, the first of these that says wins:

1. The flag or its environment variable, for one run. They also apply to `micromux attach`.
2. The `ui` section of the project's `micromux.yaml`.
3. The `ui` section of your [user config](#user-config), for every project.
4. The default.

A project setting wins over your own because it describes something about that project, such as a team that copies a lot of log output with the terminal's selection; your user config is the default for projects that do not care.

micromux releases the mouse, and the kitty keyboard protocol, whenever it lets go of the terminal: when it exits, however it is asked to, when you press `Ctrl-Z`, and when it panics. A stop signal sent from outside, such as `kill -TSTP` or `kill -STOP`, gives micromux no chance to let go first, but when it continues it takes the terminal over afresh and redraws.

### User config

Settings that are about you rather than a project live in your user config, `config.yaml` in micromux's config directory:

| Platform | Path |
|---|---|
| Linux and macOS | `$XDG_CONFIG_HOME/micromux/config.yaml`, by default `~/.config/micromux/config.yaml` |
| Windows | `%APPDATA%\romnn\micromux\config\config.yaml` |

It uses the same keys as a project's `micromux.yaml`, and holds only the mouse settings for now:

```yaml
ui:
  mouse: true
  copy_on_select: false
```

The file is optional. Unknown keys produce a warning, so a file written for a newer micromux still loads.

## Filtering structured logs

For services that emit JSON logs, the log pane can show less than the service emits without losing anything:

- **Level threshold** — `L` opens a picker for the selected service. Structured records below the chosen level are hidden; lines without a recognizable level, such as build output and panics, always show. The initial threshold comes from [`logs.level`]({{< relref "configuration/logs.md#viewing-structured-logs" >}}).
- **Timestamps** — `T` toggles a local-time timestamp at the start of each structured line of the selected service. Records without their own timestamp show when micromux captured them. The initial state comes from `logs.timestamps`.
- **Hidden fields** — `F` shows or hides the fields that `logs.fields` hides for the selected service. The initial state comes from `logs.filter_fields`.

The logs pane's top-right corner names any active filter, such as `level ≥ INFO · 4 fields hidden`. These settings only shape the display: `micromux ctl logs` and the MCP log tools always return every record and field.

{{< figure src="images/structured-logs.png" alt="Structured JSON log lines led by their timestamps, with the pane's top-right corner reading 4 fields hidden" caption="A service's JSON logs with timestamps, minus the source location and span fields its config hides." >}}

## Sending input to a service

Some processes want input — a REPL, a prompt, a dev server waiting on a keypress. Press `a` to enter **input mode**: keystrokes are forwarded to the selected service's PTY until you leave input mode with `Ctrl+]`.

## Disabling on the fly

Press `d` to disable the selected service: micromux stops it and its row turns gray, while its captured logs remain for inspection. The log pane turns gray with it: every color becomes your theme's light or dark gray by its brightness, so a progress bar or a highlighted row keeps its shape while nothing looks live. Press `d` again to re-enable and start it.

Those preserved logs would otherwise look frozen, so micromux marks each transition in the log with a blue rule — `=== service disable requested ===`, `=== service enable requested ===`, `=== waiting for postgres to become healthy ===`, `=== automatic restart scheduled after 250 ms ===`. Whatever the log is doing, the last line says why. The same rules appear in `micromux ctl logs` and in the retained run files; the MCP log tools strip their color along with the rest of the ANSI.

{{< figure src="images/disable.png" alt="A disabled service, stopped with its row grayed out" caption="A disabled service — stopped, grayed out, its logs preserved in gray." >}}

## Attach to a running session

When a session is running **headless** — started by an agent via `micromux serve`, or by `start_session` over [MCP]({{< relref "agent-control/_index.md" >}}) — open the same TUI against it without launching a second supervisor:

```bash
micromux attach                          # the session for the config resolved from the cwd
micromux attach --config ./micromux.yaml
micromux attach --session name:my-project
```

A selector may be a bare session name or `name:`, `pid:`, or `hash:`. More than one attach client can observe and operate the same session at once.

An attached TUI follows service status, logs, and healthchecks, and its lifecycle keys (`r`, `R`, `d`) still restart, enable, and disable services. `a` types into the selected service as it does locally, so an interactive service of a headless session can be driven from an attached TUI.

With several clients on one session, the last active writer wins:

- **Resizes.** Watching never resizes anything: an attached TUI resizes the service PTYs only while it is in PTY input mode, starting with its own size when you press `a`, and following its log pane while the mode lasts. The session's own TUI resizes them whenever its log pane changes size, and takes the size back when you press `a` there. Every PTY takes the size of the most recent resize the session receives. Detaching does not restore an earlier size.
- **Input.** Keystrokes and pastes from every client reach the service in the order the session receives them, with no lock or turn-taking, like two terminals typing into one tmux pane. Each client's input stays in its own order, and one batch of keys or one paste is written whole, never split by another client's. Input typed while an attached TUI shows `reconnecting…` is discarded rather than sent later, and input for a service that is not running is refused with a notice.

An attached paste is limited to 512 KiB, half the local limit, so it fits one control-protocol message. A session older than micromux's protocol 3.18 accepts no input or resizes; `a` is then unavailable.

> [!NOTE]
> Pressing `q` in an attached client only **detaches** it — it never stops the session or its services. To stop a headless session explicitly, use `micromux ctl stop` or the MCP `stop_session` tool.
