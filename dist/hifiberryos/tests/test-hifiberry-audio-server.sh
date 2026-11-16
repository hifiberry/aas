#!/bin/sh
# Test hifiberry-audio-server against a temporary root and a fake systemctl.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/../hifiberry-audio-server"
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/bin"
cat > "$ROOT/bin/systemctl" <<'FAKE'
#!/bin/sh
# Every change is asked for quietly, so that what systemctl prints about
# links it made does not reach the caller as a warning.
case "$1" in
    is-active) ;;
    --quiet) shift ;;
    *) echo "$*" >> "$AAS_SWITCH_ROOT/not-quiet.log" ;;
esac
echo "$*" >> "$AAS_SWITCH_ROOT/systemctl.log"
if [ "$1" = is-active ]; then
    [ -f "$AAS_SWITCH_ROOT/active" ]
    exit
fi
exit 0
FAKE
chmod +x "$ROOT/bin/systemctl"
PATH="$ROOT/bin:$PATH"
export AAS_SWITCH_ROOT="$ROOT" PATH

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok $*"; }
eq() { [ "$1" = "$2" ] || fail "$3: got '$1', want '$2'"; ok "$3"; }
run() { sh "$SCRIPT" "$@"; }

UNITS="pipewire.service pipewire.socket pipewire-pulse.service pipewire-pulse.socket wireplumber.service filter-chain.service pipewire-api.service vu-meter.service aes67.service aes67-agent.service usbaudio.service usbaudio-state.service analoginput.service"
L1=$ROOT/etc/alsa/conf.d/99-zz-aas.conf
L2=$ROOT/etc/pulse/client.conf.d/90-aas.conf
L3=$ROOT/etc/sysctl.d/90-aas.conf

mkdir -p "$ROOT/etc/alsa/conf.d"
echo pw > "$ROOT/etc/alsa/conf.d/99-pipewire-default.conf"
echo asound > "$ROOT/etc/asound.conf"

eq "$(run get)" "running pipewire
chosen pipewire" "get without a choice"

run set aas > /dev/null
eq "$(cat "$ROOT/etc/hifiberry/audio-server")" aas "choice file holds aas"
eq "$(readlink "$L1")" /usr/share/aas/asound.conf "alsa link"
eq "$(readlink "$L2")" /usr/share/aas/pulse-client.conf "pulse link"
eq "$(readlink "$L3")" /usr/share/aas/90-aas.conf "sysctl link"
grep -qxF -- "--global mask $UNITS" "$ROOT/systemctl.log" || fail "mask line"
ok "mask line"
grep -qxF "enable aas.service aas-config.service" "$ROOT/systemctl.log" || fail "enable line"
ok "enable line"
touch "$ROOT/active"
eq "$(run get)" "running aas
chosen aas" "get after set aas"
rm "$ROOT/active"

run set aas > /dev/null || fail "second set aas"
eq "$(readlink "$L1")" /usr/share/aas/asound.conf "links after second set aas"

# a regular file in place of the alsa link must survive set pipewire
run set pipewire > /dev/null
[ ! -e "$L1" ] && [ ! -L "$L1" ] || fail "alsa link remains"
[ ! -L "$L2" ] && [ ! -e "$L2" ] || fail "pulse link remains"
[ ! -L "$L3" ] && [ ! -e "$L3" ] || fail "sysctl link remains"
ok "links gone"
grep -qxF -- "--global unmask $UNITS" "$ROOT/systemctl.log" || fail "unmask line"
ok "unmask line"
grep -qxF "disable aas.service aas-config.service" "$ROOT/systemctl.log" || fail "disable line"
ok "disable line"
eq "$(cat "$ROOT/etc/hifiberry/audio-server")" pipewire "choice file holds pipewire"

echo mine > "$L1"
run set pipewire > /dev/null
eq "$(cat "$L1")" mine "regular file survives set pipewire"
eq "$(cat "$ROOT/etc/alsa/conf.d/99-pipewire-default.conf")" pw "pipewire-alsa file untouched"
eq "$(cat "$ROOT/etc/asound.conf")" asound "asound.conf untouched"

MASKED=$ROOT/var/lib/hifiberry/audio-server.masked
UDIR=$ROOT/etc/systemd/user
OTHERS=$(echo "$UNITS" | sed 's/ vu-meter.service//')

# set pipewire with no record unmasks nothing
rm -f "$MASKED"; : > "$ROOT/systemctl.log"
run set pipewire > /dev/null
! grep -q -- "--global unmask" "$ROOT/systemctl.log" || fail "unmask without a record"
ok "no record, no unmask"

# a mask that was there before survives a round trip
mkdir -p "$UDIR"; ln -s /dev/null "$UDIR/vu-meter.service"
: > "$ROOT/systemctl.log"
run set aas > /dev/null
grep -qxF -- "--global mask $OTHERS" "$ROOT/systemctl.log" || fail "mask without vu-meter"
! grep -qF vu-meter "$MASKED" || fail "vu-meter recorded"
ok "pre-existing mask not taken over"
: > "$ROOT/systemctl.log"
run set pipewire > /dev/null
grep -qxF -- "--global unmask $OTHERS" "$ROOT/systemctl.log" || fail "unmask without vu-meter"
[ ! -e "$MASKED" ] || fail "record remains"
ok "pre-existing mask survives set pipewire"
rm -f "$UDIR/vu-meter.service"

# a repeated set aas keeps the record: what the first one masked is ours
run set aas > /dev/null
for u in $UNITS; do ln -s /dev/null "$UDIR/$u"; done
run set aas > /dev/null
eq "$(tr '\n' ' ' < "$MASKED")" "$UNITS " "record kept by a repeated set aas"
: > "$ROOT/systemctl.log"
run set pipewire > /dev/null
grep -qxF -- "--global unmask $UNITS" "$ROOT/systemctl.log" || fail "unmask all of ours"
ok "set pipewire unmasks all of ours"
for u in $UNITS; do rm -f "$UDIR/$u"; done

# a warning for an asound.conf that sets the default, and none otherwise
run set aas 2>&1 >/dev/null | grep -q '!default' && fail "warning without a default"
printf 'pcm.!default {\n type hw\n}\n' > "$ROOT/etc/asound.conf"
run set aas 2>&1 >/dev/null | grep -q 'pcm.!default' || fail "no warning"
ok "asound.conf default warned about"
echo asound > "$ROOT/etc/asound.conf"
run set pipewire > /dev/null

# a failing link: the other steps run, the choice is written, rc is 1
if [ "$(id -u)" -ne 0 ]; then
    mkdir -p "$ROOT/etc/sysctl.d"; chmod 555 "$ROOT/etc/sysctl.d"
    : > "$ROOT/systemctl.log"
    rc=0; run set aas > /dev/null 2>&1 || rc=$?
    chmod 755 "$ROOT/etc/sysctl.d"
    eq "$rc" 1 "failing link exits 1"
    eq "$(readlink "$L1")" /usr/share/aas/asound.conf "link before the failing one made"
    eq "$(readlink "$L2")" /usr/share/aas/pulse-client.conf "link after the failing one made"
    grep -qxF "enable aas.service aas-config.service" "$ROOT/systemctl.log" || fail "enable after failing link"
    eq "$(cat "$ROOT/etc/hifiberry/audio-server")" aas "choice written after failing link"
    run set pipewire > /dev/null
else
    ok "skipped the failing link check (running as root)"
fi

rc=0; run set foo > /dev/null 2>&1 || rc=$?
eq "$rc" 2 "set foo exits 2"
rc=0; run > /dev/null 2>&1 || rc=$?
eq "$rc" 2 "no argument exits 2"
rc=0; run set > /dev/null 2>&1 || rc=$?
eq "$rc" 2 "set without server exits 2"

if [ "$(id -u)" -ne 0 ]; then
    rc=0; env -u AAS_SWITCH_ROOT sh "$SCRIPT" set aas > /dev/null 2>&1 || rc=$?
    eq "$rc" 1 "set without root exits 1"
else
    ok "skipped the non-root check (running as root)"
fi
[ ! -s "$ROOT/not-quiet.log" ] || fail "systemctl called without --quiet: $(cat "$ROOT/not-quiet.log")"
echo "all ok"
