#!/bin/sh
set -eu
umask 077
state=${SPOTIYA_STATE_DIR:-/data/opt/spotify-connect/receiver-state}
mkdir -p "$state/soloist"
chmod 700 "$state/soloist"
key_tmp=$(mktemp "$state/soloist/.api-key.XXXXXX")
tty_state=$(stty -g </dev/tty)
trap 'stty "$tty_state" </dev/tty; rm -f "$key_tmp"; unset soloist_key' EXIT
trap 'exit 1' HUP INT TERM
stty -echo </dev/tty
printf 'New Soloist API key (hidden): ' >/dev/tty
IFS= read -r soloist_key </dev/tty
stty "$tty_state" </dev/tty
printf '\n' >/dev/tty
case "$soloist_key" in
    ''|*[![:graph:]]*) echo 'Invalid key' >&2; exit 1 ;;
esac
[ "${#soloist_key}" -le 4096 ] || { echo 'Key too long' >&2; exit 1; }
printf '%s\n' "$soloist_key" >"$key_tmp"
unset soloist_key
mv "$key_tmp" "$state/soloist/api-key"
echo 'Soloist key saved. Run: spotiya retry'
