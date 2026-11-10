#!/bin/bash
set -eu
if [ "$#" -ne 5 ]; then
    echo "Usage: $0 APULSE_SOURCE CROSS_SDK GLIBC_SYSROOT ALSA_SYSROOT OUTPUT_DIR" >&2
    exit 2
fi
source_dir=$(realpath "$1")
sdk=$(realpath "$2")
sysroot=$(realpath "$3")
alsa=$(realpath "$4")
mkdir -p "$5"
output=$(realpath "$5")
repo=$(cd "$(dirname "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
src=$stage/apulse
mkdir "$src"
cp -a "$source_dir/src" "$source_dir/3rdparty" "$src/"
cp "$repo/soloist/apulse-spectrum.c" "$repo/soloist/apulse-spectrum.h" \
    "$repo/soloist/apulse-duck.c" "$repo/soloist/apulse-duck.h" "$src/src/"
patch --directory="$src" -p1 --forward --force < "$repo/soloist/apulse-spotiya.patch"
"$sdk/bin/arm-linux-gnueabihf-gcc" -Os -fPIC -ffunction-sections -c "$repo/controls/alsa_compat.c" -o "$stage/alsa_compat.o"
wrap=''
for name in get_buffer_size get_buffer_size_min get_buffer_size_max get_period_size get_period_size_min get_period_size_max set_buffer_size_near set_period_size_near; do
    wrap="$wrap -Wl,--wrap=snd_pcm_hw_params_$name,-u,__wrap_snd_pcm_hw_params_$name"
done
# shellcheck disable=SC2086
"$sdk/bin/arm-linux-gnueabihf-gcc" --sysroot="$sysroot" \
    -isystem "$sysroot/usr/include/arm-linux-gnueabihf" \
    -I"$sysroot/usr/include/glib-2.0" \
    -I"$sysroot/usr/lib/arm-linux-gnueabihf/glib-2.0/include" \
    -I"$alsa/usr/include" -I"$src/3rdparty/pulseaudio-headers" \
    -std=gnu99 -Os -flto -fPIC -fvisibility=hidden -ffunction-sections -fdata-sections \
    -mcpu=cortex-a53 -mfpu=neon-fp-armv8 -mfloat-abi=hard \
    -D_TIME_BITS=64 -D_FILE_OFFSET_BITS=64 \
    -DCONFIG_TRACE_LEVEL=1 -DCONFIG_LOG_TO_STDERR=1 \
    -shared -Wl,-soname,libpulse.so.0 -Wl,--gc-sections -Wl,--allow-shlib-undefined \
    -Wl,--version-script="$src/src/symbolmap" $wrap \
    "$src"/src/apulse-{channel-map,common,context,ext,format-info,mainloop,misc,operation,proplist,signal,stream,threaded-mainloop,volume}.c \
    "$src"/src/{ringbuffer,util,trace,apulse-spectrum,apulse-duck}.c \
    "$stage/alsa_compat.o" \
    -L"$sysroot/usr/lib/arm-linux-gnueabihf" -L"$sysroot/lib/arm-linux-gnueabihf" \
    -L"$alsa/usr/lib" -lglib-2.0 -lasound -lm -pthread \
    -o "$output/libpulse.so.0"
"$sdk/bin/arm-linux-gnueabihf-strip" --strip-unneeded "$output/libpulse.so.0"

"$sdk/bin/arm-linux-gnueabihf-gcc" -Os \
    -I"$src/3rdparty/pulseaudio-headers" "$repo/soloist/test-apulse-lifetime.c" \
    -L"$output" -Wl,--allow-shlib-undefined -l:libpulse.so.0 \
    -o "$output/test-apulse-lifetime"
"$sdk/bin/arm-linux-gnueabihf-gcc" -std=gnu99 -Os -Wall -Wextra -Werror \
    "$repo/soloist/test-apulse-spectrum.c" -lm -o "$output/test-apulse-spectrum"
"$sdk/bin/arm-linux-gnueabihf-gcc" -std=gnu99 -Os -Wall -Wextra -Werror -I"$repo/soloist" \
    "$repo/soloist/test-apulse-duck.c" "$repo/soloist/apulse-duck.c" -lm -o "$output/test-apulse-duck"
echo "Built $output/libpulse.so.0 and lifetime/spectrum/duck tests"
