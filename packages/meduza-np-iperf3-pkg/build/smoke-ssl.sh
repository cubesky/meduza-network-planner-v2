# Run with sh: smoke-ssl.sh IPERF OPENSSL WORK; local loopback, 128 KiB and throwaway keys.
set -eu
bin=$1
openssl=$2
work=$3
mkdir -p "$work"
chmod 0700 "$work"
export OPENSSL_CONF=/dev/null
"$openssl" genrsa -traditional -out "$work/private.pem" 2048 >/dev/null 2>&1
"$openssl" rsa -in "$work/private.pem" -pubout -out "$work/public.pem" >/dev/null 2>&1
hash=$(printf '{meduza}test-password' | sha256sum | cut -d' ' -f1)
printf 'meduza,%s\n' "$hash" > "$work/users.csv"
port=${IPERF_SMOKE_PORT:-18531}
pid=
trap '[ -z "$pid" ] || kill "$pid" 2>/dev/null || true' EXIT INT TERM
timeout 15 "$bin" -s -1 -d -B 127.0.0.1 -p "$port" --rsa-private-key-path "$work/private.pem" --authorized-users-path "$work/users.csv" > "$work/server.log" 2>&1 &
pid=$!
sleep 1
IPERF3_PASSWORD=test-password timeout 10 "$bin" -c 127.0.0.1 -p "$port" -u -n 128K -b 0 -J --username meduza --rsa-public-key-path "$work/public.pem" --connect-timeout 2000 > "$work/client.json"
wait "$pid"
pid=
jq -e '(.error == null) and (.end.sum_sent.bytes > 0) and (.end.sum_received.bytes > 0)' "$work/client.json" >/dev/null
grep -q 'Authentication succeeded' "$work/server.log"
timeout 15 "$bin" -s -1 -d -B 127.0.0.1 -p "$port" --rsa-private-key-path "$work/private.pem" --authorized-users-path "$work/users.csv" > "$work/reject-server.log" 2>&1 &
pid=$!
sleep 1
if IPERF3_PASSWORD=wrong-password timeout 10 "$bin" -c 127.0.0.1 -p "$port" -u -n 128K -b 0 -J --username meduza --rsa-public-key-path "$work/public.pem" --connect-timeout 2000 > "$work/reject-client.json"; then
 echo 'FAIL wrong password accepted' >&2; exit 1
else
 code=$?
 [ "$code" -ne 124 ] || { echo 'FAIL authentication check timed out' >&2; exit 1; }
fi
jq -e '(.error | type == "string") and (.start.connected | length == 0)' "$work/reject-client.json" >/dev/null
wait "$pid" || true
pid=
grep -q 'Authentication failed' "$work/reject-server.log"
echo 'ok RSA-authenticated transfer and wrong-password rejection'
