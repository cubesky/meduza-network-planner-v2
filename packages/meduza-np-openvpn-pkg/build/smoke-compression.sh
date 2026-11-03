#!/bin/sh
# Runs in the target builder on every architecture: actual library round trips plus OpenVPN's local preflight.
# No tunnel, peer, external traffic or privileged device is needed.
set -eu
bin=$1
deps=$2
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cat >"$tmp/roundtrip.c" <<'C'
#include <stdio.h>
#include <string.h>
#include <lzo/lzo1x.h>
#include <lz4.h>

static unsigned char input[4096], packed[8192], decoded[4096];
static lzo_align_t work[(LZO1X_1_MEM_COMPRESS + sizeof(lzo_align_t) - 1) / sizeof(lzo_align_t)];

int main(void)
{
    memset(input, 'M', sizeof(input));
    lzo_uint packed_len = sizeof(packed), decoded_len = sizeof(decoded);
    if (lzo_init() != LZO_E_OK
        || lzo1x_1_compress(input, sizeof(input), packed, &packed_len, work) != LZO_E_OK
        || packed_len >= sizeof(input)
        || lzo1x_decompress_safe(packed, packed_len, decoded, &decoded_len, NULL) != LZO_E_OK
        || decoded_len != sizeof(input) || memcmp(input, decoded, sizeof(input))) return 1;
    int n = LZ4_compress_default((const char *)input, (char *)packed, sizeof(input), sizeof(packed));
    if (n <= 0 || n >= (int)sizeof(input)
        || LZ4_decompress_safe((const char *)packed, (char *)decoded, n, sizeof(decoded)) != sizeof(input)
        || memcmp(input, decoded, sizeof(input))) return 2;
    puts("ok LZO and LZ4 compressed round trips");
    return 0;
}
C
cc -static -I"$deps/include" -L"$deps/lib" "$tmp/roundtrip.c" -llzo2 -llz4 -o "$tmp/roundtrip"
"$tmp/roundtrip"

"$bin" --genkey secret "$tmp/static.key"
crypto() {
	name=$1; shift
	if ! "$bin" --test-crypto --secret "$tmp/static.key" --allow-deprecated-insecure-static-crypto \
		--cipher AES-256-CBC --auth SHA256 --verb 3 "$@" >"$tmp/$name.log" 2>&1; then
		cat "$tmp/$name.log" >&2
		echo "FAIL OpenVPN compression preflight: $name" >&2
		exit 1
	fi
	grep -q 'crypto self-test mode SUCCEEDED' "$tmp/$name.log" || { cat "$tmp/$name.log" >&2; exit 1; }
	echo "ok OpenVPN compression preflight: $name"
}
crypto none --allow-compression no
crypto lzo-no --comp-lzo no
crypto lzo-asym --comp-lzo yes --allow-compression asym
crypto lz4 --compress lz4 --allow-compression asym
crypto lz4-v2 --compress lz4-v2 --allow-compression asym
crypto stub --compress stub
crypto stub-v2 --compress stub-v2
crypto migrate --compress migrate
