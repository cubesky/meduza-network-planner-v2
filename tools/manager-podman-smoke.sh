#!/bin/sh
# Manager image acceptance (§14.4) on a Linux host with Podman (rootless or rootful). Builds the production image from
# packages/meduza-np-manager/Containerfile and runs it the way deploy/podman/meduza-np-manager.container does
# (read-only root, tmpfs /tmp, no capabilities, no-new-privileges, Podman secrets mounted for uid 10001, named data
# volume, loopback-only port), then checks:
#   1. image: runs as 10001, no dev dependencies, built WebUI + package metadata present
#   2. fail closed: a plain-http PUBLIC_ORIGIN without ALLOW_INSECURE_ORIGIN refuses to start
#   3. first start: /healthz, /readyz, WebUI, container health check, bootstrap state "open"
#   4. first admin over the real WebAuthn ceremony (software passkey, tools/lab/manager-api.mjs), node creation,
#      enrollment token, node enrollment with an Ed25519 key over the node API
#   5. host admin tool: status, consistent backup into the volume; the root filesystem is not writable
#   6. graceful SIGTERM stop (exit 0), re-creation without the bootstrap secret keeps all data: bootstrap stays
#      "completed", the passkey still signs in, the node is still enrolled, recovery invite works
#   7. the Quadlet files in deploy/podman are accepted by the local Quadlet generator (dry run)
#
# Usage: tools/manager-podman-smoke.sh [WORKDIR]   (needs: podman, node 24, curl, jq)
# Env: SKIP_BUILD=1 reuses $IMAGE; KEEP=1 leaves container, volume and secrets for inspection;
#      MANAGER_SMOKE_PORT (default 18787).
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
IMAGE=${IMAGE:-localhost/meduza-np-manager:smoke}
PORT=${MANAGER_SMOKE_PORT:-18787}
ORIGIN=http://127.0.0.1:$PORT
NAME=mnp-manager-smoke
VOL=mnp-manager-smoke-data
SECRETS="mnp-smoke-session mnp-smoke-secret mnp-smoke-bootstrap"
APP=/app/packages/meduza-np-manager/dist/server/src
mkdir -p "$W"

fail() { echo "FAIL: $*" >&2; podman logs $NAME 2>&1 | tail -30 >&2 || true; exit 1; }
step() { echo "== $*"; }
api() { MANAGER=$ORIGIN ORIGIN=$ORIGIN STATE="$W/admin.json" node "$REPO/tools/lab/manager-api.mjs" "$@"; }
cleanup() {
	[ "${KEEP:-0}" = 1 ] && return 0
	podman rm -f $NAME >/dev/null 2>&1 || true
	podman volume rm -f $VOL >/dev/null 2>&1 || true
	for s in $SECRETS; do podman secret rm $s >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

wait_ready() {
	i=0
	until curl -fs "$ORIGIN/readyz" >/dev/null 2>&1; do
		i=$((i + 1))
		[ $i -gt 60 ] && fail "manager not ready after 60 s"
		[ "$(podman inspect --format '{{.State.Running}}' $NAME 2>/dev/null)" = true ] || fail "manager container exited"
		sleep 1
	done
}

# Same hardening and health check as the Quadlet unit (Podman's default OCI image format drops the Containerfile
# HEALTHCHECK, so the runtime has to define it, as the Quadlet unit's HealthCmd does); extra arguments (e.g. the bootstrap secret) go before the image.
start() {
	podman run -d --name $NAME --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL \
		--security-opt no-new-privileges -p 127.0.0.1:$PORT:8787 -v $VOL:/var/lib/meduza-np-manager:Z 		--health-cmd "node $APP/healthcheck.js" --health-interval 30s --health-timeout 5s --health-retries 3 \
		--secret mnp-smoke-session,type=mount,target=/run/secrets/session.key,mode=0400,uid=10001,gid=10001 \
		--secret mnp-smoke-secret,type=mount,target=/run/secrets/secret.key,mode=0400,uid=10001,gid=10001 \
		-e PUBLIC_ORIGIN=$ORIGIN -e ALLOW_INSECURE_ORIGIN=1 "$@" $IMAGE >/dev/null
	wait_ready
}

cleanup
if [ "${SKIP_BUILD:-0}" != 1 ]; then
	step "build the manager image"
	podman build -q -f "$REPO/packages/meduza-np-manager/Containerfile" -t $IMAGE "$REPO" >/dev/null
fi

step "1. image contents"
[ "$(podman image inspect --format '{{.Config.User}}' $IMAGE)" = "10001:10001" ] || fail "image does not run as 10001:10001"
podman run --rm --entrypoint sh $IMAGE -c '
	set -e
	test -f /usr/share/meduza-np-manager/web/index.html
	test -f /usr/share/meduza-np-manager/packages/meduza-np-wireguard/manifest.json
	test -d /usr/share/meduza-np-manager/schemas
	if ls /app/node_modules/.pnpm | grep -E "^(vite|typescript|preact|@preact|@types)[@+]"; then echo "dev dependencies in the image"; exit 1; fi
	test "$(id -u)" = 10001' || fail "image contents"

step "2. plain-http origin is refused without ALLOW_INSECURE_ORIGIN"
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | podman secret create mnp-smoke-session - >/dev/null
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' | podman secret create mnp-smoke-secret - >/dev/null
head -c 24 /dev/urandom | base64 | tr -d '/+=\n' >"$W/bootstrap"
podman secret create mnp-smoke-bootstrap "$W/bootstrap" >/dev/null
if podman run --rm --read-only -e PUBLIC_ORIGIN=$ORIGIN \
	--secret mnp-smoke-session,type=mount,target=/run/secrets/session.key,uid=10001 \
	--secret mnp-smoke-secret,type=mount,target=/run/secrets/secret.key,uid=10001 $IMAGE >"$W/insecure.log" 2>&1; then
	fail "manager started with an http origin"
fi
grep -q "must use https" "$W/insecure.log" || { cat "$W/insecure.log" >&2; fail "no configuration error for the http origin"; }

step "3. first start"
podman volume create $VOL >/dev/null
start --secret mnp-smoke-bootstrap,type=mount,target=/run/secrets/bootstrap,mode=0400,uid=10001,gid=10001 \
	-e BOOTSTRAP_TOKEN_FILE=/run/secrets/bootstrap
curl -fs "$ORIGIN/healthz" | jq -e '.status == "ok"' >/dev/null || fail "/healthz"
curl -fs "$ORIGIN/readyz" | jq -e '.status == "ready"' >/dev/null || fail "/readyz"
curl -fs "$ORIGIN/" | grep -qi '<html' || fail "WebUI not served"
podman healthcheck run $NAME >/dev/null || fail "container health check"
curl -fs "$ORIGIN/api/v1/auth/session" | jq -e '.bootstrap == "open" and .authenticated == false' >/dev/null || fail "bootstrap not open"

step "4. first admin (passkey ceremony), node, enrollment"
rm -f "$W/admin.json"
api bootstrap "$W/bootstrap" owner | grep -q '"authenticated": true' || fail "bootstrap sign-in"
api node smoke-1 static >/dev/null || fail "create node"
api token smoke-1 >"$W/enroll-token" || fail "enrollment token"
ENROLL_TOKEN=$(cat "$W/enroll-token") ORIGIN=$ORIGIN node --input-type=module -e '
	import { generateKeyPairSync } from "node:crypto";
	const { publicKey } = generateKeyPairSync("ed25519");
	const raw = publicKey.export({ format: "der", type: "spki" }).subarray(-32).toString("base64");
	const res = await fetch(`${process.env.ORIGIN}/api/v1/nodes/enroll`, {
		method: "POST", headers: { "content-type": "application/json" },
		body: JSON.stringify({ enrollmentToken: process.env.ENROLL_TOKEN, publicKey: raw, hostname: "smoke-1", arch: "x86_64" }),
	});
	const body = await res.json();
	if (!res.ok || body.nodeId !== "smoke-1") { console.error(res.status, body); process.exit(1); }' || fail "node enrollment"
curl -fs "$ORIGIN/api/v1/auth/session" | jq -e '.bootstrap == "completed"' >/dev/null || fail "bootstrap not completed"

step "5. host admin tool, read-only root"
podman exec $NAME node $APP/admin.js status >"$W/status.json" || fail "admin status"
jq -e '.bootstrap == "completed" and .admins == 1 and .nodes == 1 and .schemaVersion == .buildSchemaVersion' "$W/status.json" >/dev/null ||
	{ cat "$W/status.json" >&2; fail "admin status content"; }
podman exec $NAME node $APP/admin.js backup /var/lib/meduza-np-manager/backups/smoke.sqlite >/dev/null || fail "admin backup"
podman exec $NAME sh -c 'test -s /var/lib/meduza-np-manager/backups/smoke.sqlite' || fail "backup file missing"
if podman exec $NAME sh -c 'touch /app/x' 2>/dev/null; then fail "root filesystem is writable"; fi

step "6. graceful stop, re-creation without the bootstrap secret keeps the data"
podman stop -t 30 $NAME >/dev/null
[ "$(podman inspect --format '{{.State.ExitCode}}' $NAME)" = 0 ] || fail "SIGTERM stop did not exit 0"
podman rm $NAME >/dev/null
start
curl -fs "$ORIGIN/api/v1/auth/session" | jq -e '.bootstrap == "completed"' >/dev/null || fail "bootstrap state lost"
jq 'del(.cookie, .csrf)' "$W/admin.json" >"$W/admin.tmp" && mv "$W/admin.tmp" "$W/admin.json"
api status smoke-1 >"$W/node.json" || fail "passkey sign-in after re-creation"
grep -q 'smoke-1' "$W/node.json" || fail "node lost after re-creation"
grep -Eq '"enrolled_?[aA]t": "20' "$W/node.json" || { cat "$W/node.json" >&2; fail "node enrollment lost"; }
podman exec $NAME node $APP/admin.js recovery-invite owner | grep -q "$ORIGIN/" || fail "recovery invite"

step "7. Quadlet dry run"
QUADLET=
for q in /usr/libexec/podman/quadlet /usr/lib/podman/quadlet; do [ -x $q ] && QUADLET=$q && break; done
if [ -n "$QUADLET" ]; then
	mkdir -p "$W/quadlet"
	cp "$REPO"/packages/meduza-np-manager/deploy/podman/*.container "$REPO"/packages/meduza-np-manager/deploy/podman/*.volume \
		"$REPO"/packages/meduza-np-manager/deploy/podman/*.network "$W/quadlet/"
	QUADLET_UNIT_DIRS="$W/quadlet" $QUADLET -dryrun -user >"$W/quadlet.out" 2>&1 || { cat "$W/quadlet.out" >&2; fail "quadlet rejected the units"; }
	grep -q '^ExecStart=.*podman run' "$W/quadlet.out" || { cat "$W/quadlet.out" >&2; fail "quadlet produced no service"; }
	if grep -qi 'unsupported key' "$W/quadlet.out"; then cat "$W/quadlet.out" >&2; fail "quadlet: unsupported keys"; fi
else
	echo "   (no quadlet generator on this host; skipped)"
fi

echo "manager podman smoke: all checks passed"
