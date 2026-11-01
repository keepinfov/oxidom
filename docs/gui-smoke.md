# Isolated GUI lifecycle smoke

The Linux `gui-smoke` flake check starts the installed CLI and GUI packages with
a private Xvfb display and session bus. It runs as part of `nix flake check` in
the build workflow. The packages are the same derivations used by the other
checks, so this adds a runtime smoke without building a second copy of them.

To select only this check on an x86-64 Linux CI runner:

```sh
nix build .#checks.x86_64-linux.gui-smoke --print-build-logs
```

The implementation is in [`gui-smoke.py`](../scripts/gui-smoke.py) and
[`gui-smoke.nix`](../nix/checks/gui-smoke.nix). The script consumes built binary
and tool paths; it does not install tools at runtime. Its Python environment
contains PyGObject for private D-Bus calls and python-xlib for X11 observations
and input. Both come from the existing pinned nixpkgs input. GTK runs in the
real GUI process, outside the reducer/unit suite.

## What it asserts

The test starts one explicit session daemon, imports an invented server, starts
a proxy-only session and waits for application confirmation through an offline
fake core. It then launches `oxidom-gui --debug` in the foreground.

A visible window title alone is insufficient readiness: the startup splash has
the same title. The smoke requires a mapped main ApplicationWindow with the
source-defined main-window size, after the splash is gone. This establishes
that startup hydration has been attempted and widget construction completed;
startup can tolerate partial RPC failures, so mapping alone does not establish
that every displayed fact was hydrated successfully.

The smoke dispatches the existing navigation shortcuts through X11, closes the
window and checks that its process remains alive. A second GUI invocation must
exit and reactivate the original window with the same application bus owner and
PID. Quit must terminate that GUI while preserving the original daemon identity,
the pinned live fake-core process and its confirmed session. Explicit CLI Stop
and daemon SIGTERM finish the scenario.

Shortcut dispatch is an event-loop smoke. It does not assert which page rendered,
the text of a status field, layout, focus order, screen-reader output or visual
correctness. Those need additional evidence. GTK accessibility is disabled in
this smoke, so it supplies no AT coverage.

## Isolation and cleanup

The script constructs child environments with private HOME/XDG paths, memory
GSettings and an inaccessible system-bus address. A private session-bus config
contains no activation service directories. External supervision policy forbids
implicit daemon bootstrap. Xvfb allocates its display through `-displayfd`, uses
a private MIT cookie and disables TCP listening. The display and bus announce
readiness through pipes; process and protocol observations use bounded waits.

Configuration explicitly selects the fake `xray_binary`; a missing environment
override would still allow managed-core resolution. The fake answers the pinned
version query and the asset `run -test` check, binds only loopback inbounds and
returns canned SOCKS/HTTP responses without dialing a destination. Desktop proxy,
standby and watchdog are disabled; no TUN or nftables operation is requested.

Pidfds identify child processes. Process groups keep an unreaped leader until
fallback cleanup has signalled the group, and a temporary child subreaper reaps
orphaned fakes. Cleanup attempts all owned children even if closing X11 or D-Bus
fails; diagnostic output is bounded and a cleanup failure retains its private
root. An ordinary scenario failure prints its fixture logs before cleanup.

This check is part of tracker #216 P14. It does not complete the remaining
cross-boundary fault matrix or the P15 disposable-VM packet-path tests, and does
not demonstrate real Xray forwarding, no-leak behavior or continuous protection.
