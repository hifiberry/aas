#!/bin/sh
# Board tests for the aas ALSA plugin. Runs on the test board, as a member
# of group audio that may sudo, with the daemon running as aas.service and
# the plugin installed by dist/install-plugin.sh. Needs aplay, cc and
# libasound2-dev. Each case prints PASS or FAIL and its name. The exit
# status is the number of cases that failed.
set -u

rate=48000
work=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$work"' EXIT
failed=0

pass() { echo "PASS $1${2:+: $2}"; }
fail() { echo "FAIL $1${2:+: $2}"; failed=$((failed + 1)); }
now_ms() { echo $(($(date +%s%N) / 1000000)); }
main_pid() { systemctl show -p MainPID --value aas; }

# Waits up to $2 ms for process $1 to exit. Returns 1 if it is still there.
wait_gone() {
	waited=0
	while kill -0 "$1" 2>/dev/null; do
		[ "$waited" -ge "$2" ] && return 1
		sleep 0.01
		waited=$((waited + 10))
	done
	return 0
}

# Waits up to 5 s for the daemon to take connections.
wait_daemon() {
	i=0
	while [ $i -lt 50 ]; do
		if systemctl -q is-active aas && [ -S /run/aas/aas.sock ]; then
			return 0
		fi
		sleep 0.1
		i=$((i + 1))
	done
	return 1
}

# A helper for what aplay cannot show: pause, delay and underrun.
cat >"$work/pcmtest.c" <<'EOF'
#include <alsa/asoundlib.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static snd_pcm_t *pcm;
static unsigned int rate = 48000;
static snd_pcm_uframes_t period = 1024, buffer = 4096;
static long written;
static short silence[16384 * 2];

static double now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

static int setup(const char *device, int *can_pause)
{
	snd_pcm_hw_params_t *hw;
	int err;

	snd_pcm_hw_params_alloca(&hw);
	if ((err = snd_pcm_open(&pcm, device, SND_PCM_STREAM_PLAYBACK, 0)) < 0 ||
	    (err = snd_pcm_hw_params_any(pcm, hw)) < 0 ||
	    (err = snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED)) < 0 ||
	    (err = snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE)) < 0 ||
	    (err = snd_pcm_hw_params_set_channels(pcm, hw, 2)) < 0 ||
	    (err = snd_pcm_hw_params_set_rate(pcm, hw, rate, 0)) < 0 ||
	    (err = snd_pcm_hw_params_set_period_size_near(pcm, hw, &period, 0)) < 0 ||
	    (err = snd_pcm_hw_params_set_buffer_size_near(pcm, hw, &buffer)) < 0 ||
	    (err = snd_pcm_hw_params(pcm, hw)) < 0) {
		printf("setup: %s\n", snd_strerror(err));
		return -1;
	}
	*can_pause = snd_pcm_hw_params_can_pause(hw);
	return 0;
}

/* Writes silence. Returns 0, or the error, which it prints. */
static int put(long frames)
{
	while (frames > 0) {
		long n = frames < (long)period ? frames : (long)period;
		snd_pcm_sframes_t r = snd_pcm_writei(pcm, silence, n);
		if (r < 0) {
			printf("write: %s\n", snd_strerror(r));
			return r;
		}
		written += r;
		frames -= r;
	}
	return 0;
}

/* Frames the daemon has taken: written less what still waits in the buffer. */
static long position(void)
{
	snd_pcm_sframes_t avail = snd_pcm_avail(pcm);
	return avail < 0 ? -1 : written - ((long)buffer - avail);
}

static int pause_test(void)
{
	long paused, later, resumed;
	snd_pcm_sframes_t d1, d2;

	if (put(rate))
		return 1;
	if (snd_pcm_pause(pcm, 1) < 0) {
		printf("pause refused\n");
		return 1;
	}
	paused = position();
	snd_pcm_delay(pcm, &d1);
	usleep(500000);
	later = position();
	snd_pcm_delay(pcm, &d2);
	if (snd_pcm_state(pcm) != SND_PCM_STATE_PAUSED || snd_pcm_pause(pcm, 0) < 0) {
		printf("not paused\n");
		return 1;
	}
	if (put(rate / 2))
		return 1;
	resumed = position();
	snd_pcm_drain(pcm);
	printf("position %ld at pause, %ld 500 ms later, %ld after 500 ms more played; "
	       "delay %ld then %ld\n", paused, later, resumed, (long)d1, (long)d2);
	return !(paused >= 0 && later == paused && d1 == d2 && resumed > later + (long)rate / 4);
}

/* Frames written less frames delayed is what the listener has heard, which
 * grows with the clock. So written - delay - rate * t stays constant. The
 * delay is read at varied times after the buffer was filled, from full to
 * nearly empty, so a delay that ignores what has played since fails. */
static int delay_test(void)
{
	double t0, t;
	double low = 1e18, high = -1e18;
	snd_pcm_sframes_t d = 0, dmin = 1 << 30, dmax = 0, avail;
	long after, amin = 1 << 30, amax = -(1 << 30);
	long fill_us = (long)((buffer - period) * 1e6 / rate);
	int i = 0;

	if (put(rate / 2))
		return 1;
	t0 = now();
	do {
		if (put(buffer))
			return 1;
		usleep(fill_us * (i++ % 8) / 8);
		avail = snd_pcm_avail(pcm);
		if (avail < 0 || snd_pcm_delay(pcm, &d) < 0) {
			printf("avail or delay failed\n");
			return 1;
		}
		t = now();
		double x = written - d - rate * (t - t0);
		low = x < low ? x : low;
		high = x > high ? x : high;
		dmin = d < dmin ? d : dmin;
		dmax = d > dmax ? d : dmax;
		/* What the delay holds beyond the player's own buffer. */
		after = d - ((long)buffer - avail);
		amin = after < amin ? after : amin;
		amax = after > amax ? after : amax;
	} while (t - t0 < 3.0);
	snd_pcm_drop(pcm);
	printf("delay %ld to %ld frames, %ld to %ld of them after the buffer; "
	       "buffer %lu, period %lu; written - delay drifts %.0f frames from the clock\n",
	       (long)dmin, (long)dmax, amin, amax, buffer, period, high - low);
	return !(dmin > 0 && amin >= 0 && amax <= (long)rate / 20 && high - low <= period);
}

/* A player that falls behind gets -EPIPE, and recovers with prepare. */
static int underrun_test(void)
{
	int err;

	if (put(rate / 2))
		return 1;
	usleep(300000);
	err = snd_pcm_writei(pcm, silence, period);
	if (err != -EPIPE) {
		printf("write after the buffer ran dry gave %d, not -EPIPE\n", err);
		return 1;
	}
	if ((err = snd_pcm_prepare(pcm)) < 0) {
		printf("prepare: %s\n", snd_strerror(err));
		return 1;
	}
	if (put(rate / 4))
		return 1;
	snd_pcm_drain(pcm);
	printf("-EPIPE after the buffer ran dry, then played on after prepare\n");
	return 0;
}

int main(int argc, char **argv)
{
	int can_pause = 0;

	if (argc != 3 || setup(argv[2], &can_pause))
		return 2;
	if (!strcmp(argv[1], "pause")) {
		if (!can_pause) {
			printf("the PCM cannot pause\n");
			return 1;
		}
		return pause_test();
	}
	if (!strcmp(argv[1], "delay"))
		return delay_test();
	if (!strcmp(argv[1], "underrun"))
		return underrun_test();
	return 2;
}
EOF
if ! cc -O2 -o "$work/pcmtest" "$work/pcmtest.c" -lasound; then
	echo "FAIL setup: cannot build the helper"
	exit 1
fi
if ! wait_daemon; then
	echo "FAIL setup: aas.service is not running"
	exit 1
fi

# One second of silence at 48 kHz, S16 stereo. Played with -d 1: aplay pads
# a file that ends on a period boundary with a period of silence, which -d
# leaves out.
play_second() { aplay -q -D "$1" -t raw -f S16_LE -c 2 -r $rate -d 1 /dev/zero; }

case_aplay_plays_each_format() {
	name=aplay_plays_each_format
	bad=""
	for format in S16_LE S24_LE S24_3LE S32_LE FLOAT_LE; do
		for channels in 1 2; do
			# Straight to the plugin, so plug converts nothing.
			if ! aplay -q -D aas -t raw -f $format -c $channels -r $rate -d 1 /dev/zero \
				2>"$work/err"; then
				bad="$bad $format/$channels($(head -n1 "$work/err"))"
			fi
		done
	done
	if [ -z "$bad" ]; then pass $name; else fail $name "failed:$bad"; fi
}

case_drain_returns() {
	name=drain_returns
	start=$(now_ms)
	if ! play_second default 2>"$work/err"; then
		fail $name "aplay failed: $(cat "$work/err")"
		return
	fi
	took=$(($(now_ms) - start))
	if [ $took -ge 980 ] && [ $took -le 1200 ]; then
		pass $name "1 s played in $took ms"
	else
		fail $name "1 s played in $took ms"
	fi
}

case_pause_and_resume() {
	name=pause_and_resume
	if out=$("$work/pcmtest" pause aas); then pass $name "$out"; else fail $name "$out"; fi
}

case_delay_is_plausible() {
	name=delay_is_plausible
	if out=$("$work/pcmtest" delay aas); then pass $name "$out"; else fail $name "$out"; fi
}

case_underrun_returns_epipe() {
	name=underrun_returns_epipe
	if out=$("$work/pcmtest" underrun aas); then pass $name "$out"; else fail $name "$out"; fi
}

case_daemon_killed_during_write_returns_an_error() {
	name=daemon_killed_during_write_returns_an_error
	aplay -q -D default -t raw -f S16_LE -c 2 -r $rate /dev/zero 2>"$work/err" &
	player=$!
	sleep 2
	if ! kill -0 $player 2>/dev/null; then
		fail $name "aplay stopped early: $(cat "$work/err")"
		return
	fi
	start=$(now_ms)
	sudo systemctl kill -s KILL aas
	if ! wait_gone $player 3000; then
		kill -9 $player
		wait $player
		fail $name "aplay still running 3 s after the daemon was killed"
		return
	fi
	took=$(($(now_ms) - start))
	wait $player
	status=$?
	why=$(tr '\n' ' ' <"$work/err")
	if [ $status -ne 0 ] && [ $took -le 1000 ]; then
		pass $name "aplay exited $status after $took ms: $why"
	else
		fail $name "aplay exited $status after $took ms: $why"
	fi
}

case_client_reconnects_after_restart() {
	name=client_reconnects_after_restart
	if ! wait_daemon; then
		fail $name "the daemon did not come back"
		return
	fi
	if play_second default 2>"$work/err"; then
		pass $name "daemon restarted as pid $(main_pid)"
	else
		fail $name "aplay failed: $(cat "$work/err")"
	fi
}

case_player_killed_keeps_daemon_up() {
	name=player_killed_keeps_daemon_up
	daemon=$(main_pid)
	start=$(now_ms)
	aplay -q -D default -t raw -f S16_LE -c 2 -r $rate -d 2 /dev/zero 2>"$work/err" &
	keeper=$!
	aplay -q -D default -t raw -f S16_LE -c 2 -r $rate /dev/zero 2>/dev/null &
	victim=$!
	sleep 0.5
	kill -9 $victim
	wait $victim 2>/dev/null
	wait $keeper
	status=$?
	took=$(($(now_ms) - start))
	if [ $status -eq 0 ] && [ $took -ge 1980 ] && [ $took -le 2600 ] &&
		[ "$(main_pid)" = "$daemon" ] && systemctl -q is-active aas; then
		pass $name "the other aplay played 2 s in $took ms, daemon pid $daemon unchanged"
	else
		fail $name "status $status, $took ms, daemon pid $daemon now $(main_pid): $(cat "$work/err")"
	fi
}

case_aplay_plays_each_format
case_drain_returns
case_pause_and_resume
case_delay_is_plausible
case_underrun_returns_epipe
case_daemon_killed_during_write_returns_an_error
case_client_reconnects_after_restart
case_player_killed_keeps_daemon_up
exit $failed
