#!/bin/sh
set -eu
base=/data/opt/solo
# ARM64 compat emulation clears the exclusive monitor inside Soloist atomics.
# Use the kernel-supported hardware barriers instead of modifying Spotify code.
if [ -f /proc/sys/abi/cp15_barrier ] && [ "$(cat /proc/sys/abi/cp15_barrier)" != 2 ]; then
    echo 2 > /proc/sys/abi/cp15_barrier || {
        echo 'Soloist needs hardware CP15 barriers on this ARM64 kernel' >&2
        exit 1
    }
fi
export LD_LIBRARY_PATH="$base/lib:/system/vendor/quasar/libs:/usr/lib:/lib"
export APULSE_PLAYBACK_DEVICE="${APULSE_PLAYBACK_DEVICE:-plug:default}"
export SPOTIYA_SOLOIST_HARDWARE_VOLUME=1
export MALLOC_ARENA_MAX=1
exec "$base/soloist" "$@"
