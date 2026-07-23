#!/usr/bin/env bash
# Regenerate the documentation terminal snippets under docs/assets/terminals/.
#
# Each snippet is real micromux output — the `micromux ctl` client driven against a live headless
# `micromux serve` session for examples/demo, plus `micromux --help` — converted from ANSI to HTML
# with `terminal-to-html` (https://github.com/buildkite/terminal-to-html), which mise provides via
# its go backend. This is the line-oriented counterpart to crates/micromux-screenshot, which
# captures the full truecolor TUI as PNGs with `freeze` (terminal-to-html cannot render 24-bit
# color, so the TUI stays a screenshot while the 16-color CLI output becomes crisp inline HTML).
#
# The snippets are committed, so the Hugo site builds without micromux or terminal-to-html; run this
# only to regenerate them. The `terminal` shortcode inlines each snippet.
#
# Output is captured through a pipe, not a PTY, so colour has to be forced. The demo service logs
# carry their own explicit SGR codes regardless, but forcing colour keeps `ctl` and clap output
# consistent between a local run and CI (which runs steps with TERM=dumb).
set -euo pipefail

export CLICOLOR_FORCE=1 TERM=xterm-256color
unset NO_COLOR

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mm="$repo/target/debug/micromux"
demo="$repo/examples/demo"
out="$repo/docs/assets/terminals"

command -v terminal-to-html >/dev/null || {
  echo "terminal-to-html not found — run 'mise install' (provided via the go backend)" >&2
  exit 1
}
# Debug build to match crates/micromux-screenshot and `task screenshots`; the snippets do not need
# release performance.
[[ -x "$mm" ]] || cargo build -p micromux-cli --manifest-path "$repo/Cargo.toml"
mkdir -p "$out"

# Render captured ANSI (on stdin) to $out/<name>.html, prefixed with a shell prompt line showing the
# command that produced it. Usage: <producer> | render <name> <command label>
render() {
  local name="$1" label="$2"
  {
    printf '\033[1;32m$\033[0m %s\n\n' "$label"
    cat
  } | terminal-to-html >"$out/$name.html"
  echo "wrote $out/$name.html"
}

# Static: the top-level CLI overview.
"$mm" --help 2>&1 | render help "micromux --help"

# The rest dogfood the control plane against a real session. Run everything from the demo dir so
# `serve` discovers examples/demo/micromux.yaml and `ctl` auto-discovers that session's socket.
serve_pid=""
cleanup() {
  ("$mm" ctl stop >/dev/null 2>&1) || true
  [[ -n "$serve_pid" ]] && kill "$serve_pid" >/dev/null 2>&1 || true
}
trap cleanup EXIT

cd "$demo"
"$mm" serve >/dev/null 2>&1 &
serve_pid=$!

# Wait for the control endpoint to answer, then let services print their banners and healthchecks
# resolve (the demo is designed to settle within ~1s: healthchecks probe once with a long interval,
# `migrate` exits, `payments` fails immediately, `checkout` stays pending behind it).
ready=""
for _ in $(seq 1 50); do
  if "$mm" ctl describe >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.2
done
[[ -n "$ready" ]] || {
  echo "micromux session did not become ready" >&2
  exit 1
}
sleep 2

"$mm" ctl ls 2>&1 | render services "micromux ctl ls"
"$mm" ctl health payments 2>&1 | render health "micromux ctl health payments"
"$mm" ctl logs api --tail 8 2>&1 | render logs "micromux ctl logs api --tail 8"
