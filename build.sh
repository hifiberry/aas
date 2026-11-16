#!/bin/bash
# Builds the Debian packages: with sbuild when it is installed, else with
# dpkg-buildpackage on this machine. DIST selects sbuild's distribution.

cd "$(dirname "$0")" || exit 1

if [ -n "$DIST" ]; then
    echo "Using distribution from DIST environment variable: $DIST"
    DIST_ARG="--dist=$DIST"
else
    echo "No DIST environment variable set, using sbuild default"
    DIST_ARG=""
fi

if command -v sbuild >/dev/null 2>&1; then
    sbuild --chroot-mode=unshare \
           --enable-network \
           --no-clean-source \
           $DIST_ARG
else
    echo "sbuild is not installed, building here with dpkg-buildpackage"
    dpkg-buildpackage -us -uc -b
fi
