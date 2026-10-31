#!/bin/sh
# mDNS in the lab (AGENT §9.11, D10, D45): lab-b runs managed avahi-daemon (host network mode, so it announces and
# resolves on lab-b's LAN) and the `.local` bridge; the DNS front end of dns.sh (dnsmasq on 10.62.0.12:53) forwards
# `.local` to the bridge (mdnsForward). host-b (10.62.0.50, Alpine) runs its own avahi-daemon, so
# `host-b.local` -> 10.62.0.50 proves LAN client -> dnsmasq -> bridge -> D-Bus -> avahi-daemon -> mDNS on the LAN.
# lab-b was first deployed without the avahi component: the package arrived with the configuration (D20), so its
# planner container had no SYS_CHROOT and avahi ran with `--no-chroot` (D44, D45). Since D48 its node.env selects
# avahi, so the re-created container has SYS_CHROOT and avahi-daemon keeps its own chroot (`state` shows the chroot
# helper process). The container role starts the system D-Bus once the package is installed (D45). Needs the DNS
# front end (`tools/lab/dns.sh publish both`).
#
#   tools/lab/mdns.sh setup               host-b: avahi-daemon + dbus (OpenRC, idempotent)
#   tools/lab/mdns.sh publish on|off      avahi managed + bridge on lab-b's LAN interface, dnsmasq mdnsForward
#   tools/lab/mdns.sh q [NAME]            host-b: query the front end (A), prints the answer or "fail"/"NXDOMAIN"
#   tools/lab/mdns.sh bridge [NAME] [TYPE]  lab-b: query the bridge directly (127.0.0.1:5354), default A
#   tools/lab/mdns.sh state               avahi instances, bus + daemon processes (uid, CapEff, argv), interfaces
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
FRONT=10.62.0.12
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
p() { lx lab-b podman exec meduza-np-planner "$@"; }

case "${1:-}" in
setup)
	lx host-b sh -c 'apk add -q avahi avahi-tools dbus >/dev/null && rc-update add dbus default >/dev/null && rc-update add avahi-daemon default >/dev/null
		rc-service dbus start >/dev/null 2>&1; rc-service avahi-daemon start >/dev/null 2>&1; rc-service avahi-daemon status'
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$LAB/mdns-doc.json
	case "$2" in
	on)
		jq '.nodes["lab-b"].services.avahi = { mode: "managed", allowInterfaces: ["eth1"], bridge: { enabled: true } }
			| .nodes["lab-b"].services.dnsmasq.mdnsForward = true' $LAB/mdns-doc.json >$LAB/mdns-doc2.json
		;;
	off)
		jq 'del(.nodes["lab-b"].services.avahi) | del(.nodes["lab-b"].services.dnsmasq.mdnsForward)' $LAB/mdns-doc.json >$LAB/mdns-doc2.json
		;;
	*) echo "publish on|off" >&2; exit 2 ;;
	esac
	$L api publish $LAB/mdns-doc2.json "mdns $2" | jq -c '{revision: (.revision // .id // .), nodes: (.nodes // empty)}' 2>/dev/null || true
	;;
q)
	name=${2:-host-b.local}
	out=$(lx host-b sh -c "timeout 5 nslookup -type=a $name $FRONT 2>&1" || true)
	a=$(echo "$out" | awk '/^Name:/ { n = 1 } n && /^Address/ { print $NF; exit }')
	[ -z "$a" ] && echo "$out" | grep -q NXDOMAIN && a=NXDOMAIN
	echo "${a:-fail}"
	;;
bridge)
	p sh -c "timeout 5 nslookup -type=${3:-a} -port=5354 ${2:-host-b.local} 127.0.0.1 2>&1" | sed -n '/^Name:\|^Address\|NXDOMAIN\|SERVFAIL\|REFUSED\|answer/p' || true
	;;
state)
	p meduza-np --json status | jq -c '[.instances[]? | select(.id | startswith("avahi")) | {id, state, reason, version, hostName}]'
	p sh -c 'ls /etc/avahi/services/ 2>/dev/null | sed "s/^/static service file: /"'
	p sh -c 'for d in /proc/[0-9]*; do
		c=$(tr "\0" " " <$d/cmdline 2>/dev/null | cut -c1-150); case "$c" in *avahi*|*dbus-daemon*) ;; *) continue ;; esac
		case "$c" in "sh -c"*) continue ;; esac
		printf "%6s uid=%-5s eff=%s  %s\n" "${d#/proc/}" "$(awk "/^Uid:/{print \$2}" $d/status)" "$(awk "/^CapEff:/{print \$2}" $d/status)" "$c"
	done'
	p sh -c 'grep -h "allow-interfaces" /run/meduza-np/generations/*/avahi/main/avahi-daemon.conf 2>/dev/null | sort -u'
	lx lab-b ss -lunp | awk '$4 ~ /:5353$|:5354$/ { print "udp", $4, $NF }'
	;;
*)
	sed -n '2,14p' "$0"
	exit 2
	;;
esac
