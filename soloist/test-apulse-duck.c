/* Host test: cc -std=gnu99 -O2 -Wall -Wextra -Werror soloist/test-apulse-duck.c soloist/apulse-duck.c -o /tmp/test-apulse-duck */
#include "apulse-duck.h"
#include <assert.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>

#define RATE 44100
static float f[2 * RATE];
static int16_t s[2 * RATE];

int
main(void)
{
    float gain = 1.0f;
    for (int i = 0; i < 2 * RATE; i++)
        f[i] = 1.0f;

    /* Idle: samples are untouched. */
    spotiya_duck_apply(&gain, 0, f, sizeof(f), RATE, 2, SPOTIYA_DUCK_FLOAT32);
    assert(f[0] == 1.0f && f[2 * RATE - 1] == 1.0f);

    /* Attack: monotonic, stereo-linked, reaches the level after 80 ms. */
    spotiya_duck_apply(&gain, 1, f, sizeof(f), RATE, 2, SPOTIYA_DUCK_FLOAT32);
    assert(f[0] < 1.0f && f[0] > 0.99f);
    for (int i = 0; i < RATE - 1; i++) {
        assert(f[2 * i] == f[2 * i + 1]);
        assert(f[2 * i] >= f[2 * i + 2]);
    }
    assert(fabsf(f[2 * (RATE * 8 / 100)] - 0.2f) < 1e-3f);
    assert(fabsf(f[2 * RATE - 1] - 0.2f) < 1e-6f);

    /* The ramp does not depend on how the stream is split into buffers. */
    float whole = 1.0f, split = 1.0f;
    static float a[2 * 4096], b[2 * 4096];
    for (int i = 0; i < 2 * 4096; i++)
        a[i] = b[i] = 1.0f;
    spotiya_duck_apply(&whole, 1, a, sizeof(a), RATE, 2, SPOTIYA_DUCK_FLOAT32);
    for (int i = 0; i < 4096; i += 128)
        spotiya_duck_apply(&split, 1, b + 2 * i, 2 * 128 * sizeof(float), RATE, 2, SPOTIYA_DUCK_FLOAT32);
    for (int i = 0; i < 2 * 4096; i++)
        assert(a[i] == b[i]);

    /* Release: back to unity after 300 ms. */
    for (int i = 0; i < 2 * RATE; i++)
        f[i] = 1.0f;
    spotiya_duck_apply(&gain, 0, f, sizeof(f), RATE, 2, SPOTIYA_DUCK_FLOAT32);
    assert(f[0] > 0.2f && f[0] < 0.21f);
    assert(fabsf(f[2 * (RATE * 3 / 10)] - 1.0f) < 1e-3f);
    assert(f[2 * (RATE * 31 / 100)] == 1.0f && gain == 1.0f);

    /* S16 and a trailing partial frame. */
    gain = 0.2f;
    for (int i = 0; i < 2 * RATE; i++)
        s[i] = 10000;
    spotiya_duck_apply(&gain, 1, s, 4 * 100 + 3, RATE, 2, SPOTIYA_DUCK_S16);
    assert(s[0] == 2000 && s[199] == 2000 && s[200] == 10000);

    puts("apulse duck: ok");
    return 0;
}
