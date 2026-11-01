#!/bin/sh
# Meduza NP base container roles (§14.3.2). PID 1 is tini; this script stays the parent of the role process so it
# can take over restarts that the service itself asks for (exit 75 = "start the new version").
#
#   updater   (default) run the updater: persistent store version if healthy, else the image baseline
#   planner   wait until the updater installed planner, prepare Alpine deps of installed components, run planner
#   health-updater | health-planner   container health checks
#   store-check   this image's baseline updater checks it can take over the store (base-image.sh, read-only mount)
#   updater-cli ARGS   the updater that runs (store version, else baseline) with ARGS (base-image.sh: base-image)
#   shell     debugging shell
set -eu

DATA=${MEDUZA_NP_DATA_DIR:-/var/lib/meduza-np}
STORE="$DATA/store"
IMAGE_UPDATER=/usr/lib/meduza-np/image/meduza-np-updater
RUN_DIR=${MEDUZA_NP_RUN_DIR:-/run/meduza-np}
EX_TAKEOVER=75

log() { echo "meduza-np-container[$1]: $2" >&2; }
. "$(dirname "$0")/network.sh"
trap stop_dns_monitor EXIT

# tini signals only this script. A shell without a trap exits at once on SIGTERM, the container ends and the role
# process is SIGKILLed with it, so planner never stopped its daemons on `podman stop` / `systemctl stop` (D57). The
# stop is forwarded to the running role process instead and this script waits for it.
stopping=0
child=""
on_stop() {
	stopping=1
	stop_dns_monitor
	if [ -n "$child" ]; then kill -TERM "$child" 2>/dev/null || true; fi
}
trap on_stop TERM INT

# run_child CMD...: runs CMD as the current role process; rc = its exit status. A stop requested meanwhile is
# forwarded (also one that arrived before the pid was known); `wait` returns early for a trapped signal, so it is
# repeated until the process is gone.
run_child() {
	"$@" &
	child=$!
	if [ "$stopping" = 1 ]; then kill -TERM "$child" 2>/dev/null || true; fi
	while :; do
		set +e
		wait "$child"
		rc=$?
		set -e
		kill -0 "$child" 2>/dev/null || break
	done
	child=""
}

# After a requested stop: the role's own status, except death by the forwarded SIGTERM (no handler, e.g. the
# updater), which is the stop that was asked for and not a failure.
exit_stopped() {
	log "$1" "stopped (exit status $rc)"
	[ "$rc" -eq 143 ] && exit 0
	exit "$rc"
}

ensure_dirs() {
	mkdir -p "$RUN_DIR"
	chmod 0755 "$RUN_DIR"
}

# --- updater role ----------------------------------------------------------------------------------------------
store_updater() {
	bin="$STORE/current/usr/bin/meduza-np-updater"
	[ -x "$bin" ] && echo "$bin"
}

run_updater() {
	start_network
	ensure_dirs
	mkdir -p "$STORE"
	quick_failures=0
	while :; do
		bin=$(store_updater || true)
		if [ -n "$bin" ] && [ "$quick_failures" -lt 3 ]; then
			source=store
		else
			bin=$IMAGE_UPDATER
			source=image
			[ "$quick_failures" -ge 3 ] && log updater "store updater failed $quick_failures times quickly; falling back to the image baseline (it recovers/rolls back)"
		fi
		[ "$stopping" = 1 ] && exit 0
		started=$(date +%s)
		export MEDUZA_NP_UPDATER_SOURCE=$source
		run_child "$bin" run
		[ "$stopping" = 1 ] && exit_stopped updater
		ran=$(($(date +%s) - started))
		if [ $rc -eq $EX_TAKEOVER ]; then
			log updater "updater asked for takeover (self-update); starting the new version"
			quick_failures=0
			continue
		fi
		if [ "$source" = store ] && [ $ran -lt 60 ]; then
			quick_failures=$((quick_failures + 1))
			log updater "store updater exited with $rc after ${ran}s ($quick_failures quick failure(s))"
			sleep 2
			continue
		fi
		log updater "updater exited with $rc; leaving restart to Podman"
		exit $rc
	done
}

# --- planner role ----------------------------------------------------------------------------------------------
wait_for_planner_package() {
	waited=0
	while [ ! -x "$STORE/current/usr/bin/meduza-np-planner" ]; do
		if [ $((waited % 60)) -eq 0 ]; then
			log planner "waiting for the updater to install planner into $STORE (bootstrap or offline import pending)"
		fi
		sleep 5
		[ "$stopping" = 1 ] && exit 0
		waited=$((waited + 5))
	done
}

installed_packages() {
	"$IMAGE_UPDATER" installed 2>/dev/null || true
}

apk_deps() {
	pkgs=$(installed_packages)
	deps=""
	skip=""
	while read -r first rest; do
		case "$first" in
		'' | '#'*) continue ;;
		skip-if)
			set -- $rest
			echo "$pkgs" | grep -qx "$1" && skip="$skip $2"
			;;
		*) echo "$pkgs" | grep -qx "$first" && deps="$deps $rest" ;;
		esac
	done </usr/lib/meduza-np/container/apk-deps
	for d in $deps; do
		case " $skip " in *" $d "*) ;; *) echo "$d" ;; esac
	done | sort -u
}

prepare_deps() {
	deps=$(apk_deps | tr '\n' ' ')
	cache="$DATA/apk-cache"
	mkdir -p "$cache"
	echo "$deps" >"$DATA/apk-deps.selected"
	[ -z "${deps% }" ] && return 0
	log planner "Alpine dependencies for installed components: $deps"
	# Network first (refreshes the cache), then the persistent cache only (offline restart, §14.3.1). Both are
	# bounded: an unreachable mirror must never keep planner from starting (affected components report it).
	if timeout "${MEDUZA_NP_APK_TIMEOUT:-120}" apk add --cache-dir "$cache" $deps >/dev/null 2>&1; then
		apk cache --cache-dir "$cache" sync >/dev/null 2>&1 || true
		drop_sample_services
		return 0
	fi
	if timeout 60 apk add --no-network --cache-dir "$cache" $deps >/dev/null 2>&1; then
		log planner "installed dependencies from the offline cache"
		drop_sample_services
		return 0
	fi
	log planner "WARNING: could not install: $deps — affected components will report missing dependencies"
}

# Alpine's avahi package ships static service files (ssh, sftp-ssh) that avahi-daemon always announces, whatever
# its publish settings say (`disable-user-service-publishing` only covers D-Bus clients). In host network mode that
# advertised SSH on the host's LAN although nobody configured it (D48). They belong to this image, not to the
# operator, so they go; the container rootfs is recreated with the container and apk-deps runs again.
drop_sample_services() {
	rm -f /etc/avahi/services/ssh.service /etc/avahi/services/sftp-ssh.service
}

# The dashboard component runs next to planner (same namespace, same sockets) under a tiny loop that restarts it
# when it exits or when an update switched its binary; it never blocks planner.
dashboard_loop() {
	bin="$STORE/current/usr/bin/meduza-np-dashboard"
	pid=""
	running=""
	while :; do
		target=$(readlink -f "$bin" 2>/dev/null || true)
		if [ -n "$pid" ] && { ! kill -0 "$pid" 2>/dev/null || [ "$target" != "$running" ]; }; then
			kill "$pid" 2>/dev/null || true
			wait "$pid" 2>/dev/null || true
			pid=""
		fi
		if [ -z "$pid" ] && [ -n "$target" ] && [ -x "$target" ]; then
			MEDUZA_NP_DASHBOARD_WWW="$STORE/current/usr/share/meduza-np/dashboard/www" \
				MEDUZA_NP_DASHBOARD_LISTEN="${MEDUZA_NP_DASHBOARD_LISTEN:-127.0.0.1:8443}" "$target" &
			pid=$!
			running=$target
			log planner "dashboard started ($running, pid $pid)"
		fi
		sleep 15
	done
}

# Avahi and the `.local` bridge talk over the system D-Bus, which nothing else starts in a container (no init). While
# the avahi component is installed (rechecked, so a later install by the updater is picked up), this loop runs
# dbus-daemon as the container's system bus and restarts it when it exits. The bus is the container's own, never the
# host's: in host network mode avahi-daemon still announces on the host's interfaces.
dbus_loop() {
	while :; do
		if command -v dbus-daemon >/dev/null 2>&1 && installed_packages | grep -qx meduza-np-avahi; then
			mkdir -p /run/dbus
			rm -f /run/dbus/system_bus_socket /run/dbus/dbus.pid
			dbus-uuidgen --ensure=/etc/machine-id 2>/dev/null || true
			log planner "starting the system D-Bus (avahi component)"
			dbus-daemon --system --nofork --nopidfile || log planner "dbus-daemon exited with $?"
		fi
		sleep 15
	done
}

wait_for_bus() {
	installed_packages | grep -qx meduza-np-avahi || return 0
	i=0
	while [ ! -S /run/dbus/system_bus_socket ] && [ $i -lt 20 ]; do
		sleep 0.25
		i=$((i + 1))
	done
}

run_planner() {
	start_network
	ensure_dirs
	mkdir -p "${MEDUZA_NP_ETC_DIR:-$DATA/etc}" "${MEDUZA_NP_STATE_DIR:-$DATA}"
	wait_for_planner_package
	prepare_deps
	dbus_loop &
	# avahi validation needs the bus: give it a moment before the first apply
	wait_for_bus
	dashboard_loop &
	while :; do
		[ "$stopping" = 1 ] && exit 0
		run_child "$STORE/current/usr/bin/meduza-np-planner" run
		[ "$stopping" = 1 ] && exit_stopped planner
		if [ $rc -eq $EX_TAKEOVER ]; then
			log planner "planner restart requested (update); starting the current version"
			prepare_deps
			continue
		fi
		log planner "planner exited with $rc; leaving restart to Podman"
		exit $rc
	done
}

role=${1:-updater}
case "$role" in
updater) run_updater ;;
planner) run_planner ;;
health-updater) test -S "$RUN_DIR/updater.sock" ;;
health-planner) exec "$IMAGE_UPDATER" planner-health ;;
store-check) exec "$IMAGE_UPDATER" store-check ;;
updater-cli)
	shift
	bin=$(store_updater || true)
	exec "${bin:-$IMAGE_UPDATER}" "$@"
	;;
shell) exec /bin/sh ;;
*)
	log entrypoint "unknown role $role (updater, planner, health-updater, health-planner, store-check, updater-cli, shell)"
	exit 2
	;;
esac
