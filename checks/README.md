# Headless integration checks

`release-matrix.sh` is the executable release gate. `--source-only` runs the
complete Rust, every upstream-supported individual feature, package, and Nix
matrix. A release candidate must instead pass a
wtype-compatible input driver and a real Sway-compatible compositor, which adds
the layer-shell lifecycle and geometry acceptance below:

```sh
checks/release-matrix.sh wtype -- scroll -c '{config}'
```

The caller may set `CARGO_TARGET_DIR`; build orchestration should put it on a
filesystem with enough capacity rather than relying on a small temporary
filesystem.

The real-compositor gate builds the release binary, validates the upstream
minimal and desktop fixtures in every supported configuration format, renders
both built-in fixtures, and records startup-to-map latency, resident RSS, idle
CPU time, graph samples, and graph redraws in
`$CARGO_TARGET_DIR/cbar-performance-current.json`. Set `PERF_OUT` to choose a
different artifact path. Once a previous release record exists, pass it as
`PERF_BASELINE`; `compare-performance.py` then rejects material regressions
while allowing bounded scheduler and allocator noise. Records include a
fingerprint of the CPU, kernel, architecture, compositor argv, output, fixed
GTK/Cairo and compositor/pixman renderers, and sampling setup; mismatched
environments are not presented as comparable. The first release record is the
baseline, not a fabricated comparison target.
The gate also prints the complete small JSON record, so CI logs retain the
evidence even when no artifact publisher is configured.

`headless-session.sh` starts cbar inside a private, headless Wayland session. It proves two
layer-surface contracts that unit tests cannot observe:

- an internally dismissed resident launcher stays hidden after its next asynchronous preparation;
- the center container remains physically centred when start and end content have deliberately
  asymmetric widths.

The launcher assertion uses its opt-in progress trace plus the ordinary IPC status command. The
layout assertion uses `CBAR_LAYOUT_TRACE=1` to read GTK's final widget allocations, avoiding a
font- and theme-sensitive screenshot comparison. An opt-in graph trace counts sampler and Cairo
work during a six-second IPC-free idle window, long enough to include the five-second capability
cadence. None of these traces is active in an ordinary session.

The harness does not install or pin a compositor. Its input-driver argument must implement wtype's
named-key and sleep options (`-P`, `-p`, and `-s`). Pass an exact compositor argv after `--`; an
argument equal to `{config}` is replaced with the generated Sway-compatible fixture config:

```sh
checks/headless-session.sh target/debug/cbar wtype -- scroll -c '{config}'
```

All launcher inventory, configuration, mutable state, caches, IPC, D-Bus, and Wayland sockets live
under one private temporary directory. `HOME` and inherited compositor socket variables are removed
before startup, so the check cannot read personal configuration or target the caller's graphical
session. The compositor, cbar, and virtual-keyboard drivers each run in an owned session group;
cleanup sends the whole group bounded `TERM` then `KILL`, including in-group wrapper descendants.

`output-cycle.sh` proves that cbar keeps exactly one bar per live output while outputs come and go.
It hosts cbar in a private headless compositor with two outputs and disables and re-enables the
second one through the compositor's own IPC socket, the way a KVM-switching monitor does. After every
step each output must carry exactly one bar (or none while it is disabled), counted twice
independently: by the exclusive zone the bars reserve, and by the pixels of a whole-output
screenshot. A second scenario disables an output at the moment cbar starts building that output's
bar, the window in which the removal event used to be lost and the orphaned bar was placed on a
surviving output. With `CBAR_BAR_TRACE` set, which the check does, cbar also reports how many bars it
still holds, so a closed bar that stays referenced from a module task or receiver loop fails the check
even when every screen looks right.

```sh
checks/output-cycle.sh target/debug/cbar -- scroll -c '{config}'
CYCLES=30 RACES=5 SHOT_DIR=shots checks/output-cycle.sh target/debug/cbar -- scroll -c '{config}'
```

Every compositor call names an explicit IPC socket that was discovered inside the check's private
runtime directory; the caller's graphical session is never addressed. `SHOT_DIR` keeps a whole-output
screenshot of every stage for inspection.

The `mpris` CI selector runs the actual music polling workers on an isolated
D-Bus session with no player. It verifies that repeated bus-name churn leaves no
idle subscriber behind. The test is ignored by ordinary `cargo test` so it never
connects to a developer's desktop bus; run it through the declared selector.
