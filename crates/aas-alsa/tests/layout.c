/* Prints the layout and constants of the ALSA ioplug SDK that src/ffi.rs
 * declares by hand. tests/layout.rs compiles this against the installed
 * headers and compares. */
#include <stddef.h>
#include <stdio.h>
#include <alsa/asoundlib.h>
#include <alsa/pcm_external.h>

#define SIZE(t) printf("size %s %zu\n", #t, sizeof(t))
#define OFFSET(t, f) printf("offset %s.%s %zu\n", #t, #f, offsetof(t, f))
#define VALUE(c) printf("value %s %ld\n", #c, (long)(c))

int main(void)
{
	SIZE(snd_pcm_ioplug_t);
	OFFSET(snd_pcm_ioplug_t, version);
	OFFSET(snd_pcm_ioplug_t, name);
	OFFSET(snd_pcm_ioplug_t, flags);
	OFFSET(snd_pcm_ioplug_t, poll_fd);
	OFFSET(snd_pcm_ioplug_t, poll_events);
	OFFSET(snd_pcm_ioplug_t, mmap_rw);
	OFFSET(snd_pcm_ioplug_t, callback);
	OFFSET(snd_pcm_ioplug_t, private_data);
	OFFSET(snd_pcm_ioplug_t, pcm);
	OFFSET(snd_pcm_ioplug_t, stream);
	OFFSET(snd_pcm_ioplug_t, state);
	OFFSET(snd_pcm_ioplug_t, appl_ptr);
	OFFSET(snd_pcm_ioplug_t, hw_ptr);
	OFFSET(snd_pcm_ioplug_t, nonblock);
	OFFSET(snd_pcm_ioplug_t, access);
	OFFSET(snd_pcm_ioplug_t, format);
	OFFSET(snd_pcm_ioplug_t, channels);
	OFFSET(snd_pcm_ioplug_t, rate);
	OFFSET(snd_pcm_ioplug_t, period_size);
	OFFSET(snd_pcm_ioplug_t, buffer_size);

	SIZE(snd_pcm_ioplug_callback_t);
	OFFSET(snd_pcm_ioplug_callback_t, start);
	OFFSET(snd_pcm_ioplug_callback_t, stop);
	OFFSET(snd_pcm_ioplug_callback_t, pointer);
	OFFSET(snd_pcm_ioplug_callback_t, transfer);
	OFFSET(snd_pcm_ioplug_callback_t, close);
	OFFSET(snd_pcm_ioplug_callback_t, hw_params);
	OFFSET(snd_pcm_ioplug_callback_t, hw_free);
	OFFSET(snd_pcm_ioplug_callback_t, sw_params);
	OFFSET(snd_pcm_ioplug_callback_t, prepare);
	OFFSET(snd_pcm_ioplug_callback_t, drain);
	OFFSET(snd_pcm_ioplug_callback_t, pause);
	OFFSET(snd_pcm_ioplug_callback_t, resume);
	OFFSET(snd_pcm_ioplug_callback_t, poll_descriptors_count);
	OFFSET(snd_pcm_ioplug_callback_t, poll_descriptors);
	OFFSET(snd_pcm_ioplug_callback_t, poll_revents);
	OFFSET(snd_pcm_ioplug_callback_t, dump);
	OFFSET(snd_pcm_ioplug_callback_t, delay);
	OFFSET(snd_pcm_ioplug_callback_t, query_chmaps);
	OFFSET(snd_pcm_ioplug_callback_t, get_chmap);
	OFFSET(snd_pcm_ioplug_callback_t, set_chmap);

	SIZE(snd_pcm_channel_area_t);
	OFFSET(snd_pcm_channel_area_t, addr);
	OFFSET(snd_pcm_channel_area_t, first);
	OFFSET(snd_pcm_channel_area_t, step);

	SIZE(snd_pcm_uframes_t);
	SIZE(snd_pcm_sframes_t);
	SIZE(snd_pcm_stream_t);
	SIZE(snd_pcm_state_t);
	SIZE(snd_pcm_access_t);
	SIZE(snd_pcm_format_t);

	VALUE(SND_PCM_IOPLUG_VERSION);
	VALUE(SND_PCM_IOPLUG_FLAG_BOUNDARY_WA);
	VALUE(SND_PCM_IOPLUG_HW_ACCESS);
	VALUE(SND_PCM_IOPLUG_HW_FORMAT);
	VALUE(SND_PCM_IOPLUG_HW_CHANNELS);
	VALUE(SND_PCM_IOPLUG_HW_RATE);
	VALUE(SND_PCM_IOPLUG_HW_PERIOD_BYTES);
	VALUE(SND_PCM_IOPLUG_HW_BUFFER_BYTES);
	VALUE(SND_PCM_IOPLUG_HW_PERIODS);
	VALUE(SND_PCM_STREAM_PLAYBACK);
	VALUE(SND_PCM_NONBLOCK);
	VALUE(SND_PCM_STATE_SETUP);
	VALUE(SND_PCM_STATE_PREPARED);
	VALUE(SND_PCM_STATE_RUNNING);
	VALUE(SND_PCM_STATE_XRUN);
	VALUE(SND_PCM_STATE_DRAINING);
	VALUE(SND_PCM_STATE_PAUSED);
	VALUE(SND_PCM_STATE_DISCONNECTED);
	VALUE(SND_PCM_ACCESS_MMAP_INTERLEAVED);
	VALUE(SND_PCM_ACCESS_RW_INTERLEAVED);
	VALUE(SND_PCM_FORMAT_S16_LE);
	VALUE(SND_PCM_FORMAT_S24_LE);
	VALUE(SND_PCM_FORMAT_S24_3LE);
	VALUE(SND_PCM_FORMAT_S32_LE);
	VALUE(SND_PCM_FORMAT_FLOAT_LE);
	return 0;
}
