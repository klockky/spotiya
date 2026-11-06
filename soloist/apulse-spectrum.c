#include "apulse-spectrum.h"
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

struct band {
    float b0, a1, a2, z1[2], z2[2], energy, envelope;
};
struct spotiya_spectrum {
    int socket, enabled;
    char *setting;
    struct sockaddr_un target;
    uint64_t checked;
    unsigned channels, frames, window;
    struct band bands[4];
};

static uint64_t milliseconds(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now))
        return 0;
    return (uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

struct spotiya_spectrum *spotiya_spectrum_new(unsigned rate, unsigned channels)
{
    const char *path = getenv("SPOTIYA_SOLOIST_LEVELS");
    const char *setting = getenv("SPOTIYA_SPECTRUM_SETTING");
    if (!path || !setting || !*path || !*setting || rate < 16000 || rate > 192000 ||
        channels < 1 || channels > 2 || strlen(path) >= sizeof(((struct sockaddr_un *)0)->sun_path))
        return NULL;
    struct spotiya_spectrum *s = calloc(1, sizeof(*s));
    if (!s)
        return NULL;
    s->socket = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    s->setting = strdup(setting);
    if (s->socket < 0 || !s->setting) {
        spotiya_spectrum_free(s);
        return NULL;
    }
    s->target.sun_family = AF_UNIX;
    strcpy(s->target.sun_path, path);
    s->channels = channels;
    s->window = rate / 20;
    const float centers[] = {100, 400, 1600, 6400};
    for (unsigned i = 0; i < 4; i++) {
        float omega = 6.28318530718f * centers[i] / rate;
        float alpha = sinf(omega) / 1.4f, a0 = 1 + alpha;
        s->bands[i].b0 = alpha / a0;
        s->bands[i].a1 = -2 * cosf(omega) / a0;
        s->bands[i].a2 = (1 - alpha) / a0;
    }
    return s;
}

void spotiya_spectrum_free(struct spotiya_spectrum *s)
{
    if (!s)
        return;
    if (s->socket >= 0)
        close(s->socket);
    free(s->setting);
    free(s);
}

void spotiya_spectrum_reset(struct spotiya_spectrum *s)
{
    if (!s)
        return;
    s->frames = 0;
    for (unsigned i = 0; i < 4; i++) {
        struct band *b = &s->bands[i];
        memset(b->z1, 0, sizeof(b->z1));
        memset(b->z2, 0, sizeof(b->z2));
        b->energy = b->envelope = 0;
    }
}

static int active(struct spotiya_spectrum *s)
{
    uint64_t now = milliseconds();
    if (!s->checked || now - s->checked >= 1000) {
        s->checked = now;
        FILE *file = fopen(s->setting, "re");
        int enabled = file && fgetc(file) == '1';
        if (file)
            fclose(file);
        if (enabled != s->enabled)
            spotiya_spectrum_reset(s);
        s->enabled = enabled;
    }
    return s->enabled;
}

void spotiya_spectrum_feed(struct spotiya_spectrum *s, const void *pcm,
                          size_t frames, enum spotiya_sample format)
{
    if (!s || !pcm || !active(s))
        return;
    const unsigned char *bytes = pcm;
    for (size_t n = 0; n < frames; n++) {
        for (unsigned c = 0; c < s->channels; c++) {
            size_t index = n * s->channels + c;
            float input;
            if (format == SPOTIYA_FLOAT32) {
                memcpy(&input, bytes + index * sizeof(float), sizeof(float));
            } else {
                int16_t value;
                memcpy(&value, bytes + index * sizeof(value), sizeof(value));
                input = value / 32768.0f;
            }
            input = isfinite(input) ? fminf(1, fmaxf(-1, input)) : 0;
            for (unsigned i = 0; i < 4; i++) {
                struct band *b = &s->bands[i];
                float out = b->b0 * input + b->z1[c];
                b->z1[c] = -b->a1 * out + b->z2[c];
                b->z2[c] = -b->b0 * input - b->a2 * out;
                b->energy += out * out;
            }
        }
        if (++s->frames == s->window) {
            uint8_t levels[4];
            for (unsigned i = 0; i < 4; i++) {
                struct band *b = &s->bands[i];
                float rms = sqrtf(b->energy / (s->window * s->channels));
                float target = fminf(1, fmaxf(0, (20 * log10f(fmaxf(rms, 1e-7f)) + 54) / 48));
                b->envelope += (target - b->envelope) * (target > b->envelope ? .8f : .2f);
                levels[i] = (uint8_t)roundf(b->envelope * 100);
                b->energy = 0;
            }
            // Dropped/full/missing receiver: never block audio or retain PCM.
            (void)sendto(s->socket, levels, sizeof(levels), MSG_DONTWAIT | MSG_NOSIGNAL,
                         (struct sockaddr *)&s->target, sizeof(s->target));
            s->frames = 0;
        }
    }
}
