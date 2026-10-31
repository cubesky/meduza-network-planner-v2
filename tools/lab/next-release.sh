#!/bin/sh
# Builds the next signed test release from a previous site: rebuilds the musl binaries from the current tree,
# repackages the packages whose fingerprint changed (or that are forced with FORCE="pkg ..."), reuses the rest.
# Recipe-built providers (openvpn-pkg, mosdns-pkg) are packaged from their recipe output: TREES="pkg=DIR ..."
# (e.g. meduza-np-mosdns-pkg=/root/mnp/packages/meduza-np-mosdns-pkg/out/amd64); they must also be in PKGS.
#
#   tools/lab/next-release.sh PREV_SITE NEW_SITE VERSION SEQUENCE KEYFILE
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
prev=$1 out=$2 version=$3 seq=$4 key=$5
TARGET=x86_64-unknown-linux-musl
BIN="$REPO/target/$TARGET/release"
PKGS=${PKGS:-"meduza-np-planner meduza-np-updater meduza-np-cli meduza-np-wireguard meduza-np-tinc meduza-np-dashboard"}
W=$(mktemp -d)

tree_of() { for t in ${TREES:-}; do [ "${t%%=*}" = "$1" ] && echo "${t#*=}"; done; return 0; }
cd "$REPO"
# shellcheck disable=SC2046
cargo zigbuild -q --locked --release --target $TARGET $(for p in $PKGS; do [ -n "$(tree_of "$p")" ] || printf -- '-p %s ' "$p"; done)
cargo build -q --locked --release -p meduza-np-release
REL="$REPO/target/release/meduza-np-release"
baseline=$(ls "$prev"/releases/*/manifest.json | sort -V | tail -1)
args=""; for p in $PKGS; do args="$args --package $p"; done
# shellcheck disable=SC2086
"$REL" fingerprint --targets $TARGET $args >"$W/fp.json"
for p in ${FORCE:-}; do jq --arg p "$p" --arg v "$version" '.[$p] = "forced-" + $v' "$W/fp.json" >"$W/fp2.json" && mv "$W/fp2.json" "$W/fp.json"; done
"$REL" matrix --fingerprints "$W/fp.json" --group-version "$version" --baseline "$baseline" >"$W/matrix.json"
jq -r 'to_entries[] | "\(.key) \(.value.version) \(if .value.changed then "CHANGED" else "reused from \(.value.reusedFromGroup)" end)"' "$W/matrix.json"
mkdir -p "$W/dist"
for p in $(jq -r 'to_entries[] | select(.value.changed) | .key' "$W/matrix.json"); do
	# a package new to this group has no baseline entry: the matrix marks it changed
	v=$(jq -r --arg p "$p" '.[$p].version' "$W/matrix.json")
	t=$(tree_of "$p")
	if [ -n "$t" ]; then
		"$REL" package --package "$p" --version "$v" --platform linux/amd64 --tree "$t" --out "$W/dist" >/dev/null
	else
		"$REL" package --package "$p" --version "$v" --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist" >/dev/null
	fi
done
rm -rf "$out" && cp -r "$prev" "$out"
"$REL" group --group-version "$version" --sequence "$seq" --commit lab --matrix "$W/matrix.json" --artifacts "$W/dist" \
	--baseline "$baseline" --url-base packages --site "$out" --platforms linux/amd64 --key "$key" >/dev/null
cp "$W"/dist/*.tar.gz "$out/packages/" 2>/dev/null || true
rm -rf "$W"
echo "site $out: $version (sequence $seq)"
