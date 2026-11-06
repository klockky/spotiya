#ifndef SPOTIYA_APULSE_SPECTRUM_H
#define SPOTIYA_APULSE_SPECTRUM_H
#include <stddef.h>
struct spotiya_spectrum;
enum spotiya_sample { SPOTIYA_FLOAT32, SPOTIYA_S16 };
struct spotiya_spectrum *spotiya_spectrum_new(unsigned rate, unsigned channels);
void spotiya_spectrum_free(struct spotiya_spectrum *s);
void spotiya_spectrum_reset(struct spotiya_spectrum *s);
void spotiya_spectrum_feed(struct spotiya_spectrum *s, const void *pcm,
                          size_t frames, enum spotiya_sample format);
#endif
