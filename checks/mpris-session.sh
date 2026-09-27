#!/usr/bin/env bash
# Exercise polling on a private bus with the selected package's configuration.
set -euo pipefail

daemon=$(command -v dbus-daemon)
runner=$(command -v dbus-run-session)
session_conf=
for program in "$daemon" "$runner"; do
    prefix=$(dirname "$(dirname "$(readlink -f "$program")")")
    candidate=$prefix/share/dbus-1/session.conf
    if [[ -r $candidate ]]; then
        session_conf=$candidate
        break
    fi
done
if [[ ! -r $session_conf ]]; then
    printf 'no readable D-Bus session configuration for %s\n' "$daemon" >&2
    exit 2
fi
exec "$runner" "--dbus-daemon=$daemon" "--config-file=$session_conf" -- \
    cargo test --locked --bin cbar \
    clients::music::mpris::tests::idle_polling_releases_bus_subscriptions \
    -- --exact --ignored
