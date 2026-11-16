# Run with sh: build.sh LOCK OUT; only the pinned source/build dependencies are used.
set -eu
lock=$1
out=$2
work=${BUILD_WORK_DIR:-/build}
mkdir -p "$work" "$out"
version=$(jq -r '.sources[0].version' "$lock")
archive="$work/mtr-$version.tar.gz"
cached="${SOURCE_CACHE_DIR:-/archives}/mtr-$version.tar.gz"
if [ -f "$cached" ]; then
 cp "$cached" "$archive"
else
 curl --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 300 -fsSL --retry 3 -o "$archive" "$(jq -r '.sources[0].url' "$lock")"
fi
echo "$(jq -r '.sources[0].sha256' "$lock")  $archive" | sha256sum -c -
tar -xzf "$archive" -C "$work"
cd "$work/mtr-$version"
prefix=/usr/lib/meduza-np/providers/mtr
./configure --prefix="$prefix" --sbindir="$prefix/bin" --without-gtk --without-ncurses --without-ncursesw --without-jansson --without-ipinfo \
 CFLAGS='-Os' LDFLAGS='-static' ac_cv_lib_cap_cap_set_proc=no
make -j"${BUILD_JOBS:-$(nproc)}"
for binary in mtr mtr-packet; do
 if readelf -l "$binary" | grep -q 'Requesting program interpreter'; then echo "FAIL dynamic interpreter: $binary" >&2; exit 1; fi
 if readelf -d "$binary" 2>/dev/null | grep -q '(NEEDED)'; then echo "FAIL dynamic dependency: $binary" >&2; exit 1; fi
 strip "$binary"
done
machine=$(readelf -h mtr | sed -n 's/^ *Machine: *//p')
case "${TARGETPLATFORM:-}" in
 linux/amd64) expected='Advanced Micro Devices X86-64' ;;
 linux/arm64) expected='AArch64' ;;
 linux/arm/v7) expected='ARM' ;;
 '') expected="$machine" ;;
 *) echo "unsupported target $TARGETPLATFORM" >&2; exit 1 ;;
esac
[ "$machine" = "$expected" ] || { echo "FAIL machine $machine != $expected" >&2; exit 1; }
# Do not call make install: its upstream install hook adds setuid/file capabilities.
install -D -m 0755 mtr "$out/usr/lib/meduza-np/providers/mtr/bin/mtr"
install -D -m 0755 mtr-packet "$out/usr/lib/meduza-np/providers/mtr/bin/mtr-packet"
"$out/usr/lib/meduza-np/providers/mtr/bin/mtr" --version | grep -F "$version"
# Protocol smoke verifies the real sibling helper starts; does not send packets.
if printf '1 check-support feature ip-4\n' | "$out/usr/lib/meduza-np/providers/mtr/bin/mtr-packet" > "$work/packet-smoke.out" 2> "$work/packet-smoke.err"; then
 grep -q 'feature-support support ok' "$work/packet-smoke.out"
elif grep -q 'Failure to open.*sockets: Permission denied' "$work/packet-smoke.err" && [ "${MTR_REQUIRE_SOCKET_SMOKE:-0}" != 1 ]; then
 echo 'packet socket smoke unavailable in build sandbox; runtime CAP_NET_RAW is required'
else
 cat "$work/packet-smoke.err" >&2
 exit 1
fi
licenses="$out/usr/share/licenses/meduza-np-mtr-pkg/mtr"
mkdir -p "$licenses"
for file in $(jq -r '.sources[0].licenseFiles[]' "$lock"); do cp "$file" "$licenses/$file"; done
meta="$out/usr/share/meduza-np/packages/meduza-np-mtr-pkg"
mkdir -p "$meta"
jq -n --slurpfile lock "$lock" --arg sha "$(sha256sum mtr | cut -d' ' -f1)" --arg helperSha "$(sha256sum mtr-packet | cut -d' ' -f1)" \
 --arg machine "$(readelf -h mtr | sed -n 's/^ *Machine: *//p')" --arg platform "${TARGETPLATFORM:-unknown}" \
 '{buildInfoVersion:1,platform:$platform,elfMachine:$machine,static:true,binary:{path:"/usr/lib/meduza-np/providers/mtr/bin/mtr",sha256:$sha},helper:{path:"/usr/lib/meduza-np/providers/mtr/bin/mtr-packet",sha256:$helperSha},builder:$lock[0].builder.image,sources:$lock[0].sources}' > "$meta/build-info.json"
echo "ok mtr $version (static, matching packet helper)"
