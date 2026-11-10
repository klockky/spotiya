/* Soloist renders audio in its own process, so the receiver's gain stage never
 * sees its samples. Spotiya keeps SPOTIYA_DUCK_FILE while Alice listens, thinks
 * or speaks and removes it afterwards. A file older than the hold time is
 * ignored, so a crashed receiver cannot leave the music ducked forever. */
#include "apulse-duck.h"
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>

#define SPOTIYA_DUCK_FILE "/run/spotiya-duck"
#define SPOTIYA_DUCK_HOLD 65
#define SPOTIYA_DUCK_LEVEL 0.2f
#define SPOTIYA_DUCK_ATTACK 0.08f
#define SPOTIYA_DUCK_RELEASE 0.3f

void
spotiya_duck_apply(float *gain, int busy, void *pcm, size_t bytes, unsigned rate,
                   unsigned channels, enum spotiya_duck_format format)
{
    const float target = busy ? SPOTIYA_DUCK_LEVEL : 1.0f;
    if ((*gain == 1.0f && target == 1.0f) || rate == 0 || channels == 0)
        return;

    const float seconds = target < *gain ? SPOTIYA_DUCK_ATTACK : SPOTIYA_DUCK_RELEASE;
    const float step = (1.0f - SPOTIYA_DUCK_LEVEL) / (seconds * rate);
    const size_t sample_size = format == SPOTIYA_DUCK_FLOAT32 ? sizeof(float) : sizeof(int16_t);
    const size_t frame_size = sample_size * channels;
    char *p = pcm;
    char *last = p + bytes - bytes % frame_size;

    while (p < last) {
        if (target < *gain)
            *gain = *gain - step > target ? *gain - step : target;
        else
            *gain = *gain + step < target ? *gain + step : target;

        for (unsigned k = 0; k < channels; k++, p += sample_size) {
            if (format == SPOTIYA_DUCK_FLOAT32) {
                float sample;
                memcpy(&sample, p, sizeof(sample));
                sample *= *gain;
                memcpy(p, &sample, sizeof(sample));
            } else {
                int16_t sample;
                memcpy(&sample, p, sizeof(sample));
                sample = (int16_t)(sample * *gain);
                memcpy(p, &sample, sizeof(sample));
            }
        }
    }
}

void
spotiya_duck(void *pcm, size_t bytes, unsigned rate, unsigned channels,
             enum spotiya_duck_format format)
{
    static float gain = 1.0f;
    struct stat st;
    int busy = stat(SPOTIYA_DUCK_FILE, &st) == 0 &&
               time(NULL) - st.st_mtime < SPOTIYA_DUCK_HOLD;
    spotiya_duck_apply(&gain, busy, pcm, bytes, rate, channels, format);
}
