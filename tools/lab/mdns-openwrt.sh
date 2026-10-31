#!/bin/sh
# Avahi `system` mode on OpenWrt (AGENT §9.11, D54): the distribution's avahi-dbus-daemon under procd (no D-Bus bus
# activation, pid file under /var/run = /tmp/run) next to Meduza NP installed from OpenWrt apk packages built from this
# tree. lab-ow is an OpenWrt 25.12 LXC (images:openwrt/25.12) with wan eth0 10.50.0.18 on mnpwan and lan eth1
# 10.62.0.23 on lanb (DHCP/RA off: lab-b serves that LAN). host-b (10.62.0.50) and lab-b announce on the same LAN.
#
#   tools/lab/mdns-openwrt.sh build [VERSION]  musl binaries + apk packages of planner, cli, avahi (default 0.8.7)
#   tools/lab/mdns-openwrt.sh setup            create LXC lab-ow, UCI network, apk add avahi-dbus-daemon + bind-dig,
#                                              install the built packages (planner enabled + started by the package)
#   tools/lab/mdns-openwrt.sh deploy           manager node lab-ow, `meduza-np enroll`
#   tools/lab/mdns-openwrt.sh publish system|managed|off   as tools/lab/mdns-system.sh, for lab-ow
#   tools/lab/mdns-openwrt.sh check            status, avahi instances, procd service + pid file, own name from host-b,
#                                              names through lab-ow's bridge
#   tools/lab/mdns-openwrt.sh outage           /etc/init.d/avahi-daemon stop, watch status + bridge, start, time to up
#   tools/lab/mdns-openwrt.sh reboot           restart the LXC (procd START order dbus < avahi-daemon < planner), check
#   tools/lab/mdns-openwrt.sh remove           delete lab-ow (publish off first)
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/mdns-openwrt
N=lab-ow
IMAGE=${OPENWRT_LXC_IMAGE:-images:openwrt/25.12}
KEYS=${KEYS:-/root/e2e-run/entry.json}
TARGET=x86_64-unknown-linux-musl
PKGS="meduza-np-planner meduza-np-cli meduza-np-avahi"
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
np() { lx $N meduza-np "$@"; }
avahi_states() { np --json status 2>/dev/null | jq -c '[.instances[]? | select(.id | startswith("avahi")) | {id, state, reason}]'; }
bridge_q() {
	# A record through the bridge (port 5354): address, NXDOMAIN or fail (busybox has no timeout: dig's own limits)
	lx $N sh -c "dig +tries=1 +time=6 -p 5354 @127.0.0.1 $1 A 2>&1" |
		awk '/status: NXDOMAIN/{print "NXDOMAIN"; f=1; exit} /^[^;].*\tIN\tA\t/{print $NF; f=1; exit} END{if(!f)print "fail"}'
}
wait_up() { # seconds: until the probe reported avahi/main and every avahi instance is up
	i=0; until avahi_states | jq -e 'any(.[]; .id == "avahi/main") and all(.[]; .state == "up")' >/dev/null || [ $i -gt "$1" ]; do sleep 2; i=$((i + 2)); done
}
mkdir -p $S

case "${1:-}" in
build)
	v=${2:-0.8.7}
	cd "$REPO"
	cargo zigbuild --locked --release --target $TARGET -p meduza-np-planner -p meduza-np-cli -p meduza-np-avahi
	cargo build --locked --release -p meduza-np-release
	printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$KEYS")" >$S/release-keys.json
	rm -rf $S/dist && mkdir -p $S/dist
	for p in $PKGS; do
		target/release/meduza-np-release openwrt --package "$p" --version "$v" --target $TARGET \
			--bin-dir "$REPO/target/$TARGET/release" --trust $S/release-keys.json --out $S/dist >/dev/null
	done
	tools/openwrt-apk.sh $S/dist "$REPO/target/release/meduza-np-release" >/dev/null
	ls -l $S/dist/*.apk
	;;
setup)
	ls $S/dist/*.apk >/dev/null || { echo "run build first" >&2; exit 1; }
	if ! incus info $N >/dev/null 2>&1; then
		incus init -q "$IMAGE" $N -p default -p mnplab
		incus config device add $N eth0 nic network=mnpwan ipv4.address=10.50.0.18 >/dev/null
		incus config device add $N eth1 nic network=lanb ipv4.address=10.62.0.23 >/dev/null
		incus start $N
	fi
	i=0; until lx $N ubus list network >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	# wan = eth0 (manager, package feeds), lan = eth1 (mDNS); fw4's default zones accept input on lan only
	lx $N sh -eu -c '
		uci -q batch <<EOF
set network.lan=interface
set network.lan.device=eth1
set network.lan.proto=static
set network.lan.ipaddr=10.62.0.23
set network.lan.netmask=255.255.255.0
delete network.lan.ip6assign
set network.wan=interface
set network.wan.device=eth0
set network.wan.proto=static
set network.wan.ipaddr=10.50.0.18
set network.wan.netmask=255.255.255.0
set network.wan.gateway=10.50.0.1
delete network.wan.dns
add_list network.wan.dns=10.50.0.1
delete network.wan6
set dhcp.lan=dhcp
set dhcp.lan.interface=lan
set dhcp.lan.ignore=1
set dhcp.lan.dhcpv6=disabled
set dhcp.lan.ra=disabled
set system.@system[0].hostname=lab-ow
commit
EOF
		/etc/init.d/system reload; /etc/init.d/network restart; /etc/init.d/dnsmasq restart; /etc/init.d/odhcpd restart 2>/dev/null || true'
	i=0; until lx $N ping -c1 -W1 10.50.0.10 >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	lx $N sh -c '. /etc/openwrt_release; echo "OpenWrt $DISTRIB_RELEASE ($DISTRIB_ARCH)"'
	lx $N sh -eu -c 'apk update >/dev/null && apk add avahi-dbus-daemon bind-dig >/dev/null'
	# dbus-daemon was started by its own package before avahi's bus policy existed: without a restart avahi-daemon is
	# "not allowed to own the service org.freedesktop.Avahi" and procd respawns it until it gives up
	lx $N sh -c '/etc/init.d/dbus enable; /etc/init.d/avahi-daemon enable; /etc/init.d/dbus restart; /etc/init.d/avahi-daemon restart'
	lx $N mkdir -p /tmp/pkgs
	for p in $PKGS; do
		f=$(ls $S/dist/${p}_*_x86_64.apk)
		incus file push -q "$f" $N/tmp/pkgs/
		lx $N apk add --allow-untrusted "/tmp/pkgs/$(basename "$f")" >/dev/null
	done
	i=0; until np status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && exit 1; sleep 1; done
	np --json packages | jq -c '[.packages[] | {name, version, registration}]'
	lx $N sh -c 'ls /etc/rc.d | grep -E "dbus|avahi|meduza" | tr "\n" " "; echo'
	lx $N ip -4 addr show
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
	system) jq '.nodes["lab-ow"] = ((.nodes["lab-ow"] // { behavior: "static" })
		| .services.avahi = { mode: "system", bridge: { enabled: true } })' $S/doc.json >$S/doc2.json ;;
	managed) jq '.nodes["lab-ow"].services.avahi = { mode: "managed", allowInterfaces: ["eth1"], bridge: { enabled: true } }' $S/doc.json >$S/doc2.json ;;
	off) jq 'del(.nodes["lab-ow"])' $S/doc.json >$S/doc2.json ;;
	*) echo "publish system|managed|off" >&2; exit 2 ;;
	esac
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | "\(.severity) \(.path): \(.message)"'
	$L api publish $S/doc2.json "lab-ow avahi $2" | jq -c '{revision: (.revision // .id // .)}' 2>/dev/null || true
	;;
check)
	echo "== status"
	np --json status | jq -c '{applied: .sync.appliedRevision, stage: .sync.applyStage, err: .sync.lastError}'
	np --json status | jq -c '.instances[]? | select(.id | startswith("avahi")) | {id, state, reason, detail}'
	echo "== avahi-daemon (procd service), pid file, processes"
	lx $N sh -c '/etc/init.d/avahi-daemon status; cat /var/run/avahi-daemon/pid 2>/dev/null; ps w | grep -E "[a]vahi|[d]bus-daemon" | cut -c1-140'
	echo "== host-b resolves lab-ow.local (expect 10.62.0.23)"
	lx host-b sh -c 'timeout 8 avahi-resolve -4 -n lab-ow.local 2>&1 || true'
	echo "== lab-ow's bridge"
	for n in host-b.local lab-b.local lab-ow.local nosuch-host.local; do printf '%s: %s\n' $n "$(bridge_q $n)"; done
	;;
outage)
	t0=$(date +%s)
	lx $N /etc/init.d/avahi-daemon stop
	echo "stopped system avahi-daemon"
	i=0; until avahi_states | jq -e 'any(.[]; .id == "avahi/main" and .state == "down")' >/dev/null || [ $i -gt 40 ]; do sleep 3; i=$((i + 1)); done
	echo "status after $(($(date +%s) - t0)) s: $(avahi_states)"
	echo "bridge while down: host-b.local=$(bridge_q host-b.local) nosuch=$(bridge_q nosuch-host.local)"
	sleep 10
	lx $N sh -c 'pidof avahi-daemon >/dev/null && echo "avahi-daemon came back on its own (bus activation?)" || echo "avahi-daemon stays down"'
	t1=$(date +%s)
	lx $N /etc/init.d/avahi-daemon start
	echo "started system avahi-daemon"
	wait_up 120
	echo "status after $(($(date +%s) - t1)) s: $(avahi_states)"
	echo "bridge after start: host-b.local=$(bridge_q host-b.local) lab-ow.local=$(bridge_q lab-ow.local)"
	;;
reboot)
	t0=$(date +%s)
	incus restart $N
	i=0; until np status >/dev/null 2>&1 || [ $i -gt 60 ]; do sleep 2; i=$((i + 1)); done
	echo "planner answers after $(($(date +%s) - t0)) s"
	lx $N sh -c 'ls /etc/rc.d | grep -E "dbus|avahi|meduza" | tr "\n" " "; echo'
	wait_up 120
	echo "avahi instances after $(($(date +%s) - t0)) s: $(avahi_states)"
	np --json status | jq -c '{applied: .sync.appliedRevision, err: .sync.lastError}'
	lx $N logread | grep -iE 'meduza.*(reject|avahi|invalid)' | tail -8 || true
	echo "bridge: host-b.local=$(bridge_q host-b.local) lab-ow.local=$(bridge_q lab-ow.local)"
	;;
remove)
	incus delete -f $N
	;;
*)
	sed -n '2,17p' "$0"
	exit 2
	;;
esac
