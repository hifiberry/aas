#!/bin/sh
# Test the bluez-alsa drop-ins: each runs its unit only when the choice
# file reads aas, and replaces Debian's ExecStart rather than adding one.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok $*"; }

CHOICE=/etc/hifiberry/audio-server

for unit in bluealsa bluealsa-aplay; do
    f="$HERE/../$unit.conf"
    cond=$(sed -n 's/^ExecCondition=//p' "$f")
    [ -n "$cond" ] || fail "$unit: no ExecCondition"
    case "$cond" in *"$CHOICE"*) ;; *) fail "$unit: condition does not read $CHOICE" ;; esac
    # The condition, pointed at a choice file of the test's own.
    test_cond=$(printf '%s' "$cond" | sed "s|$CHOICE|$TMP/choice|")
    runs() { eval "$test_cond"; }

    rm -f "$TMP/choice"
    runs && fail "$unit: runs with no choice file"
    ok "$unit: skipped with no choice file"
    echo pipewire > "$TMP/choice"
    runs && fail "$unit: runs under pipewire"
    ok "$unit: skipped under pipewire"
    echo aasx > "$TMP/choice"
    runs && fail "$unit: runs for a choice that only starts with aas"
    ok "$unit: skipped for aasx"
    echo aas > "$TMP/choice"
    runs || fail "$unit: skipped under aas"
    ok "$unit: runs under aas"

    starts=$(grep '^ExecStart=' "$f")
    first=$(printf '%s\n' "$starts" | head -n 1)
    [ "$first" = "ExecStart=" ] || fail "$unit: ExecStart not cleared first"
    [ "$(printf '%s\n' "$starts" | wc -l)" -eq 2 ] || fail "$unit: want one ExecStart after the reset"
    ok "$unit: replaces ExecStart"
done

grep -q -- ' -p a2dp-sink$' "$HERE/../bluealsa.conf" || fail "bluealsa: not the sink only"
ok "bluealsa: sink only"
for opt in --pcm=default --volume=mixer --mixer-device=default --mixer-name=Master; do
    grep -q -- "$opt" "$HERE/../bluealsa-aplay.conf" || fail "bluealsa-aplay: no $opt"
done
ok "bluealsa-aplay: plays to default, volume on Master"
