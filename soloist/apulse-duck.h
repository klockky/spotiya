#ifndef SPOTIYA_APULSE_DUCK_H
#define SPOTIYA_APULSE_DUCK_H
#include <stddef.h>

/* Fades playback to SPOTIYA_DUCK_LEVEL while Spotiya reports that Alice is busy.
 * format: 0 = FLOAT32 native endian, 1 = S16 native endian. */
enum spotiya_duck_format { SPOTIYA_DUCK_FLOAT32, SPOTIYA_DUCK_S16 };
void spotiya_duck(void *pcm, size_t bytes, unsigned rate, unsigned channels,
                  enum spotiya_duck_format format);
/* Same ramp with an explicit state; used by the host test. */
void spotiya_duck_apply(float *gain, int busy, void *pcm, size_t bytes, unsigned rate,
                        unsigned channels, enum spotiya_duck_format format);
#endif
