#!/bin/sh
# Board tests for the aas PulseAudio endpoint. Runs on the test board, as a
# member of group audio, with the daemon running as aas.service and
# dist/install-pulse.sh installed. Needs pactl, paplay, mpv, aasctl and
# python3. Each case prints PASS or FAIL and its name. The exit status is
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

exit "$failed"
