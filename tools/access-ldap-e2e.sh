#!/bin/sh
# OpenVPN Access against a live OpenLDAP (§6.5, §9.3) on a Linux host with Podman or Docker. Starts OpenLDAP
# (osixia/openldap, memberOf overlay, read-only service account) with a CA and server certificate generated here,
# ldaps on 127.0.0.1:$LDAPS_PORT and StartTLS on 127.0.0.1:$LDAP_PORT, then runs
# packages/meduza-np-manager/server/test/access-ldap.test.ts:
#   1. manager AccessService through the node route: allow/deny matrix, filter injection, ambiguous uid,
#      StartTLS, memberOf, certificate verification, wrong service password, unreachable server, throttling
#   2. the hook chain: target/debug/openvpn-auth (via-file) → planner Access socket → manager → LDAP
#
# Usage: tools/access-ldap-e2e.sh [WORKDIR]   (needs: podman or docker, openssl, node 24 + pnpm, and
#        `cargo build -p meduza-np-planner -p meduza-np-openvpn` for part 2; the test fails if they are missing)
# Env: CONTAINER=podman|docker, LDAP_IMAGE, LDAP_PORT (11389), LDAPS_PORT (11636), KEEP=1 leaves the container.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
CONTAINER=${CONTAINER:-$(command -v podman >/dev/null 2>&1 && echo podman || echo docker)}
LDAP_IMAGE=${LDAP_IMAGE:-docker.io/osixia/openldap:1.5.0}
LDAP_PORT=${LDAP_PORT:-11389}
LDAPS_PORT=${LDAPS_PORT:-11636}
NAME=mnp-access-ldap
mkdir -p "$W/certs"

cleanup() {
	[ "${KEEP:-0}" = 1 ] && return 0
	$CONTAINER rm -f $NAME >/dev/null 2>&1 || true
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; $CONTAINER logs $NAME 2>&1 | tail -40 >&2 || true; exit 1; }

echo "== certificates"
C=$W/certs
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=meduza-np test LDAP CA" \
	-keyout "$C/ca.key" -out "$C/ca.crt" 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=localhost" \
	-keyout "$C/ldap.key" -out "$C/ldap.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' >"$C/ext.cnf"
openssl x509 -req -in "$C/ldap.csr" -CA "$C/ca.crt" -CAkey "$C/ca.key" -CAcreateserial -days 2 -extfile "$C/ext.cnf" \
	-out "$C/ldap.crt" 2>/dev/null
# osixia generates DH parameters on first start unless given (slow). The named RFC 7919 group is instant; GnuTLS in
# slapd does not read the X9.42 parameters `openssl dhparam -dsaparam` writes.
openssl genpkey -genparam -algorithm DH -pkeyopt group:ffdhe2048 -out "$C/dhparam.pem" 2>/dev/null
chmod 644 "$C"/*

ADMIN_PASSWORD=$(openssl rand -hex 16)
SVC_PASSWORD=$(openssl rand -hex 16)

echo "== OpenLDAP ($LDAP_IMAGE)"
$CONTAINER rm -f $NAME >/dev/null 2>&1 || true
# slapd sizes tables by the open-file limit; the runtime default (1M+) makes it allocate gigabytes.
$CONTAINER run -d --name $NAME --ulimit nofile=1024:1024 \
	-p 127.0.0.1:$LDAP_PORT:389 -p 127.0.0.1:$LDAPS_PORT:636 \
	-v "$C:/container/service/slapd/assets/certs:ro" \
	-e LDAP_ORGANISATION="Meduza NP test" -e LDAP_DOMAIN=example.test \
	-e LDAP_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
	-e LDAP_READONLY_USER=true -e LDAP_READONLY_USER_USERNAME=readonly -e LDAP_READONLY_USER_PASSWORD="$SVC_PASSWORD" \
	-e LDAP_TLS=true -e LDAP_TLS_CRT_FILENAME=ldap.crt -e LDAP_TLS_KEY_FILENAME=ldap.key \
	-e LDAP_TLS_CA_CRT_FILENAME=ca.crt -e LDAP_TLS_DH_PARAM_FILENAME=dhparam.pem \
	-e LDAP_TLS_VERIFY_CLIENT=never -e LDAP_TLS_ENFORCE=false \
	"$LDAP_IMAGE" --copy-service >/dev/null

i=0
until $CONTAINER exec $NAME ldapsearch -x -H ldap://localhost -b dc=example,dc=test -D cn=readonly,dc=example,dc=test \
	-w "$SVC_PASSWORD" -s base dn >/dev/null 2>&1; do
	i=$((i + 1))
	[ $i -gt 90 ] && fail "OpenLDAP not ready after 90 s"
	[ "$($CONTAINER inspect --format '{{.State.Running}}' $NAME 2>/dev/null)" = true ] || fail "OpenLDAP container exited"
	sleep 1
done
echo "   ready after ${i} s"

echo "== manager build"
cd "$REPO"
pnpm --filter @meduza-np/manager exec tsc -p server

echo "== access-ldap test"
MEDUZA_NP_LDAP_TEST_REQUIRED=1 \
	MEDUZA_NP_LDAP_TEST_LDAPS="ldaps://localhost:$LDAPS_PORT" \
	MEDUZA_NP_LDAP_TEST_STARTTLS="ldap://localhost:$LDAP_PORT" \
	MEDUZA_NP_LDAP_TEST_CA="$C/ca.crt" \
	MEDUZA_NP_LDAP_TEST_BASE=dc=example,dc=test \
	MEDUZA_NP_LDAP_TEST_ADMIN_PASSWORD="$ADMIN_PASSWORD" \
	MEDUZA_NP_LDAP_TEST_SVC_DN=cn=readonly,dc=example,dc=test \
	MEDUZA_NP_LDAP_TEST_SVC_PASSWORD="$SVC_PASSWORD" \
	node --test --test-concurrency=1 packages/meduza-np-manager/dist/server/test/access-ldap.test.js || fail "access-ldap test"
echo "PASS access-ldap e2e"
