#!/usr/bin/env bash
# A reproducible headless bench for exercising oxidom-gui without a display,
# a window manager, or the user's live daemon. Verified shape: the gui changes
# in the v0.3.0 cycle were checked on exactly this arrangement.
#
# What it does: starts Xvfb and a session D-Bus, builds a throwaway HOME with
# its own XDG dirs, points the daemon at a nonexistent Xray core, and leaves
# you in a shell where `oxidom daemon`, `oxidom-gui` and `busctl --user` all
# talk to the sandbox and never to your real state.
#
# Traps this bench walks around, each learned the hard way:
#   - There is no window manager: `xdotool getactivewindow` fails, and
#     tooltips - all of them, including GTK's own - never show. Anything that
#     must be read on the bench must be a label, not a tooltip. Click by
#     absolute coordinates; capture with `import -window root`.
#   - Screenshots are too slow for 160 ms animations; catch those by logging.
#   - `nohup` does not survive this harness killing its process group - use
#     `setsid` for anything that must outlive a step.
#   - `pkill -f <pattern>` matches the shell that invoked it, killing it. Take
#     pids with `ps -eo pid,args | awk '/[p]attern/{print $1}'` instead.
#   - `gdbus` may be absent - talk to the daemon with
#     `busctl --user --address="$DBUS_SESSION_BUS_ADDRESS"`. The bus name is
#     dev.keepinfov.oxidom.Daemon, the path /dev/keepinfov/oxidom/Daemon.
#   - Probe history lives only in the daemon's memory; restarting the daemon
#     empties the "Recent checks" block until the next check.
#   - The default latency method needs a core; on a coreless bench set
#     {"latency_method":"tcp"} through SetSettings before expecting readings.
set -euo pipefail

DISPLAY_NUMBER="${OXIDOM_BENCH_DISPLAY:-:77}"
BENCH_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/oxidom-bench.XXXXXX")"

XVFB="$(nix build --no-link --print-out-paths nixpkgs#xorg.xvfb)/bin/Xvfb"
# Also handy on the bench, fetched here so the paths are in front of you:
XDOTOOL="$(nix build --no-link --print-out-paths nixpkgs#xdotool)/bin/xdotool"
MAGICK="$(nix build --no-link --print-out-paths nixpkgs#imagemagick)/bin/magick"

"$XVFB" "$DISPLAY_NUMBER" -screen 0 1400x1000x24 -nolisten tcp &
XVFB_PID=$!

# A session bus of the bench's own; --fork keeps it alive across steps.
DBUS_SESSION_BUS_ADDRESS="$(dbus-daemon --session --print-address --fork)"

export HOME="$BENCH_ROOT/home"
export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_RUNTIME_DIR="$BENCH_ROOT/runtime"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME"
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"

export DBUS_SESSION_BUS_ADDRESS
# Point the system bus at nowhere so nothing escapes to the host's daemon.
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$BENCH_ROOT/no-system-bus"
export DISPLAY="$DISPLAY_NUMBER"
export GSK_RENDERER=cairo
export GTK_A11Y=none
# The app must start and say "no core" rather than find the host's Xray.
export OXIDOM_XRAY_BIN=/nonexistent-xray

cleanup() {
    kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$BENCH_ROOT"
}
trap cleanup EXIT

cat <<INFO
oxidom headless bench
  DISPLAY   $DISPLAY
  HOME      $HOME
  bus       $DBUS_SESSION_BUS_ADDRESS
  xdotool   $XDOTOOL
  magick    $MAGICK
Start a daemon with:   cargo run -p oxidom -- daemon --debug &
Start the gui with:    cargo run -p oxidom-gui
Talk to the daemon:    busctl --user --address="\$DBUS_SESSION_BUS_ADDRESS" \\
                         introspect dev.keepinfov.oxidom.Daemon /dev/keepinfov/oxidom/Daemon
This shell owns the sandbox; exiting it tears the bench down.
INFO

"${SHELL:-bash}"
