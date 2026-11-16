#!/bin/sh
# Board tests for the aas PulseAudio endpoint. Runs on the test board, as a
# member of group audio, with the daemon running as aas.service and
# dist/install-pulse.sh installed. Needs pactl, paplay, mpv, aasctl and
# python3, and parecord for the record check. Each case prints PASS or FAIL and its name. The exit status is
# the number of cases that failed, or 2 if a tool is missing.
set -u

failed=0
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

pass() { echo "PASS $1${2:+: $2}"; }
fail() { echo "FAIL $1${2:+: $2}"; failed=$((failed + 1)); }

for tool in pactl paplay mpv aasctl python3; do
	if ! command -v "$tool" >/dev/null 2>&1; then
		echo "needs $tool" >&2
		exit 2
	fi
done

if ! pactl info >/dev/null 2>&1; then
	fail connect
	exit "$failed"
fi
pass connect

sink=$(pactl info | sed -n 's/^Default Sink: //p')
if [ "$sink" = aas ]; then
	pass default_sink
else
	fail default_sink "got ${sink:-none}"
fi

# One second of silence, raw, so no file is needed.
if head -c 192000 /dev/zero |
	paplay --raw --rate=48000 --channels=2 --format=s16le >/dev/null 2>&1; then
	pass paplay
else
	fail paplay
fi

python3 - "$work/tone.wav" <<'PY'
import struct, sys, wave
path = sys.argv[1]
with wave.open(path, "wb") as out:
    out.setnchannels(2)
    out.setsampwidth(2)
    out.setframerate(48000)
    out.writeframes(b"\0\0\0\0" * 24000)
PY
if mpv --no-video --ao=pulse "$work/tone.wav" >/dev/null 2>&1; then
	pass mpv
else
	fail mpv
fi

pactl set-sink-volume @DEFAULT_SINK@ 40% 2>/dev/null
volume=$(aasctl outputs 2>/dev/null |
	python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["volume"])' 2>/dev/null)
if [ "$volume" = 40 ]; then
	pass volume
else
	fail volume "aasctl says ${volume:-nothing}"
fi
pactl set-sink-volume @DEFAULT_SINK@ 100% >/dev/null 2>&1

# Length in seconds and peak (0 to 1) of a raw mono s16le file at 44100 Hz.
raw_stats() {
	python3 - "$1" <<'PY'
import array, sys
data = array.array("h")
with open(sys.argv[1], "rb") as f:
    raw = f.read()
data.frombytes(raw[: len(raw) // 2 * 2])
peak = max((abs(v) for v in data), default=0)
print(len(data) / 44100.0, peak / 32768.0)
PY
}

if ! command -v parecord >/dev/null 2>&1; then
	echo "SKIP record: needs parecord"
else
	python3 - "$work/rec.wav" <<'PY'
import math, struct, sys, wave
with wave.open(sys.argv[1], "wb") as out:
    out.setnchannels(2)
    out.setsampwidth(2)
    out.setframerate(48000)
    for n in range(96000):
        v = int(16384 * math.sin(2 * math.pi * 1000 * n / 48000))
        out.writeframes(struct.pack("<hh", v, v))
PY
	paplay "$work/rec.wav" >/dev/null 2>&1 &
	player=$!
	sleep 0.3
	timeout 1 parecord -d aas.monitor --channels=1 --format=s16le --rate=44100 \
		--raw "$work/monitor.raw" >/dev/null 2>&1
	wait "$player" 2>/dev/null
	stats=$(raw_stats "$work/monitor.raw" 2>/dev/null)
	if python3 -c 'import sys; s, p = map(float, sys.argv[1:3]); sys.exit(not (s >= 0.8 and p > 0.25))' $stats 2>/dev/null; then
		pass record "monitor, $stats"
	else
		fail record "monitor, got ${stats:-nothing}"
	fi

	if pactl list short sources 2>/dev/null | grep -q 'aas\.analog'; then
		timeout 1 parecord -d aas.analog --channels=1 --format=s16le --rate=44100 \
			--raw "$work/analog.raw" >/dev/null 2>&1
		stats=$(raw_stats "$work/analog.raw" 2>/dev/null)
		if python3 -c 'import sys; sys.exit(not float(sys.argv[1]) >= 0.8)' $stats 2>/dev/null; then
			pass record_analog "$stats"
		else
			fail record_analog "got ${stats:-nothing}"
		fi
	fi
fi

exit "$failed"
