#!/bin/sh
# Native updater launcher (systemd / procd), the counterpart of the base-container `updater` role (§13.3 "updater
# self-update needs a system supervisor or short-lived helper"). Runs the updater installed in the store; exit 75 =
# a new updater version was activated → start it at once; three quick failures of the store version → run the
# baseline copy (installed with the first bootstrap), whose recovery rolls the generation back.
# OpenWrt (opkg/apk backend, D29): there is no store; the package manager installs /usr/bin/meduza-np-updater and a
# self-update replaces it in place, so exit 75 simply starts the same path again.
set -u

DATA=${MEDUZA_NP_DATA_DIR:-/var/lib/meduza-np}
[ -f /etc/openwrt_release ] && [ -z "${MEDUZA_NP_DATA_DIR:-}" ] && DATA=/etc/meduza-np-state
STORE="$DATA/store"
BASELINE=${MEDUZA_NP_BASELINE_UPDATER:-/usr/lib/meduza-np/baseline/meduza-np-updater}
SYSTEM=/usr/bin/meduza-np-updater
EX_TAKEOVER=75

native_pm() {
	case "${MEDUZA_NP_UPDATE_BACKEND:-}" in
	opkg | apk) return 0 ;;
	store) return 1 ;;
	esac
	[ -f /etc/openwrt_release ]
}

# procd signals only this shell (systemd would kill the whole cgroup): forward stop requests to the running updater
# so a service stop/restart never leaves an orphaned second updater behind.
child=""
stop() {
	[ -n "$child" ] && kill -TERM "$child" 2>/dev/null && wait "$child" 2>/dev/null
	exit 143
}
trap stop TERM INT

quick=0
while :; do
	bin="$STORE/current/usr/bin/meduza-np-updater"
	if native_pm && [ -x "$SYSTEM" ]; then
		bin=$SYSTEM
		source=system
	elif [ -x "$bin" ] && [ $quick -lt 3 ]; then
		source=store
	elif [ -x "$BASELINE" ]; then
		bin=$BASELINE
		source=image
	else
		echo "meduza-np-updater: no updater in $STORE and no baseline at $BASELINE" >&2
		exit 1
	fi
	started=$(date +%s)
	MEDUZA_NP_DATA_DIR=$DATA MEDUZA_NP_UPDATER_SOURCE=$source "$bin" run &
	child=$!
	wait "$child"
	rc=$?
	child=""
	if [ $rc -eq $EX_TAKEOVER ]; then
		quick=0
		continue
	fi
	if [ "$source" = store ] && [ $(($(date +%s) - started)) -lt 60 ]; then
		quick=$((quick + 1))
		echo "meduza-np-updater: store updater exited $rc quickly ($quick)" >&2
		sleep 2
		continue
	fi
	exit $rc
done
