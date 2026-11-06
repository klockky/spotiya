// Include the implementation to advance its setting poll deterministically.
#include "apulse-spectrum.c"
#include <assert.h>
#include <errno.h>

int main(void)
{
    char dir[] = "/tmp/spotiya-spectrum-XXXXXX";
    assert(mkdtemp(dir));
    char target[108], setting[128];
    snprintf(target, sizeof(target), "%s/levels", dir);
    snprintf(setting, sizeof(setting), "%s/enabled", dir);
    assert(!setenv("SPOTIYA_SOLOIST_LEVELS", target, 1));
    assert(!setenv("SPOTIYA_SPECTRUM_SETTING", setting, 1));
    FILE *file = fopen(setting, "w"); assert(file); fputs("1\n", file); fclose(file);
    int receiver = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK, 0); assert(receiver >= 0);
    struct sockaddr_un address = {.sun_family = AF_UNIX}; strcpy(address.sun_path, target);
    assert(!bind(receiver, (struct sockaddr *)&address, sizeof(address)));
    for (unsigned rate = 44100; rate <= 48000; rate += 3900) {
        for (unsigned wanted = 0; wanted < 4; wanted++) {
            const float frequency[] = {100, 400, 1600, 6400};
            struct spotiya_spectrum *s = spotiya_spectrum_new(rate, 2); assert(s);
            uint8_t levels[5]; unsigned packets = 0;
            for (unsigned n = 0; n < rate / 2; n++) {
                float value = .25f * sinf(6.28318530718f * frequency[wanted] * n / rate);
                float samples[] = {value, -value}, before[2]; memcpy(before, samples, sizeof(samples));
                spotiya_spectrum_feed(s, samples, 1, SPOTIYA_FLOAT32);
                assert(!memcmp(samples, before, sizeof(samples)));
                ssize_t size = recv(receiver, levels, sizeof(levels), 0);
                if (size >= 0) { assert(size == 4); packets++; }
            }
            assert(packets == 10);
            unsigned loudest = 0;
            for (unsigned i = 0; i < 4; i++) {
                assert(levels[i] <= 100);
                if (levels[i] > levels[loudest]) loudest = i;
            }
            assert(loudest == wanted && levels[wanted] > 40);
            spotiya_spectrum_reset(s);
            for (unsigned n = 0; n < rate / 20; n++) {
                float samples[] = {NAN, INFINITY};
                spotiya_spectrum_feed(s, samples, 1, SPOTIYA_FLOAT32);
            }
            assert(recv(receiver, levels, sizeof(levels), 0) == 4);
            assert(!memcmp(levels, "\0\0\0\0", 4));
            file = fopen(setting, "w"); assert(file); fputs("0\n", file); fclose(file);
            s->checked = 0;
            int16_t samples[4800] = {0};
            spotiya_spectrum_feed(s, samples, 2400, SPOTIYA_S16);
            assert(!s->enabled && s->frames == 0);
            assert(recv(receiver, levels, sizeof(levels), 0) == -1 && errno == EAGAIN);
            file = fopen(setting, "w"); assert(file); fputs("1\n", file); fclose(file);
            s->checked = 0;
            spotiya_spectrum_feed(s, samples, rate / 20, SPOTIYA_S16);
            assert(recv(receiver, levels, sizeof(levels), 0) == 4);
            spotiya_spectrum_free(s);
        }
    }
    // An absent or full receiver must not stall the playback thread.
    struct spotiya_spectrum *s = spotiya_spectrum_new(48000, 2); assert(s);
    float silence[4800] = {0};
    for (unsigned i = 0; i < 100; i++) spotiya_spectrum_feed(s, silence, 2400, SPOTIYA_FLOAT32);
    close(receiver); unlink(target);
    spotiya_spectrum_feed(s, silence, 2400, SPOTIYA_FLOAT32);
    spotiya_spectrum_free(s);
    unlink(setting); rmdir(dir);
    puts("spectrum: bands, rates, anti-phase, silence, settings and nonblocking IPC passed");
}
