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
echo "all ok"
