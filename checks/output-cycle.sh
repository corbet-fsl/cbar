#!/usr/bin/env bash
# Prove that cbar keeps exactly one bar per live output across output disable/enable cycles.
#
# A private headless compositor with two outputs hosts cbar. The harness disables and re-enables
# the second output through the compositor's own IPC socket (the way ddcwatch does when a KVM
# switches an input away) and asserts, after every step, that
#   - an enabled output carries exactly one bar, and a disabled output carries none, and
#   - the surviving output never grows a second, orphaned bar.
# Bars are counted two independent ways: by the exclusive zone each bar reserves (the compositor's
# workspace rectangle shrinks by one bar height per bar) and by the pixels of a whole-output
# screenshot (the bar background is a solid colour on a black wallpaper). Optionally the harness also
# disables the second output while cbar is still building its first bars, which is the window in
# which an output event used to be lost.
#
# The compositor command is an argv, not a shell fragment. An exact `{config}` argument is replaced
# with the generated Sway-compatible fixture path; with no compositor arguments the harness
# supplies `-c {config}`. Every IPC call uses an explicit socket that was discovered inside the
# private runtime directory, so the check cannot reach the caller's graphical session.
#
# Usage:
#   checks/output-cycle.sh <cbar> -- <compositor> [arguments...]
# Environment:
#   CYCLES     disable/enable cycles to run (default 6)
#   RACES      startup-race attempts to run (default 3)
#   LIVE_BARS  set to 0 to skip the bars-still-held assertion (a cbar without the lifetime trace)
#   SHOT_DIR   keep whole-output screenshots of every stage here (default: not kept)
#   KEEP_RIG   set to 1 to keep the private fixture directory for inspection
set -euo pipefail

BAR_HEIGHT=32
CYCLES=${CYCLES:-6}
RACES=${RACES:-3}
LIVE_BARS=${LIVE_BARS:-1}

usage() {
    printf '%s\n' \
        "usage: $0 <cbar> -- <compositor> [arguments...]" \
        "example: $0 target/debug/cbar -- scroll -c '{config}'" >&2
    exit 2
}

[[ $# -ge 3 ]] || usage
CBAR=$1
[[ $2 == -- ]] || usage
shift 2
COMPOSITOR_ARGV=("$@")

command_exists() {
    if [[ $1 == */* ]]; then
        [[ -x $1 ]]
    else
        command -v "$1" >/dev/null 2>&1
    fi
}

command_exists "$CBAR" || { printf 'no cbar binary: %s\n' "$CBAR" >&2; exit 2; }
command_exists "${COMPOSITOR_ARGV[0]}" || {
    printf 'no compositor: %s\n' "${COMPOSITOR_ARGV[0]}" >&2
    exit 2
}
for dependency in bash date dbus-daemon dbus-run-session grim python3 readlink setsid; do
    command_exists "$dependency" || { printf 'missing dependency: %s\n' "$dependency" >&2; exit 2; }
done
# Unix-domain socket paths are short; the directory is private because cbar refuses an IPC socket
# under a group- or world-accessible runtime directory.
RIG=$(mktemp -d /tmp/cbar-output-cycle.XXXXXX)
chmod 700 "$RIG"
cleanup() {
    if [[ ${KEEP_RIG:-0} == 1 ]]; then
        printf 'kept output-cycle fixture: %s\n' "$RIG" >&2
        return
    fi
    case $RIG in
        /tmp/cbar-output-cycle.??????) rm -rf -- "$RIG" ;;
        *) printf 'refusing to remove unexpected fixture path: %s\n' "$RIG" >&2 ;;
    esac
}
trap cleanup EXIT

if [[ -n ${SHOT_DIR:-} ]]; then
    mkdir -p -- "$SHOT_DIR"
fi

python3 - "$RIG" "$BAR_HEIGHT" <<'PY'
import pathlib
import sys

rig = pathlib.Path(sys.argv[1])
height = int(sys.argv[2])
for directory in ("config", "state", "cache"):
    (rig / directory).mkdir(mode=0o700)

# One bar per output, addressed by its connector name exactly like the declared seat configuration.
# The default exclusive zone is kept on purpose: each bar reserves its height, which is how the
# harness counts bars without looking at pixels.
def bar(output):
    return f"""\
[monitors.{output}]
name = "{output}"
position = "top"
height = {height}
anchor_to_edges = true

[[monitors.{output}.start]]
type = "label"
label = "cbar on {output}"

[[monitors.{output}.start]]
type = "workspaces"

[[monitors.{output}.start]]
type = "music"
player_type = "mpris"
format = "{{title}}"

[[monitors.{output}.start]]
type = "script"
cmd = "echo script"
mode = "poll"
interval = 1000

[[monitors.{output}.center]]
type = "system_graph"

[[monitors.{output}.center]]
type = "clock"

[[monitors.{output}.end]]
type = "sys_info"
format = ["{{cpu_percent}}%"]

[monitors.{output}.end.interval]
cpu = 1

[[monitors.{output}.end]]
type = "custom"
class = "menu"

[[monitors.{output}.end.bar]]
type = "button"
label = "menu"
on_click = "popup:toggle"

[[monitors.{output}.end.popup]]
type = "label"
label = "popup"

[[monitors.{output}.end]]
type = "tray"
"""

(rig / "bar.toml").write_text(
    "[monitors]\n\n" + bar("HEADLESS-1") + "\n" + bar("HEADLESS-2"),
    encoding="utf-8",
)
(rig / "bar.css").write_text(
    "#bar { background-color: #1e5f8a; color: #ffffff; }\n", encoding="utf-8"
)
# A minimal client for the compositor's i3-compatible IPC. The harness talks to the socket it
# discovered inside the private runtime directory and to nothing else: no message tool that could
# fall back to discovering a running session's socket is involved.
(rig / "ipc.py").write_text(
    """\
import socket
import struct
import sys

COMMAND, GET_WORKSPACES, GET_OUTPUTS = 0, 1, 3
TYPES = {"get_workspaces": GET_WORKSPACES, "get_outputs": GET_OUTPUTS}

def read_exactly(connection, count):
    data = b""
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise SystemExit("compositor closed the IPC connection")
        data += chunk
    return data

socket_path, *words = sys.argv[1:]
if words[:1] == ["-t"]:
    kind, payload = TYPES[words[1]], b""
else:
    kind, payload = COMMAND, " ".join(words).encode()
connection = socket.socket(socket.AF_UNIX)
connection.connect(socket_path)
connection.sendall(b"i3-ipc" + struct.pack("=II", len(payload), kind) + payload)
header = read_exactly(connection, 14)
if header[:6] != b"i3-ipc":
    raise SystemExit("not an i3-compatible IPC reply")
length, _ = struct.unpack("=II", header[6:])
sys.stdout.write(read_exactly(connection, length).decode())
""",
    encoding="utf-8",
)
(rig / "compositor.conf").write_text(
    "output * bg #000000 solid_color\n"
    "output HEADLESS-1 mode 1920x1080 position 0 0\n"
    "output HEADLESS-2 mode 1280x720 position 1920 0\n",
    encoding="utf-8",
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

dbus_daemon=$(command -v dbus-daemon)
dbus_run_session=$(command -v dbus-run-session)
DBUS=("$dbus_run_session" "--dbus-daemon=$dbus_daemon")
# Resolve the session bus configuration beside the selected daemon, not through its compiled-in
# sysconfdir (which points outside the store in a Nix shell).
if [[ -n ${DBUS_SESSION_CONF:-} ]]; then
    dbus_session_conf=$DBUS_SESSION_CONF
else
    dbus_session_conf=
    for dbus_program in "$dbus_daemon" "$dbus_run_session"; do
        dbus_prefix=$(dirname "$(dirname "$(readlink -f "$dbus_program")")")
        candidate=$dbus_prefix/share/dbus-1/session.conf
        if [[ -r $candidate ]]; then
            dbus_session_conf=$candidate
            break
        fi
    done
fi
[[ -r $dbus_session_conf ]] || {
    printf 'no readable D-Bus session configuration for %s\n' "$dbus_daemon" >&2
    exit 2
}
DBUS+=("--config-file=$dbus_session_conf")

if ! "${DBUS[@]}" -- bash -s -- \
    "$RIG" "$CBAR" "$BAR_HEIGHT" "$CYCLES" "$RACES" "$LIVE_BARS" "${SHOT_DIR:-}" "${COMPOSITOR_ARGV[@]}" \
    >"$RIG/session.log" 2>&1 <<'SESSION'
set -euo pipefail

rig=$1
cbar=$2
bar_height=$3
cycles=$4
races=$5
live_bars_check=$6
shot_dir=$7
shift 7
compositor=("$@")
bar_pid=
compositor_pid=

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    return 1
}

process_group_alive() {
    local group=${1:-}
    [[ $group =~ ^[1-9][0-9]*$ ]] && kill -0 -- "-$group" 2>/dev/null
}

stop_process_group() {
    local group=${1:-}
    [[ $group =~ ^[1-9][0-9]*$ ]] || return 0
    process_group_alive "$group" || { wait "$group" 2>/dev/null || true; return 0; }
    kill -TERM -- "-$group" 2>/dev/null || true
    for _ in $(seq 1 40); do
        process_group_alive "$group" || { wait "$group" 2>/dev/null || true; return 0; }
        sleep 0.05
    done
    kill -KILL -- "-$group" 2>/dev/null || true
    for _ in $(seq 1 40); do
        process_group_alive "$group" || { wait "$group" 2>/dev/null || true; return 0; }
        sleep 0.05
    done
    printf 'cleanup could not stop process group %s\n' "$group" >&2
    return 1
}

cleanup_session() {
    local session_status=$?
    local cleanup_status=0
    trap - EXIT
    set +e
    stop_process_group "$bar_pid" || cleanup_status=1
    stop_process_group "$compositor_pid" || cleanup_status=1
    if (( session_status != 0 )); then
        exit "$session_status"
    fi
    exit "$cleanup_status"
}
trap cleanup_session EXIT

assert_session_leader() {
    local pid=$1 label=$2 group= session=
    for _ in $(seq 1 40); do
        read -r group session < <(ps -o pgid= -o sid= -p "$pid") || true
        [[ $pid == "$group" && $pid == "$session" ]] && return 0
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.01
    done
    fail "$label was not an owned session leader (pid=$pid)"
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
export WLR_HEADLESS_OUTPUTS=2
export WLR_LIBINPUT_NO_DEVICES=1
export GSK_RENDERER=cairo
unset HOME WAYLAND_DISPLAY WAYLAND_SOCKET SWAYSOCK SCROLLSOCK I3SOCK NIRI_SOCKET \
    HYPRLAND_INSTANCE_SIGNATURE DISPLAY

setsid -- "${compositor[@]}" >"$rig/compositor.log" 2>&1 &
compositor_pid=$!
assert_session_leader "$compositor_pid" "compositor"

wayland_display=
for _ in $(seq 1 150); do
    for socket in "$rig"/wayland-*; do
        if [[ -S $socket && $socket != *.lock ]]; then
            wayland_display=${socket##*/}
            break 2
        fi
    done
    process_group_alive "$compositor_pid" || fail "compositor exited before creating a Wayland socket"
    sleep 0.1
done
[[ -n $wayland_display ]] || fail "compositor never created a Wayland socket"
export WAYLAND_DISPLAY=$wayland_display

# The compositor IPC socket must live inside the private runtime directory: every call below names
# it explicitly, and an unverified socket aborts the check instead of falling through to discovery.
compositor_socket=
compositor_socket_kind=
for _ in $(seq 1 150); do
    for socket in "$rig"/scroll-ipc.*.sock; do
        if [[ -S $socket ]]; then
            compositor_socket=$socket
            compositor_socket_kind=scroll
            break 2
        fi
    done
    for socket in "$rig"/sway-ipc.*.sock; do
        if [[ -S $socket ]]; then
            compositor_socket=$socket
            compositor_socket_kind=sway
            break 2
        fi
    done
    process_group_alive "$compositor_pid" || fail "compositor exited before creating its IPC socket"
    sleep 0.1
done
[[ -n $compositor_socket ]] || fail "compositor never created an IPC socket"
[[ $compositor_socket == "$rig"/* ]] || fail "compositor socket is outside the private runtime directory"
if [[ $compositor_socket_kind == scroll ]]; then
    export SCROLLSOCK=$compositor_socket
else
    export SWAYSOCK=$compositor_socket
fi

msg() {
    python3 "$rig/ipc.py" "$compositor_socket" "$@"
}

for _ in $(seq 1 100); do
    msg -t get_outputs >/dev/null 2>&1 && break
    sleep 0.05
done
[[ $(msg -t get_outputs | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))') == 2 ]] ||
    fail "the private compositor did not start with exactly two outputs"

# usable_rows OUTPUT: the number of rows at the top of the output that bars reserve, from the
# compositor's workspace rectangle. Prints "absent" when the output has no workspace.
usable_rows() {
    msg -t get_workspaces | python3 -c '
import json, sys
output = sys.argv[1]
for workspace in json.load(sys.stdin):
    if workspace.get("output") == output and workspace.get("visible"):
        print(workspace["rect"]["y"])
        break
else:
    print("absent")
' "$1"
}

# pixel_rows OUTPUT: the number of contiguous rows from the top edge whose pixel in the first column
# is not the black wallpaper, measured on a whole-output screenshot. Prints "absent" when the
# output is not in the compositor's output list.
pixel_rows() {
    local output=$1 label=$2 shot
    shot=$rig/shot-$output.ppm
    if ! msg -t get_outputs | python3 -c '
import json, sys
sys.exit(0 if any(o["name"] == sys.argv[1] and o.get("active") for o in json.load(sys.stdin)) else 1)
' "$output"; then
        printf 'absent\n'
        return 0
    fi
    grim -t ppm -o "$output" "$shot"
    if [[ -n $shot_dir ]]; then
        grim -t png -o "$output" "$shot_dir/$label-$output.png"
    fi
    python3 - "$shot" <<'PY'
import sys

with open(sys.argv[1], "rb") as handle:
    assert handle.readline().strip() == b"P6"
    width, height = map(int, handle.readline().split())
    assert handle.readline().strip() == b"255"
    data = handle.read()
rows = 0
for y in range(height):
    offset = (y * width) * 3
    if data[offset:offset + 3] == b"\x00\x00\x00":
        break
    rows += 1
print(rows)
PY
}

# One bar reserves its configured height plus the widget frame; the unit is read off the first
# bar once and must be a single bar (a stacked pair would reserve at least twice the height).
unit_height=
calibrate() {
    local rows=
    for _ in $(seq 1 60); do
        rows=$(usable_rows HEADLESS-1)
        [[ $rows =~ ^[1-9][0-9]*$ ]] && break
        process_group_alive "$bar_pid" || fail "cbar exited before its first bar mapped"
        sleep 0.1
    done
    [[ $rows =~ ^[1-9][0-9]*$ ]] || fail "startup: HEADLESS-1 never reserved space for a bar"
    (( rows >= bar_height && rows < 2 * bar_height )) ||
        fail "startup: HEADLESS-1 reserves $rows px, which is not exactly one bar of $bar_height px"
    unit_height=$rows
    printf 'one bar reserves %s px (configured height %s px)\n' "$unit_height" "$bar_height"
}

# expect_bars OUTPUT COUNT LABEL: the output reserves COUNT bars and shows COUNT bars of pixels, or
# (COUNT = -1, a disabled output) is absent altogether. Polls briefly because a bar maps one
# compositor round trip after the output appears.
expect_bars() {
    local output=$1 count=$2 label=$3 usable= pixels= want_usable want_pixels
    if (( count < 0 )); then
        want_usable=absent
        want_pixels=absent
    else
        want_usable=$((count * unit_height))
        want_pixels=$((count * unit_height))
    fi
    for _ in $(seq 1 60); do
        usable=$(usable_rows "$output")
        pixels=$(pixel_rows "$output" "$label")
        if [[ $usable == "$want_usable" && $pixels == "$want_pixels" ]]; then
            printf 'ok   %-26s %-10s bars=%s usable=%s pixels=%s\n' "$label" "$output" "$count" "$usable" "$pixels"
            return 0
        fi
        process_group_alive "$bar_pid" || fail "cbar exited during $label"
        sleep 0.1
    done
    fail "$label: $output expected bars=$count (usable=$want_usable pixels=$want_pixels), got usable=$usable pixels=$pixels"
}

# live_bars: the number of bars cbar still holds, from its own lifetime trace. A bar that was closed
# but is still referenced from a module task or a receiver loop stays counted, so a leak of closed
# bars shows up here even when every screen looks right.
live_bars() {
    # Only coreutils and grep: the CI shell that runs this check has no sed or awk.
    grep -o 'cbar-bar-trace event=[a-z]* .* live=[0-9][0-9]*' "$rig/cbar.log" |
        tail -n 1 | grep -o '[0-9][0-9]*$' || true
}

expect_live_bars() {
    [[ $live_bars_check == 1 ]] || return 0
    local want=$1 label=$2 live=
    for _ in $(seq 1 100); do
        live=$(live_bars)
        if [[ $live == "$want" ]]; then
            printf 'ok   %-26s bars held by cbar=%s\n' "$label" "$live"
            return 0
        fi
        process_group_alive "$bar_pid" || fail "cbar exited during $label"
        sleep 0.1
    done
    fail "$label: cbar still holds ${live:-no} bars, expected $want (a closed bar is still referenced)"
}

# An orphan can show up late (the compositor re-homes a surface after the output is gone), so a
# result is only believed after a settle window of consistent samples on every output.
settle() {
    sleep 1
}

start_bar() {
    CBAR_BAR_TRACE=1 \
    CBAR_CONFIG="$rig/bar.toml" \
    CBAR_CSS="$rig/bar.css" \
    setsid -- "$cbar" >"$rig/cbar.log" 2>&1 &
    bar_pid=$!
    assert_session_leader "$bar_pid" "cbar"
}

process_status() {
    python3 - "$bar_pid" <<'PY'
import os, pathlib, sys

pid = sys.argv[1]
status = {}
for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
    key, _, value = line.partition(":")
    status[key] = value.split()
stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
fields = stat[stat.rfind(")") + 2 :].split()
print(
    f"threads={status['Threads'][0]} rss_kib={status['VmRSS'][0]} "
    f"fds={len(os.listdir(f'/proc/{pid}/fd'))} "
    f"cpu_ms={(int(fields[11]) + int(fields[12])) * 1000 // os.sysconf('SC_CLK_TCK')}"
)
PY
}

# Disable an output the instant cbar starts building that output's bar, so that the output is gone
# while its bar is still under construction and before cbar has finished its first pass over the
# outputs. This is the window in which the removal event used to be lost, and a bar mapped for a
# vanished output was placed on a surviving one by the compositor. The watcher tails cbar's own log
# and speaks the compositor IPC protocol directly so that its reaction time is a millisecond or two.
disable_when_bar_is_built() {
    python3 - "$rig/cbar.log" "$compositor_socket" "$rig/race-output" <<'PY'
import re
import socket
import struct
import sys
import time

log_path, socket_path, result_path = sys.argv[1:4]
pattern = re.compile(rb"Initializing bar '(HEADLESS-[12])'")

def command(text):
    connection = socket.socket(socket.AF_UNIX)
    connection.connect(socket_path)
    payload = text.encode()
    connection.sendall(b"i3-ipc" + struct.pack("=II", len(payload), 0) + payload)
    header = connection.recv(14)
    connection.close()
    return header

deadline = time.monotonic() + 30
while time.monotonic() < deadline:
    try:
        with open(log_path, "rb") as handle:
            match = pattern.search(handle.read())
    except FileNotFoundError:
        match = None
    if match:
        output = match.group(1).decode()
        command(f"output {output} disable")
        with open(result_path, "w", encoding="utf-8") as result:
            result.write(output)
        sys.exit(0)
    time.sleep(0.0005)
sys.exit("the bar of no output was ever built")
PY
}

other_output() {
    [[ $1 == HEADLESS-1 ]] && printf 'HEADLESS-2\n' || printf 'HEADLESS-1\n'
}

restart_bar() {
    stop_process_group "$bar_pid" || fail "could not stop cbar"
    bar_pid=
    # A killed cbar leaves its control socket behind; only these two private files are removed.
    rm -f -- "$rig/cbar-ipc.sock" "$rig/race-output"
    start_bar
}

scenario_cycle() {
    start_bar
    calibrate
    expect_bars HEADLESS-1 1 "startup"
    expect_bars HEADLESS-2 1 "startup"
    settle
    expect_bars HEADLESS-1 1 "startup-settled"
    expect_bars HEADLESS-2 1 "startup-settled"
    expect_live_bars 2 "startup-settled"
    printf 'process after startup: %s\n' "$(process_status)"

    for cycle in $(seq 1 "$cycles"); do
        msg 'output HEADLESS-2 disable' >/dev/null
        expect_bars HEADLESS-2 -1 "cycle$cycle-disabled"
        settle
        expect_bars HEADLESS-1 1 "cycle$cycle-disabled-settled"
        expect_bars HEADLESS-2 -1 "cycle$cycle-disabled-settled"
        expect_live_bars 1 "cycle$cycle-disabled-settled"
        msg 'output HEADLESS-2 enable' >/dev/null
        expect_bars HEADLESS-2 1 "cycle$cycle-enabled"
        settle
        expect_bars HEADLESS-1 1 "cycle$cycle-enabled-settled"
        expect_bars HEADLESS-2 1 "cycle$cycle-enabled-settled"
        expect_live_bars 2 "cycle$cycle-enabled-settled"
        if (( cycle % 10 == 0 )); then
            printf 'process after %s cycles: %s\n' "$cycle" "$(process_status)"
        fi
    done
    printf 'process after %s cycles: %s\n' "$cycles" "$(process_status)"

    # The connector of a vanished output must be reported as gone, never silently kept.
    grep -q 'Monitor HEADLESS-2 disconnected' "$rig/cbar.log" || fail "cbar never logged the disconnect of HEADLESS-2"
    grep -q 'Monitor HEADLESS-2 connected' "$rig/cbar.log" || fail "cbar never logged the reconnect of HEADLESS-2"
    printf 'output cycle OK (%s cycles)\n' "$cycles"
}

scenario_race() {
    local attempt gone survivor
    for attempt in $(seq 1 "$races"); do
        restart_bar
        disable_when_bar_is_built
        gone=$(<"$rig/race-output")
        survivor=$(other_output "$gone")
        printf 'race %s: %s was disabled while its bar was being built\n' "$attempt" "$gone"
        settle
        expect_bars "$survivor" 1 "race$attempt-vanished"
        expect_bars "$gone" -1 "race$attempt-vanished"
        expect_live_bars 1 "race$attempt-vanished"
        if grep -q "Monitor $gone disconnected" "$rig/cbar.log"; then
            printf 'race %s: cbar saw the removal of %s\n' "$attempt" "$gone"
        else
            printf 'race %s: cbar never saw the removal of %s\n' "$attempt" "$gone"
        fi
        msg "output $gone enable" >/dev/null
        expect_bars "$gone" 1 "race$attempt-restored"
        settle
        expect_bars "$survivor" 1 "race$attempt-restored-settled"
        expect_bars "$gone" 1 "race$attempt-restored-settled"
        expect_live_bars 2 "race$attempt-restored-settled"
    done
    printf 'startup race OK (%s attempts)\n' "$races"
}

scenario_cycle
scenario_race
SESSION
then
    printf 'output-cycle session failed\n' >&2
    tail -n 60 "$RIG/session.log" >&2 || true
    tail -n 60 "$RIG/cbar.log" >&2 || true
    tail -n 40 "$RIG/compositor.log" >&2 || true
    exit 1
fi

cat "$RIG/session.log"
if [[ -n ${TRACE_OUT:-} ]]; then
    cp -- "$RIG/cbar.log" "$TRACE_OUT"
fi
