#!/bin/sh
# Installs /etc/pulse/client.conf so that libpulse finds aas on
# /run/aas/pulse and does not try to start a PulseAudio daemon. Part of the
# switch to aas: while PipeWire runs, its own pipewire-pulse discovery
# stays. An /etc/pulse/client.conf that is not this one is kept as
# /etc/pulse/client.conf.before-aas.
set -eu

conf=/etc/pulse/client.conf
sudo install -d /etc/pulse
if [ -e "$conf" ] && ! cmp -s "$conf" dist/pulse-client.conf; then
	if [ -e "$conf.before-aas" ]; then
		echo "$conf.before-aas exists already; leaving $conf alone" >&2
		exit 1
	fi
	sudo mv "$conf" "$conf.before-aas"
	echo "kept the old $conf as $conf.before-aas"
fi
sudo install -m 0644 dist/pulse-client.conf "$conf"
echo "installed $conf"
