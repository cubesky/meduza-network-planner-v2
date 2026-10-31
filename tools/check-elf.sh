#!/bin/sh
# Verifies every ELF executable in DIR targets the expected machine, is statically linked and has no
# program interpreter (§14.1). Usage: check-elf.sh DIR "MACHINE NAME AS PRINTED BY readelf -h"
set -eu

dir=$1
machine=$2
status=0
found=0

for f in "$dir"/*; do
	[ -f "$f" ] && [ -x "$f" ] || continue
	case "$f" in *.d) continue ;; esac
	head -c 4 "$f" | grep -q 'ELF' || continue
	found=$((found + 1))
	actual=$(readelf -h "$f" | sed -n 's/^ *Machine: *//p')
	if [ "$actual" != "$machine" ]; then
		echo "FAIL $f: machine '$actual' != '$machine'"
		status=1
	fi
	if readelf -l "$f" | grep -q 'Requesting program interpreter'; then
		echo "FAIL $f: has a program interpreter (dynamically linked)"
		status=1
	fi
	needed=$(readelf -d "$f" 2>/dev/null | grep '(NEEDED)' || true)
	if [ -n "$needed" ]; then
		echo "FAIL $f: dynamic dependencies:"
		echo "$needed"
		status=1
	fi
	[ $status -eq 0 ] && echo "ok   $f ($actual, static)"
done

if [ $found -eq 0 ]; then
	echo "no binaries found in $dir"
	exit 1
fi
exit $status
