#!/bin/sh
# Builds the aas ALSA plugin and installs it, with dist/asound.conf as
# /etc/asound.conf, so that ALSA's default device plays through the daemon.
# Run from the repository root as a user that may sudo. Needs
# libasound2-dev. An /etc/asound.conf that is not ours is kept as
# /etc/asound.conf.before-aas.
set -eu

cargo build --release -p aas-alsa
target=${CARGO_TARGET_DIR:-target}/release/libaas_alsa.so

arch=$(dpkg-architecture -qDEB_HOST_MULTIARCH 2>/dev/null || gcc -print-multiarch 2>/dev/null || true)
dir=/usr/lib/${arch:-aarch64-linux-gnu}/alsa-lib

sudo install -d "$dir"
sudo install -m 0644 "$target" "$dir/libasound_module_pcm_aas.so"
echo "installed $dir/libasound_module_pcm_aas.so"

conf=/etc/asound.conf
if [ -e "$conf" ] && ! cmp -s "$conf" dist/asound.conf; then
	if [ -e "$conf.before-aas" ]; then
		echo "$conf.before-aas exists already; leaving $conf alone" >&2
		exit 1
	fi
	sudo mv "$conf" "$conf.before-aas"
	echo "kept the old $conf as $conf.before-aas"
fi
sudo install -m 0644 dist/asound.conf "$conf"
echo "installed $conf"
