#!/bin/sh
set -eu
SPOTIYA_BASE=/data/opt/spotify-connect
case "${1:-tui}" in
    start|stop|restart)
        exec /etc/init.d/S99spotify-connect "$1"
        ;;
    play)
        shift
        [ "$#" -gt 0 ] || { echo 'Usage: spotiya play SONG_OR_ARTIST' >&2; exit 1; }
        set -- --play "$*"
        ;;
    navidrome|spotify)
        SPOTIYA_COMMAND=$1
        shift
        [ "$#" -gt 0 ] || { echo "Usage: spotiya $SPOTIYA_COMMAND SONG_OR_ARTIST" >&2; exit 1; }
        set -- "--$SPOTIYA_COMMAND" "$*"
        ;;
    navidrome-setup)
        shift
        [ "$#" -eq 2 ] || { echo 'Usage: spotiya navidrome-setup URL USER (password from stdin)' >&2; exit 1; }
        set -- --navidrome-setup "$@"
        ;;
    tui|login|status|pause|resume|toggle|next|previous|volume|voice|spectrum|source|release|version|help)
        SPOTIYA_COMMAND=${1:-tui}
        [ "$#" -eq 0 ] || shift
        set -- "--$SPOTIYA_COMMAND" "$@"
        ;;
esac
case "${1:-}" in
    --tui|--login|--play|--navidrome|--spotify|--navidrome-setup|--source|--pause|--resume|--toggle|--next|--previous|--volume|--voice|--spectrum|--release)
        "$SPOTIYA_BASE/start.sh" >/dev/null
        ;;
esac
exec "$SPOTIYA_BASE/spotiya" "$@"
