# Cbar architecture

Cbar is an opinionated Rust/GTK4 desktop panel derived from Ironbar. It keeps
Ironbar's configuration model and module behaviour as a directly maintained
superset while integrating the launcher and the native system graphs into the
same runtime.

The bar compatibility promise is source-level, not a translation layer:
supported Ironbar bar configuration is parsed by the same configuration types,
and cbar-specific bar modules extend those types. There is no generated
compatibility configuration or parser for retired runtime paths.

The integrated launcher deliberately has a separate, bounded JSON schema and
reload fault domain. An absent or invalid launcher configuration cannot make a
valid bar configuration fall back or prevent the bar from starting. Its default is
`$XDG_CONFIG_HOME/cbar/launcher.json`, overridable with
`$CBAR_LAUNCHER_CONFIG`; neither path is a compatibility alias for nixlaunch.

## Portability invariant

Cbar is a public desktop component for arbitrary Linux systems, not a compiled
copy of one operator's desktop. The public runtime contains no assumptions
about host names, CPU count, machine count, interface names, output models,
hardware vendors, Nix store paths or a particular fleet topology.

Opinionated means that interaction, layout priorities and failure behaviour
have strong defaults. Hardware and deployment remain capability driven. Tokio
keeps its platform-derived scheduler defaults unless cross-machine benchmarks
justify a portable policy; one workstation's thread count is not such a
benchmark.

Nixdesktop and a private `desktop.nix` may supply personal policy, but cbar also
builds and runs without Nix. Public tests exercise absent, minimal and multiple
devices, arbitrary provider names and small through large machine sets.

Cbar's supported-user floor is Ironbar's current supported-user set. Existing
compositors, architectures, distributions, configuration formats, modules and
Cargo feature combinations remain supported. New integrated functionality is
additive and cannot make Nix, remote machines, a particular compositor or
specialised hardware a runtime requirement.

## First release boundary

The first release owns:

- the GTK layer-shell panel;
- the application launcher;
- native system and network graphs;
- typed compositor integration, including Scroll-compatible workspace events;
  and
- a local control socket for reveal, hide and other panel actions.

The compositor, notification daemon, OSD and lock screen remain separate
processes. Their integration is expressed through narrow providers so they can
be replaced independently. In particular, replacing SwayNC is a future clean
cut after notification-spec parity; cbar must not run a partial notification
daemon alongside it.

## Runtime model

GTK widgets and their mutable presentation state live on the GTK main thread.
Blocking file, process, compositor and network work never runs there. Async
providers publish bounded state updates; presentation code consumes the newest
complete update and is allowed to discard obsolete intermediate samples.

Provider failures are local. A failed graph source, remote machine or
compositor subscription must not terminate the panel or stall unrelated
providers. Tasks are supervised and restarted with bounded exponential backoff.

On Linux with an available user service manager, the launcher hands applications
to collected transient services. Those applications do not inherit cbar's
lifetime or service sandbox, so restarting or hardening cbar does not terminate
or accidentally restrict them.

The portable backend used when no service manager is available owns direct
children in one bounded reaper. They survive an ordinary parent exit on POSIX,
but process APIs cannot move them out of a cgroup or sandbox already inherited
from cbar. Packaged Linux sessions therefore treat the transient-service backend
as the isolation contract; the direct backend is an explicit portability path,
not an equivalent sandbox boundary.

## Offline-first launcher

Opening, searching and launching local applications never waits for a network
operation. The first frame uses local inventory plus independently stored
last-known-good inventories for remote machines.

Each remote machine is a separate asynchronous provider with its own:

- timeout;
- connection and refresh task;
- last-known-good inventory;
- failure count and backoff deadline; and
- recovery lifecycle.

Results are opportunistic. A response from one machine is merged as soon as it
arrives. A slow or unreachable machine does not delay local results or any other
machine and does not produce a global spinner, modal error or blocking empty
state. Fresh results must not reset the query, keyboard focus, selected item or
scroll position. Stable keys and ordering prevent rows from jumping while the
user is navigating.

The provider contract is tested with a controllable clock and fake transports:
one machine times out while another succeeds, the launcher remains immediately
usable, cached data remains available, and the failed machine can recover on a
later attempt.

## Native graphs

System graphs use one Rust sampler and one GTK4/Cairo drawing surface. They do
not spawn a shell or enter Lua for every source and frame. Sampling cadence and
drawing cadence are independent: an unchanged or hidden graph does not force a
new sample or redraw.

Bar placement follows the information's time semantics:

- navigation and actions belong at the start;
- runtime quantities whose history is the useful information belong in the
  centre; and
- snapshot state that needs only a current value belongs at the end.

The system-history canvas is therefore centred as one responsive cluster.
Surplus allocation is split symmetrically around the fitted cluster rather than
left aligned inside the centre zone. A missing capability consumes no cell,
divider or residual width.

Graph priority is:

1. CPU
2. RAM
3. swap
4. storage I/O
5. VPU media encode/decode
6. GPU
7. NPU
8. primary wired network
9. WLAN
10. WWAN
11. WireGuard/Netbird VPN

Only available sources are shown. The layout first compresses history within a
readable lower bound and only then removes the lowest-priority visible graph.
It restores graphs in priority order when space returns. Multiple interfaces in
one network class rotate without attention-grabbing motion; totals and tooltips
identify the active interface.

RAM displays reclaimable headroom rather than treating the kernel's page cache
as irreversibly occupied. Network graphs preserve direction and rate units.
Hardware-specific probes are capability driven and remain dormant on machines
without that hardware.

## Compositor integration

Cbar is compositor neutral. Compositor-specific behaviour stays behind the
existing adapter boundary, and generic UI, launcher and graph code does not
invoke compositor-specific commands. Sway/Scroll, Hyprland and Niri support
must not regress; a missing adapter disables only the capability it provides.
GTK/GDK and standard Wayland protocols are preferred where they express the
required operation.

Each compositor stays behind the common typed adapter. For Sway/i3 protocol
backends, cbar owns a small, self-contained implementation of the bounded wire
framing. It deserializes only the workspace, input and mode fields the panel
consumes; unrelated recursive node fields and layout spellings are ignored, so
compatible compositors can extend their event payloads without breaking the
panel. Cbar does not ship an IPC proxy, rewrite raw protocol messages or depend
on a private compositor-protocol patch.

Favourite workspaces are native cbar state: a missing favourite is addressed
using the compositor's native name or numeric-index semantics, and an empty
workspace event cannot silently remove its button. These behaviours have
protocol-fixture tests and require no live compositor connection.

## Security boundary

The local control socket is created with user-only permissions, verifies the
peer user, and is disabled unless `XDG_RUNTIME_DIR` is a real, user-owned
directory with no group or other access. Persistent launcher state is atomic,
mode `0600`, size bounded and opened without following symlinks. Desktop
entries become argument vectors; they are never interpolated into a shell
command.

Ironbar's Lua, Cairo and script extension points remain available in the
upstream-compatible build. Runtime hardening must preserve them rather than
silently narrowing the supported user set. Hardening is applied to cbar itself,
not inherited by launched applications.

## Configuration ownership

Cbar owns executable UI mechanisms. Nixdesktop owns reusable Home Manager and
systemd policy. A machine's private `desktop.nix` supplies values and hardware
capabilities. Network, audio, GPU, remote-host and compositor modules remain the
authorities for their respective data; cbar consumes typed state and actions
instead of duplicating policy.

## Release gates

A release is blocked unless:

- upstream Ironbar configuration fixtures still parse and render;
- Scroll layout and favourite-workspace regressions pass without a live session;
- per-machine launcher isolation, offline startup and recovery tests pass;
- graph parsers pass against injected procfs/sysfs fixtures;
- synthetic fixtures cover no accelerators, multiple vendors and devices,
  arbitrary network names and different CPU topologies;
- narrow and wide layout snapshots preserve priority and readability;
- IPC and state-file permission tests pass; and
- idle CPU, resident memory, startup latency and redraw counts are recorded
  against the previous release.
