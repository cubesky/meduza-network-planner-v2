#!/bin/sh
# Base image replacement acceptance run (§14.3.2, D53) on a Linux host with rootful Podman. Builds three base images
# (A, B, and C whose planner health check always fails), pushes them to a local registry, pins them in three signed
# test groups (real release tool, throwaway key), deploys on A with run.sh and drives deploy/podman/base-image.sh:
#   1. plan names the verified image of the group; nothing changes; "already running" on the same image
#   2. replace is refused while a package update is half done (store-check), nothing changes
#   3. replace A -> B: both containers re-created on B, healthy, installed set kept, planner restarted, recorded
#   4. replace B -> C (unhealthy): restored to B automatically, previous image unchanged
#   5. rollback -> A after the store check of A
#   6. a reference that is not in the signed group is never deployed (tampered group refused by the updater)
#   7. (BASE_IMAGE_E2E_QUADLET=1, writes /etc/containers/systemd and /etc/meduza-np/node.env) the same replace and
#      rollback with the Quadlet units
#
# Usage: tools/base-image-e2e.sh [WORKDIR]   (needs: cargo + cargo-zigbuild, podman, jq, curl; pulls registry:2)
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
TARGET=x86_64-unknown-linux-musl
BIN="$REPO/target/$TARGET/release"
PKGS="meduza-np-planner meduza-np-updater meduza-np-cli"
REG=localhost:5055
IMG=$REG/e2e/meduza-np-base-container
REGISTRY_IMAGE=${REGISTRY_IMAGE:-docker.io/library/registry:2}

fail() {
	echo "FAIL: $*" >&2
	podman logs meduza-np-updater 2>&1 | tail -20 >&2 || true
	podman logs meduza-np-planner 2>&1 | tail -20 >&2 || true
	exit 1
}
step() { echo "== $*"; }

step "build binaries and tools"
cd "$REPO"
cargo zigbuild --locked --release --target $TARGET -p meduza-np-planner -p meduza-np-updater -p meduza-np-cli
cargo build --locked --release -p meduza-np-release -p meduza-np-updater
REL="$REPO/target/release/meduza-np-release"
UPD="$REPO/target/release/meduza-np-updater"

rm -rf "$W" && mkdir -p "$W/dist" "$W/ctx/bin/amd64" "$W/ctx/trust" "$W/deploy" "$W/import"
"$UPD" keygen "$W/test.key" "base-image-e2e throwaway key" >"$W/entry.json"
printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$W/entry.json")" >"$W/ctx/trust/release-keys.json"

step "local registry $REG"
podman rm -f mnp-e2e-registry >/dev/null 2>&1 || true
podman run -d --name mnp-e2e-registry -p 127.0.0.1:5055:5000 "$REGISTRY_IMAGE" >/dev/null
trap 'podman rm -f mnp-e2e-registry >/dev/null 2>&1 || true' EXIT

step "images A, B, C (C: planner health always fails)"
cp -r "$REPO/packages/meduza-np-base-container/." "$W/ctx/"
# The release tool requires all three platforms in a base image index. Only amd64 runs here; the arm images get the
# amd64 updater too (test images only: it runs natively during their build, the release builds real arm binaries).
for a in amd64 arm64 armv7; do mkdir -p "$W/ctx/bin/$a" && cp "$BIN/meduza-np-updater" "$W/ctx/bin/$a/"; done
cp -r "$W/ctx" "$W/ctx-broken"
sed -i 's|^health-planner) .*|health-planner) exit 1 ;;|' "$W/ctx-broken/rootfs/usr/lib/meduza-np/container/entrypoint.sh"
grep -q '^health-planner) exit 1 ;;' "$W/ctx-broken/rootfs/usr/lib/meduza-np/container/entrypoint.sh" || fail "could not break image C"
for v in a b c; do
	ctx="$W/ctx"
	[ $v = c ] && ctx="$W/ctx-broken"
	podman manifest rm "localhost/mnp-bi:$v" >/dev/null 2>&1 || true
	podman build -q --platform linux/amd64,linux/arm64,linux/arm/v7 --manifest "localhost/mnp-bi:$v" --label "io.meduza-np.e2e=$v" \
		--label io.meduza-np.updater.version=0.1.0 --build-arg UPDATER_VERSION=0.1.0 \
		--build-arg UPDATE_BASE_URL=file:///var/lib/meduza-np/import "$ctx" >/dev/null
	podman manifest push -q --all --tls-verify=false --digestfile "$W/digest-$v" "localhost/mnp-bi:$v" "docker://$IMG:$v"
	curl -sf -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json' \
		"http://$REG/v2/e2e/meduza-np-base-container/manifests/$(cat "$W/digest-$v")" >"$W/raw-$v.json"
done
REF_A="$IMG@$(cat "$W/digest-a")"
REF_B="$IMG@$(cat "$W/digest-b")"
REF_C="$IMG@$(cat "$W/digest-c")"

step "signed groups v0.1.0 (A), v0.2.0 (B), v0.3.0 (C)"
# the image record carries the base-container version the matrix assigns in that group
describe() {
	mkdir -p "$W/oci-$1"
	"$REL" oci-describe --package meduza-np-base-container --version "$(jq -r '."meduza-np-base-container".version' "$2")" --image "$IMG" \
		--raw "$W/raw-$1.json" --out "$W/oci-$1/meduza-np-base-container.oci.json" >/dev/null
}
args=""; for p in $PKGS meduza-np-base-container; do args="$args --package $p"; done
# shellcheck disable=SC2086
"$REL" fingerprint --targets $TARGET $args >"$W/fp1.json"
for p in $PKGS; do "$REL" package --package "$p" --version 0.1.0 --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist" >/dev/null; done
"$REL" matrix --fingerprints "$W/fp1.json" --group-version v0.1.0 >"$W/m1.json"
describe a "$W/m1.json"
"$REL" group --group-version v0.1.0 --sequence 1 --commit e2e1 --matrix "$W/m1.json" --artifacts "$W/dist" --oci "$W/oci-a" \
	--url-base packages --site "$W/site1" --platforms linux/amd64 --key "$W/test.key" >/dev/null
mkdir -p "$W/site1/packages" && cp "$W"/dist/*.tar.gz "$W/site1/packages/"
prev=site1 fp=fp1
for n in 2 3; do
	v=$(echo $n | tr 23 bc)
	jq ".\"meduza-np-base-container\" = \"image-$v\"" "$W/$fp.json" >"$W/fp$n.json"
	"$REL" matrix --fingerprints "$W/fp$n.json" --group-version "v0.$n.0" --baseline "$W/$prev/releases/v0.$((n - 1)).0/manifest.json" >"$W/m$n.json"
	describe $v "$W/m$n.json"
	mkdir -p "$W/empty"
	cp -r "$W/$prev" "$W/site$n"
	"$REL" group --group-version "v0.$n.0" --sequence "$n" --commit "e2e$n" --matrix "$W/m$n.json" --artifacts "$W/empty" --oci "$W/oci-$v" \
		--baseline "$W/$prev/releases/v0.$((n - 1)).0/manifest.json" --url-base packages --site "$W/site$n" --platforms linux/amd64 --key "$W/test.key" >/dev/null
	prev=site$n fp=fp$n
done
for n in 1 2 3; do
	want=$(eval echo "\$REF_$(echo $n | tr 123 ABC)")
	got=$(jq -r '.packages[] | select(.name=="meduza-np-base-container") | .artifacts[] | select(.platform=="linux/amd64") | .oci' "$W/site$n/releases/v0.$n.0/manifest.json")
	[ "$got" = "$want" ] || fail "group v0.$n.0 pins $got, expected $want"
done

cat >"$W/node.env" <<EOF
MEDUZA_NP_COMPONENTS=cli
MEDUZA_NP_UPDATE_BASE_URL=file:///var/lib/meduza-np/import
MEDUZA_NP_UPDATE_AUTO=0
EOF
publish() { rm -rf "$W/import"/* && cp -r "$W/$1/." "$W/import/"; }
cli() { podman exec meduza-np-planner meduza-np "$@"; }
wait_for() { i=0; until eval "$1" >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt "${2:-60}" ] && fail "timeout waiting for: $1"; sleep 2; done; }
running_image() { podman container inspect --format '{{.Image}}' "$1"; }
image_id() { podman image inspect --format '{{.Id}}' "$1"; }
boot_id() { cli status --json | jq -r .bootId; }
installed() { cli update status --json | jq -c '.installed | {generation, groupVersion, packages: (.packages | map_values(.version))}'; }
export MEDUZA_NP_DEPLOY_STATE="$W/deploy" MEDUZA_NP_TLS_VERIFY=false
H="$W/ctx/deploy/podman/base-image.sh"
on_image() { [ "$(running_image meduza-np-updater)" = "$(image_id "$1")" ] && [ "$(running_image meduza-np-planner)" = "$(image_id "$1")" ]; }

step "deploy on A (run.sh, blank volumes)"
"$W/ctx/deploy/podman/run.sh" --stop
podman volume rm -f meduza-np-store meduza-np-node meduza-np-ipc >/dev/null 2>&1 || true
podman pull -q --tls-verify=false "$REF_A" >/dev/null
publish site1
"$W/ctx/deploy/podman/run.sh" --env-file "$W/node.env" --image "$REF_A" --import "$W/import"
wait_for "podman exec meduza-np-planner meduza-np status" 90
wait_for "podman healthcheck run meduza-np-planner" 30
cli update apply >/dev/null 2>&1 || true
before_set=$(installed)

step "1: plan"
"$H" plan | tee "$W/plan1.txt"
grep -q "already running $REF_A" "$W/plan1.txt" || fail "same image must be a no-op"
publish site2
"$H" plan | tee "$W/plan2.txt"
grep -q "plan: replace $REF_A with $REF_B" "$W/plan2.txt" || fail "plan must name B"
on_image "$REF_A" || fail "plan changed the containers"

step "2: refused while a package update is half done"
podman exec meduza-np-updater sh -c 'cd /var/lib/meduza-np/store && { [ ! -f journal.json ] || cp journal.json journal.keep; } &&
	echo "{\"formatVersion\":1,\"groupVersion\":\"v0.2.0\",\"phase\":\"switched\",\"startedAt\":\"t\",\"updatedAt\":\"t\"}" >journal.json'
if "$H" replace >"$W/refused.txt" 2>&1; then fail "replace must be refused during an update"; fi
cat "$W/refused.txt"
grep -q "is in phase switched" "$W/refused.txt" || fail "store-check reason missing"
on_image "$REF_A" || fail "a refused replace changed the containers"
podman exec meduza-np-updater sh -c 'cd /var/lib/meduza-np/store && if [ -f journal.keep ]; then mv journal.keep journal.json; else rm journal.json; fi'

step "3: replace A -> B"
boot_a=$(boot_id)
"$H" replace --timeout 180
on_image "$REF_B" || fail "not on B"
wait_for "podman exec meduza-np-planner meduza-np status" 30
[ "$(boot_id)" != "$boot_a" ] || fail "planner not restarted"
[ "$(installed)" = "$before_set" ] || fail "installed set changed: $before_set -> $(installed)"
[ "$(cat "$W/deploy/base-image.previous")" = "$REF_A" ] || fail "previous image not recorded"
grep -q "	replace	ok	$REF_B	" "$W/deploy/base-image.history" || fail "history lacks the replace"
"$H" status

step "4: replace B -> C (unhealthy) restores B"
publish site3
if "$H" replace --timeout 45 >"$W/broken.txt" 2>&1; then fail "replace to C must fail"; fi
cat "$W/broken.txt"
on_image "$REF_B" || fail "not restored to B"
wait_for "podman healthcheck run meduza-np-planner" 60
[ "$(cat "$W/deploy/base-image.previous")" = "$REF_A" ] || fail "a failed replace must keep the previous record"
grep -q "	replace	unhealthy	$REF_C	" "$W/deploy/base-image.history" || fail "history lacks the failed replace"
grep -q "	rollback	ok	$REF_B	" "$W/deploy/base-image.history" || fail "history lacks the automatic rollback"

step "5: rollback -> A"
"$H" rollback --timeout 180
on_image "$REF_A" || fail "not on A"
[ "$(cat "$W/deploy/base-image.previous")" = "$REF_B" ] || fail "previous should now be B"
[ "$(installed)" = "$before_set" ] || fail "installed set changed by the rollback"

step "6: a group whose signature does not match is refused"
publish site3
m="$W/import/releases/v0.3.0/manifest.json"
jq --arg r "$REF_B" '(.packages[] | select(.name=="meduza-np-base-container") | .artifacts[].oci) = $r' "$m" >"$m.tmp" && mv "$m.tmp" "$m"
if "$H" plan >"$W/tampered.txt" 2>&1; then fail "a tampered group must be refused"; fi
cat "$W/tampered.txt"
on_image "$REF_A" || fail "tampered plan changed the containers"
"$H" history

if [ "${BASE_IMAGE_E2E_QUADLET:-0}" = 1 ]; then
	step "7: Quadlet units: replace A -> B, rollback -> A"
	Q=/etc/containers/systemd
	for u in meduza-np-updater meduza-np-planner; do
		[ ! -e "/etc/systemd/system/$u.service" ] || fail "/etc/systemd/system/$u.service (native install) would shadow the Quadlet unit"
	done
	[ ! -e /etc/meduza-np/node.env ] || cp /etc/meduza-np/node.env "$W/node.env.host-backup"
	"$W/ctx/deploy/podman/run.sh" --stop
	rm -f "$W/deploy/run.args"
	mkdir -p /etc/meduza-np "$Q" && cp "$W/node.env" /etc/meduza-np/node.env
	cp "$W/ctx/deploy/podman/"meduza-np*.container "$W/ctx/deploy/podman/"meduza-np*.volume "$W/ctx/deploy/podman/meduza-np.network" "$Q/"
	sed -i "s|^Image=.*|Image=$REF_A|" "$Q/meduza-np-updater.container" "$Q/meduza-np-planner.container"
	sed -i "s|^#Volume=/srv/meduza-np-import:|Volume=$W/import:|" "$Q/meduza-np-updater.container"
	cleanup_quadlet() {
		systemctl stop meduza-np-planner meduza-np-updater 2>/dev/null || true
		rm -f "$Q"/meduza-np*.container "$Q"/meduza-np*.volume "$Q/meduza-np.network"
		systemctl daemon-reload
		if [ -f "$W/node.env.host-backup" ]; then cp "$W/node.env.host-backup" /etc/meduza-np/node.env; else rm -f /etc/meduza-np/node.env; fi
	}
	trap 'cleanup_quadlet; podman rm -f mnp-e2e-registry >/dev/null 2>&1 || true' EXIT
	systemctl daemon-reload
	# the node accepted sequence 3 already (anti-rollback): B is selected by pinning its group (--group)
	publish site3
	systemctl start meduza-np-updater meduza-np-planner
	wait_for "podman healthcheck run meduza-np-planner" 60
	on_image "$REF_A" || fail "Quadlet units not on A"
	"$H" replace --group v0.2.0 --timeout 180
	on_image "$REF_B" || fail "Quadlet: not on B"
	grep -qx "Image=$REF_B" "$Q/meduza-np-planner.container" || fail "planner unit not rewritten"
	[ "$(installed)" = "$before_set" ] || fail "Quadlet: installed set changed"
	"$H" rollback --timeout 180
	on_image "$REF_A" || fail "Quadlet: rollback not on A"
	grep -qx "Image=$REF_A" "$Q/meduza-np-updater.container" || fail "updater unit not restored"
	[ "$(systemctl is-active meduza-np-planner)" = active ] || fail "planner unit not active"
fi

echo "PASS: base image plan, refusal, replace, automatic restore and rollback"
