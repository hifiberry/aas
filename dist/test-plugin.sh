#!/bin/sh
# Board tests for the aas ALSA plugin. Runs on the test board, as a member
# of group audio that may sudo, with the daemon running as aas.service and
# the plugin installed by dist/install-plugin.sh. Needs aplay, arecord, amixer,
# alsactl, cc and libasound2-dev, and for the mixer case mpd, mpc and
# python3. Each case prints PASS or FAIL and its name. The exit status is
# the number of cases that failed.
#
# With arguments, only the cases named run: sh dist/test-plugin.sh drain_returns
#
# On a board whose output is wired to its input, set AAS_TEST_CAPTURE to
# the capture device, for example plughw:sndrpihifiberry. The format case
# then plays a tone and checks what the input hears. Without it, the case
# plays silence and checks only that each format plays.
#
# With ANALOG=1 the cases of the analog input run too, on a board whose
# card has one and whose output is wired to it. They restart the daemon
# with the input enabled at -20 dB, so that the loop through the cable
# stays quiet, and put the config back after. Without ANALOG=1 they are
# not run and not counted.
set -u
# alsactl is in sbin.
PATH=$PATH:/usr/sbin

rate=48000
work=$(mktemp -d)
conf=/etc/aas/aas.toml
# What analog_on did to the config, for analog_off to undo: "kept" with
# the old file in $work, "added" where there was none.
analog_conf=""
analog_dir=""
# Full volume again at the end, however the run ends, for whatever plays
# next, and the config the daemon had.
trap 'kill $(jobs -p) 2>/dev/null; analog_off; amixer -q -D default sset Master 100% 2>/dev/null; rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP
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

# Restarts the daemon with the analog input enabled at -20 dB, on top of
# the config it has. A config with an [analog] table of its own is left
# alone and the case fails.
analog_on() {
	if [ -e $conf ]; then
		grep -q '^[[:space:]]*\[analog\]' $conf && return 1
		cp $conf "$work/aas.toml.before"
		analog_conf=kept
	else
		[ -d /etc/aas ] || analog_dir=made
		: >"$work/aas.toml.before"
		analog_conf=added
	fi
	{
		cat "$work/aas.toml.before"
		printf '\n[analog]\nenabled = true\nvolume_db = -20\n'
	} >"$work/analog.toml"
	sudo install -D -m 0644 "$work/analog.toml" $conf || return 1
	# Each case starts the daemon twice. More than five starts in ten
	# seconds would reach systemd's limit.
	sudo systemctl reset-failed aas
	sudo systemctl restart aas
	wait_daemon || return 1
	# The capture reads zeros for its first second, while the ADC settles.
	sleep 1.5
}

# Puts the config back as analog_on found it and restarts the daemon. Does
# nothing if analog_on changed nothing.
analog_off() {
	case $analog_conf in
	kept) sudo install -m 0644 "$work/aas.toml.before" $conf ;;
	added)
		sudo rm -f $conf
		[ "$analog_dir" = made ] && sudo rmdir /etc/aas
		;;
	*) return 0 ;;
	esac
	analog_conf=""
	analog_dir=""
	sudo systemctl reset-failed aas
	sudo systemctl restart aas
	wait_daemon
}

# The log of the daemon that ran as process $1, from its start to its
# "stopped" line, one line each as "<seconds.micro> <message>". By its pid,
# so that a restart's lines from the daemon before cannot be taken for
# its own.
daemon_log() {
	journalctl -q _SYSTEMD_UNIT=aas.service _PID="$1" -o short-unix --no-pager 2>/dev/null |
		sed 's/^\([0-9.]*\) [^:]*: /\1 /'
}

# Waits up to $3 ms for the first line with $2 in the log of the daemon
# that ran as process $1, and prints its time in ms since the epoch.
# Returns 1 if none came.
log_wait() {
	waited=0
	while :; do
		at=$(daemon_log "$1" | grep -F -- "$2" | head -n1 | cut -d' ' -f1)
		if [ -n "$at" ]; then
			echo "$at" | awk '{ printf "%d\n", $1 * 1000 }'
			return 0
		fi
		[ "$waited" -ge "$3" ] && return 1
		sleep 0.05
		waited=$((waited + 50))
	done
}

# Whether the daemon holds a capture stream of a card open and running.
capture_running() {
	daemon=$(main_pid)
	for status in /proc/asound/card*/pcm*c/sub*/status; do
		[ -r "$status" ] || continue
		if grep -q "owner_pid *: $daemon\$" "$status" && grep -q 'state: RUNNING' "$status"; then
			return 0
		fi
	done
	return 1
}

# A helper for what aplay cannot show: pause, delay, underrun, polling
# with descriptors fetched once, and a non-blocking drain. The cases whose
# name starts with "capture" open the PCM for capture.
cat >"$work/pcmtest.c" <<'EOF'
#include <alsa/asoundlib.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static snd_pcm_t *pcm;
static snd_pcm_stream_t direction = SND_PCM_STREAM_PLAYBACK;
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

/* Sets the hardware parameters of the open PCM. */
static int params(int *can_pause)
{
	snd_pcm_hw_params_t *hw;
	int err;

	snd_pcm_hw_params_alloca(&hw);
	if ((err = snd_pcm_hw_params_any(pcm, hw)) < 0 ||
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

static int setup(const char *device, int mode, int *can_pause)
{
	int err = snd_pcm_open(&pcm, device, direction, mode);

	if (err < 0) {
		printf("open: %s\n", snd_strerror(err));
		return -1;
	}
	return params(can_pause);
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

static int drain_result;
static double drain_took;

static void *drain_thread(void *arg)
{
	double t0 = now();

	(void)arg;
	drain_result = snd_pcm_drain(pcm);
	drain_took = now() - t0;
	return NULL;
}

/* A blocking drain in one thread does not hold up a drop from another:
 * the drop returns at once, the drain returns with it, and the PCM plays
 * on. The buffer holds a second, so the drain alone would take that long. */
static int drop_during_drain_test(void)
{
	pthread_t thread;
	double t0, drop_took, after;
	snd_pcm_state_t state;
	int err;

	if (put(rate - period))
		return 1;
	if (pthread_create(&thread, NULL, drain_thread, NULL)) {
		printf("no thread\n");
		return 1;
	}
	usleep(200000);
	state = snd_pcm_state(pcm);
	t0 = now();
	err = snd_pcm_drop(pcm);
	drop_took = now() - t0;
	pthread_join(thread, NULL);
	if (state != SND_PCM_STATE_DRAINING || err < 0 || drain_result < 0) {
		printf("state %s before the drop, drop gave %d, drain gave %d\n",
		       snd_pcm_state_name(state), err, drain_result);
		return 1;
	}
	t0 = now();
	if (snd_pcm_prepare(pcm) < 0 || put(rate / 4) || snd_pcm_drain(pcm) < 0) {
		printf("the PCM did not play on after the drop\n");
		return 1;
	}
	after = now() - t0;
	printf("drop took %.1f ms during a drain of %.0f ms, which returned after %.0f ms; "
	       "250 ms more played in %.0f ms\n", drop_took * 1000,
	       (rate - period) * 1000.0 / rate, drain_took * 1000, after * 1000);
	return !(drop_took < 0.1 && drain_took < 0.4 && after > 0.2 && after < 0.6);
}

/* Mappings of a ring this process holds. */
static int rings(void)
{
	char line[512];
	int n = 0;
	FILE *maps = fopen("/proc/self/maps", "r");

	if (!maps)
		return -1;
	while (fgets(line, sizeof line, maps))
		n += strstr(line, "aas-ring") != NULL;
	fclose(maps);
	return n;
}

/* hw_free gives the stream up, and the PCM can be set up again at another
 * rate on the same handle. */
static int hw_free_test(void)
{
	int before = rings(), after, again, err, can_pause;
	double t0, took;

	if (put(rate / 4) || snd_pcm_drop(pcm) < 0)
		return 1;
	if ((err = snd_pcm_hw_free(pcm)) < 0) {
		printf("hw_free: %s\n", snd_strerror(err));
		return 1;
	}
	after = rings();
	rate = 44100;
	if (params(&can_pause))
		return 1;
	again = rings();
	t0 = now();
	if (put(rate / 2) || (err = snd_pcm_drain(pcm)) < 0) {
		printf("no playback after the second hw_params\n");
		return 1;
	}
	took = now() - t0;
	printf("%d ring mapped before hw_free, %d after, %d after hw_params at %u Hz; "
	       "500 ms played in %.0f ms\n", before, after, again, rate, took * 1000);
	return !(before == 1 && after == 0 && again == 1 && took > 0.45 && took < 0.8);
}

/* Reads `frames`, blocking. Returns 0, or the error, which it prints
 * unless it is -EPIPE. */
static int get(long frames)
{
	while (frames > 0) {
		long n = frames < (long)period ? frames : (long)period;
		snd_pcm_sframes_t r = snd_pcm_readi(pcm, silence, n);
		if (r < 0) {
			if (r != -EPIPE)
				printf("read: %s\n", snd_strerror(r));
			return r;
		}
		frames -= r;
	}
	return 0;
}

/* A recorder that falls behind by more than its buffer gets -EPIPE, as
 * from a card, and reads on in real time after prepare. */
static int capture_overrun_test(void)
{
	double t0, took;
	int err;

	if (get(rate / 4))
		return 1;
	usleep(300000);
	err = get(rate / 4);
	if (err != -EPIPE) {
		printf("a read 300 ms late with a buffer of %lu frames gave %d, not -EPIPE\n",
		       buffer, err);
		return 1;
	}
	if ((err = snd_pcm_prepare(pcm)) < 0) {
		printf("prepare: %s\n", snd_strerror(err));
		return 1;
	}
	t0 = now();
	if (get(rate / 2))
		return 1;
	took = now() - t0;
	snd_pcm_drop(pcm);
	printf("-EPIPE after falling 300 ms behind, then 500 ms read in %.0f ms after prepare\n",
	       took * 1000);
	return !(took > 0.4 && took < 0.7);
}

/* Reads `frames` by polling the descriptors in `pfd`. Fails on a poll
 * that times out, an error, or too many wake-ups. */
static int read_polled(struct pollfd *pfd, int n, long frames, int *polls)
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
		if (!(revents & POLLIN))
			continue;
		snd_pcm_sframes_t avail = snd_pcm_avail_update(pcm);
		if (avail < 0) {
			printf("avail: %s\n", snd_strerror(avail));
			return 1;
		}
		long want = avail < frames - done ? avail : frames - done;
		want = want > 16384 ? 16384 : want;
		snd_pcm_sframes_t r = snd_pcm_readi(pcm, silence, want);
		if (r < 0 && r != -EAGAIN) {
			printf("read: %s\n", snd_strerror(r));
			return 1;
		}
		done += r > 0 ? r : 0;
	}
	return 0;
}

/* A non-blocking recorder polls the descriptors it fetched once, with
 * POLLIN when a period is there to read. A drop and a prepare keep them,
 * and a second polled read goes on in real time. */
static int capture_poll_test(void)
{
	struct pollfd cached[8], fresh[8];
	int n = snd_pcm_poll_descriptors_count(pcm), polls1, polls2, i, err;
	double t0, first, second;

	if (n <= 0 || n > 8 || snd_pcm_poll_descriptors(pcm, cached, n) != n) {
		printf("no poll descriptors\n");
		return 1;
	}
	if ((err = snd_pcm_start(pcm)) < 0) {
		printf("start: %s\n", snd_strerror(err));
		return 1;
	}
	t0 = now();
	if (read_polled(cached, n, rate / 2, &polls1))
		return 1;
	first = now() - t0;
	if (snd_pcm_drop(pcm) < 0 || snd_pcm_prepare(pcm) < 0) {
		printf("drop and prepare failed\n");
		return 1;
	}
	snd_pcm_poll_descriptors(pcm, fresh, n);
	for (i = 0; i < n; i++) {
		if (fresh[i].fd != cached[i].fd) {
			printf("descriptor %d moved from %d to %d\n", i, cached[i].fd, fresh[i].fd);
			return 1;
		}
	}
	if ((err = snd_pcm_start(pcm)) < 0) {
		printf("start after prepare: %s\n", snd_strerror(err));
		return 1;
	}
	t0 = now();
	if (read_polled(cached, n, rate / 2, &polls2))
		return 1;
	second = now() - t0;
	snd_pcm_drop(pcm);
	printf("500 ms read polling in %.0f ms with %d wake-ups, and after a drop and a "
	       "prepare in %.0f ms with %d on the same descriptors\n",
	       first * 1000, polls1, second * 1000, polls2);
	return !(first > 0.4 && first < 0.7 && second > 0.4 && second < 0.7);
}

int main(int argc, char **argv)
{
	int can_pause = 0, mode = 0, spare = -1;

	if (argc != 3)
		return 2;
	if (!strncmp(argv[1], "capture", 7))
		direction = SND_PCM_STREAM_CAPTURE;
	if (!strcmp(argv[1], "capturepoll"))
		mode = SND_PCM_NONBLOCK;
	if (!strcmp(argv[1], "dropdrain")) {
		period = 4096;
		buffer = rate;
	}
	if (!strcmp(argv[1], "nonblock"))
		mode = SND_PCM_NONBLOCK;
	/* Held while the PCM is set up, freed before it is set up again. */
	if (!strcmp(argv[1], "stable"))
		spare = open("/dev/null", O_RDONLY);
	if (setup(argv[2], mode, &can_pause))
		return 2;
	if (!strcmp(argv[1], "stable"))
		return stable_test(spare);
	if (!strcmp(argv[1], "capturexrun"))
		return capture_overrun_test();
	if (!strcmp(argv[1], "capturepoll"))
		return capture_poll_test();
	if (!strcmp(argv[1], "nonblock"))
		return nonblock_drain_test();
	if (!strcmp(argv[1], "dropdrain"))
		return drop_during_drain_test();
	if (!strcmp(argv[1], "hwfree"))
		return hw_free_test();
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
if ! cc -O2 -pthread -o "$work/pcmtest" "$work/pcmtest.c" -lasound; then
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

# A helper for the tone the format case plays and hears. `gen` writes one
# second of a 1 kHz sine at -20 dBFS, at 48 kHz, in the format and channel
# count given. `hear` reads a recording, S32_LE stereo at 48 kHz or at the
# rate given, and prints for each channel the tone's level in dBFS, the
# share of the signal's power that is the tone, and for how many tenths of
# a second the tone was there. `freq` reads the same and prints for each
# channel the frequency of its strongest component between 500 and 2000 Hz,
# to 0.01 Hz. `widen` turns S16_LE into S32_LE, exactly.
cat >"$work/tone.c" <<'EOF'
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define RATE 48000
#define FREQ 1000.0
#define MAX_BLOCK (RATE / 10)
#define MAX_BLOCKS 64

/* The rate of a recording `hear` and `freq` read, at most RATE. */
static int rate = RATE;

static int by_value(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return (x > y) - (x < y);
}

static double median(double *v, int n)
{
	qsort(v, n, sizeof *v, by_value);
	return v[n / 2];
}

static int gen(const char *format, int channels)
{
	for (int i = 0; i < RATE; i++) {
		double x = 0.1 * sin(2 * M_PI * FREQ * i / RATE);
		for (int c = 0; c < channels; c++) {
			if (!strcmp(format, "S16_LE")) {
				int16_t v = lrint(x * 32767);
				fwrite(&v, 2, 1, stdout);
			} else if (!strcmp(format, "S24_LE")) {
				int32_t v = lrint(x * 8388607) & 0xffffff;
				fwrite(&v, 4, 1, stdout);
			} else if (!strcmp(format, "S24_3LE")) {
				int32_t v = lrint(x * 8388607);
				fwrite(&v, 3, 1, stdout);
			} else if (!strcmp(format, "S32_LE")) {
				int32_t v = lrint(x * 2147483647);
				fwrite(&v, 4, 1, stdout);
			} else if (!strcmp(format, "FLOAT_LE")) {
				float v = x;
				fwrite(&v, 4, 1, stdout);
			} else {
				return 2;
			}
		}
	}
	return 0;
}

static int hear(void)
{
	static int32_t block[MAX_BLOCK][2];
	static double tone[2][MAX_BLOCKS], share[2][MAX_BLOCKS];
	int blocks = 0;
	const int BLOCK = rate / 10;

	while (blocks < MAX_BLOCKS && fread(block, sizeof block[0], BLOCK, stdin) == (size_t)BLOCK) {
		for (int c = 0; c < 2; c++) {
			double re = 0, im = 0, sum = 0, squares = 0;
			for (int i = 0; i < BLOCK; i++) {
				double x = block[i][c] / 2147483648.0;
				double w = 2 * M_PI * FREQ * i / rate;
				re += x * cos(w);
				im += x * sin(w);
				sum += x;
				squares += x * x;
			}
			double mean = sum / BLOCK;
			double power = squares / BLOCK - mean * mean;
			tone[c][blocks] = 2 * (re * re + im * im) / ((double)BLOCK * BLOCK);
			share[c][blocks] = power > 0 ? tone[c][blocks] / power : 0;
		}
		blocks++;
	}
	for (int c = 0; c < 2; c++) {
		double loudest = 0, levels[MAX_BLOCKS], shares[MAX_BLOCKS];
		int on = 0;
		for (int b = 0; b < blocks; b++)
			loudest = tone[c][b] > loudest ? tone[c][b] : loudest;
		/* Blocks the tone fills, or nearly: at least half its power. */
		for (int b = 0; b < blocks; b++) {
			if (loudest > 0 && tone[c][b] >= loudest / 2) {
				levels[on] = tone[c][b];
				shares[on++] = share[c][b];
			}
		}
		if (!on) {
			printf("-200.0 0.000 0 ");
			continue;
		}
		/* The power of a sine is half its peak squared. */
		printf("%.1f %.3f %d ", 10 * log10(2 * median(levels, on)),
		       median(shares, on), on);
	}
	printf("\n");
	return 0;
}

/* The power at `f` Hz of channel `c` of `n` frames, by Goertzel. */
static double power_at(int32_t (*x)[2], long n, int c, double f)
{
	double k = 2 * cos(2 * M_PI * f / rate), s1 = 0, s2 = 0;

	for (long i = 0; i < n; i++) {
		double s = x[i][c] / 2147483648.0 + k * s1 - s2;
		s2 = s1;
		s1 = s;
	}
	return s1 * s1 + s2 * s2 - k * s1 * s2;
}

static int freq(void)
{
	static int32_t x[RATE * 4][2];
	long n = fread(x, sizeof x[0], RATE * 4, stdin);

	for (int c = 0; c < 2; c++) {
		double best = 500, most = -1;
		for (double f = 500; f <= 2000; f += 1) {
			double p = power_at(x, n, c, f);
			if (p > most) {
				most = p;
				best = f;
			}
		}
		double coarse = best;
		for (double f = coarse - 1; f <= coarse + 1; f += 0.01) {
			double p = power_at(x, n, c, f);
			if (p > most) {
				most = p;
				best = f;
			}
		}
		printf("%.2f ", best);
	}
	printf("\n");
	return 0;
}

static int widen(void)
{
	int16_t v;

	while (fread(&v, 2, 1, stdin) == 1) {
		int32_t w = (int32_t)((uint32_t)(uint16_t)v << 16);
		fwrite(&w, 4, 1, stdout);
	}
	return 0;
}

int main(int argc, char **argv)
{
	if (argc == 3 && (!strcmp(argv[1], "freq") || !strcmp(argv[1], "hear"))) {
		rate = atoi(argv[2]);
		if (rate < 1000 || rate > RATE)
			return 2;
	}
	if ((argc == 2 || argc == 3) && !strcmp(argv[1], "freq"))
		return freq();
	if (argc == 4 && !strcmp(argv[1], "gen"))
		return gen(argv[2], atoi(argv[3]));
	if ((argc == 2 || argc == 3) && !strcmp(argv[1], "hear"))
		return hear();
	if (argc == 2 && !strcmp(argv[1], "widen"))
		return widen();
	return 2;
}
EOF
if ! cc -O2 -o "$work/tone" "$work/tone.c" -lm; then
	echo "FAIL setup: cannot build the tone helper"
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

# Plays one second in each format the plugin takes, mono and stereo. With
# AAS_TEST_CAPTURE it is a tone, recorded from the board's input: each
# format must come out as a clean 1 kHz for the whole second, on both
# channels, at the level of the first. The analog path is not exact, so
# it is the tone's level and purity that are compared, not the samples.
case_aplay_plays_each_format() {
	name=aplay_plays_each_format
	capture=${AAS_TEST_CAPTURE:-}
	bad=""
	first=""
	heard=""
	[ -n "$capture" ] && amixer -q -D default sset Master 100%
	for format in S16_LE S24_LE S24_3LE S32_LE FLOAT_LE; do
		for channels in 1 2; do
			if [ -z "$capture" ]; then
				# Straight to the plugin, so plug converts nothing.
				if ! aplay -q -D aas -t raw -f $format -c $channels -r $rate -d 1 /dev/zero \
					2>"$work/err"; then
					bad="$bad $format/$channels($(head -n1 "$work/err"))"
				fi
				continue
			fi
			"$work/tone" gen $format $channels >"$work/tone.raw"
			# 1.6 s: the tone with 0.3 s before and after.
			arecord -q -D "$capture" -t raw -f S32_LE -c 2 -r $rate -s $((rate * 16 / 10)) \
				"$work/heard.raw" 2>"$work/caperr" &
			recorder=$!
			sleep 0.3
			if ! aplay -q -D aas -t raw -f $format -c $channels -r $rate "$work/tone.raw" \
				2>"$work/err"; then
				bad="$bad $format/$channels($(head -n1 "$work/err"))"
			fi
			if ! wait $recorder; then
				bad="$bad $format/$channels(capture: $(head -n1 "$work/caperr"))"
				continue
			fi
			heard=$("$work/tone" hear <"$work/heard.raw")
			[ -z "$first" ] && first=${heard%% *}
			# Level within 1 dB of the first, nine tenths of the power in
			# the tone, and the tone there for 9 to 11 tenths of a second.
			if ! echo "$first $heard" | awk '{
				for (c = 0; c < 2; c++) {
					level = $(2 + 3 * c); share = $(3 + 3 * c); on = $(4 + 3 * c)
					if (level < $1 - 1 || level > $1 + 1 || share < 0.9 || on < 9 || on > 11)
						exit 1
				}
			}'; then
				bad="$bad $format/$channels(heard $heard, first level $first)"
			fi
		done
	done
	if [ -n "$bad" ]; then
		fail $name "failed:$bad"
	elif [ -n "$capture" ]; then
		pass $name "each heard on $capture as 1 kHz at $first dBFS; the last, per channel as level, share of the power and tenths of a second: $heard"
	else
		pass $name
	fi
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

case_drop_during_a_blocking_drain() {
	name=drop_during_a_blocking_drain
	# A drain that does not let go would hang here, so it gets 10 s.
	if out=$(timeout 10 "$work/pcmtest" dropdrain aas); then pass $name "$out"; else fail $name "$out"; fi
}

case_hw_free_gives_the_stream_up() {
	name=hw_free_gives_the_stream_up
	if out=$(timeout 10 "$work/pcmtest" hwfree aas); then pass $name "$out"; else fail $name "$out"; fi
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

# Plays silence at 48 kHz through the plugin in the background, so that the
# daemon's output is open at that rate while a case records. With the
# analog input disabled the daemon writes a recorder only while its output
# is open. The holder plays silence, which a recorder never hears anyway.
hold_output() {
	aplay -q -D aas -t raw -f S16_LE -c 2 -r $rate /dev/zero 2>/dev/null &
	holder=$!
	sleep 0.3
}
release_output() {
	kill $holder 2>/dev/null
	wait $holder 2>/dev/null
}

# Whether file $1 is $2 bytes long and every byte of it is zero.
all_zero() { [ "$(stat -c %s "$1")" = "$2" ] && cmp -s -n "$2" "$1" /dev/zero; }

# Records one second from the default device in each format and rate a
# recorder is likely to ask for. plug converts from what the daemon gives.
case_arecord_records_each_format() {
	name=arecord_records_each_format
	bad=""
	hold_output
	for r in 44100 48000; do
		for spec in "S16_LE 2" "S24_LE 4" "S32_LE 4"; do
			set -- $spec
			if ! timeout 5 arecord -q -D default -t raw -f $1 -c 2 -r $r -s $r \
				"$work/rec.raw" 2>"$work/err"; then
				bad="$bad $1/$r($(head -n1 "$work/err"))"
				continue
			fi
			size=$(stat -c %s "$work/rec.raw")
			[ "$size" = $((r * 2 * $2)) ] || bad="$bad $1/$r($size bytes)"
		done
	done
	release_output
	if [ -z "$bad" ]; then
		pass $name "S16_LE, S24_LE and S32_LE at 44.1 and 48 kHz, 1 s each of the length asked for"
	else
		fail $name "failed:$bad"
	fi
}

# With the analog input disabled, the default install, the device records
# silence: every sample is 0.
case_capture_is_silent_without_signal() {
	name=capture_is_silent_without_signal
	hold_output
	timeout 5 arecord -q -D default -t raw -f S32_LE -c 2 -r $rate -s $rate \
		"$work/rec.raw" 2>"$work/err"
	status=$?
	release_output
	if [ $status -ne 0 ]; then
		fail $name "arecord exited $status: $(head -n1 "$work/err")"
	elif all_zero "$work/rec.raw" $((rate * 8)); then
		pass $name "1 s recorded, every sample 0"
	else
		fail $name "$(stat -c %s "$work/rec.raw") bytes, not all 0"
	fi
}

# The device never delivers the mix: a tone played through the daemon
# while the analog input is disabled gives an all-zero recording.
case_capture_never_hears_a_player() {
	name=capture_never_hears_a_player
	"$work/tone" gen S16_LE 2 >"$work/tone.raw"
	volume=$(master)
	amixer -q -D default sset Master 100%
	hold_output
	timeout 5 arecord -q -D default -t raw -f S32_LE -c 2 -r $rate -s $((rate * 16 / 10)) \
		"$work/rec.raw" 2>"$work/caperr" &
	recorder=$!
	sleep 0.3
	aplay -q -D aas -t raw -f S16_LE -c 2 -r $rate "$work/tone.raw" 2>"$work/err"
	played=$?
	wait $recorder
	status=$?
	release_output
	amixer -q -D default sset Master "$volume%"
	if [ $played -ne 0 ]; then
		fail $name "aplay exited $played: $(head -n1 "$work/err")"
	elif [ $status -ne 0 ]; then
		fail $name "arecord exited $status: $(head -n1 "$work/caperr")"
	elif all_zero "$work/rec.raw" $((rate * 16 / 10 * 8)); then
		pass $name "1 s of tone played during 1.6 s recorded, every sample 0"
	else
		fail $name "the recording is not all 0: $("$work/tone" hear <"$work/rec.raw")"
	fi
}

# The output's rate changes while a recording runs: the recording ends
# with an error within a second, and a new one records at the new rate.
case_capture_ends_on_a_rate_change() {
	name=capture_ends_on_a_rate_change
	# The output at 48 kHz, with no player left to hold a rate.
	aplay -q -D aas -t raw -f S16_LE -c 2 -r $rate -d 1 /dev/zero 2>"$work/err"
	arecord -q -D default -t raw -f S32_LE -c 2 -r $rate "$work/rec.raw" 2>"$work/caperr" &
	recorder=$!
	sleep 0.5
	if ! kill -0 $recorder 2>/dev/null; then
		wait $recorder
		fail $name "arecord stopped before the rate change: $(head -n1 "$work/caperr")"
		return
	fi
	aplay -q -D aas -t raw -f S16_LE -c 2 -r 44100 -d 2 /dev/zero 2>"$work/err" &
	player=$!
	start=$(now_ms)
	if ! wait_gone $recorder 3000; then
		kill -9 $recorder $player 2>/dev/null
		wait $recorder $player 2>/dev/null
		fail $name "arecord still runs 3 s after a player moved the output to 44.1 kHz"
		return
	fi
	took=$(($(now_ms) - start))
	wait $recorder
	status=$?
	why=$(tr '\n' ' ' <"$work/caperr")
	timeout 5 arecord -q -D default -t raw -f S32_LE -c 2 -r $rate -s $rate \
		"$work/rec2.raw" 2>"$work/caperr"
	again=$?
	size=$(stat -c %s "$work/rec2.raw" 2>/dev/null)
	wait $player
	if [ $status -ne 0 ] && [ $took -le 1000 ] && [ $again -eq 0 ] &&
		[ "$size" = $((rate * 8)) ]; then
		pass $name "arecord exited $status after $took ms: $why; a new one recorded 1 s"
	else
		fail $name "arecord exited $status after $took ms: $why; a new one exited $again with $size bytes: $(head -n1 "$work/caperr")"
	fi
}

# A recorder killed mid-stream costs the daemon nothing: another recorder
# records on, and the daemon keeps its pid.
case_recorder_killed_keeps_daemon_up() {
	name=recorder_killed_keeps_daemon_up
	daemon=$(main_pid)
	hold_output
	start=$(now_ms)
	timeout 5 arecord -q -D default -t raw -f S16_LE -c 2 -r $rate -s $((rate * 2)) \
		"$work/rec.raw" 2>"$work/err" &
	keeper=$!
	arecord -q -D default -t raw -f S16_LE -c 2 -r $rate /dev/null 2>/dev/null &
	victim=$!
	sleep 0.5
	kill -9 $victim 2>/dev/null
	wait $victim 2>/dev/null
	wait $keeper
	status=$?
	took=$(($(now_ms) - start))
	release_output
	size=$(stat -c %s "$work/rec.raw" 2>/dev/null)
	if [ $status -eq 0 ] && [ "$size" = $((rate * 8)) ] && [ $took -ge 1900 ] &&
		[ $took -le 2600 ] && [ "$(main_pid)" = "$daemon" ] && systemctl -q is-active aas; then
		pass $name "the other arecord recorded 2 s in $took ms, daemon pid $daemon unchanged"
	else
		fail $name "status $status, $size bytes, $took ms, daemon pid $daemon now $(main_pid): $(cat "$work/err")"
	fi
}

# A recorder that falls behind gets -EPIPE and goes on after prepare.
case_capture_overrun_returns_epipe() {
	name=capture_overrun_returns_epipe
	hold_output
	out=$(timeout 10 "$work/pcmtest" capturexrun default)
	status=$?
	release_output
	if [ $status -eq 0 ]; then pass $name "$out"; else fail $name "$out"; fi
}

# A non-blocking recorder polls, and keeps its descriptors through a
# drop and a prepare.
case_capture_polls_and_survives_a_reset() {
	name=capture_polls_and_survives_a_reset
	hold_output
	out=$(timeout 10 "$work/pcmtest" capturepoll default)
	status=$?
	release_output
	if [ $status -eq 0 ]; then pass $name "$out"; else fail $name "$out"; fi
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

# A mixer that holds the control open across a restart of the daemon sets
# the volume with its next call. amixer -s keeps one handle for every
# command it reads.
case_control_reconnects_after_restart() {
	name=control_reconnects_after_restart
	mkfifo "$work/commands"
	amixer -D default -s <"$work/commands" >"$work/out" 2>&1 &
	mixer=$!
	exec 3>"$work/commands"
	# An amixer that has died must fail this case, not end the script
	# with SIGPIPE on the next write to it.
	trap '' PIPE
	echo "sset Master 40%" >&3 2>/dev/null
	sleep 0.5
	before=$(master)
	daemon=$(main_pid)
	# The cases around this one start the daemon four times in a few
	# seconds. One more would reach systemd's limit of five in ten.
	sudo systemctl reset-failed aas
	sudo systemctl restart aas
	if ! wait_daemon; then
		exec 3>&-
		trap - PIPE
		fail $name "the daemon did not come back"
		return
	fi
	echo "sset Master 60%" >&3 2>/dev/null
	exec 3>&-
	trap - PIPE
	if ! wait_gone $mixer 5000; then
		kill -9 $mixer
		wait $mixer 2>/dev/null
		fail $name "amixer still runs 5 s after its last command"
		return
	fi
	wait $mixer
	after=$(master)
	if [ "$before" = 40 ] && [ "$after" = 60 ] && [ "$(main_pid)" != "$daemon" ]; then
		pass $name "one amixer set 40% on pid $daemon and 60% on pid $(main_pid)"
	else
		fail $name "$before% before the restart, $after% after: $(tr '\n' ' ' <"$work/out")"
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

# CPU time of process $1 so far, in clock ticks.
cpu_ticks() { awk '{ print $14 + $15 }' "/proc/$1/stat"; }

# A client that polls the control's descriptor without asking libasound
# what it means, as alsactl monitor does, sleeps once the daemon is gone
# instead of waking again and again.
case_monitor_sleeps_when_the_daemon_is_gone() {
	name=monitor_sleeps_when_the_daemon_is_gone
	alsactl monitor default >"$work/monitor" 2>&1 &
	monitor=$!
	sleep 0.5
	sudo systemctl kill -s KILL aas
	sleep 0.5
	if ! kill -0 $monitor 2>/dev/null; then
		wait $monitor
		status=$?
		wait_daemon
		fail $name "alsactl monitor exited $status: $(tr '\n' ' ' <"$work/monitor")"
		return
	fi
	before=$(cpu_ticks $monitor)
	sleep 5
	after=$(cpu_ticks $monitor)
	kill $monitor 2>/dev/null
	wait $monitor 2>/dev/null
	used=$((after - before))
	hz=$(getconf CLK_TCK)
	if ! wait_daemon; then
		fail $name "the daemon did not come back"
	elif [ $used -le $((hz / 20)) ]; then
		pass $name "alsactl monitor used $used ticks of CPU (1/$hz s) in the 5 s after the daemon was killed"
	else
		fail $name "alsactl monitor used $used ticks of CPU (1/$hz s) in the 5 s after the daemon was killed"
	fi
}

# The analog input opens when a -20 dBFS tone reaches it through the
# cable, within 200 ms of the start, and closes when the hold of 3 s has
# passed after the tone, within a second more. The tone lasts 2 s from
# the start: aplay exits later than it ends, after its drain. The first
# open of the daemon is taken, so one before the tone fails the case.
case_analog_input_opens_on_signal() {
	name=analog_input_opens_on_signal
	if ! analog_on; then
		fail $name "the daemon did not start with the analog input"
		analog_off
		return
	fi
	daemon=$(main_pid)
	volume=$(master)
	amixer -q -D default sset Master 100%
	if ! capture_running; then
		fail $name "the daemon holds no running capture: $(daemon_log $daemon | grep analog | tr '\n' ' ')"
		amixer -q -D default sset Master "$volume%"
		analog_off
		return
	fi
	"$work/tone" gen S16_LE 2 >"$work/tone.raw"
	cat "$work/tone.raw" "$work/tone.raw" >"$work/tone2.raw"
	start=$(now_ms)
	aplay -q -D default -t raw -f S16_LE -c 2 -r $rate "$work/tone2.raw" 2>"$work/err"
	status=$?
	end=$(now_ms)
	opened=$(log_wait $daemon "analog input: signal" 1000)
	closed=$(log_wait $daemon "analog input: quiet" 6000)
	amixer -q -D default sset Master "$volume%"
	analog_off
	took=$((end - start))
	end=$((start + 2000))
	if [ $status -ne 0 ]; then
		fail $name "aplay exited $status: $(head -n1 "$work/err")"
	elif [ -z "$opened" ]; then
		fail $name "no open in the log during 2 s of tone"
	elif [ -z "$closed" ]; then
		fail $name "opened after $((opened - start)) ms, not closed 6 s after the tone"
	elif [ $opened -ge $start ] && [ $((opened - start)) -le 200 ] &&
		[ $((closed - end)) -ge 3000 ] &&
		[ $((closed - end)) -le 4000 ]; then
		pass $name "opened $((opened - start)) ms after the tone started, closed $((closed - end)) ms after it ended; aplay took $took ms"
	else
		fail $name "opened $((opened - start)) ms after the tone started, closed $((closed - end)) ms after it ended; aplay took $took ms"
	fi
}

# With nothing playing, the input stays closed from the start of the
# daemon and for 10 s more, with the capture running all the while. The
# start counts: the ADC's offset settling as the capture starts must not
# open it.
case_analog_input_stays_closed_on_silence() {
	name=analog_input_stays_closed_on_silence
	if ! analog_on; then
		fail $name "the daemon did not start with the analog input"
		analog_off
		return
	fi
	daemon=$(main_pid)
	sleep 10
	running=no
	capture_running && running=yes
	opens=$(daemon_log $daemon | grep -c -F "analog input: signal")
	log=$(daemon_log $daemon | grep analog | tr '\n' ' ')
	analog_off
	if [ $running = yes ] && [ "$opens" -eq 0 ]; then
		pass $name "capture running, no open in 10 s"
	else
		fail $name "capture running: $running, opens: $opens: $log"
	fi
}

# The output changes rate from 44.1 to 96 kHz while the capture is open.
# Both players finish, the capture runs at the new rate, and the daemon
# counts no xrun and no wake-up gap.
case_rate_change_with_capture_open() {
	name=rate_change_with_capture_open
	if ! analog_on; then
		fail $name "the daemon did not start with the analog input"
		analog_off
		return
	fi
	daemon=$(main_pid)
	aplay -q -D default -t raw -f S16_LE -c 2 -r 44100 -d 1 /dev/zero 2>"$work/err"
	first=$?
	aplay -q -D default -t raw -f S16_LE -c 2 -r 96000 -d 1 /dev/zero 2>>"$work/err"
	second=$?
	sleep 0.5
	running=no
	capture_running && running=yes
	analog_off
	log_wait $daemon "aas: stopped:" 2000 >/dev/null
	log=$(daemon_log $daemon)
	counters=$(echo "$log" | grep -F "aas: stopped:" | head -n1 | cut -d' ' -f2-)
	rates=$(echo "$log" | grep -o "output at [0-9]* Hz" | tr '\n' ' ')
	failures=$(echo "$log" | grep -F "analog input" | grep -c -e fail -e "trying again")
	if [ $first -ne 0 ] || [ $second -ne 0 ]; then
		fail $name "aplay exited $first and $second: $(head -n2 "$work/err" | tr '\n' ' ')"
	elif [ $running = yes ] && [ "$failures" -eq 0 ] &&
		echo "$rates" | grep -q "output at 96000 Hz" &&
		echo "$counters" | grep -q ": 0 xruns, 0 wake gaps,"; then
		pass $name "$rates; pid $daemon $counters"
	else
		fail $name "capture running: $running, capture failures: $failures; $rates; pid $daemon $counters"
	fi
}

# A 1 kHz tone played through the daemon reaches the analog input through
# the cable and is in what the device records: at 1 kHz within 2 Hz on
# both channels, nine tenths of the power in the tone. Recorded three
# times: from the default device with read access, from the plugin itself
# with mmap access, the way plug reads it for any conversion, and from the
# default device as S16_LE at 44.1 kHz, which plug converts.
case_capture_hears_the_analog_input() {
	name=capture_hears_the_analog_input
	if ! analog_on; then
		fail $name "the daemon did not start with the analog input"
		analog_off
		return
	fi
	volume=$(master)
	amixer -q -D default sset Master 100%
	"$work/tone" gen S16_LE 2 >"$work/tone.raw"
	bad=""
	report=""
	for how in "-D default -f S32_LE -r $rate" "-M -D aas -f S32_LE -r $rate" \
		"-D default -f S16_LE -r 44100"; do
		at=${how##* }
		timeout 5 arecord -q $how -t raw -c 2 -s $((at * 16 / 10)) \
			"$work/rec.raw" 2>"$work/caperr" &
		recorder=$!
		sleep 0.3
		aplay -q -D default -t raw -f S16_LE -c 2 -r $rate "$work/tone.raw" 2>"$work/err"
		played=$?
		wait $recorder
		status=$?
		if [ $played -ne 0 ]; then
			bad="$bad $how(aplay exited $played: $(head -n1 "$work/err"))"
			continue
		fi
		if [ $status -ne 0 ]; then
			bad="$bad $how(arecord exited $status: $(head -n1 "$work/caperr"))"
			continue
		fi
		case $how in
		*S16_LE*)
			"$work/tone" widen <"$work/rec.raw" >"$work/rec32.raw"
			mv "$work/rec32.raw" "$work/rec.raw"
			;;
		esac
		heard=$("$work/tone" hear $at <"$work/rec.raw")
		heard=${heard% }
		freqs=$("$work/tone" freq $at <"$work/rec.raw")
		freqs=${freqs% }
		line="$how: $freqs Hz, level, share and tenths per channel $heard"
		if echo "$freqs $heard" | awk '{
			for (c = 0; c < 2; c++)
				if ($(1 + c) < 998 || $(1 + c) > 1002 || $(4 + 3 * c) < 0.9)
					exit 1
		}'; then
			report="$report $line;"
		else
			bad="$bad $line;"
		fi
	done
	amixer -q -D default sset Master "$volume%"
	analog_off
	if [ -z "$bad" ]; then
		report="${report# }"
		pass $name "${report%;}"
	else
		fail $name "failed:$bad"
	fi
}

cases="aplay_plays_each_format drain_returns pause_and_resume delay_is_plausible
underrun_returns_epipe poll_descriptors_survive_a_reset nonblocking_drain
drop_during_a_blocking_drain hw_free_gives_the_stream_up
small_buffer_rounds_up_or_is_refused daemon_killed_during_write_returns_an_error
client_reconnects_after_restart player_killed_keeps_daemon_up
amixer_reads_and_sets_master second_client_sees_the_change mpd_mixer_works
volume_survives_a_restart control_reconnects_after_restart
amixer_fails_without_the_daemon watcher_hears_the_daemon_go
monitor_sleeps_when_the_daemon_is_gone arecord_records_each_format
capture_is_silent_without_signal capture_never_hears_a_player
capture_ends_on_a_rate_change recorder_killed_keeps_daemon_up
capture_overrun_returns_epipe capture_polls_and_survives_a_reset"
analog_cases="analog_input_opens_on_signal analog_input_stays_closed_on_silence
rate_change_with_capture_open capture_hears_the_analog_input"
[ "${ANALOG:-}" = 1 ] && cases="$cases $analog_cases"
[ $# -gt 0 ] && cases="$*"
for name in $cases; do
	if [ "${ANALOG:-}" != 1 ] && echo "$analog_cases" | grep -qw -- "$name"; then
		echo "SKIP $name: needs ANALOG=1"
		continue
	fi
	if ! command -v "case_$name" >/dev/null; then
		fail "$name" "no such case"
		continue
	fi
	"case_$name"
done
exit $failed
