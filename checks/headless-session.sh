#!/usr/bin/env bash
# Exercise cbar's layer surfaces against a real headless Wayland compositor.
#
# The compositor command is an argv, not a shell fragment. An exact `{config}` argument is
# replaced with the generated Sway-compatible fixture path. With no compositor arguments, the
# harness supplies `-c {config}`. This keeps the check usable with Scroll, Sway, and wrappers
# around either without making one compositor a package or repository dependency.
#
# Usage:
#   checks/headless-session.sh <cbar> <wtype-compatible-input-driver> -- <compositor> [arguments...]
# Example:
#   checks/headless-session.sh target/debug/ironbar wtype -- scroll -c '{config}'
set -euo pipefail

usage() {
    printf '%s\n' \
        "usage: $0 <cbar> <input-driver> -- <compositor> [arguments...]" \
        "example: $0 target/debug/ironbar wtype -- scroll -c '{config}'" >&2
    exit 2
}

[[ $# -ge 4 ]] || usage
CBAR=$1
INPUT_DRIVER=$2
[[ $3 == -- ]] || usage
shift 3
COMPOSITOR_ARGV=("$@")

command_exists() {
    if [[ $1 == */* ]]; then
        [[ -x $1 ]]
    else
        command -v "$1" >/dev/null 2>&1
    fi
}

command_exists "$CBAR" || { printf 'no cbar binary: %s\n' "$CBAR" >&2; exit 2; }
command_exists "$INPUT_DRIVER" || {
    printf 'no Wayland input driver: %s\n' "$INPUT_DRIVER" >&2
    exit 2
}
command_exists "${COMPOSITOR_ARGV[0]}" || {
    printf 'no compositor: %s\n' "${COMPOSITOR_ARGV[0]}" >&2
    exit 2
}
for dependency in bash dbus-run-session ps python3 setsid; do
    command_exists "$dependency" || { printf 'missing dependency: %s\n' "$dependency" >&2; exit 2; }
done

# Keep the runtime path short: Unix-domain sockets have a small fixed path limit. The directory is
# private because cbar deliberately refuses an IPC socket under a group/world-accessible runtime.
RIG=$(mktemp -d /tmp/cbar-headless.XXXXXX)
chmod 700 "$RIG"
cleanup() {
    if [[ ${KEEP_RIG:-0} == 1 ]]; then
        printf 'kept headless fixture: %s\n' "$RIG" >&2
        return
    fi
    case $RIG in
        /tmp/cbar-headless.*) rm -rf -- "$RIG" ;;
        *) printf 'refusing to remove unexpected fixture path: %s\n' "$RIG" >&2 ;;
    esac
}
trap cleanup EXIT

python3 - "$RIG" <<'PY'
import json
import pathlib
import sys

rig = pathlib.Path(sys.argv[1])
for directory in ("config", "state", "cache"):
    (rig / directory).mkdir(mode=0o700)

(rig / "bar.toml").write_text(
    """\
name = "headless-center"
position = "top"
height = 32
anchor_to_edges = true
exclusive_zone = false

[[start]]
type = "label"
label = "asymmetric navigation fixture"

[[center]]
type = "system_graph"

[[end]]
type = "label"
label = "7"
""",
    encoding="utf-8",
)

(rig / "bar.css").write_text(
    """\
#bar { background-color: #050505; }
#start { min-width: 500px; }
#center { min-width: 260px; }
#end { min-width: 80px; }
""",
    encoding="utf-8",
)

inventory = {
    "host": "fixture",
    "error": None,
    "folders": [
        {
            "label": "Terminals",
            "apps": [
                {
                    "name": "Fixture",
                    "id": "fixture.desktop",
                    "icon": "",
                    "exec": "true",
                    "terminal": False,
                }
            ],
        }
    ],
}
inventory_json = json.dumps(inventory, separators=(",", ":"))
inventory_code = f"import sys;sys.stdout.write({inventory_json!r})"
launcher = {
    "surface": "layer",
    "keyboard": "exclusive",
    "exit_on_focus_loss": False,
    "folders": ["Terminals"],
    "subrows": {},
    "keys": {},
    "terminal": ["true"],
    "outputs": [],
    "layout": {"equal_columns": False},
    "machines": [
        {
            "name": "fixture",
            "aliases": [],
            "accent": "#22C55E",
            "inventory": [sys.executable, "-c", inventory_code],
            "inventory_timeout_ms": 5000,
            "launch": [],
        }
    ],
}
(rig / "launcher.json").write_text(
    json.dumps(launcher, separators=(",", ":")), encoding="utf-8"
)

(rig / "compositor.conf").write_text(
    "output HEADLESS-1 mode 1920x1080\n", encoding="utf-8"
)
PY

if [[ ${#COMPOSITOR_ARGV[@]} -eq 1 ]]; then
    COMPOSITOR_ARGV+=("-c" "$RIG/compositor.conf")
else
    for index in "${!COMPOSITOR_ARGV[@]}"; do
        if [[ ${COMPOSITOR_ARGV[$index]} == '{config}' ]]; then
            COMPOSITOR_ARGV[$index]=$RIG/compositor.conf
        fi
    done
fi

DBUS=(dbus-run-session)
if [[ -n ${DBUS_SESSION_CONF:-} ]]; then
    DBUS+=(--config-file="$DBUS_SESSION_CONF")
fi

if ! "${DBUS[@]}" -- bash -s -- \
    "$RIG" "$CBAR" "$INPUT_DRIVER" "${COMPOSITOR_ARGV[@]}" \
    >"$RIG/session.log" 2>&1 <<'SESSION'
set -euo pipefail

rig=$1
cbar=$2
input_driver=$3
shift 3
compositor=("$@")
bar_pid=
compositor_pid=
input_keeper_pid=
input_injector_pid=

process_group_alive() {
    local group=${1:-}
    [[ $group =~ ^[1-9][0-9]*$ ]] && kill -0 -- "-$group" 2>/dev/null
}

assert_session_leader() {
    local pid=$1
    local label=$2
    local group=
    local session=
    for _ in $(seq 1 40); do
        group=
        session=
        read -r group session < <(ps -o pgid= -o sid= -p "$pid") || true
        [[ $pid == "$group" && $pid == "$session" ]] && return 0
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.01
    done
    fail "$label was not an owned session leader (pid=$pid pgid=${group:-none} sid=${session:-none})"
}

reap_if_exited() {
    local pid=${1:-}
    [[ $pid =~ ^[1-9][0-9]*$ ]] || return 0
    # `wait` is used only after the kernel says the direct child no longer exists, so it cannot
    # become an unbounded cleanup wait. Descendants are handled through the process group below.
    if ! kill -0 "$pid" 2>/dev/null; then
        wait "$pid" 2>/dev/null || true
    fi
}

stop_process_group() {
    local group=${1:-}
    [[ $group =~ ^[1-9][0-9]*$ ]] || return 0
    if ! process_group_alive "$group"; then
        reap_if_exited "$group"
        return 0
    fi

    kill -TERM -- "-$group" 2>/dev/null || true
    for _ in $(seq 1 40); do
        reap_if_exited "$group"
        process_group_alive "$group" || return 0
        sleep 0.05
    done

    kill -KILL -- "-$group" 2>/dev/null || true
    for _ in $(seq 1 40); do
        reap_if_exited "$group"
        process_group_alive "$group" || return 0
        sleep 0.05
    done
    printf 'cleanup could not stop process group %s\n' "$group" >&2
    return 1
}

wait_for_process() {
    local pid=$1
    local label=$2
    local status
    for _ in $(seq 1 100); do
        if ! kill -0 "$pid" 2>/dev/null; then
            # The failed liveness probe proves this wait is non-blocking.
            if wait "$pid"; then
                return 0
            else
                status=$?
                return "$status"
            fi
        fi
        sleep 0.05
    done
    stop_process_group "$pid" || true
    fail "$label timed out"
}

cleanup_session() {
    local session_status=$?
    local cleanup_status=0
    trap - EXIT
    set +e
    stop_process_group "$input_injector_pid" || cleanup_status=1
    stop_process_group "$input_keeper_pid" || cleanup_status=1
    stop_process_group "$bar_pid" || cleanup_status=1
    stop_process_group "$compositor_pid" || cleanup_status=1
    if (( session_status != 0 )); then
        exit "$session_status"
    fi
    exit "$cleanup_status"
}
trap cleanup_session EXIT

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    return 1
}

export XDG_RUNTIME_DIR=$rig
export XDG_CONFIG_HOME=$rig/config
export XDG_STATE_HOME=$rig/state
export XDG_CACHE_HOME=$rig/cache
export GSETTINGS_BACKEND=memory
export GDK_BACKEND=wayland
export GIO_USE_VFS=local
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman
export WLR_HEADLESS_OUTPUTS=1
export WLR_LIBINPUT_NO_DEVICES=1
unset HOME WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK SCROLLSOCK I3SOCK NIRI_SOCKET \
    HYPRLAND_INSTANCE_SIGNATURE DISPLAY CBAR_LAUNCHER_NO_LAYER

setsid -- "${compositor[@]}" >"$rig/compositor.log" 2>&1 &
compositor_pid=$!
assert_session_leader "$compositor_pid" "compositor"

wayland_display=
for _ in $(seq 1 150); do
    for socket in "$rig"/wayland-*; do
        if [[ -S $socket ]]; then
            wayland_display=${socket##*/}
            break 2
        fi
    done
    process_group_alive "$compositor_pid" || fail "compositor exited before creating a Wayland socket"
    sleep 0.1
done
[[ -n $wayland_display ]] || fail "compositor never created a Wayland socket"
export WAYLAND_DISPLAY=$wayland_display

# A headless seat starts without a physical keyboard. Keep one neutral virtual keyboard object
# alive for the session so the compositor establishes keyboard focus before the layer surface maps;
# a one-shot injector created only after map can disappear before that seat transition is applied.
setsid -- "$input_driver" -P Shift_L -p Shift_L -s 180000 >"$rig/input-keeper.log" 2>&1 &
input_keeper_pid=$!
assert_session_leader "$input_keeper_pid" "input keeper"
sleep 0.1
process_group_alive "$input_keeper_pid" || fail "input driver could not keep a virtual keyboard alive"

CBAR_LAYOUT_TRACE=1 \
CBAR_LAUNCHER_TRACE=1 \
CBAR_LAUNCHER_CONFIG="$rig/launcher.json" \
CBAR_LAUNCHER_CACHE="$rig/cache/cbar/launcher/inventory" \
IRONBAR_CONFIG="$rig/bar.toml" \
IRONBAR_CSS="$rig/bar.css" \
setsid -- "$cbar" >"$rig/cbar.log" 2>&1 &
bar_pid=$!
assert_session_leader "$bar_pid" "cbar"

for _ in $(seq 1 150); do
    [[ -S $rig/ironbar-ipc.sock ]] && break
    process_group_alive "$bar_pid" || fail "cbar exited before creating its IPC socket"
    sleep 0.1
done
[[ -S $rig/ironbar-ipc.sock ]] || fail "cbar never created its IPC socket"

launcher_status() {
    "$cbar" launcher status 2>/dev/null
}

wait_for_status() {
    local expected=$1
    local label=$2
    local value=
    for _ in $(seq 1 100); do
        value=$(launcher_status || true)
        [[ $value == "$expected" ]] && return 0
        process_group_alive "$bar_pid" || fail "cbar exited while waiting for $label"
        sleep 0.1
    done
    fail "$label: expected launcher status $expected, got ${value:-<none>}"
}

count_launcher_maps() {
    grep -c 'cbar-launcher-trace map$' "$rig/cbar.log" 2>/dev/null || true
}

count_preparation_finishes() {
    grep -c 'cbar-launcher-owner-trace prepare-finish ' "$rig/cbar.log" 2>/dev/null || true
}

count_cancel_keys() {
    grep -c 'cbar-launcher-trace cancel-key$' "$rig/cbar.log" 2>/dev/null || true
}

count_layer_shell_selections() {
    grep -c 'cbar-launcher-trace surface=layer-shell ' "$rig/cbar.log" 2>/dev/null || true
}

wait_for_counter_above() {
    local counter=$1
    local before=$2
    local label=$3
    local current=0
    for _ in $(seq 1 100); do
        current=$($counter)
        (( current > before )) && return 0
        process_group_alive "$bar_pid" || fail "cbar exited while waiting for $label"
        sleep 0.1
    done
    fail "$label did not complete (counter stayed at $current)"
}

require_resident_quiet_window() {
    local expected_maps=$1
    local label=$2
    local status=
    local maps=0
    # One full second is comfortably longer than the headless map/configure/present round trip.
    # Poll throughout it: a single sleep followed by one observation could miss a transient reopen.
    for _ in $(seq 1 20); do
        sleep 0.05
        status=$(launcher_status || true)
        maps=$(count_launcher_maps)
        [[ $status == resident ]] ||
            fail "$label: launcher left resident state (status=${status:-none})"
        (( maps == expected_maps )) ||
            fail "$label: launcher map count changed from $expected_maps to $maps"
        process_group_alive "$bar_pid" || fail "cbar exited during $label"
    done
    printf 'launcher quiet_ms=1000 maps=%s status=resident\n' "$expected_maps"
}

# Warmup must build one hidden resident UI before the race test starts. This makes the first show a
# true resident map and lets the refresh below exercise replacement/visibility ownership directly.
wait_for_status resident "launcher warmup"
wait_for_counter_above count_layer_shell_selections 0 "layer-shell selection"

prepare_before=$(count_preparation_finishes)
"$cbar" launcher show >/dev/null
wait_for_status visible "resident show"
wait_for_counter_above count_preparation_finishes "$prepare_before" "show preparation"
wait_for_counter_above count_launcher_maps 0 "first launcher map"
first_maps=$(count_launcher_maps)
# Layer-shell focus is granted after the map/configure round trip, not when IPC first reports the
# GTK visibility flag. Give that protocol transition one bounded beat before injecting Escape.
sleep 0.5

# Escape is handled inside the launcher window. It must also update the outer resident owner's
# desired state; otherwise the next completed async preparation maps the window behind the user's
# back. Waiting for a traced preparation completion makes this a deterministic race assertion.
cancel_before=$(count_cancel_keys)
process_group_alive "$input_keeper_pid" || fail "virtual keyboard keeper exited before Escape"
setsid -- "$input_driver" -P Escape -s 50 -p Escape >"$rig/input-injector.log" 2>&1 &
input_injector_pid=$!
assert_session_leader "$input_injector_pid" "input injector"
if wait_for_process "$input_injector_pid" "Escape input"; then
    input_status=0
else
    input_status=$?
fi
stop_process_group "$input_injector_pid" || fail "Escape input leaked process-group members"
input_injector_pid=
(( input_status == 0 )) || fail "Escape input exited with status $input_status"
wait_for_counter_above count_cancel_keys "$cancel_before" "Escape delivery"
wait_for_status resident "Escape dismissal"
prepare_before=$(count_preparation_finishes)
"$cbar" launcher refresh >/dev/null
wait_for_counter_above count_preparation_finishes "$prepare_before" "post-dismiss refresh"
[[ $(launcher_status) == resident ]] || fail "async refresh reopened an internally dismissed launcher"
[[ $(count_launcher_maps) -eq $first_maps ]] || fail "async refresh emitted a new launcher map"
require_resident_quiet_window "$first_maps" "post-dismiss refresh quiet window"

# Explicit show/hide remains resident and reuses the same process/window ownership path.
explicit_maps_before=$(count_launcher_maps)
prepare_before=$(count_preparation_finishes)
"$cbar" launcher show >/dev/null
wait_for_status visible "second resident show"
wait_for_counter_above count_preparation_finishes "$prepare_before" "second show preparation"
wait_for_counter_above count_launcher_maps "$explicit_maps_before" "second launcher map"
"$cbar" launcher hide >/dev/null
wait_for_status resident "explicit hide"

printf 'launcher first_maps=%s final_maps=%s final_status=%s\n' \
    "$first_maps" "$(count_launcher_maps)" "$(launcher_status)"
SESSION
then
    printf 'headless session failed\n' >&2
    tail -n 80 "$RIG/session.log" >&2 || true
    tail -n 80 "$RIG/cbar.log" >&2 || true
    tail -n 80 "$RIG/compositor.log" >&2 || true
    exit 1
fi

python3 - "$RIG/cbar.log" <<'PY'
import re
import sys

path = sys.argv[1]
lines = [
    line.split("cbar-layout-trace ", 1)[1].strip()
    for line in open(path, encoding="utf-8", errors="replace")
    if "cbar-layout-trace " in line
]
if not lines:
    raise SystemExit("FAIL: cbar emitted no bar-allocation trace")

fields = dict(piece.split("=", 1) for piece in lines[-1].split())

def allocation(name):
    try:
        values = tuple(int(value) for value in fields[name].split(","))
    except (KeyError, ValueError) as error:
        raise SystemExit(f"FAIL: invalid {name} allocation in {lines[-1]!r}: {error}")
    if len(values) != 4:
        raise SystemExit(f"FAIL: invalid {name} allocation in {lines[-1]!r}")
    return values

bar = allocation("bar")
start = allocation("start")
center = allocation("center")
end = allocation("end")
bx, _, bw, bh = bar
sx, _, sw, _ = start
cx, _, cw, _ = center
ex, _, ew, _ = end

failures = []
if bw <= 0 or bh <= 0 or cw <= 0:
    failures.append(f"non-positive allocation: bar={bar} center={center}")
if bx != 0 or bw != 1920:
    failures.append(f"bar did not span the deterministic 1920px output: bar={bar}")
if sw - ew < 250:
    failures.append(f"fixture was not materially asymmetric: start={sw}px end={ew}px")

# Compare doubled midpoints so half-pixel centres remain exact integer arithmetic. GTK may round a
# one-pixel odd-width split either way, hence the one-pixel (two doubled units) tolerance.
bar_midpoint_2 = 2 * bx + bw
center_midpoint_2 = 2 * cx + cw
midpoint_delta_2 = abs(bar_midpoint_2 - center_midpoint_2)
if midpoint_delta_2 > 2:
    failures.append(
        f"center midpoint drifted by {midpoint_delta_2 / 2:.1f}px: "
        f"bar={bar} center={center} start={start} end={end}"
    )
if cx < bx or cx + cw > bx + bw:
    failures.append(f"center escaped the bar allocation: bar={bar} center={center}")
if sx + sw > cx or cx + cw > ex:
    failures.append(
        f"start/center/end allocations overlap: start={start} center={center} end={end}"
    )

print(
    f"layout bar={bw}x{bh} start={sw}px center={cw}px end={ew}px "
    f"midpoint_delta={midpoint_delta_2 / 2:.1f}px"
)
if failures:
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    raise SystemExit(1)
print("headless cbar session OK")
PY

cat "$RIG/session.log"
if [[ -n ${TRACE_OUT:-} ]]; then
    cp -- "$RIG/cbar.log" "$TRACE_OUT"
fi
