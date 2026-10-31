#!/bin/sh
# Builds a fully static OpenVPN (musl + OpenSSL + libcap-ng) from the sources pinned in upstream.lock.json and
# installs the provider tree into OUT (§14.1). Runs inside the pinned Alpine builder for the *target* platform
# (natively or under QEMU), so `./config` and `configure` detect the right architecture without a cross toolchain.
#
# Usage: build.sh LOCK OUT
set -eu

lock=$1
out=$2
work=/build
mkdir -p "$work" "$out"

src() { jq -r --arg n "$1" --arg k "$2" '.sources[] | select(.name == $n) | .[$k] // empty' "$lock"; }

fetch_tarball() {
	name=$1
	url=$(src "$name" url)
	sum=$(src "$name" sha256)
	ver=$(src "$name" version)
	file="$work/$name-$ver.tar.gz"
	curl -fsSL --retry 3 -o "$file" "$url"
	echo "$sum  $file" | sha256sum -c -
	tar -xzf "$file" -C "$work"
}

fetch_git() {
	name=$1
	repo=$(src "$name" git)
	tag=$(src "$name" tag)
	commit=$(src "$name" commit)
	git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" "$repo" "$work/$name"
	actual=$(git -C "$work/$name" rev-parse HEAD)
	if [ "$actual" != "$commit" ]; then
		echo "FAIL $name: tag $tag is $actual, lock says $commit" >&2
		exit 1
	fi
}

copy_licenses() {
	name=$1
	dir=$2
	dest="$out/usr/share/licenses/meduza-np-openvpn-pkg/$name"
	mkdir -p "$dest"
	for f in $(jq -r --arg n "$name" '.sources[] | select(.name == $n) | .licenseFiles[]' "$lock"); do
		cp "$dir/$f" "$dest/$f"
	done
}

deps=/opt/deps
jobs=$(nproc)

fetch_tarball openvpn
fetch_tarball openssl
fetch_git libcap-ng

openvpn_ver=$(src openvpn version)
openssl_ver=$(src openssl version)

# libcap-ng: required by OpenVPN on Linux (capability retention after dropping privileges).
cd "$work/libcap-ng"
./autogen.sh >/dev/null
./configure --prefix="$deps" --libdir="$deps/lib" --disable-shared --enable-static \
	--without-python --without-python3 --disable-cap-audit
make -j"$jobs"
make install

# OpenSSL: static libraries only; no engines/modules to load at runtime, no legacy provider (configs use AEAD).
cd "$work/openssl-$openssl_ver"
./config --prefix="$deps" --libdir=lib --openssldir=/etc/ssl \
	no-shared no-tests no-docs no-module no-engine no-dso no-legacy no-comp no-ssl3 no-weak-ssl-ciphers
make -j"$jobs" build_libs
make install_dev

# OpenVPN: no compression, no DCO (needs libnl + kernel module), no plugins (Access uses the via-file
# auth-user-pass-verify hook, not a .so plugin), management interface kept for status (§9.3).
cd "$work/openvpn-$openvpn_ver"
PKG_CONFIG_PATH="$deps/lib/pkgconfig" PKG_CONFIG="pkg-config --static" \
	./configure --prefix=/usr --sysconfdir=/etc \
	--disable-lzo --disable-lz4 --disable-dco --disable-plugins --disable-plugin-auth-pam \
	--disable-plugin-down-root --disable-pkcs11 --disable-systemd --disable-selinux --disable-unit-tests \
	--disable-dns-updown-by-default --enable-management --with-crypto-library=openssl \
	LDFLAGS="-static"
make -j"$jobs" LDFLAGS="-all-static"

bin="$work/openvpn-$openvpn_ver/src/openvpn/openvpn"
strip "$bin"

# Static ELF gate: no interpreter, no NEEDED entries (§14.1: libc included, not only libssl).
if readelf -l "$bin" | grep -q 'Requesting program interpreter'; then
	echo "FAIL openvpn has a program interpreter" >&2
	exit 1
fi
if readelf -d "$bin" 2>/dev/null | grep -q '(NEEDED)'; then
	echo "FAIL openvpn has dynamic dependencies:" >&2
	readelf -d "$bin" | grep '(NEEDED)' >&2
	exit 1
fi

# Smoke: the binary runs on this platform and reports the pinned versions and the capabilities we rely on.
version=$("$bin" --version 2>&1 || true)
echo "$version" | head -n 3
echo "$version" | grep -q "OpenVPN $openvpn_ver" || { echo "FAIL unexpected version" >&2; exit 1; }
echo "$version" | grep -q "OpenSSL $openssl_ver" || { echo "FAIL not linked against OpenSSL $openssl_ver" >&2; exit 1; }
# 2.7 no longer prints compile-time defines; the configure result is authoritative.
cfg_h="$work/openvpn-$openvpn_ver/config.h"
grep -q '^#define ENABLE_MANAGEMENT 1' "$cfg_h" || { echo "FAIL management interface missing" >&2; exit 1; }
for off in ENABLE_DCO ENABLE_LZO ENABLE_LZ4 ENABLE_PLUGIN; do
	if grep -q "^#define $off 1" "$cfg_h"; then
		echo "FAIL $off is enabled" >&2
		exit 1
	fi
done
# Functional: the management interface really binds (key-less smoke run: held before connecting, then stopped;
# the static-crypto allowance only lets 2.7 start without TLS material, it is not used by generated configs).
"$bin" --dev null --allow-deprecated-insecure-static-crypto --management 127.0.0.1 0 --management-hold --verb 3 >/tmp/ovpn-mgmt.log 2>&1 &
pid=$!
sleep 2
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
grep -q 'MANAGEMENT: TCP Socket listening' /tmp/ovpn-mgmt.log || { cat /tmp/ovpn-mgmt.log >&2; echo "FAIL management did not listen" >&2; exit 1; }

install -D -m 0755 "$bin" "$out/usr/lib/meduza-np/providers/openvpn/sbin/openvpn"
copy_licenses openvpn "$work/openvpn-$openvpn_ver"
copy_licenses openssl "$work/openssl-$openssl_ver"
copy_licenses libcap-ng "$work/libcap-ng"

meta="$out/usr/share/meduza-np/packages/meduza-np-openvpn-pkg"
mkdir -p "$meta"
machine=$(readelf -h "$bin" | sed -n 's/^ *Machine: *//p')
jq -n --slurpfile lock "$lock" \
	--arg sha "$(sha256sum "$bin" | cut -d' ' -f1)" \
	--arg machine "$machine" \
	--arg platform "${TARGETPLATFORM:-unknown}" \
	--arg version "$(echo "$version" | head -n 1)" \
	'{buildInfoVersion: 1, platform: $platform, elfMachine: $machine, static: true,
	  binary: {path: "/usr/lib/meduza-np/providers/openvpn/sbin/openvpn", sha256: $sha, versionLine: $version},
	  builder: $lock[0].builder.image,
	  sources: [$lock[0].sources[] | {name, version, license, url, git, commit, sha256} | with_entries(select(.value != null))]}' \
	>"$meta/build-info.json"
echo "ok openvpn $openvpn_ver ($machine, static)"
