#!/bin/sh
# Planner log sinks on real service managers (D55): procd/logread on the OpenWrt node lab-ow and journald on the
# native Debian node lab-sys. Uses the planner built by `tools/lab/mdns-openwrt.sh build` (musl binary + apk).
#
#   tools/lab/log-sinks.sh openwrt   upgrade lab-ow's planner + avahi apks from the build, restart the planner, send
#                                    the supervised avahi/bridge SIGTERM once (it must exit 0; logged as WARN), then
#                                    check logread: planner INFO only as daemon.info, WARN as daemon.err, no `<N>`
#   tools/lab/log-sinks.sh systemd   on lab-sys, transient units of the built binary with isolated directories and a
#                                    dry-run network backend (the installed planner is not touched): INFO/WARN/ERROR
#                                    land with journal PRIORITY 6/4/3 and no prefix in MESSAGE, `journalctl -p warning`
#                                    filters, and with stderr redirected to a file (JOURNAL_STREAM names stdout only)
#                                    the lines stay plain
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
S=/root/lab/mdns-openwrt
BIN="$REPO/target/x86_64-unknown-linux-musl/release/meduza-np-planner"
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

case "${1:-}" in
openwrt)
	N=lab-ow
	lx $N mkdir -p /tmp/pkgs
	for p in meduza-np-planner meduza-np-avahi; do
		f=$(ls $S/dist/${p}_*_x86_64.apk)
		incus file push -q "$f" $N/tmp/pkgs/
		lx $N apk add --allow-untrusted "/tmp/pkgs/$(basename "$f")" >/dev/null
	done
	lx $N sh -c 'apk info -v 2>/dev/null | grep -E "meduza-np-(planner|avahi)-" | tr "\n" " "; echo'
	lx $N /etc/init.d/meduza-np-planner restart
	i=0; until lx $N meduza-np status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "planner not up"; sleep 1; done
	pid=$(lx $N pidof meduza-np-planner)
	sleep 20
	# a supervised child that dies is logged as WARN "<id>: exited (...) after Ns"
	child=$(lx $N sh -c "pgrep -P $pid | head -1")
	[ -n "$child" ] || fail "no supervised child of planner pid $pid"
	lx $N kill "$child"
	sleep 8
	lx $N logread -e "meduza-np-planner\[$pid\]" >/tmp/log-sinks-ow.txt || true
	grep -E 'daemon\.(info|err)' /tmp/log-sinks-ow.txt | sed 's/^.*\(daemon\.[a-z]*\) [^:]*: /\1 /' | cut -c1-150 | tail -15
	grep -q 'daemon\.info .* INFO ' /tmp/log-sinks-ow.txt || fail "no INFO line as daemon.info"
	if grep -q 'daemon\.err .* INFO ' /tmp/log-sinks-ow.txt; then fail "INFO line logged as daemon.err"; fi
	grep -q 'daemon\.err .* WARN .*exited' /tmp/log-sinks-ow.txt || fail "no WARN (child exit) as daemon.err"
	grep -q 'WARN .*avahi/bridge: exited (exit status: 0)' /tmp/log-sinks-ow.txt || fail "avahi/bridge did not exit 0 on SIGTERM"
	if grep -q 'daemon\.info .* \(WARN\|ERROR\) ' /tmp/log-sinks-ow.txt; then fail "WARN/ERROR logged as daemon.info"; fi
	if grep -q ': <[0-9]>' /tmp/log-sinks-ow.txt; then fail "sd-daemon prefix under procd"; fi
	ok "procd: INFO=daemon.info, WARN=daemon.err, no prefixes"
	;;
systemd)
	N=lab-sys
	[ -x "$BIN" ] || fail "build first (tools/lab/mdns-openwrt.sh build)"
	lx $N sh -c 'rm -rf /tmp/np-log && mkdir -p /tmp/np-log/bin'
	incus file push -q "$BIN" $N/tmp/np-log/bin/meduza-np-planner
	# isolated directories: nothing of the installed node (state, sockets, packages) is read or written
	cat >/tmp/log-sinks-unit.sh <<'EOF'
set -eu
name=$1; shift
d=/tmp/np-log/$name
mkdir -p $d/etc $d/state $d/run $d/packages $d/libexec
systemctl reset-failed "np-log-$name" 2>/dev/null || true
systemd-run --quiet --unit="np-log-$name" --collect "$@" \
	-p Environment=MEDUZA_NP_ETC_DIR=$d/etc -p Environment=MEDUZA_NP_STATE_DIR=$d/state \
	-p Environment=MEDUZA_NP_RUN_DIR=$d/run -p Environment=MEDUZA_NP_PACKAGES_DIR=$d/packages \
	-p Environment=MEDUZA_NP_LIBEXEC_DIR=$d/libexec -p Environment=MEDUZA_NP_DRY_RUN_NETWORK=1 \
	/tmp/np-log/bin/meduza-np-planner run
sleep 6
systemctl stop "np-log-$name"
EOF
	incus file push -q /tmp/log-sinks-unit.sh $N/tmp/np-log/unit.sh
	since=$(lx $N date '+%Y-%m-%d %H:%M:%S')
	# a: not enrolled -> WARN "manager connection not configured"; b: enrollment from the environment against a
	# closed port -> ERROR "enrollment from environment failed"; c: stderr to a file
	lx $N sh /tmp/np-log/unit.sh a
	lx $N sh /tmp/np-log/unit.sh b -p Environment=MEDUZA_NP_ENROLL_URL=http://127.0.0.1:9 -p Environment=MEDUZA_NP_ENROLL_TOKEN=x
	lx $N sh /tmp/np-log/unit.sh c -p StandardError=file:/tmp/np-log/c.err
	j() { lx $N journalctl --since "$since" -o json "$@" | jq -r 'select(.MESSAGE | type == "string") | "\(.PRIORITY) \(.MESSAGE)"'; }
	j -u np-log-a -u np-log-b | grep -E ' (INFO|WARN|ERROR) ' | cut -c1-150 >/tmp/log-sinks-sys.txt || true
	cat /tmp/log-sinks-sys.txt
	grep -q '^6 .* INFO ' /tmp/log-sinks-sys.txt || fail "no INFO with priority 6"
	grep -q '^4 .* WARN .*manager connection not configured' /tmp/log-sinks-sys.txt || fail "no WARN with priority 4"
	grep -q '^3 .* ERROR .*enrollment from environment failed' /tmp/log-sinks-sys.txt || fail "no ERROR with priority 3"
	if grep -Eq '^[0-9] <[0-9]>' /tmp/log-sinks-sys.txt; then fail "prefix left in MESSAGE"; fi
	if grep -Eq '^(6 .* (WARN|ERROR)|[34] .* INFO) ' /tmp/log-sinks-sys.txt; then fail "level/priority mismatch"; fi
	ok "journald: INFO=6 WARN=4 ERROR=3, prefixes stripped"
	w=$(lx $N journalctl --since "$since" -u np-log-a -u np-log-b -p warning -o cat | grep -c ' INFO ' || true)
	[ "$w" = 0 ] || fail "journalctl -p warning shows $w INFO lines"
	ok "journalctl -p warning hides INFO"
	lx $N cat /tmp/np-log/c.err | cut -c1-150 | head -4
	lx $N grep -q ' WARN .*manager connection not configured' /tmp/np-log/c.err || fail "no WARN in the redirected stderr"
	if lx $N grep -q '^<[0-9]>' /tmp/np-log/c.err; then fail "prefix in a redirected stderr"; fi
	ok "stderr redirected to a file: plain lines"
	lx $N rm -rf /tmp/np-log
	;;
*)
	sed -n '2,12p' "$0"
	exit 2
	;;
esac
