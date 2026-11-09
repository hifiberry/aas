#!/bin/sh
# Board tests for the aas ALSA plugin. Runs on the test board, as a member
# of group audio that may sudo, with the daemon running as aas.service and
# the plugin installed by dist/install-plugin.sh. Needs aplay, amixer,
# alsactl, cc and libasound2-dev, and for the mixer case mpd, mpc and
# python3. Each case prints PASS or FAIL and its name. The exit status is
# the number of cases that failed.
set -u
# alsactl is in sbin.
PATH=$PATH:/usr/sbin

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

# A helper for what aplay cannot show: pause, delay, underrun, polling
# with descriptors fetched once, and a non-blocking drain.
cat >"$work/pcmtest.c" <<'EOF'
#include <alsa/asoundlib.h>
#include <fcntl.h>
#include <poll.h>
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

static int setup(const char *device, int mode, int *can_pause)
{
	snd_pcm_hw_params_t *hw;
	int err;

	snd_pcm_hw_params_alloca(&hw);
	if ((err = snd_pcm_open(&pcm, device, SND_PCM_STREAM_PLAYBACK, mode)) < 0 ||
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

/* Writes silence. Returns 0, or the error, which it prints. A
 * non-blocking PCM waits in snd_pcm_wait for room. */
static int put(long frames)
{
	while (frames > 0) {
		long n = frames < (long)period ? frames : (long)period;
		snd_pcm_sframes_t r = snd_pcm_writei(pcm, silence, n);
		if (r == -EAGAIN) {
			snd_pcm_wait(pcm, 1000);
			continue;
		}
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

/* Plays `frames` by polling the descriptors in `pfd`, as a player with
 * its own event loop does. Fails on a poll that times out, an error, or
 * so many wake-ups that the descriptors must be stale. */
static int play_polled(struct pollfd *pfd, int n, long frames, int *polls)
{
	long done = 0;

	*polls = 0;
	while (done < frames) {
		unsigned short revents = 0;
		if (++*polls > 2000) {
			printf("over 2000 wake-ups for %ld frames\n", frames);
			return 1;
		}
		if (poll(pfd, n, 1000) <= 0) {
			printf("no wake-up within 1 s\n");
			return 1;
		}
		if (snd_pcm_poll_descriptors_revents(pcm, pfd, n, &revents) < 0 ||
		    (revents & POLLERR)) {
			printf("poll error after %ld frames\n", done);
			return 1;
		}
		if (!(revents & POLLOUT))
			continue;
		snd_pcm_sframes_t avail = snd_pcm_avail_update(pcm);
		if (avail < 0) {
			printf("avail: %s\n", snd_strerror(avail));
			return 1;
		}
		long n2 = avail < frames - done ? avail : frames - done;
		n2 = n2 > 16384 ? 16384 : n2;
		snd_pcm_sframes_t r = snd_pcm_writei(pcm, silence, n2);
		if (r < 0 && r != -EAGAIN) {
			printf("write: %s\n", snd_strerror(r));
			return 1;
		}
		done += r > 0 ? r : 0;
	}
	return 0;
}

/* A player fetches the poll descriptors once. The stream is then set up
 * again, after an xrun and after a seek, and the player keeps polling what
 * it fetched. A descriptor freed before the reset makes the fresh ones
 * land on other numbers if the plugin does not keep its own. */
static int stable_test(int spare)
{
	struct pollfd cached[8], fresh[8];
	int n = snd_pcm_poll_descriptors_count(pcm), polls, i;

	if (n <= 0 || n > 8 || snd_pcm_poll_descriptors(pcm, cached, n) != n) {
		printf("no poll descriptors\n");
		return 1;
	}
	close(spare);
	if (put(rate / 4))
		return 1;
	usleep(300000);
	if (snd_pcm_writei(pcm, silence, period) != -EPIPE || snd_pcm_prepare(pcm) < 0) {
		printf("no xrun to recover from\n");
		return 1;
	}
	if (put(rate / 4) || snd_pcm_drop(pcm) < 0 || snd_pcm_prepare(pcm) < 0) {
		printf("seek failed\n");
		return 1;
	}
	snd_pcm_poll_descriptors(pcm, fresh, n);
	for (i = 0; i < n; i++) {
		if (fresh[i].fd != cached[i].fd) {
			printf("descriptor %d moved from %d to %d\n", i, cached[i].fd, fresh[i].fd);
			return 1;
		}
	}
	if (play_polled(cached, n, rate, &polls))
		return 1;
	snd_pcm_drain(pcm);
	printf("descriptors %d and %d kept through an xrun and a seek; 1 s played "
	       "polling them, %d wake-ups\n", cached[0].fd, cached[1].fd, polls);
	return 0;
}

/* A non-blocking drain returns -EAGAIN at once, and the PCM leaves
 * DRAINING when the audio has played, as alsa-lib's ioplug does. A drop
 * during the drain cancels it, and the PCM plays on. */
static int nonblock_drain_test(void)
{
	struct pollfd pfd[8];
	int n = snd_pcm_poll_descriptors_count(pcm), err, calls = 1;
	double t0, first, done;
	snd_pcm_sframes_t queued;

	if (put(rate))
		return 1;
	queued = (long)buffer - snd_pcm_avail(pcm);
	t0 = now();
	err = snd_pcm_drain(pcm);
	first = now() - t0;
	if (err != -EAGAIN) {
		printf("non-blocking drain gave %d, not -EAGAIN\n", err);
		return 1;
	}
	snd_pcm_poll_descriptors(pcm, pfd, n);
	while (snd_pcm_state(pcm) == SND_PCM_STATE_DRAINING) {
		unsigned short revents = 0;
		if (now() - t0 > 3.0 || poll(pfd, n, 1000) <= 0) {
			printf("the drain did not end\n");
			return 1;
		}
		snd_pcm_poll_descriptors_revents(pcm, pfd, n, &revents);
		if (revents & POLLERR) {
			printf("poll error while draining\n");
			return 1;
		}
		if (revents & POLLOUT) {
			calls++;
			err = snd_pcm_drain(pcm);
			if (err < 0 && err != -EAGAIN) {
				printf("drain: %s\n", snd_strerror(err));
				return 1;
			}
		}
	}
	done = now() - t0;
	double least = queued / (double)rate;
	if (first > 0.05 || done < least - 0.01 || done > least + 0.3) {
		printf("first call took %.0f ms; drained in %.0f ms with %.0f ms queued\n",
		       first * 1000, done * 1000, least * 1000);
		return 1;
	}
	/* Cancel one: drop while it drains, then play on and drain blocking. */
	if (snd_pcm_prepare(pcm) < 0 || put(rate / 2) || snd_pcm_drain(pcm) != -EAGAIN ||
	    snd_pcm_drop(pcm) < 0 || snd_pcm_prepare(pcm) < 0 || put(rate / 4)) {
		printf("drop during a drain did not leave the PCM usable\n");
		return 1;
	}
	snd_pcm_nonblock(pcm, 0);
	if ((err = snd_pcm_drain(pcm)) < 0) {
		printf("blocking drain after the cancel: %s\n", snd_strerror(err));
		return 1;
	}
	printf("-EAGAIN after %.1f ms, drained in %.0f ms with %.0f ms queued, %d calls; "
	       "a drop cancelled the next drain and the PCM played on\n",
	       first * 1000, done * 1000, least * 1000, calls);
	return 0;
}

int main(int argc, char **argv)
{
	int can_pause = 0, mode = 0, spare = -1;

	if (argc != 3)
		return 2;
	if (!strcmp(argv[1], "nonblock"))
		mode = SND_PCM_NONBLOCK;
	/* Held while the PCM is set up, freed before it is set up again. */
	if (!strcmp(argv[1], "stable"))
		spare = open("/dev/null", O_RDONLY);
	if (setup(argv[2], mode, &can_pause))
		return 2;
	if (!strcmp(argv[1], "stable"))
		return stable_test(spare);
	if (!strcmp(argv[1], "nonblock"))
		return nonblock_drain_test();
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

# A helper that watches the default control as mixers do: it polls the
# descriptors, asks what they mean and reads events, until a read fails.
cat >"$work/ctlwatch.c" <<'EOF'
#include <alsa/asoundlib.h>
#include <poll.h>
#include <stdio.h>

int main(void)
{
	snd_ctl_t *ctl;
	snd_ctl_event_t *event;
	struct pollfd pfd[4];
	int n, err, events = 0, wakes;

	snd_ctl_event_alloca(&event);
	if ((err = snd_ctl_open(&ctl, "default", 0)) < 0 ||
	    (err = snd_ctl_subscribe_events(ctl, 1)) < 0) {
		printf("open: %s\n", snd_strerror(err));
		return 2;
	}
	n = snd_ctl_poll_descriptors(ctl, pfd, 4);
	printf("ready\n");
	fflush(stdout);
	for (wakes = 1; wakes <= 1000; wakes++) {
		unsigned short revents = 0;
		if (poll(pfd, n, 5000) <= 0) {
			printf("no wake-up within 5 s\n");
			return 1;
		}
		snd_ctl_poll_descriptors_revents(ctl, pfd, n, &revents);
		while ((err = snd_ctl_read(ctl, event)) > 0)
			events++;
		if (err < 0 && err != -EAGAIN) {
			printf("%d events, then poll gave%s POLLERR and the read %s, wake-up %d\n",
			       events, revents & POLLERR ? "" : " no", snd_strerror(err), wakes);
			return !(revents & POLLERR) || events < 1;
		}
	}
	printf("no error after 1000 wake-ups\n");
	return 1;
}
EOF
if ! cc -O2 -o "$work/ctlwatch" "$work/ctlwatch.c" -lasound; then
	echo "FAIL setup: cannot build the control helper"
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

case_poll_descriptors_survive_a_reset() {
	name=poll_descriptors_survive_a_reset
	if out=$("$work/pcmtest" stable aas); then pass $name "$out"; else fail $name "$out"; fi
}

case_nonblocking_drain() {
	name=nonblocking_drain
	if out=$("$work/pcmtest" nonblock aas); then pass $name "$out"; else fail $name "$out"; fi
}

# A buffer smaller than the daemon can keep fed is rounded up, or refused
# at hw_params. One that is taken plays without an underrun.
case_small_buffer_rounds_up_or_is_refused() {
	name=small_buffer_rounds_up_or_is_refused
	bad=""
	report=""
	for spec in "S16_LE 48000" "S32_LE 48000" "S32_LE 192000"; do
		set -- $spec
		start=$(now_ms)
		if aplay -v -D aas -t raw -f $1 -c 2 -r $2 --buffer-size=64 -d 1 /dev/zero \
			>"$work/out" 2>&1; then
			took=$(($(now_ms) - start))
			size=$(sed -n 's/^ *buffer_size *: *//p' "$work/out" | head -n1)
			if grep -qi underrun "$work/out" || [ $took -gt 1300 ]; then
				bad="$bad $1/$2(${size} frames, $took ms, underrun or slow)"
			else
				report="$report $1/$2: buffer ${size} frames, played in $took ms;"
			fi
		elif grep -q "hw params\|hw_params" "$work/out"; then
			report="$report $1/$2: refused at hw_params;"
		else
			bad="$bad $1/$2($(tr '\n' ' ' <"$work/out"))"
		fi
	done
	report="${report# }"
	if [ -z "$bad" ]; then pass $name "${report%;}"; else fail $name "$bad"; fi
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

# The daemon's volume in percent, read through the control.
master() {
	amixer -D default sget Master | sed -n 's/.*Playback [0-9]* \[\([0-9]*\)%\].*/\1/p' | head -n1
}

case_amixer_reads_and_sets_master() {
	name=amixer_reads_and_sets_master
	if ! amixer -q -D default sset Master 50% 2>"$work/err"; then
		fail $name "sset failed: $(cat "$work/err")"
		return
	fi
	line=$(amixer -D default sget Master | grep '\[[0-9]*%\]')
	if [ "$(master)" = 50 ] && echo "$line" | grep -q -- '-30.00dB'; then
		pass $name "sget after sset 50%:$line"
	else
		fail $name "sget after sset 50%:$line"
	fi
}

# A client watching the control is told of a change another one makes.
case_second_client_sees_the_change() {
	name=second_client_sees_the_change
	stdbuf -oL alsactl monitor default >"$work/monitor" 2>&1 &
	monitor=$!
	sleep 0.5
	amixer -q -D default sset Master 30%
	i=0
	while [ $i -lt 20 ] && ! grep -q 'Master Playback Volume' "$work/monitor"; do
		sleep 0.1
		i=$((i + 1))
	done
	kill $monitor 2>/dev/null
	wait $monitor 2>/dev/null
	line=$(grep 'Master Playback Volume' "$work/monitor" | head -n1)
	if [ -n "$line" ]; then
		pass $name "alsactl monitor: $line"
	else
		fail $name "alsactl monitor saw nothing: $(tr '\n' ' ' <"$work/monitor")"
	fi
}

# mpd's hardware mixer on the control. mpd opens its mixer with the
# output, so it plays silence meanwhile.
case_mpd_mixer_works() {
	name=mpd_mixer_works
	for tool in mpd mpc python3; do
		if ! command -v $tool >/dev/null; then
			fail $name "$tool is not installed"
			return
		fi
	done
	mkdir -p "$work/music"
	python3 -c '
import sys, wave
w = wave.open(sys.argv[1], "wb")
w.setnchannels(2)
w.setsampwidth(2)
w.setframerate(48000)
w.writeframes(bytes(4 * 48000 * 10))
' "$work/music/silence.wav"
	cat >"$work/mpd.conf" <<-CONF
	music_directory "$work/music"
	db_file "$work/mpd.db"
	log_file "$work/mpd.log"
	pid_file "$work/mpd.pid"
	bind_to_address "$work/mpd.sock"
	audio_output {
		type "alsa"
		name "aas"
		device "default"
		mixer_type "hardware"
		mixer_device "default"
		mixer_control "Master"
	}
	CONF
	amixer -q -D default sset Master 50%
	mpd --no-daemon "$work/mpd.conf" 2>"$work/err" &
	mpd=$!
	i=0
	while [ $i -lt 50 ] && [ ! -S "$work/mpd.sock" ]; do
		sleep 0.1
		i=$((i + 1))
	done
	export MPD_HOST="$work/mpd.sock"
	mpc -q update --wait && mpc -q add silence.wav && mpc -q repeat on && mpc -q play
	sleep 1
	mpc -q volume 40
	sleep 0.3
	reported=$(mpc volume)
	daemon=$(master)
	mpc -q stop
	unset MPD_HOST
	kill $mpd 2>/dev/null
	wait $mpd 2>/dev/null
	if [ "$reported" = "volume: 40%" ] && [ -n "$daemon" ] && [ "$daemon" != 50 ]; then
		pass $name "mpc volume 40 moved the daemon from 50% to $daemon%; mpc reads $reported"
	else
		log=$(tail -n3 "$work/mpd.log" 2>/dev/null | tr '\n' ' ')
		fail $name "mpc reads '$reported', daemon at '$daemon%': $(tr '\n' ' ' <"$work/err") $log"
	fi
}

case_volume_survives_a_restart() {
	name=volume_survives_a_restart
	amixer -q -D default sset Master 50%
	daemon=$(main_pid)
	sudo systemctl restart aas
	if ! wait_daemon; then
		fail $name "the daemon did not come back"
		return
	fi
	if [ "$(master)" = 50 ] && [ "$(main_pid)" != "$daemon" ]; then
		pass $name "50% after the restart, pid $daemon then $(main_pid)"
	else
		fail $name "$(master)% after the restart, pid $daemon then $(main_pid)"
	fi
}

# With the daemon gone, the control fails at once instead of hanging.
case_amixer_fails_without_the_daemon() {
	name=amixer_fails_without_the_daemon
	sudo systemctl stop aas
	start=$(now_ms)
	timeout 5 amixer -D default sget Master >"$work/out" 2>&1
	status=$?
	took=$(($(now_ms) - start))
	sudo systemctl start aas
	why=$(head -n2 "$work/out" | tr '\n' ' ')
	if ! wait_daemon; then
		fail $name "the daemon did not come back"
	elif [ $status -ne 0 ] && [ $status -ne 124 ] && [ $took -le 1000 ]; then
		pass $name "amixer exited $status after $took ms: $why"
	else
		fail $name "amixer exited $status after $took ms: $why"
	fi
}

# A client watching the control hears when the daemon goes: poll reports
# POLLERR and reading an event fails, as for a card that is unplugged.
case_watcher_hears_the_daemon_go() {
	name=watcher_hears_the_daemon_go
	"$work/ctlwatch" >"$work/watch" 2>&1 &
	watcher=$!
	i=0
	while [ $i -lt 50 ] && ! grep -q ready "$work/watch"; do
		sleep 0.1
		i=$((i + 1))
	done
	amixer -q -D default sset Master 40%
	sleep 0.3
	start=$(now_ms)
	sudo systemctl kill -s KILL aas
	if ! wait_gone $watcher 3000; then
		kill -9 $watcher
		wait $watcher 2>/dev/null
		fail $name "the watcher still waits 3 s after the daemon was killed"
		wait_daemon
		return
	fi
	took=$(($(now_ms) - start))
	wait $watcher
	status=$?
	why=$(tr '\n' ' ' <"$work/watch")
	if ! wait_daemon; then
		fail $name "the daemon did not come back"
	elif [ $status -eq 0 ] && [ $took -le 1000 ]; then
		pass $name "after $took ms: $why"
	else
		fail $name "exited $status after $took ms: $why"
	fi
}

case_aplay_plays_each_format
case_drain_returns
case_pause_and_resume
case_delay_is_plausible
case_underrun_returns_epipe
case_poll_descriptors_survive_a_reset
case_nonblocking_drain
case_small_buffer_rounds_up_or_is_refused
case_daemon_killed_during_write_returns_an_error
case_client_reconnects_after_restart
case_player_killed_keeps_daemon_up
case_amixer_reads_and_sets_master
case_second_client_sees_the_change
case_mpd_mixer_works
case_volume_survives_a_restart
case_amixer_fails_without_the_daemon
case_watcher_hears_the_daemon_go
# Full volume again, for whatever plays next.
amixer -q -D default sset Master 100%
exit $failed
