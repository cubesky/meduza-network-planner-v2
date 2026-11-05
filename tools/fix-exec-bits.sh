#!/bin/sh
# Marks tracked/staged scripts with a shebang interpreter path as executable in the git index (mode 100755). Run it after
# `git add` when the files were added on Windows (or from a zip): git there records every new file as 100644, and the
# CI, the release scripts and the OpenWrt/container packaging run these scripts directly.
#
#   sh tools/fix-exec-bits.sh          mark them; prints each file whose mode changes
#   sh tools/fix-exec-bits.sh --check  only list the scripts that are not executable yet (exit 1 if any)
set -eu
cd "$(git rev-parse --show-toplevel)"
missing=$(git ls-files -s | while read -r mode _ _ path; do
	# Rust inner attributes (`#![cfg(unix)]`, optionally spaced) are source, not script shebangs.
	if [ "$mode" = 100644 ] && [ "$(head -c 2 "$path" 2>/dev/null)" = "#!" ]; then
		if head -n 1 "$path" | LC_ALL=C grep -Eq '^#![[:blank:]]*/'; then
			echo "$path"
		fi
	fi
	true
done)
[ -n "$missing" ] || exit 0
if [ "${1:-}" = --check ]; then
	echo "$missing" | sed 's/^/not executable: /'
	exit 1
fi
echo "$missing" | while read -r path; do
	git update-index --chmod=+x -- "$path"
	echo "+x $path"
done
