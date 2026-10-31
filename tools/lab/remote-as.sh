#!/bin/sh
# Two unmanaged networks behind lab-ext, for the topology view of ASes known only from AS paths:
#   AS 65010 (lab-a) - AS 65100 (lab-ext) - AS 65200 (lab-net1) - AS 65300 (lab-net2)
# lab-net1 (10.50.0.31 on mnpwan, network 10.71.0.0/24) peers with lab-ext; lab-net2 (10.50.0.32, network
# 10.72.0.0/24) peers with lab-net1. Plain Alpine + FRR (a few MB, unlike a Debian LXC), no Meduza software: the
# manager learns about them only from the AS paths of the routes lab-ext passes on. lab-a's session to lab-ext
# accepts both networks and lab-ext as a transit AS (`noTransit: false`; by default only routes lab-ext originates
# itself are accepted), published through the manager, so its routes carry 65100 65200 and 65100 65200 65300.
#
#   tools/lab/remote-as.sh setup     create/refresh both routers, peer lab-ext, accept the prefixes on lab-a
#   tools/lab/remote-as.sh check     sessions on the chain and the AS paths lab-a reports
#   tools/lab/remote-as.sh remove    delete both routers, lab-ext's neighbor and the accepted prefixes
set -eu
L="$(dirname "$0")/lab.sh"
S=/root/lab/remote-as
X=lab-ext
NETS='"10.71.0.0/24", "10.72.0.0/24"'
mkdir -p $S
lx() { n=$1; shift; incus exec "$n" -- "$@"; }
pl() { lx lab-a podman exec meduza-np-planner "$@"; }

# router NAME ADDRESS ASN NETWORK-GATEWAY NEIGHBOR-ADDRESS NEIGHBOR-ASN [NEIGHBOR-ADDRESS NEIGHBOR-ASN]
router() {
	n=$1 addr=$2 asn=$3 gw=$4
	shift 4
	if ! incus info "$n" >/dev/null 2>&1; then
		incus init alp "$n" -p default -c limits.memory=128MiB >/dev/null
		incus config device add "$n" eth0 nic network=mnpwan ipv4.address="$addr" >/dev/null
		incus start "$n"
		sleep 6
	fi
	# DHCP and DNS take a few seconds after the first start
	lx "$n" sh -c 'command -v vtysh >/dev/null && exit 0; for i in 1 2 3 4 5 6; do apk add -q frr iproute2 >/dev/null 2>&1 && exit 0; sleep 5; done; apk add frr iproute2'
	# the network the AS announces, on a dummy interface (as lab-ext's ext0)
	lx "$n" sh -c "mkdir -p /etc/local.d && printf '#!/bin/sh\nip link add net0 type dummy 2>/dev/null || true\nip addr replace $gw/24 dev net0\nip link set net0 up\n' >/etc/local.d/net0.start && chmod +x /etc/local.d/net0.start && /etc/local.d/net0.start && rc-update add local default >/dev/null 2>&1 || true"
	neighbors=""
	while [ $# -ge 2 ]; do
		neighbors="$neighbors neighbor $1 remote-as $2
 neighbor $1 timers 10 30
"
		shift 2
	done
	lx "$n" sh -c "sed -i 's/^bgpd=no/bgpd=yes/' /etc/frr/daemons && cat >/etc/frr/frr.conf" <<EOF
frr defaults traditional
hostname $n
log syslog informational
!
router bgp $asn
 bgp router-id $gw
 no bgp ebgp-requires-policy
$neighbors !
 address-family ipv4 unicast
  network ${gw%.*}.0/24
 exit-address-family
!
EOF
	lx "$n" sh -c 'rc-update add frr default >/dev/null 2>&1; rc-service frr restart >/dev/null 2>&1'
}

# lab-a's session to lab-ext: accept (or stop accepting) the two networks, through lab-ext as transit.
accept() {
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	jq --argjson nets "[$NETS]" --arg mode "$1" '(.nodes["lab-a"].services.openvpn.instances[] | select(.id == "ext") | .bgp.accept) |=
		(if $mode == "on" then (. + $nets | unique) else (. - $nets) end) |
		(.nodes["lab-a"].services.openvpn.instances[] | select(.id == "ext") | .bgp) |=
		(if $mode == "on" then .noTransit = false else del(.noTransit) end)' $S/doc.json >$S/doc2.json
	if cmp -s $S/doc.json $S/doc2.json; then echo "lab-a accept list unchanged"; return; fi
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" | jq -r '(.problems // .)[]? | "\(.severity) \(.path): \(.message)"'
	$L api publish $S/doc2.json "remote ASes behind lab-ext: accept $1" | jq -c '{revision: (.revision // .id // .)}' 2>/dev/null || true
}

case "${1:-}" in
setup)
	router lab-net1 10.50.0.31 65200 10.71.0.1 10.50.0.30 65100 10.50.0.32 65300
	router lab-net2 10.50.0.32 65300 10.72.0.1 10.50.0.31 65200
	lx $X vtysh -c 'configure terminal' -c 'router bgp 65100' -c 'neighbor 10.50.0.31 remote-as 65200' \
		-c 'neighbor 10.50.0.31 description lab-net1' -c 'neighbor 10.50.0.31 timers 10 30' -c 'end' -c 'write memory' >/dev/null
	accept on
	echo "remote ASes ready: lab-net1 AS 65200 (10.71.0.0/24), lab-net2 AS 65300 (10.72.0.0/24)"
	;;
check)
	echo "== sessions along the chain"
	lx $X vtysh -c 'show bgp ipv4 unicast summary' | grep -E '^10\.50\.0\.31|^10\.99\.0\.1' || true
	lx lab-net1 vtysh -c 'show bgp ipv4 unicast summary' | grep -E '^10\.50\.0\.3[02]' || true
	echo "== lab-ext's table: who sends what"
	lx $X vtysh -c 'show bgp ipv4 unicast' | grep -E '10\.7[0-2]\.0\.0' || true
	echo "== lab-a's report on its session to lab-ext"
	pl meduza-np --json status | jq -c '.instances[] | select(.id == "frr/main/bgp-ext") | {bgpState, hostname, routerId, routes, asPaths}'
	;;
remove)
	accept off
	lx $X vtysh -c 'configure terminal' -c 'router bgp 65100' -c 'no neighbor 10.50.0.31' -c 'end' -c 'write memory' >/dev/null 2>&1 || true
	for n in lab-net1 lab-net2; do incus delete -f "$n" 2>/dev/null || true; done
	echo "remote ASes removed"
	;;
*)
	sed -n '2,13p' "$0"
	exit 2
	;;
esac
