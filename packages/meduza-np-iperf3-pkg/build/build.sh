# Run with sh: build.sh LOCK OUT. Build both iperf3 and OpenSSL statically from pinned sources.
set -eu
lock=$1
out=$2
work=${BUILD_WORK_DIR:-/build-iperf3}
mkdir -p "$work" "$out"
fetch() {
 name=$1
 version=$(jq -r --arg name "$name" '.sources[] | select(.name==$name) | .version' "$lock")
 archive="$work/$name-$version.tar.gz"
 cached="${SOURCE_CACHE_DIR:-/archives}/$name-$version.tar.gz"
 if [ -f "$cached" ]; then cp "$cached" "$archive"; else
  curl --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 600 -fsSL --retry 3 -o "$archive" "$(jq -r --arg name "$name" '.sources[] | select(.name==$name) | .url' "$lock")"
 fi
 echo "$(jq -r --arg name "$name" '.sources[] | select(.name==$name) | .sha256' "$lock")  $archive" | sha256sum -c -
 tar -xzf "$archive" -C "$work"
}
fetch iperf3
fetch openssl
iperf_version=$(jq -r '.sources[] | select(.name=="iperf3") | .version' "$lock")
ssl_version=$(jq -r '.sources[] | select(.name=="openssl") | .version' "$lock")
deps="$work/ssl"
jobs=${BUILD_JOBS:-$(nproc)}
cd "$work/openssl-$ssl_version"
./config --prefix="$deps" --libdir=lib --openssldir=/etc/ssl no-shared no-tests no-docs no-module no-engine no-dso no-legacy no-comp
make -j"$jobs" build_libs
make install_dev
# Build-only CLI for the RSA authentication smoke; it is not part of the provider package.
make -j"$jobs" apps/openssl
cd "$work/iperf-$iperf_version"
PKG_CONFIG_PATH="$deps/lib/pkgconfig" PKG_CONFIG='pkg-config --static' \
 ./configure --prefix=/usr/lib/meduza-np/providers/iperf3 --disable-shared --enable-static --enable-static-bin \
 --without-sctp --without-ldconfig --with-openssl="$deps" CFLAGS='-Os' LDFLAGS='-static'
grep -q '^#define HAVE_SSL 1' src/iperf_config.h
make -j"$jobs"
bin="$work/iperf-$iperf_version/src/iperf3"
strip "$bin"
if readelf -l "$bin" | grep -q 'Requesting program interpreter'; then echo 'FAIL dynamic interpreter' >&2; exit 1; fi
if readelf -d "$bin" 2>/dev/null | grep -q '(NEEDED)'; then echo 'FAIL dynamic dependency' >&2; exit 1; fi
machine=$(readelf -h "$bin" | sed -n 's/^ *Machine: *//p')
case "${TARGETPLATFORM:-}" in
 linux/amd64) expected='Advanced Micro Devices X86-64' ;;
 linux/arm64) expected='AArch64' ;;
 linux/arm/v7) expected='ARM' ;;
 '') expected="$machine" ;;
 *) echo "unsupported target $TARGETPLATFORM" >&2; exit 1 ;;
esac
[ "$machine" = "$expected" ] || { echo "FAIL machine $machine != $expected" >&2; exit 1; }
"$bin" --version > "$work/version.txt"
grep -F "iperf $iperf_version" "$work/version.txt"
grep -qi 'authentication' "$work/version.txt"
sh "$(dirname "$0")/smoke-ssl.sh" "$bin" "$work/openssl-$ssl_version/apps/openssl" "$work/smoke-ssl"
install -D -m 0755 "$bin" "$out/usr/lib/meduza-np/providers/iperf3/bin/iperf3"
for name in iperf3 openssl; do
 case "$name" in iperf3) source="$work/iperf-$iperf_version" ;; openssl) source="$work/openssl-$ssl_version" ;; esac
 dest="$out/usr/share/licenses/meduza-np-iperf3-pkg/$name"
 mkdir -p "$dest"
 for file in $(jq -r --arg name "$name" '.sources[] | select(.name==$name) | .licenseFiles[]' "$lock"); do cp "$source/$file" "$dest/$file"; done
done
meta="$out/usr/share/meduza-np/packages/meduza-np-iperf3-pkg"
mkdir -p "$meta"
jq -n --slurpfile lock "$lock" --arg sha "$(sha256sum "$bin" | cut -d' ' -f1)" --arg machine "$machine" --arg platform "${TARGETPLATFORM:-unknown}" \
 '{buildInfoVersion:1,platform:$platform,elfMachine:$machine,static:true,features:{openssl:true,authentication:true},binary:{path:"/usr/lib/meduza-np/providers/iperf3/bin/iperf3",sha256:$sha},builder:$lock[0].builder.image,sources:$lock[0].sources}' > "$meta/build-info.json"
echo "ok iperf3 $iperf_version with OpenSSL $ssl_version (static)"
