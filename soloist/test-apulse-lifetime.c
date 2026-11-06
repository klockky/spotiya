#include <assert.h>
#include <stdio.h>
#include <pulse/pulseaudio.h>

static int callbacks;
static void volume_done(pa_context *context, int success, void *userdata)
{
    assert(context);
    assert(!success);
    callbacks++;
}
static void stream_done(pa_stream *stream, int success, void *userdata)
{
    assert(stream && success);
    callbacks++;
}

int main(void)
{
    pa_mainloop *loop = pa_mainloop_new();
    pa_mainloop_api *api = pa_mainloop_get_api(loop);
    pa_sample_spec spec = { PA_SAMPLE_FLOAT32LE, 44100, 2 };
    for (int i = 0; i < 100; i++) {
        pa_context *context = pa_context_new(api, "lifetime-test");
        pa_stream *stream = pa_stream_new(context, "unused", &spec, NULL);
        pa_context_unref(context);
        pa_stream_unref(stream);

        context = pa_context_new(api, "operation-test");
        pa_cvolume volume;
        pa_cvolume_set(&volume, 2, PA_VOLUME_NORM);
        pa_operation *operation = pa_context_set_sink_input_volume(
            context, 999, &volume, volume_done, NULL);
        pa_context_unref(context);
        pa_mainloop_iterate(loop, 0, NULL);
        assert(pa_operation_get_state(operation) == PA_OPERATION_DONE);
        pa_operation_unref(operation);

        context = pa_context_new(api, "stream-operation-test");
        stream = pa_stream_new(context, "unused", &spec, NULL);
        operation = pa_stream_trigger(stream, stream_done, NULL);
        pa_context_unref(context);
        pa_stream_unref(stream);
        pa_mainloop_iterate(loop, 0, NULL);
        assert(pa_operation_get_state(operation) == PA_OPERATION_DONE);
        pa_operation_unref(operation);

        context = pa_context_new(api, "cancel-test");
        operation = pa_context_set_sink_input_volume(
            context, 999, &volume, volume_done, NULL);
        pa_operation_cancel(operation);
        pa_context_unref(context);
        pa_mainloop_iterate(loop, 0, NULL);
        assert(pa_operation_get_state(operation) == PA_OPERATION_CANCELLED);
        pa_operation_unref(operation);
    }
    assert(callbacks == 200);
    pa_mainloop_free(loop);
    puts("apulse lifetime: 400 cases passed; no audio device opened");
}
