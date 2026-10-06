#!/usr/bin/env bash
# Shared-first toolchain lock for Crow cached runners.
#
# Protocol: SHARED first. Probe readiness under shared; take EXCLUSIVE only
# when something is actually missing; recheck after upgrade; provision;
# release exclusive, re-acquire shared, re-verify. Same lock file as the old
# whole-job-exclusive runners, so old jobs interoperate (they serialize).
#
# Hard rules encoded here:
# - NEVER request exclusive while holding shared (flock upgrade deadlock).
#   Shared is always released before the exclusive request.
# - NEVER take exclusive unconditionally and "downgrade" afterwards; that
#   keeps the 10-40 min serialization this exists to remove.
# - Admission gate (fd 7 on $TOOLCHAIN_LOCK_FILE.gate) prevents writer
#   starvation: it is taken before main-shared and held ONLY until readiness
#   is decided (warm: released, main shared kept) or provisioning finishes.
#   A waiting writer holds it, so later readers queue at the gate instead of
#   piling fresh shared holds onto main. The gate is NEVER held during
#   product checks or helper cargo installs. Global lock order is always
#   gate -> main; main is never touched while the gate is requested.
# - On ANY failure the lock is released and existing toolchains are left
#   intact: there is deliberately NO cleanup of toolchain state.
# - errexit is suppressed inside functions called as `if func` conditions,
#   so EVERY mutating command carries an explicit `|| return 1` and every
#   probe chain short-circuits with `|| return 1`. A bare failing command
#   must never be followed by a success that masks it.
#
# Caller (run-cached.sh) sets before sourcing/calling:
#   TOOLCHAIN_LOCK_FILE      full path to $toolchains/install.lock (required)
#   TOOLCHAIN_LOCK_TIMEOUT   flock wait seconds (default 2700, mirrors CI_TIMEOUT)
#   TOOLCHAIN_WANT_NIGHTLY   1/0 (default 1)
#   TOOLCHAIN_WASM_TARGETS   space-separated "toolchain:target" pairs (default empty)
#   CARGO_HOME, shared_cargo_home (as today), PATH with proto/rustup shims.
# Provides: toolchain_ensure (returns holding SHARED on fd 8),
#           toolchain_release, toolchain_ready, repo_install_exact.
# NOTE: no test doubles in this file. Tests stub rustup/proto/cargo on PATH.
#
# Sourcing is side-effect free (no required-var checks here); toolchain_ensure
# validates. Pipe discipline: NEVER `producer | grep -q` directly — under
# `set -o pipefail` (all runners) grep -q's early exit SIGPIPEs the producer
# and the pipeline spuriously fails. Capture first, then grep the variable.

: "${TOOLCHAIN_LOCK_TIMEOUT:=2700}"
_TOOLCHAIN_MAX_ROUNDS="${_TOOLCHAIN_MAX_ROUNDS:-3}"
TOOLCHAIN_WANT_NIGHTLY="${TOOLCHAIN_WANT_NIGHTLY:-1}"
TOOLCHAIN_WASM_TARGETS="${TOOLCHAIN_WASM_TARGETS:-}"

_tc_rustup() { "$CARGO_HOME/bin/rustup" "$@"; }

# rustup refuses installs/updates when $HOME differs from the euid home.
# Run each install/update with HOME set to the euid home for that command
# only; both roots stay explicit and absolute, parent env is unchanged, and
# a failed lookup fails closed instead of using the mismatched HOME.
_tc_euid_home() {
  local h
  h=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6) || { echo 'toolchain-lock: cannot resolve euid home (getent failed)' >&2; return 1; }
  [[ -n $h ]] || { echo 'toolchain-lock: cannot resolve euid home (empty entry)' >&2; return 1; }
  printf '%s' "$h"
}

_tc_run_install() { # <cmd...>: run one mutating install/update with euid HOME
  [[ ${CARGO_HOME:-} = /* ]] || { echo 'toolchain-lock: CARGO_HOME must be absolute' >&2; return 1; }
  [[ ${RUSTUP_HOME:-} = /* ]] || { echo 'toolchain-lock: RUSTUP_HOME must be absolute' >&2; return 1; }
  local eh
  eh=$(_tc_euid_home) || return 1
  if [[ $eh != "${HOME:-}" ]]; then
    HOME="$eh" "$@" || return 1
  else
    "$@" || return 1
  fi
  return 0
}

# --- readiness probes (read-only; safe under shared) ---
tc_stable_ready() {
  _tc_rustup run stable rustc --version >/dev/null 2>&1 || return 1
  local comps
  comps=$(_tc_rustup component list --toolchain stable --installed 2>/dev/null) || return 1
  grep -q '^rustfmt' <<<"$comps" || return 1
  grep -q '^clippy' <<<"$comps" || return 1
  return 0
}

tc_nightly_ready() {
  _tc_rustup run nightly rustc --version >/dev/null 2>&1 || return 1
  local comps
  comps=$(_tc_rustup component list --toolchain nightly --installed 2>/dev/null) || return 1
  _tc_has_llvm_tools "$comps" || return 1
  return 0
}

# Actual Crow output is `llvm-tools-<target>` (e.g.
# llvm-tools-x86_64-unknown-linux-gnu); older toolchains report
# `llvm-tools-preview[-<target>]`. Accept exactly those two shapes — a loose
# `^llvm-tools` prefix would bless arbitrary component names, and the old
# `^llvm-tools-preview` probe fails on current Crow even after a good install.
# Bare `llvm-tools` alone is rejected: installed components always carry a
# target suffix (or the legacy preview name).
_tc_has_llvm_tools() { # <component-list>
  grep -Eq '^llvm-tools(-preview(-[a-z][a-z0-9_]*(-[a-z0-9_]+){2,})?|-[a-z][a-z0-9_]*(-[a-z0-9_]+){2,})( |$)' <<<"$1"
}

tc_target_ready() { # <toolchain> <target>
  local tgts
  tgts=$(_tc_rustup target list --toolchain "$1" --installed 2>/dev/null) || return 1
  grep -q "^$2" <<<"$tgts" || return 1
  return 0
}

# Registry link state is "ready" unless a linkable source exists and the link
# is missing (that creation requires exclusive).
tc_registry_ready() {
  if [[ -e $CARGO_HOME/registry ]]; then
    return 0
  fi
  if [[ -n ${shared_cargo_home:-} && -d $shared_cargo_home/registry ]]; then
    return 1
  fi
  return 0
}

toolchain_ready() {
  tc_stable_ready || return 1
  if [[ $TOOLCHAIN_WANT_NIGHTLY == 1 ]]; then
    tc_nightly_ready || return 1
  fi
  local pair tc tgt
  for pair in $TOOLCHAIN_WASM_TARGETS; do
    tc=${pair%%:*}
    tgt=${pair#*:}
    tc_target_ready "$tc" "$tgt" || return 1
  done
  tc_registry_ready || return 1
  return 0
}

# --- provisioning (exclusive only). Current per-repo provision blocks, each
# mutation with explicit failure propagation; never replaces or reinstalls a
# working stable toolchain (guarded component adds only).
toolchain_install() {
  local stable_comps
  mkdir -p "$CARGO_HOME" || return 1
  if [[ ! -e $CARGO_HOME/registry && -n ${shared_cargo_home:-} && -d $shared_cargo_home/registry ]]; then
    ln -s "$shared_cargo_home/registry" "$CARGO_HOME/registry" || return 1
  fi
  # Repeated `proto install rust stable` can replace an installed toolchain.
  # Reuse a working provisioned stable compiler; explicit upgrades are separate.
  # Component adds run UNCONDITIONALLY after (not in an else): a fresh proto
  # install may provide a bare compiler without rustfmt/clippy, and
  # tc_stable_ready demands both. Never a reinstall — adds only.
  if ! _tc_rustup run stable rustc --version >/dev/null 2>&1; then
    _tc_run_install proto install rust stable || return 1
  fi
  stable_comps=$(_tc_rustup component list --toolchain stable --installed 2>/dev/null) || return 1
  if ! grep -q '^rustfmt' <<<"$stable_comps"; then
    _tc_run_install "$CARGO_HOME/bin/rustup" component add rustfmt --toolchain stable || return 1
  fi
  if ! grep -q '^clippy' <<<"$stable_comps"; then
    _tc_run_install "$CARGO_HOME/bin/rustup" component add clippy --toolchain stable || return 1
  fi
  if [[ $TOOLCHAIN_WANT_NIGHTLY == 1 ]]; then
    if ! _tc_rustup run nightly rustc --version >/dev/null 2>&1; then
      _tc_run_install "$CARGO_HOME/bin/rustup" toolchain install nightly --component llvm-tools-preview || return 1
    else
      local comps
      comps=$(_tc_rustup component list --toolchain nightly --installed 2>/dev/null) || return 1
      if ! _tc_has_llvm_tools "$comps"; then
        # Alias accepted by rustup; installs the component list reports as
        # llvm-tools-<target> on current toolchains.
        _tc_run_install "$CARGO_HOME/bin/rustup" component add llvm-tools-preview --toolchain nightly || return 1
      fi
    fi
  fi
  local pair tc tgt tgts
  for pair in $TOOLCHAIN_WASM_TARGETS; do
    tc=${pair%%:*}
    tgt=${pair#*:}
    tgts=$(_tc_rustup target list --toolchain "$tc" --installed 2>/dev/null) || return 1
    if ! grep -q "^$tgt" <<<"$tgts"; then
      _tc_run_install "$CARGO_HOME/bin/rustup" target add "$tgt" --toolchain "$tc" || return 1
    fi
  done
  return 0
}

toolchain_ensure() {
  : "${TOOLCHAIN_LOCK_FILE:?TOOLCHAIN_LOCK_FILE must be set to \$toolchains/install.lock}"
  local round=0
  exec 7>"$TOOLCHAIN_LOCK_FILE.gate" || return 1
  exec 8>"$TOOLCHAIN_LOCK_FILE" || { exec 7>&- 2>/dev/null || true; return 1; }
  # Admission gate first (exclusive, normally uncontended). A writer that
  # finds state missing keeps it while taking main exclusive, so readers
  # arriving later queue here instead of starving the writer.
  flock -x -w "$TOOLCHAIN_LOCK_TIMEOUT" 7 || { exec 7>&- 8>&- 2>/dev/null || true; return 1; }
  _TOOLCHAIN_GATE_HELD=1
  while (( round < _TOOLCHAIN_MAX_ROUNDS )); do
    round=$((round + 1))
    flock -s -w "$TOOLCHAIN_LOCK_TIMEOUT" 8 || { _toolchain_ensure_fail; return 1; }
    if toolchain_ready; then
      _toolchain_gate_release # warm: gate goes, main SHARED stays for the job
      return 0
    fi
    # Missing state: KEEP the gate, drop main shared (never upgrade), then
    # take main exclusive. New readers now queue at the gate.
    flock -u 8 || { _toolchain_ensure_fail; return 1; }
    flock -x -w "$TOOLCHAIN_LOCK_TIMEOUT" 8 || { _toolchain_ensure_fail; return 1; }
    if toolchain_ready; then
      : # peer provisioned while we waited; fall through to downgrade
    else
      if toolchain_install; then
        if ! toolchain_ready; then
          _toolchain_ensure_fail # recheck failed: release all, keep toolchains intact, fail loudly
          return 1
        fi
      else
        _toolchain_ensure_fail # install failed: release all, keep toolchains intact, fail loudly
        return 1
      fi
    fi
    flock -u 8 || { _toolchain_ensure_fail; return 1; } # downgrade; loop re-acquires shared + reverifies
  done
  _toolchain_ensure_fail
  return 1
}

# Failure exit: unlock main if held, release the gate exactly once, and close
# both descriptors, so no failure path leaks a held lock or an open fd.
_toolchain_ensure_fail() {
  flock -u 8 2>/dev/null || true
  _toolchain_gate_release
  exec 8>&- 2>/dev/null || true
}

# Gate release is flag-guarded: exactly once per toolchain_ensure, so a
# defensive call can never close an fd 7 owned by someone else.
_TOOLCHAIN_GATE_HELD=0
_toolchain_gate_release() {
  if (( _TOOLCHAIN_GATE_HELD )); then
    _TOOLCHAIN_GATE_HELD=0
    flock -u 7 2>/dev/null || true
    exec 7>&- 2>/dev/null || true
  fi
}

toolchain_release() {
  _toolchain_gate_release
  flock -u 8 2>/dev/null || true
  exec 8>&- 2>/dev/null || true
}

# Per-repo cargo-installed helper (runs under the per-repo fd 9 lock, AFTER
# toolchain_ensure returns shared so `cargo` resolves). Package and binary
# names differ (e.g. package wasm-bindgen-cli, binary wasm-bindgen): the
# guard probes the exact binary path, the install names the package.
# Requires CARGO_INSTALL_ROOT set by the caller (a per-repo $state subdir).
# Equality is exact (`<binary> <version>`); substring matching would accept
# e.g. 0.2.1000 for a 0.2.100 pin. Output is captured, never piped (pipefail).
repo_install_exact() { # <binary> <package> <version>
  local binary="$1" package="$2" version="$3"
  : "${CARGO_INSTALL_ROOT:?CARGO_INSTALL_ROOT must be a per-repo install root}"
  local have
  have=$("$CARGO_INSTALL_ROOT/bin/$binary" --version 2>/dev/null || true)
  if [[ $have == "$binary $version" ]]; then
    return 0
  fi
  cargo install --locked --version "$version" --root "$CARGO_INSTALL_ROOT" "$package" || return 1
  have=$("$CARGO_INSTALL_ROOT/bin/$binary" --version 2>/dev/null || true)
  [[ $have == "$binary $version" ]] || return 1
  return 0
}
