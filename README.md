# cbar

Cbar is an opinionated GTK4 desktop panel for Wayland, written in Rust. It is
derived from Ironbar and integrates three pieces that benefit from sharing one
resident process:

- a configurable layer-shell bar;
- an offline-first application launcher; and
- native Cairo system and network graphs.

The notification daemon, OSD, lock screen, polkit agent, portals, and
compositor remain separate replaceable processes.

## Compositors

Cbar has typed adapters for Scroll/Sway, niri, Hyprland, and i3-compatible
runtimes. Scroll is detected directly through `SCROLLSOCK`; no IPC proxy or
payload-rewriting service is required. A missing compositor capability disables
only the module that consumes it.

## Configuration

Cbar keeps Ironbar's bar configuration model as a directly maintained source
superset. Existing supported bar configuration can therefore be migrated by
moving it into Cbar's own namespace; Cbar does not install legacy paths or
binary aliases.

The default files are:

```text
~/.config/cbar/config.{corn,json,toml,yaml}
~/.config/cbar/style.css
~/.config/cbar/launcher.json
```

`CBAR_CONFIG`, `CBAR_CSS`, and `CBAR_LAUNCHER_CONFIG` override those locations.
The launcher schema and mutable launcher state are separate fault domains, so a
bad launcher configuration cannot prevent a valid bar from starting.

Open the launcher with:

```sh
cbar launcher show
```

Other control commands are available through `cbar --help`.

## Building

Build the complete feature set with Cargo:

```sh
cargo build --release --all-features --locked
```

Or build the Nix package:

```sh
nix build .#cbar
```

The release gate validates every supported feature combination and then runs a
real headless layer-shell session:

```sh
checks/release-matrix.sh wtype -- sway -c '{config}'
```

See [checks/README.md](checks/README.md) for the measured startup, memory, idle,
and redraw contracts. See [docs/Cbar architecture.md](docs/Cbar%20architecture.md)
for ownership and failure boundaries.

## Upstream and license

Cbar is MIT licensed. The fork started from Ironbar commit
`5b96bcffac54dd82347badcc07f79d58efa715c7`; the integrated launcher came from
nixlaunch commit `8168771811a225448d682113379f91ef1373e7ae`. Exact provenance
and the upstream update policy are recorded in [UPSTREAM.md](UPSTREAM.md) and
[NOTICE](NOTICE).
