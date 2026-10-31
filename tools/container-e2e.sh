#!/bin/sh
# Base-container acceptance run (§14.3.4) on a Linux host with rootful Podman. Builds a signed test release with the
# real release tool and a throwaway key, builds the image, then checks:
#   1. blank volumes bootstrap from the (offline) source: only planner + selected components (+deps) are installed
#   2. planner starts after the updater installed it and registers the components
#   3. container re-creation keeps the installed set
#   4. an upgrade where only planner changed: maintenance lock, planner restart (new boot id), verification
#   5. a release planner refuses to register is rolled back to the previous generation
#   6. the dashboard component: one-time local link, session, API over planner.sock, CSRF, static UI caching
#
# Usage: tools/container-e2e.sh [WORKDIR]   (needs: cargo + cargo-zigbuild for x86_64-unknown-linux-musl, pnpm, podman, jq)
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
TARGET=x86_64-unknown-linux-musl
BIN="$REPO/target/$TARGET/release"
IMAGE=localhost/meduza-np-base-container:e2e
PKGS="meduza-np-planner meduza-np-updater meduza-np-cli meduza-np-wireguard meduza-np-dashboard"

fail() { echo "FAIL: $*" >&2; podman logs meduza-np-updater 2>&1 | tail -20 >&2 || true; podman logs meduza-np-planner 2>&1 | tail -20 >&2 || true; exit 1; }
step() { echo "== $*"; }

step "build binaries and tools"
cd "$REPO"
cargo zigbuild --locked --release --target $TARGET -p meduza-np-planner -p meduza-np-updater -p meduza-np-cli -p meduza-np-wireguard -p meduza-np-dashboard
pnpm install --frozen-lockfile >/dev/null && pnpm --filter @meduza-np/dashboard-web run build >/dev/null
cargo build --locked --release -p meduza-np-release -p meduza-np-updater
REL="$REPO/target/release/meduza-np-release"
UPD="$REPO/target/release/meduza-np-updater"

step "signed test release v0.1.0, v0.2.0 (planner changed), v0.3.0 (broken wireguard)"
rm -rf "$W" && mkdir -p "$W/dist1" "$W/dist2" "$W/dist3" "$W/ctx/bin/amd64" "$W/ctx/trust"
"$UPD" keygen "$W/test.key" "container-e2e throwaway key" >"$W/entry.json"
printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$W/entry.json")" >"$W/ctx/trust/release-keys.json"
args=""; for p in $PKGS; do args="$args --package $p"; done
# shellcheck disable=SC2086
"$REL" fingerprint --targets $TARGET $args >"$W/fp1.json"
"$REL" matrix --fingerprints "$W/fp1.json" --group-version v0.1.0 >"$W/m1.json"
for p in $PKGS; do "$REL" package --package "$p" --version 0.1.0 --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist1" >/dev/null; done
"$REL" group --group-version v0.1.0 --sequence 1 --commit e2e1 --matrix "$W/m1.json" --artifacts "$W/dist1" \
	--url-base packages --site "$W/site1" --platforms linux/amd64 --key "$W/test.key" >/dev/null
mkdir -p "$W/site1/packages" && cp "$W"/dist1/*.tar.gz "$W/site1/packages/"
jq '."meduza-np-planner" = "changed"' "$W/fp1.json" >"$W/fp2.json"
"$REL" matrix --fingerprints "$W/fp2.json" --group-version v0.2.0 --baseline "$W/site1/releases/v0.1.0/manifest.json" >"$W/m2.json"
"$REL" package --package meduza-np-planner --version 0.2.0 --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist2" >/dev/null
cp -r "$W/site1" "$W/site2"
"$REL" group --group-version v0.2.0 --sequence 2 --commit e2e2 --matrix "$W/m2.json" --artifacts "$W/dist2" \
	--baseline "$W/site1/releases/v0.1.0/manifest.json" --url-base packages --site "$W/site2" --platforms linux/amd64 --key "$W/test.key" >/dev/null
cp "$W"/dist2/*.tar.gz "$W/site2/packages/"
# v0.3.0: a wireguard package whose UI descriptor is invalid -> planner refuses it -> rollback
mkdir -p "$W/broken/packages/meduza-np-wireguard/ui"
cp -r "$REPO/packages/meduza-np-wireguard/." "$W/broken/packages/meduza-np-wireguard/"
echo '{}' >"$W/broken/packages/meduza-np-wireguard/ui/descriptor.json"
jq '."meduza-np-wireguard" = "broken"' "$W/fp2.json" >"$W/fp3.json"
"$REL" matrix --fingerprints "$W/fp3.json" --group-version v0.3.0 --baseline "$W/site2/releases/v0.2.0/manifest.json" >"$W/m3.json"
MEDUZA_NP_REPO="$W/broken" "$REL" package --package meduza-np-wireguard --version 0.3.0 --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist3" >/dev/null
cp -r "$W/site2" "$W/site3"
"$REL" group --group-version v0.3.0 --sequence 3 --commit e2e3 --matrix "$W/m3.json" --artifacts "$W/dist3" \
	--baseline "$W/site2/releases/v0.2.0/manifest.json" --url-base packages --site "$W/site3" --platforms linux/amd64 --key "$W/test.key" >/dev/null
cp "$W"/dist3/*.tar.gz "$W/site3/packages/"

step "image"
cp -r "$REPO/packages/meduza-np-base-container/." "$W/ctx/"
cp "$BIN/meduza-np-updater" "$W/ctx/bin/amd64/"
podman build -q --platform linux/amd64 --build-arg UPDATER_VERSION=0.1.0 --build-arg UPDATE_BASE_URL=file:///var/lib/meduza-np/import -t $IMAGE "$W/ctx"

cat >"$W/node.env" <<EOF
MEDUZA_NP_COMPONENTS=wireguard,cli,dashboard
MEDUZA_NP_UPDATE_BASE_URL=file:///var/lib/meduza-np/import
MEDUZA_NP_UPDATE_AUTO=0
EOF
mkdir -p "$W/import"
publish() { rm -rf "$W/import"/* && cp -r "$W/$1/." "$W/import/"; }
cli() { podman exec meduza-np-planner meduza-np "$@"; }
wait_for() { i=0; until eval "$1" >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt "${2:-60}" ] && fail "timeout waiting for: $1"; sleep 2; done; }
installed_version() { cli update status --json | jq -r ".installed.packages[\"$1\"].version"; }
boot_id() { cli status --json | jq -r .bootId; }

step "1+2: blank volumes bootstrap"
"$W/ctx/deploy/podman/run.sh" --stop
podman volume rm -f meduza-np-store meduza-np-node meduza-np-ipc >/dev/null 2>&1 || true
publish site1
"$W/ctx/deploy/podman/run.sh" --env-file "$W/node.env" --image $IMAGE --import "$W/import"
wait_for "podman exec meduza-np-planner meduza-np status" 90
[ "$(cli update status --json | jq -r '.installed.packages | keys | join(",")')" = "meduza-np-cli,meduza-np-dashboard,meduza-np-planner,meduza-np-updater,meduza-np-wireguard" ] || fail "unexpected installed set"
[ "$(cli packages --json | jq -r '.packages[] | select(.name=="meduza-np-wireguard") | .registration')" = registered ] || fail "wireguard not registered"

step "3: re-create containers, state kept"
"$W/ctx/deploy/podman/run.sh" --env-file "$W/node.env" --image $IMAGE --import "$W/import"
wait_for "podman exec meduza-np-planner meduza-np status" 60
[ "$(installed_version meduza-np-planner)" = 0.1.0 ] || fail "installed set lost"

step "4: upgrade (planner only)"
before=$(boot_id)
publish site2
cli update apply >/dev/null
wait_for '[ "$(installed_version meduza-np-planner)" = 0.2.0 ] && [ "$(cli update status --json | jq -r .busy)" = null ]' 60
[ "$(cli update status --json | jq -r .lastJournal.phase)" = done ] || fail "upgrade not done: $(cli update status --json | jq -c .lastJournal)"
[ "$(boot_id)" != "$before" ] || fail "planner was not restarted"
[ "$(installed_version meduza-np-wireguard)" = 0.1.0 ] || fail "reused package changed"

step "5: broken release rolled back"
publish site3
cli update apply >/dev/null
wait_for '[ "$(cli update status --json | jq -r .lastJournal.groupVersion)" = v0.3.0 ] && [ "$(cli update status --json | jq -r .busy)" = null ]' 60
[ "$(cli update status --json | jq -r .lastJournal.phase)" = rolled-back ] || fail "expected rollback: $(cli update status --json | jq -c .lastJournal)"
[ "$(installed_version meduza-np-wireguard)" = 0.1.0 ] || fail "wireguard not restored"
[ "$(cli packages --json | jq -r '.packages[] | select(.name=="meduza-np-wireguard") | .version')" = 0.1.0 ] || fail "planner sees the wrong wireguard"

step "6: dashboard"
dash() { podman exec meduza-np-planner curl -s "$@"; }
wait_for 'dash -o /dev/null -w "%{http_code}" http://127.0.0.1:8443/api/session | grep -q 200' 30
[ "$(dash http://127.0.0.1:8443/api/session | jq -r .authenticated)" = false ] || fail "unauthenticated session expected"
[ "$(dash -o /dev/null -w '%{http_code}' http://127.0.0.1:8443/api/status)" = 401 ] || fail "API must require a session"
link=$(cli dashboard-link 2>/dev/null)
dash -c /tmp/jar -o /dev/null "$link"
[ "$(dash -o /dev/null -w '%{http_code}' "$link")" = 403 ] || fail "login link must be single use"
podman exec meduza-np-planner sh -c 'curl -s -b /tmp/jar http://127.0.0.1:8443/api/session' >"$W/session.json"
[ "$(jq -r .role "$W/session.json")" = admin ] || fail "local link should give an admin session: $(cat "$W/session.json")"
podman exec meduza-np-planner sh -c 'curl -s -b /tmp/jar http://127.0.0.1:8443/api/status' | jq -e .bootId >/dev/null || fail "status through planner.sock"
podman exec meduza-np-planner sh -c 'curl -s -b /tmp/jar http://127.0.0.1:8443/api/update/status' | jq -e .installed.groupVersion >/dev/null || fail "update status through updater.sock"
code=$(podman exec meduza-np-planner sh -c 'curl -s -o /dev/null -w "%{http_code}" -b /tmp/jar -X POST http://127.0.0.1:8443/api/sync')
[ "$code" = 403 ] || fail "POST without CSRF token must be refused (got $code)"
csrf=$(jq -r .csrf "$W/session.json")
code=$(podman exec meduza-np-planner sh -c "curl -s -o /dev/null -w '%{http_code}' -b /tmp/jar -H 'x-csrf-token: $csrf' -H 'content-type: application/json' -d '{}' -X POST http://127.0.0.1:8443/api/sync")
[ "$code" = 200 ] || [ "$code" = 503 ] || fail "POST with CSRF token: $code"
dash -D - -o /dev/null http://127.0.0.1:8443/ | grep -qi "cache-control: no-cache" || fail "index.html must be revalidated"
asset=$(dash http://127.0.0.1:8443/ | grep -o 'assets/[^"]*\.js' | head -1)
dash -D - -o /dev/null -H 'accept-encoding: gzip' "http://127.0.0.1:8443/$asset" | grep -qi "immutable" || fail "hashed assets must be immutable"
dash -D - -o /dev/null http://127.0.0.1:8443/ | grep -qi "content-security-policy" || fail "security headers missing"

echo "PASS: base-container bootstrap, re-creation, upgrade, rollback and dashboard"
