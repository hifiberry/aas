#!/bin/sh
# Installs the files that put the daemon's HTTP API behind HiFiBerry OS's
# nginx and its auth gate: dist/nginx/hifiberry-aas.nginx as the location
# /api/aas/v1/, and dist/auth.d/aas.json as the manifest that classes its
# requests. Then checks and reloads nginx, and restarts hifiberry-auth,
# which reads its manifests only at startup; either only if it is running.
# Run from the repository root as a user that may sudo.
set -eu

nginx_dir=/etc/nginx/hifiberry-api.d
auth_dir=/etc/hifiberry/auth.d

sudo install -d "$nginx_dir" "$auth_dir"
sudo install -o root -g root -m 0644 dist/nginx/hifiberry-aas.nginx "$nginx_dir/hifiberry-aas.nginx"
echo "installed $nginx_dir/hifiberry-aas.nginx"
sudo install -o root -g root -m 0644 dist/auth.d/aas.json "$auth_dir/aas.json"
echo "installed $auth_dir/aas.json"

if systemctl is-active --quiet nginx; then
	if ! sudo nginx -t; then
		# Leave no location nginx refuses: it would stop the next start.
		sudo rm -f "$nginx_dir/hifiberry-aas.nginx"
		echo "nginx refused the configuration; removed $nginx_dir/hifiberry-aas.nginx" >&2
		exit 1
	fi
	sudo systemctl reload nginx
	echo "reloaded nginx"
else
	echo "nginx is not running; it reads the location when it starts"
fi

if systemctl is-active --quiet hifiberry-auth; then
	sudo systemctl restart hifiberry-auth
	echo "restarted hifiberry-auth"
else
	echo "hifiberry-auth is not running; it reads the manifest when it starts"
fi

# The location names port 2718, the daemon's default. api_port is a flat
# key, so only the lines before the first [section] can set it.
conf=/etc/aas/aas.toml
if [ -r "$conf" ]; then
	port=$(awk '/^[[:space:]]*\[/ { exit }
		/^[[:space:]]*api_port[[:space:]]*=/ {
			sub(/^[^=]*=[[:space:]]*/, ""); sub(/[[:space:]]*(#.*)?$/, ""); print; exit
		}' "$conf")
	if [ -n "$port" ] && [ "$port" != 2718 ]; then
		echo "warning: $conf sets api_port = $port, but $nginx_dir/hifiberry-aas.nginx" \
			"proxies to port 2718; change its proxy_pass to match" >&2
	fi
fi
