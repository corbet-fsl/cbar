#!/usr/bin/env bash
set -euo pipefail
# Merge stderr into stdout so the captured Crow log shows gate diagnostics.
exec 2>&1
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export GSK_RENDERER="${GSK_RENDERER:-cairo}"

# Both gates below run inside the repo flake dev shell, which supplies the
# native GTK4/dbus/pulse/luajit toolchain from locked flake inputs.
# Worker PATH stays first so the verified Rust toolchain wins over the shell
# cargo; CARGO_TARGET_DIR flows through to the scripts that read it.
# Flake inputs stay frozen via --no-update-lock-file.
export CBAR_CI_WORKER_PATH="$PATH"
exec nix develop --no-update-lock-file --command bash -euo pipefail -c '
  exec 2>&1
  export PATH="$CBAR_CI_WORKER_PATH:$PATH"
  bash checks/release-matrix.sh --source-only
  sccache --show-stats || true
  nix shell --no-update-lock-file --inputs-from . nixpkgs#bash nixpkgs#coreutils nixpkgs#dbus nixpkgs#gnugrep nixpkgs#procps nixpkgs#python3 nixpkgs#util-linux nixpkgs#wtype github:julian-corbet/nixscroll-corbet-ch/d875e6544b93fcea9f03d59cba4afbfebf11fde4#scroll --command checks/release-matrix.sh wtype -- scroll -c '\''{config}'\''
'
