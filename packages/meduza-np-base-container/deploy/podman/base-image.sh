#!/bin/sh
# Base image replacement for the two node containers (AGENT §13.5, §14.3.2, D53). Runs on the host as the user that
# owns the containers (root, or the rootless user); nothing inside the containers gets the Podman socket.
#
#   ./base-image.sh status                     running image, recorded previous image, last results
#   ./base-image.sh plan    [--group VERSION]  verified target image, pulled and checked; changes nothing
#   ./base-image.sh replace [--group VERSION] [--timeout SECONDS]
#   ./base-image.sh rollback [--timeout SECONDS]   back to the image that ran before the last replace
#   ./base-image.sh history
#
# Only this project's containers (meduza-np-updater, meduza-np-planner) and store volume are touched, and only an
# image the running updater names from the signed package group (exact digest, `updater-cli base-image`) or the
# image recorded as running before. Steps: pull by digest → platform digest + image labels → the new image's
# baseline updater checks the store read-only (`store-check`: formats, platform, no update half done) → re-create
# both containers (Quadlet units or run.sh --again) → wait until both are healthy on the new image → record; on
# failure the previous image is checked the same way and restored. Volumes are never re-created or written here.
set -eu

UPDATER=meduza-np-updater
PLANNER=meduza-np-planner
STORE_VOLUME=meduza-np-store
ENTRY=/usr/lib/meduza-np/container/entrypoint.sh
HERE=$(cd "$(dirname "$0")" && pwd)

if [ "$(id -u)" = 0 ]; then
	DEPLOY_STATE=${MEDUZA_NP_DEPLOY_STATE:-/var/lib/meduza-np-deploy}
	QUADLET_DIR=${MEDUZA_NP_QUADLET_DIR:-/etc/containers/systemd}
	SYSTEMCTL="systemctl"
else
	DEPLOY_STATE=${MEDUZA_NP_DEPLOY_STATE:-${XDG_DATA_HOME:-$HOME/.local/share}/meduza-np-deploy}
	QUADLET_DIR=${MEDUZA_NP_QUADLET_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd}
	SYSTEMCTL="systemctl --user"
fi
# false only for a local test registry
TLS_VERIFY=${MEDUZA_NP_TLS_VERIFY:-true}
HISTORY=$DEPLOY_STATE/base-image.history
PREVIOUS=$DEPLOY_STATE/base-image.previous

log() { echo "base-image: $*" >&2; }
die() {
	log "$*"
	exit 1
}

cmd=${1:-status}
[ $# -gt 0 ] && shift
GROUP=""
TIMEOUT=600
while [ $# -gt 0 ]; do
	case "$1" in
	--group) GROUP=$2; shift 2 ;;
	--timeout) TIMEOUT=$2; shift 2 ;;
	*) die "unknown option $1" ;;
	esac
done
case "$GROUP" in *[!A-Za-z0-9._+-]*) die "invalid --group $GROUP" ;; esac
case "$TIMEOUT" in '' | *[!0-9]*) die "invalid --timeout $TIMEOUT" ;; esac

mkdir -p "$DEPLOY_STATE"

# --- deployment ------------------------------------------------------------------------------------------------
mode() {
	if [ -f "$QUADLET_DIR/$UPDATER.container" ] && [ -f "$QUADLET_DIR/$PLANNER.container" ]; then
		# a unit of the same name elsewhere (e.g. a native install in /etc/systemd/system) shadows the generated one
		for c in $UPDATER $PLANNER; do
			case $($SYSTEMCTL show -p FragmentPath --value "$c.service" 2>/dev/null) in
			*/generator/*) ;;
			*) die "$c.service is not the Quadlet unit ($($SYSTEMCTL show -p FragmentPath --value "$c.service")); remove the other unit first" ;;
			esac
		done
		echo quadlet
	elif [ -f "$DEPLOY_STATE/run.args" ]; then
		echo run
	else
		die "no deployment found: neither Quadlet units in $QUADLET_DIR nor a run.sh record in $DEPLOY_STATE"
	fi
}

image_id() { podman container inspect --format '{{.Image}}' "$1" 2>/dev/null; }
image_name() { podman container inspect --format '{{.ImageName}}' "$1" 2>/dev/null; }
label() { podman image inspect --format "{{index .Labels \"$2\"}}" "$1" 2>/dev/null; }

# A reference this helper deploys: <registry>/<path>/meduza-np-base-container@sha256:<64 lowercase hex>.
valid_reference() {
	printf '%s\n' "$1" | grep -Eq '^[a-z0-9._:-]+(/[a-z0-9._-]+)*/meduza-np-base-container@sha256:[0-9a-f]{64}$'
}

# --- verification ----------------------------------------------------------------------------------------------
# Asks the running updater for the base image of the verified group (KEY=VALUE lines, see the updater's help).
TARGET_REF="" TARGET_DIGEST="" TARGET_GROUP="" TARGET_VERSION="" TARGET_UPDATER=""
read_target() {
	[ "$(podman container inspect --format '{{.State.Running}}' "$UPDATER" 2>/dev/null)" = true ] ||
		die "$UPDATER is not running; it verifies the release metadata"
	env_opt=""
	[ -n "$GROUP" ] && env_opt="--env MEDUZA_NP_GROUP_VERSION=$GROUP"
	# shellcheck disable=SC2086
	out=$(podman exec $env_opt "$UPDATER" "$ENTRY" updater-cli base-image) ||
		die "the updater could not name a verified base image (reason above; an updater image older than this helper has no updater-cli role)"
	TARGET_REF=$(printf '%s\n' "$out" | sed -n 's/^reference=//p')
	TARGET_DIGEST=$(printf '%s\n' "$out" | sed -n 's/^platformDigest=//p')
	TARGET_GROUP=$(printf '%s\n' "$out" | sed -n 's/^group=//p')
	TARGET_VERSION=$(printf '%s\n' "$out" | sed -n 's/^version=//p')
	TARGET_UPDATER=$(printf '%s\n' "$out" | sed -n 's/^updaterVersion=//p')
	valid_reference "$TARGET_REF" || die "updater returned an invalid reference: $TARGET_REF"
	printf '%s\n' "$TARGET_DIGEST" | grep -Eq '^sha256:[0-9a-f]{64}$' || die "updater returned an invalid platform digest"
}

# Pulls the verified reference and checks that the local image is that platform manifest of our image.
pull_verified() {
	ref=$1
	log "pulling $ref"
	podman pull -q --tls-verify="$TLS_VERIFY" "$ref" >/dev/null || die "pull of $ref failed"
	digest=$(podman image inspect --format '{{.Digest}}' "$ref")
	repo=${ref%@*}
	if [ "$digest" != "$TARGET_DIGEST" ] &&
		! podman image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$ref" | grep -qx "$repo@$TARGET_DIGEST"; then
		die "$ref resolved to $digest, the verified group names $TARGET_DIGEST for this platform"
	fi
	[ "$(label "$ref" org.opencontainers.image.title)" = meduza-np-base-container ] || die "$ref is not a meduza-np-base-container image"
	have=$(label "$ref" io.meduza-np.updater.version)
	if [ -n "$TARGET_UPDATER" ] && [ "$have" != "$TARGET_UPDATER" ]; then
		die "$ref embeds updater $have, group $TARGET_GROUP has $TARGET_UPDATER"
	fi
}

# The image's baseline updater (the recovery fallback after the switch) checks the store read-only.
CHECK_PACKAGES=""
store_check() {
	set +e
	out=$(podman run --rm --network none --cap-drop ALL --security-opt no-new-privileges \
		--volume "$STORE_VOLUME:/var/lib/meduza-np/store:ro" "$1" store-check 2>&1)
	rc=$?
	set -e
	CHECK_PACKAGES=$(printf '%s\n' "$out" | sed -n 's/^packages=//p')
	printf '%s\n' "$out" | sed -n -e 's/^problem=/  problem: /p' -e 's/^warning=/  warning: /p' >&2
	if [ $rc -eq 3 ]; then
		return 1
	elif [ $rc -ne 0 ]; then
		printf '%s\n' "$out" >&2
		die "store check with $1 could not run (exit $rc)"
	fi
}

# --- switching -------------------------------------------------------------------------------------------------
recreate() {
	ref=$1
	case $MODE in
	quadlet)
		for c in $UPDATER $PLANNER; do
			f=$QUADLET_DIR/$c.container
			cp -p "$f" "$DEPLOY_STATE/$c.container.prev"
			sed "s|^Image=.*|Image=$ref|" "$f" >"$f.tmp"
			grep -qx "Image=$ref" "$f.tmp" || die "$f has no Image= line"
			mv "$f.tmp" "$f"
		done
		$SYSTEMCTL daemon-reload
		$SYSTEMCTL restart "$UPDATER.service" "$PLANNER.service"
		;;
	run)
		[ -x "$HERE/run.sh" ] || die "$HERE/run.sh not found (keep base-image.sh next to run.sh)"
		"$HERE/run.sh" --again --image "$ref" >&2
		;;
	esac
}

# Both containers run the image and their health checks pass, within the timeout.
wait_healthy() {
	want=$(podman image inspect --format '{{.Id}}' "$1")
	deadline=$(($(date +%s) + TIMEOUT))
	while [ "$(date +%s)" -lt "$deadline" ]; do
		ok=1
		for c in $UPDATER $PLANNER; do
			[ "$(image_id "$c")" = "$want" ] && podman healthcheck run "$c" >/dev/null 2>&1 || ok=0
		done
		[ $ok = 1 ] && return 0
		sleep 5
	done
	return 1
}

record() {
	# time  action  result  reference  image-id  group  packages
	printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" "$3" "$4" "${5:--}" "${6:--}" >>"$HISTORY"
}

# Reference that recreates the currently running image: its digest reference when it was started by one, else the
# image id (a tag may have moved since).
current_reference() {
	name=$(image_name "$UPDATER")
	if valid_reference "$name"; then echo "$name"; else image_id "$UPDATER"; fi
}

lock() {
	mkdir "$DEPLOY_STATE/base-image.lock" 2>/dev/null || die "another base-image run holds $DEPLOY_STATE/base-image.lock"
	trap 'rmdir "$DEPLOY_STATE/base-image.lock" 2>/dev/null || true' EXIT INT TERM
}

# Switches to $1 (already verified); on failure restores $2 when its own baseline accepts the store.
switch() {
	new=$1 old=$2 action=$3 group=$4
	new_id=$(podman image inspect --format '{{.Id}}' "$new")
	record "$action" started "$new" "$new_id" "$group" "$CHECK_PACKAGES"
	recreate "$new"
	if wait_healthy "$new"; then
		printf '%s\n' "$old" >"$PREVIOUS"
		record "$action" ok "$new" "$new_id" "$group" "$CHECK_PACKAGES"
		log "$action done: both containers healthy on $new ($new_id)"
		return 0
	fi
	record "$action" unhealthy "$new" "$new_id" "$group" "$CHECK_PACKAGES"
	log "containers not healthy on $new within ${TIMEOUT}s; restoring $old"
	podman logs --tail 20 "$PLANNER" >&2 2>&1 || true
	if ! store_check "$old"; then
		record rollback refused "$old" - - "$CHECK_PACKAGES"
		die "the previous image cannot take over the store (above); left on $new for repair"
	fi
	if [ "$MODE" = quadlet ]; then
		for c in $UPDATER $PLANNER; do cp -p "$DEPLOY_STATE/$c.container.prev" "$QUADLET_DIR/$c.container"; done
		$SYSTEMCTL daemon-reload
		$SYSTEMCTL restart "$UPDATER.service" "$PLANNER.service"
	else
		"$HERE/run.sh" --again --image "$old" >&2
	fi
	if wait_healthy "$old"; then
		record rollback ok "$old" "$(podman image inspect --format '{{.Id}}' "$old")" - "$CHECK_PACKAGES"
		die "$action failed; restored $old"
	fi
	record rollback unhealthy "$old" - - -
	die "$action failed and $old is not healthy either; see podman logs $UPDATER / $PLANNER"
}

# --- commands --------------------------------------------------------------------------------------------------
case "$cmd" in history) ;; *) MODE=$(mode) || exit 1 ;; esac
case "$cmd" in
status)
	echo "deployment: $MODE"
	for c in $UPDATER $PLANNER; do
		printf '%s: %s (%s) health=%s\n' "$c" "$(image_name "$c" || echo absent)" "$(image_id "$c" || echo -)" \
			"$(podman container inspect --format '{{.State.Health.Status}}' "$c" 2>/dev/null || echo -)"
	done
	echo "previous: $(cat "$PREVIOUS" 2>/dev/null || echo none)"
	[ -f "$HISTORY" ] && tail -n 3 "$HISTORY"
	;;
history) cat "$HISTORY" 2>/dev/null || true ;;
plan | replace)
	lock
	read_target
	echo "group $TARGET_GROUP: meduza-np-base-container $TARGET_VERSION $TARGET_REF ($TARGET_DIGEST)"
	pull_verified "$TARGET_REF"
	cur=$(current_reference)
	if [ "$(podman image inspect --format '{{.Id}}' "$TARGET_REF")" = "$(image_id "$UPDATER")" ] &&
		[ "$(image_id "$PLANNER")" = "$(image_id "$UPDATER")" ]; then
		echo "already running $TARGET_REF"
		exit 0
	fi
	store_check "$TARGET_REF" || die "$TARGET_REF cannot take over the store (above); nothing changed"
	echo "store check passed: ${CHECK_PACKAGES:-blank store}"
	[ "$cmd" = plan ] && { echo "plan: replace $cur with $TARGET_REF"; exit 0; }
	switch "$TARGET_REF" "$cur" replace "$TARGET_GROUP"
	;;
rollback)
	lock
	prev=$(cat "$PREVIOUS" 2>/dev/null) || die "no previous image recorded"
	podman image exists "$prev" || die "previous image $prev is no longer present locally"
	[ "$(label "$prev" org.opencontainers.image.title)" = meduza-np-base-container ] || die "$prev is not a meduza-np-base-container image"
	store_check "$prev" || die "$prev cannot take over the store (above, e.g. formats written by the newer updater); nothing changed"
	switch "$prev" "$(current_reference)" rollback -
	;;
*) die "unknown command $cmd (status, plan, replace, rollback, history)" ;;
esac
