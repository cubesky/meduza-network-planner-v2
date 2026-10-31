#!/bin/sh
# Runs every node binary with --version natively or under QEMU user emulation (§14.1 smoke).
# Usage: smoke-bins.sh DIR [qemu-binary]
set -eu

dir=$1
qemu=${2:-}
status=0

for f in "$dir"/meduza-np-* "$dir"/mnp-*; do
	[ -f "$f" ] && [ -x "$f" ] || continue
	case "$f" in *.d) continue ;; esac
	head -c 4 "$f" | grep -q 'ELF' || continue
	if out=$($qemu "$f" --version 2>&1); then
		echo "ok   $(basename "$f"): $out"
	else
		echo "FAIL $(basename "$f"): $out"
		status=1
	fi
done
exit $status
