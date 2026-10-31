#!/bin/sh
# Avahi `system` mode (AGENT §9.11): a native (non-container) Debian node that runs the distribution's own
# avahi-daemon (systemd, system D-Bus). lab-sys is a plain Debian 13 LXC on lab-b's LAN (eth0 10.50.0.17 on mnpwan,
# eth1 10.62.0.22 on lanb); Meduza NP is installed per docs/operations.md §2.2 from a signed lab release site with
# the components avahi,cli. host-b (10.62.0.50) and lab-b announce on the same LAN.
#
#   tools/lab/mdns-system.sh setup SITE      create LXC lab-sys, apt avahi-daemon + avahi-utils, native install of SITE
#   tools/lab/mdns-system.sh deploy          manager node lab-sys, `meduza-np enroll`
#   tools/lab/mdns-system.sh publish system|managed|off   system: bridge on the system daemon; managed: must be
#                                            rejected (a system daemon runs); off: remove the node from the document
#   tools/lab/mdns-system.sh check           status, avahi instances + probe detail, daemon owner, own name from host-b,
#                                            names through lab-sys's bridge
#   tools/lab/mdns-system.sh outage          stop the system avahi-daemon, watch status + bridge, start it again,
#                                            time until both are up
#   tools/lab/mdns-system.sh reboot          restart the LXC (boot ordering planner vs avahi-daemon), then check
#   tools/lab/mdns-system.sh remove          delete lab-sys (publish off first)
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/mdns-system
N=lab-sys
KEYS=${KEYS:-/root/e2e-run/entry.json}
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
np() { lx $N meduza-np "$@"; }
avahi_states() { np --json status 2>/dev/null | jq -c '[.instances[]? | select(.id | startswith("avahi")) | {id, state, reason}]'; }
bridge_q() {
	# A record through the bridge (port 5354): address, NXDOMAIN or fail
	lx $N sh -c "timeout 8 dig +tries=1 +time=6 -p 5354 @127.0.0.1 $1 A 2>&1" |
		awk '/status: NXDOMAIN/{print "NXDOMAIN"; f=1; exit} /^[^;].*\tIN\tA\t/{print $NF; f=1; exit} END{if(!f)print "fail"}'
}
mkdir -p $S

case "${1:-}" in
setup)
	site=$2
	if ! incus info $N >/dev/null 2>&1; then
		incus init -q deb13 $N -p default -p mnplab
		incus config device add $N eth0 nic network=mnpwan ipv4.address=10.50.0.17 >/dev/null
		incus config device add $N eth1 nic network=lanb ipv4.address=10.62.0.22 >/dev/null
		incus start $N
	fi
	i=0; until lx $N ip -4 route show default 2>/dev/null | grep -q via; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	lx $N sh -c 'printf "[Match]\nName=eth1\n\n[Network]\nAddress=10.62.0.22/24\nDHCP=no\n" >/etc/systemd/network/20-eth1.network && networkctl reload'
	lx $N sh -c 'DEBIAN_FRONTEND=noninteractive apt-get -qq update && DEBIAN_FRONTEND=noninteractive apt-get -qq install -y --no-install-recommends avahi-daemon avahi-utils jq bind9-dnsutils >/dev/null'
	lx $N systemctl is-active avahi-daemon
	lx $N sh -c 'mkdir -p /srv/site /srv/incoming && find /srv/site /srv/incoming -mindepth 1 -delete'
	incus file push -q -r "$site" $N/srv/incoming/
	lx $N sh -c "cp -a /srv/incoming/$(basename "$site")/. /srv/site/ && rm -rf /srv/incoming"
	g=$(ls "$site/releases" | sort -V | tail -1)
	uv=$(jq -r '.packages[] | select(.name == "meduza-np-updater") | .version' "$site/releases/$g/manifest.json")
	printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$KEYS")" >$S/release-keys.json
	incus file push -q $S/release-keys.json $N/root/release-keys.json
	# docs §2.2 steps 2-4, as root on the node
	lx $N sh -eu -c "
		mkdir -p /tmp/u && rm -rf /tmp/u/* && tar -xzf /srv/site/packages/meduza-np-updater_${uv}_linux-amd64.tar.gz -C /tmp/u
		install -D -m 0755 /tmp/u/usr/bin/meduza-np-updater /usr/lib/meduza-np/baseline/meduza-np-updater
		install -D -m 0644 /root/release-keys.json /etc/meduza-np/trust/release-keys.json
		rc=0; MEDUZA_NP_UPDATE_BASE_URL=file:///srv/site MEDUZA_NP_COMPONENTS=avahi,cli /usr/lib/meduza-np/baseline/meduza-np-updater apply || rc=\$?
		[ \$rc -eq 0 ] || [ \$rc -eq 75 ] || { echo bootstrap apply exited \$rc; exit 1; }
		/usr/lib/meduza-np/baseline/meduza-np-updater system-links /
		install -m 0644 /usr/share/meduza-np/packaging/updater/systemd/meduza-np-updater.service /etc/systemd/system/
		install -m 0644 /usr/share/meduza-np/packaging/planner/systemd/meduza-np-planner.service /etc/systemd/system/
		printf 'MEDUZA_NP_UPDATE_BASE_URL=file:///srv/site\nMEDUZA_NP_UPDATE_AUTO=0\n' >/etc/meduza-np/updater.env
		systemctl daemon-reload
		systemctl enable --now meduza-np-updater meduza-np-planner
		i=0; until meduza-np status >/dev/null 2>&1; do i=\$((i + 1)); [ \$i -gt 30 ] && exit 1; sleep 1; done"
	np --json update status | jq -c '{groupVersion: .installed.groupVersion, packages: (.installed.packages | keys)}'
	lx $N ip -br addr
	;;
deploy)
	$L api node $N static >/dev/null 2>&1 || true
	np enroll --url http://10.50.0.10:8787 --token "$($L api token $N)" --insecure
	i=0; until np --json status 2>/dev/null | jq -e '.nodeId | select(. != null and . != "")' >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && exit 1; sleep 2; done
	np --json status | jq -c '{nodeId, sync: .sync.state}'
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	case "$2" in
	system) jq '.nodes["lab-sys"] = ((.nodes["lab-sys"] // { behavior: "static" })
		| .services.avahi = { mode: "system", bridge: { enabled: true } })' $S/doc.json >$S/doc2.json ;;
	managed) jq '.nodes["lab-sys"].services.avahi = { mode: "managed", allowInterfaces: ["eth1"], bridge: { enabled: true } }' $S/doc.json >$S/doc2.json ;;
	off) jq 'del(.nodes["lab-sys"])' $S/doc.json >$S/doc2.json ;;
	*) echo "publish system|managed|off" >&2; exit 2 ;;
	esac
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | "\(.severity) \(.path): \(.message)"'
	$L api publish $S/doc2.json "lab-sys avahi $2" | jq -c '{revision: (.revision // .id // .)}' 2>/dev/null || true
	;;
check)
	echo "== status"
	np --json status | jq -c '{applied: .sync.appliedRevision, stage: .sync.applyStage, err: .sync.lastError}'
	np --json status | jq -c '.instances[]? | select(.id | startswith("avahi")) | {id, state, reason, detail}'
	echo "== avahi-daemon (system unit) and its children"
	lx $N sh -c 'systemctl is-active avahi-daemon; cat /run/avahi-daemon/pid 2>/dev/null; ps -eo pid,ppid,user,args | grep -E "[a]vahi" | cut -c1-140'
	echo "== host-b resolves lab-sys.local (expect 10.62.0.22)"
	lx host-b sh -c 'timeout 8 avahi-resolve -4 -n lab-sys.local 2>&1 || true'
	echo "== lab-sys's bridge"
	for n in host-b.local lab-b.local lab-sys.local nosuch-host.local; do printf '%s: %s\n' $n "$(bridge_q $n)"; done
	;;
outage)
	t0=$(date +%s)
	lx $N systemctl stop avahi-daemon.socket avahi-daemon.service
	echo "stopped system avahi-daemon"
	i=0; until avahi_states | jq -e 'any(.[]; .id == "avahi/main" and .state == "down")' >/dev/null || [ $i -gt 40 ]; do sleep 3; i=$((i + 1)); done
	echo "status after $(($(date +%s) - t0)) s: $(avahi_states)"
	echo "bridge while down: host-b.local=$(bridge_q host-b.local) nosuch=$(bridge_q nosuch-host.local)"
	t1=$(date +%s)
	lx $N systemctl start avahi-daemon.service
	echo "started system avahi-daemon"
	i=0; until avahi_states | jq -e 'length > 0 and all(.[]; .state == "up")' >/dev/null || [ $i -gt 60 ]; do sleep 2; i=$((i + 1)); done
	echo "status after $(($(date +%s) - t1)) s: $(avahi_states)"
	echo "bridge after start: host-b.local=$(bridge_q host-b.local) lab-sys.local=$(bridge_q lab-sys.local)"
	;;
reboot)
	t0=$(date +%s)
	incus restart $N
	i=0; until np status >/dev/null 2>&1 || [ $i -gt 60 ]; do sleep 2; i=$((i + 1)); done
	echo "planner answers after $(($(date +%s) - t0)) s"
	lx $N sh -c 'systemctl show -p ActiveEnterTimestampMonotonic avahi-daemon meduza-np-planner | paste -sd" "'
	i=0; until avahi_states | jq -e 'length > 0 and all(.[]; .state == "up")' >/dev/null || [ $i -gt 60 ]; do sleep 2; i=$((i + 1)); done
	echo "avahi instances after $(($(date +%s) - t0)) s: $(avahi_states)"
	np --json status | jq -c '{applied: .sync.appliedRevision, err: .sync.lastError}'
	lx $N journalctl -b -u meduza-np-planner --no-pager | grep -iE 'reject|avahi|invalid' | tail -8
	echo "bridge: host-b.local=$(bridge_q host-b.local) lab-sys.local=$(bridge_q lab-sys.local)"
	;;
remove)
	incus delete -f $N
	;;
*)
	sed -n '2,19p' "$0"
	exit 2
	;;
esac
