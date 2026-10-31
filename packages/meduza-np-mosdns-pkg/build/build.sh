#!/bin/sh
# Builds a static MosDNS for the target platform from the commit pinned in upstream.lock.json, with the pinned
# Go toolchain, and installs the provider tree into OUT. Go cross-compiles, so this runs on the build platform.
#
# Usage: build.sh LOCK OUT TARGETARCH [TARGETVARIANT]
set -eu

lock=$1
out=$2
target_arch=$3
target_variant=${4:-}
work=/build
mkdir -p "$work" "$out"

case "$(uname -m)" in
x86_64) host=amd64 ;;
aarch64) host=arm64 ;;
*) echo "unsupported build host $(uname -m)" >&2; exit 1 ;;
esac

case "$target_arch/$target_variant" in
amd64/) goarch=amd64; goarm= ; machine="Advanced Micro Devices X86-64" ;;
arm64/ | arm64/v8) goarch=arm64; goarm= ; machine="AArch64" ;;
arm/v7) goarch=arm; goarm=7; machine="ARM" ;;
*) echo "unsupported target $target_arch/$target_variant" >&2; exit 1 ;;
esac

# Toolchain
go_url=$(jq -r --arg h "$host" '.toolchain.archives[$h].url' "$lock")
go_sum=$(jq -r --arg h "$host" '.toolchain.archives[$h].sha256' "$lock")
go_ver=$(jq -r '.toolchain.version' "$lock")
curl -fsSL --retry 3 -o "$work/go.tar.gz" "$go_url"
echo "$go_sum  $work/go.tar.gz" | sha256sum -c -
tar -xzf "$work/go.tar.gz" -C /usr/local
export PATH="/usr/local/go/bin:$PATH" GOPATH="$work/gopath" GOCACHE="$work/gocache" GOTOOLCHAIN=local
go version | grep -q "$go_ver " || { echo "FAIL toolchain is not $go_ver" >&2; exit 1; }

# Source, pinned by commit
repo=$(jq -r '.sources[0].git' "$lock")
tag=$(jq -r '.sources[0].tag' "$lock")
commit=$(jq -r '.sources[0].commit' "$lock")
git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" "$repo" "$work/mosdns"
actual=$(git -C "$work/mosdns" rev-parse HEAD)
[ "$actual" = "$commit" ] || { echo "FAIL tag $tag is $actual, lock says $commit" >&2; exit 1; }

cd "$work/mosdns"
jq -r '.goBuild.env | to_entries[] | "\(.key)=\(.value)"' "$lock" >"$work/go.env"
while IFS= read -r kv; do
	export "$kv"
done <"$work/go.env"
go mod download
go mod verify
GOOS=linux GOARCH=$goarch GOARM=$goarm \
	go build -ldflags "$(jq -r '.goBuild.ldflags' "$lock")" -o "$work/mosdns.bin" "$(jq -r '.goBuild.package' "$lock")"
bin="$work/mosdns.bin"

actual_machine=$(readelf -h "$bin" | sed -n 's/^ *Machine: *//p')
[ "$actual_machine" = "$machine" ] || { echo "FAIL machine '$actual_machine' != '$machine'" >&2; exit 1; }
if readelf -l "$bin" | grep -q 'Requesting program interpreter'; then
	echo "FAIL mosdns has a program interpreter" >&2
	exit 1
fi
if readelf -d "$bin" 2>/dev/null | grep -q '(NEEDED)'; then
	echo "FAIL mosdns has dynamic dependencies" >&2
	exit 1
fi

install -D -m 0755 "$bin" "$out/usr/lib/meduza-np/providers/mosdns/mosdns"

# Licenses: MosDNS, the Go standard library, and every module linked into the binary.
lic="$out/usr/share/licenses/meduza-np-mosdns-pkg"
mkdir -p "$lic/mosdns" "$lic/go" "$lic/modules"
cp LICENSE "$lic/mosdns/LICENSE"
cp /usr/local/go/LICENSE "$lic/go/LICENSE"
missing=""
GOOS=linux GOARCH=$goarch GOARM=$goarm go list -deps -f '{{with .Module}}{{if not .Main}}{{.Path}}@{{.Version}} {{.Dir}}{{end}}{{end}}' . |
	sort -u >"$work/modules.txt"
while read -r mod dir; do
	dest="$lic/modules/$mod"
	found=0
	for f in "$dir"/LICENSE* "$dir"/LICENCE* "$dir"/COPYING* "$dir"/NOTICE*; do
		[ -f "$f" ] || continue
		mkdir -p "$dest"
		cp "$f" "$dest/"
		found=1
	done
	[ $found -eq 1 ] || missing="$missing $mod"
done <"$work/modules.txt"
if [ -n "$missing" ]; then
	echo "FAIL modules without a license file:$missing" >&2
	exit 1
fi

meta="$out/usr/share/meduza-np/packages/meduza-np-mosdns-pkg"
mkdir -p "$meta"
jq -n --slurpfile lock "$lock" \
	--arg sha "$(sha256sum "$bin" | cut -d' ' -f1)" \
	--arg machine "$actual_machine" \
	--arg platform "linux/$target_arch${target_variant:+/$target_variant}" \
	--rawfile modules "$work/modules.txt" \
	'{buildInfoVersion: 1, platform: $platform, elfMachine: $machine, static: true,
	  binary: {path: "/usr/lib/meduza-np/providers/mosdns/mosdns", sha256: $sha},
	  builder: $lock[0].builder.image, toolchain: $lock[0].toolchain.version,
	  sources: [$lock[0].sources[] | {name, version, license, git, commit}],
	  modules: ($modules | split("\n") | map(select(length > 0) | split(" ")[0]))}' \
	>"$meta/build-info.json"
echo "ok mosdns $tag ($actual_machine, static)"
