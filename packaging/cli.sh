#!/bin/sh
set -eu
SPOTIYA_BASE=/data/opt/spotify-connect
case "${1:-tui}" in
    backend)
        shift
        [ "$#" -eq 1 ] || { echo 'Usage: spotiya backend soloist|librespot' >&2; exit 1; }
        set -- --backend-select "$1"
        ;;
    soloist)
        shift
        [ "$#" -eq 1 ] && [ "$1" = key ] || { echo 'Usage: spotiya soloist key < PRIVATE_KEY_FILE' >&2; exit 1; }
        set -- --soloist-key
        ;;
    uninstall)
        shift
        if [ "$#" -eq 0 ] || { [ "$#" -eq 1 ] && [ "$1" = --yes ]; }; then
            set -- --uninstall "$@"
        else echo 'Usage: spotiya uninstall [--yes]' >&2; exit 1; fi
        ;;
    addon)
        shift
        if [ "$#" -eq 2 ] && [ "$1" = install ]; then
            set -- --addon-install "$2"
        elif [ "$#" -eq 2 ] && [ "$1" = remove ]; then
            set -- --addon-remove "$2"
        elif [ "$#" -eq 4 ] && [ "$1" = file ]; then
            set -- --addon-install-file "$2" "$3" "$4"
        else echo 'Usage: spotiya addon install ID | spotiya addon remove ID | spotiya addon file ID ARCHIVE SHA256SUMS' >&2; exit 1; fi
        ;;
    start|stop|restart)
        exec /etc/init.d/S99spotify-connect "$1"
        ;;
    doctor)
        shift
        if [ "$#" -eq 0 ]; then set -- --doctor
        elif [ "$1" = report ] && [ "$#" -le 2 ]; then set -- --doctor-report "${2:--}"
        else echo 'Usage: spotiya doctor [report [FILE]]' >&2; exit 1; fi
        ;;
    update)
        shift
        case "${1:-}" in
            '') set -- --update ;;
            check) [ "$#" -eq 1 ] || exit 1; set -- --update-check ;;
            file) [ "$#" -eq 3 ] || { echo 'Usage: spotiya update file ARCHIVE SHA256SUMS' >&2; exit 1; }; set -- --update-file "$2" "$3" ;;
            *) [ "$#" -eq 1 ] || { echo 'Usage: spotiya update [check|VERSION|file ARCHIVE SHA256SUMS]' >&2; exit 1; }; set -- --update-tag "$1" ;;
        esac
        ;;
    play)
        shift
        [ "$#" -gt 0 ] || { echo 'Usage: spotiya play SONG_OR_ARTIST' >&2; exit 1; }
        set -- --play "$*"
        ;;
    profile)
        shift
        if [ "${1:-}" = select ]; then
            shift
            [ "$#" -eq 1 ] || { echo 'Usage: spotiya profile select ID_OR_JSON' >&2; exit 1; }
            set -- --profile-select "$1"
        else
            [ "$#" -eq 0 ] || { echo 'Usage: spotiya profile [select ID_OR_JSON]' >&2; exit 1; }
            set -- --profile-info
        fi
        ;;
    youtube)
        shift
        case "${1:-}" in
            on|off) set -- --youtube "$1" ;;
            '') echo 'Usage: spotiya youtube on|off|LINK' >&2; exit 1 ;;
            *) set -- --youtube-url "$1" ;;
        esac
        ;;
    tui|addons|login|status|pause|resume|toggle|next|previous|volume|voice|spectrum|airplay|retry|release|version|help|profiles)
        SPOTIYA_COMMAND=${1:-tui}
        [ "$#" -eq 0 ] || shift
        set -- "--$SPOTIYA_COMMAND" "$@"
        ;;
esac
case "${1:-}" in
    --tui|--addons|--login|--play|--pause|--resume|--toggle|--next|--previous|--volume|--voice|--spectrum|--airplay|--youtube|--youtube-url|--retry|--release)
        "$SPOTIYA_BASE/start.sh" >/dev/null
        ;;
esac
exec "$SPOTIYA_BASE/spotiya" "$@"
