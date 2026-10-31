#!/bin/sh
# Turns the `.apk.stage/` directories written by `meduza-np-release openwrt` into apk-tools 3 packages (D29).
# OpenWrt's own apk is built without `mkpkg`, so the pinned Alpine image (same pin as the base container) runs it;
# the package architecture is metadata, so the amd64 image builds packages for every OpenWrt architecture.
# Afterwards `meduza-np-release artifact --stage` records size + SHA-256 and the stage is removed.
#
# Usage: tools/openwrt-apk.sh DIR RELEASE_TOOL      (needs podman)
set -eu

DIR=$(cd "$1" && pwd)
REL=$2
IMAGE=docker.io/library/alpine:3.24@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a

ls "$DIR"/*.apk.stage >/dev/null 2>&1 || { echo "no apk stages in $DIR"; exit 0; }
# --network=none: mkpkg needs nothing from outside; files keep root ownership inside the container
podman run --rm --network=none --platform linux/amd64 -v "$DIR:/w:Z" "$IMAGE" sh -euc '
	for s in /w/*.apk.stage; do
		(cd "$s" && sh ./mkpkg.sh)
	done
	chown -R "$(stat -c %u:%g /w)" /w
'
for s in "$DIR"/*.apk.stage; do
	"$REL" artifact --stage "$s" >/dev/null
	rm -rf "$s"
done
ls "$DIR"/*.apk
