#!/usr/bin/env bash
# Run the complete source release matrix, optionally followed by the real compositor check.
set -euo pipefail

usage() {
    printf '%s\n' \
        "usage: $0 --source-only" \
        "       $0 <wtype-compatible-input-driver> -- <compositor> [arguments...]" >&2
    exit 2
}

[[ $# -ge 1 ]] || usage

source_only=false
input_driver=
compositor=()
if [[ $1 == --source-only ]]; then
    [[ $# -eq 1 ]] || usage
    source_only=true
else
    [[ $# -ge 3 && $2 == -- ]] || usage
    input_driver=$1
    shift 2
    compositor=("$@")
fi

repo=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$repo"

step() {
    printf 'release-gate=%s\n' "$1"
}

# Sparse feature builds intentionally leave neighbouring module code unused.
# Preserve upstream's narrow exemptions while still rejecting every other
# compiler warning; the all-feature Clippy gate below remains fully strict.
sparse_rustflags='-Dwarnings -A unused-imports -A unused-variables -A unused-mut -A unused-macros -A dead-code'

step format
cargo fmt --all -- --check

step performance-comparator-self-test
python3 checks/compare-performance.py --self-test

step check-all-targets-features
cargo check --workspace --all-targets --all-features --locked

step clippy-all-targets-features
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

step check-no-default-features
RUSTFLAGS="$sparse_rustflags" cargo check --no-default-features --locked

# Keep the upstream-supported single-feature surface executable in this one
# authoritative gate. An all-feature build cannot expose cfg/dependency holes
# which appear only when neighbouring modules are absent.
feature_checks=(
    http
    ipc
    inhibit
    cli
    config+all
    config+json
    config+yaml
    config+toml
    config+corn
    battery
    bindmode+all
    bindmode+sway
    bindmode+hyprland
    bluetooth
    brightness
    cairo
    clipboard
    clock
    custom
    focused
    keyboard+all
    keyboard+sway
    keyboard+hyprland
    label
    launcher
    matrix_launcher
    menu
    music+all
    music+mpris
    music+mpd
    network_manager
    notifications
    sys_info
    script
    system_graph
    tray
    volume
    workspaces+all
    workspaces+sway
    workspaces+hyprland
    workspaces+niri
    extras
)
for feature in "${feature_checks[@]}"; do
    step "check-feature-$feature"
    RUSTFLAGS="$sparse_rustflags" \
        cargo check --no-default-features --features "$feature" --locked
done

step check-matrix-launcher
cargo check --no-default-features --features matrix_launcher --locked

step check-custom-matrix-launcher
cargo check --no-default-features --features custom,matrix_launcher --all-targets --locked

step check-sway-feature-families
cargo check --no-default-features \
    --features 'workspaces+sway,keyboard+sway,bindmode+sway' --locked

step test-default-workspace
cargo test --workspace --all-targets --locked

step test-all-feature-workspace
cargo test --workspace --all-targets --all-features --locked

step doctest-all-features
cargo test --workspace --doc --all-features --locked

step package-launch-service
cargo package --manifest-path launch-service/Cargo.toml --locked

step package-launcher-core
cargo package --manifest-path launcher-core/Cargo.toml --locked

step package-launcher-gtk
cargo package --manifest-path launcher-gtk/Cargo.toml --locked \
    --config "patch.crates-io.cbar-launcher-core.path='$repo/launcher-core'" \
    --config "patch.crates-io.cbar-launch-service.path='$repo/launch-service'"

step package-root
cargo package --locked \
    --config "patch.crates-io.cbar-launcher.path='$repo/launcher-gtk'" \
    --config "patch.crates-io.cbar-launcher-core.path='$repo/launcher-core'" \
    --config "patch.crates-io.cbar-launch-service.path='$repo/launch-service'"

if command -v nix >/dev/null 2>&1; then
    step nix-flake-check
    nix flake check --no-write-lock-file
else
    printf 'release-gate=nix-flake-check unavailable\n' >&2
    exit 2
fi

if $source_only; then
    printf 'release-matrix=SOURCE_ONLY_PASS\n'
    exit 0
fi

step build-release-layer-binary
cargo build --release --all-features --locked --bin cbar

target_dir=${CARGO_TARGET_DIR:-$repo/target}
if [[ $target_dir != /* ]]; then
    target_dir=$repo/$target_dir
fi

step validate-upstream-config-formats
for fixture in minimal desktop; do
    for format in corn json toml yaml; do
        "$target_dir/release/cbar" --validate-config \
            --config "$repo/examples/$fixture/config.$format"
    done
done

performance_out=${PERF_OUT:-$target_dir/cbar-performance-current.json}
if [[ $performance_out != /* ]]; then
    performance_out=$repo/$performance_out
fi

step real-layer-headless
PERF_OUT="$performance_out" \
    checks/headless-session.sh "$target_dir/release/cbar" "$input_driver" -- "${compositor[@]}"

if [[ -n ${PERF_BASELINE:-} ]]; then
    step performance-regression
    python3 checks/compare-performance.py "$PERF_BASELINE" "$performance_out"
fi

printf 'performance-record=%s\n' "$performance_out"
cat -- "$performance_out"

printf 'release-matrix=PASS\n'
