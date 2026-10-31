#!/bin/sh
# Bridge-mode node in the lab (AGENT §14.3.3, D41): lab-br runs base-container with the planner on the Podman
# bridge network (`run.sh --network bridge`), so its WireGuard interface, nft tables and sysctls live in the
# planner container's namespace, not in lab-br's. Only published ports reach it:
#
#   lab-ext 10.50.0.30 (unmanaged, wg-quick `wg-br`, 10.96.0.1) ── udp 10.50.0.15:51820 (published) ──▶ lab-br
#           lab-echo.py on :8089 answers with the peer address        planner container: wg-wg0 10.96.0.2/24
#   lab-vpn 10.50.0.40 ── tcp 10.50.0.15:8089 (published) ──▶ port forward `echo` (DNAT 10.96.0.1:8089 + SNAT)
#                                                              ──▶ through the tunnel to lab-ext
#
# WireGuard is an external connection (D17): lab-br's peer is lab-ext by public key, not a managed node. The port
# forward is a host-network capability (D41), so the manager warns before publishing and the node reports it as
# "container network only" while it still works for traffic that arrives through the published port.
#
#   tools/lab/bridge.sh setup SITE      create LXC lab-br (10.50.0.15 on mnpwan), load the base image, copy SITE;
#                                       WireGuard + echo service on lab-ext (idempotent)
#   tools/lab/bridge.sh deploy          manager node + WireGuard key, enroll lab-br in bridge mode, 51820/udp and
#                                       8089/tcp published
#   tools/lab/bridge.sh publish on|off  add/remove lab-br in the live cluster document; prints the validation
#                                       warnings first, then publishes
#   tools/lab/bridge.sh check           namespaces, published ports, tunnel, port forward, reported mode, limitations
#   tools/lab/bridge.sh upgrade SITE VERSION   roll SITE out to lab-br while lab-ext pings 10.96.0.2 every 0.2 s;
#                                       prints the outcome, ping gaps, whether wg-wg0 was kept and the port forward
#   tools/lab/bridge.sh recreate        re-run run.sh (new planner container + network namespace) under the same
#                                       ping; prints the time until the next handshake, what planner recreated,
#                                       the ping gaps and the port forward
#   tools/lab/bridge.sh bgp on|off      eBGP AS 65020 (lab-br) <-> AS 65100 (lab-ext) over the WireGuard tunnel; lab-br
#                                       declares the LAN 10.65.0.0/24, which exists only on lab-br's own dummy `lan0`
#                                       (outside the planner container); prints the warnings, then publishes
#   tools/lab/bridge.sh bgp-check       session, where the learned 10.70.0.0/24 landed (container vs lab-br), traffic
#                                       from the container to it, whether lab-ext learned the LAN, FRR feature status
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/bridge
B=lab-br
X=lab-ext
BR_IP=10.50.0.15
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
pl() { lx $B podman exec meduza-np-planner "$@"; }
wg_index() { pl cat /sys/class/net/wg-wg0/ifindex 2>/dev/null || echo none; }
# lab-ext pings lab-br's tunnel address every 0.2 s in the background until ping_report
ping_start() {
	lx $X sh -c 'rm -f /tmp/br-ping.log; ping -D -i 0.2 -W1 10.96.0.2 >/tmp/br-ping.log 2>&1 & echo $! >/tmp/br-ping.pid'
	sleep 2
}
ping_report() {
	lx $X sh -c 'kill -INT $(cat /tmp/br-ping.pid); sleep 1; tail -2 /tmp/br-ping.log'
	lx $X sh -c "grep -o 'seq=[0-9]*' /tmp/br-ping.log | cut -d= -f2" |
		awk 'NR>1 && $1!=p+1 {printf "gap: seq %d -> %d (%.1f s)\n", p, $1, ($1-p-1)*0.2} {p=$1}'
}
forward_probe() {
	lx lab-vpn timeout 5 bash -c "exec 3<>/dev/tcp/$BR_IP/8089 && cat <&3" | sed 's/^/port forward answer: /' || echo "port forward: no answer"
}
mkdir -p $S

case "${1:-}" in
setup)
	site=$2
	if ! incus info $B >/dev/null 2>&1; then
		incus init -q deb13-podman $B -p default -p mnplab
		incus config device add $B eth0 nic network=mnpwan ipv4.address=$BR_IP >/dev/null
		incus start $B
	fi
	i=0; until lx $B ip -4 route show default | grep -q via; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	lx $B podman image exists localhost/meduza-np-base-container:e2e || {
		incus file push -q $LAB/base.tar $B/root/base.tar
		lx $B podman load -q -i /root/base.tar
		lx $B rm -f /root/base.tar
	}
	incus file push -q -r "$REPO/packages/meduza-np-base-container/deploy/podman" $B/root/
	lx $B sh -c 'sed -i "s/\r$//" /root/podman/run.sh && chmod +x /root/podman/run.sh'
	lx $B sh -c 'mkdir -p /srv/import /srv/incoming && rm -rf /srv/incoming/*'
	incus file push -q -r "$site" $B/srv/incoming/
	lx $B sh -c "find /srv/import -mindepth 1 -delete && cp -a /srv/incoming/$(basename "$site")/. /srv/import/"
	# lab-ext: the external WireGuard side and the "service" behind the port forward
	lx $X sh -c 'command -v wg >/dev/null && command -v python3 >/dev/null && command -v ping >/dev/null ||
		{ apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq wireguard-tools python3 iputils-ping >/dev/null; }'
	lx $X sh -c 'umask 077; mkdir -p /etc/wireguard; [ -s /etc/wireguard/br.key ] || wg genkey >/etc/wireguard/br.key'
	lx $X sh -c 'wg pubkey </etc/wireguard/br.key' >$S/ext.pub
	lx $X mkdir -p /usr/local/lib/lab
	incus file push -q "$REPO/tools/lab/lab-echo.py" $X/usr/local/lib/lab/lab-echo.py
	lx $X sh -c 'cat >/etc/systemd/system/lab-echo-br.service' <<EOF
[Unit]
Description=lab echo behind lab-br's port forward
[Service]
ExecStart=/usr/bin/python3 /usr/local/lib/lab/lab-echo.py 0.0.0.0 8089
Restart=always
[Install]
WantedBy=multi-user.target
EOF
	lx $X systemctl daemon-reload
	lx $X systemctl enable --now lab-echo-br.service >/dev/null 2>&1
	echo "lab-br ready; lab-ext WireGuard public key $(cat $S/ext.pub)"
	;;
deploy)
	$L api node $B static >/dev/null 2>&1 || true
	$L api pubkey wg-$B >/dev/null 2>&1 || $L api wgkey wg-$B $B >/dev/null
	$L node $B wireguard,cli bridge "$($L api token $B)" 51820/udp 8089/tcp
	# lab-ext dials lab-br's published port; the key is only known once the manager generated it
	pub=$($L api pubkey wg-$B)
	lx $X sh -c "umask 077; cat >/etc/wireguard/wg-br.conf" <<EOF
[Interface]
Address = 10.96.0.1/24
ListenPort = 51830
PostUp = wg set %i private-key /etc/wireguard/br.key

[Peer]
PublicKey = $pub
Endpoint = $BR_IP:51820
AllowedIPs = 10.96.0.2/32
PersistentKeepalive = 15
EOF
	lx $X systemctl enable wg-quick@wg-br >/dev/null 2>&1
	lx $X systemctl restart wg-quick@wg-br
	echo "lab-br deployed (bridge); lab-ext wg-quick@wg-br up"
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	if [ "$2" = on ]; then
		jq --arg ext "$(cat $S/ext.pub)" '.nodes["lab-br"] = {
			behavior: "static",
			networks: { portForwards: [ { id: "echo", proto: "tcp", listenPort: 8089,
				targetAddress: "10.96.0.1", targetPort: 8089, snat: true } ] },
			services: { wireguard: { instances: [ {
				id: "wg0", privateKeyRef: "wg-lab-br", addresses: ["10.96.0.2/24"], listenPort: 51820,
				publicEndpoint: "10.50.0.15:51820",
				peers: [ { id: "lab-ext", publicKey: $ext, allowedIps: ["10.96.0.1/32"] } ] } ] } } }' $S/doc.json >$S/doc2.json
	else
		jq 'del(.nodes["lab-br"])' $S/doc.json >$S/doc2.json
	fi
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | select(.severity == "warning") | "warning \(.path): \(.message)"'
	$L api publish $S/doc2.json "bridge node $2" | jq -c '{revision: (.revision // .id // .), changed: .changed?}' 2>/dev/null || true
	;;
check)
	echo "== lab-br: containers and published ports"
	lx $B podman ps --format '{{.Names}} {{.Networks}} {{.Ports}}'
	echo "== WireGuard device: in the planner container, not on lab-br"
	pl ip -br addr show wg-wg0 || echo "missing in the container"
	if lx $B ip link show wg-wg0 >/dev/null 2>&1; then echo "UNEXPECTED: wg-wg0 on lab-br"; else echo "lab-br has no wg-wg0"; fi
	pl sh -c 'nft list tables 2>/dev/null' | sed 's/^/container nft: /'
	lx $B sh -c 'nft list tables 2>/dev/null | grep meduza || echo "lab-br has no meduza nft tables"'
	echo "== tunnel lab-ext -> 10.96.0.2"
	lx $X sh -c 'echo "handshake $(( $(date +%s) - $(wg show wg-br latest-handshakes | cut -f2) )) s ago"'
	lx $X ping -c 3 -W 1 -q 10.96.0.2 | tail -2
	echo "== port forward lab-vpn -> $BR_IP:8089 -> 10.96.0.1:8089 (answer = source seen by lab-ext)"
	lx lab-vpn timeout 5 bash -c "exec 3<>/dev/tcp/$BR_IP/8089 && cat <&3" || echo "port forward: no answer"
	echo "== report + features"
	$L api status $B | jq -r '"networkMode: \([.. | objects | .networkMode? // empty][0] // "none")"'
	pl meduza-np --json status | jq -r '.features[] | select(.enabled) |
		"\(.id): running=\(.running) healthy=\(.healthy) limitation=\(.limitationCode // "-")"'
	;;
upgrade)
	site=$2 version=$3
	before=$(wg_index)
	$L site "$site" 2>&1 | tr '\r' '\n' | grep -v Pushing | tail -1 || true
	ping_start
	pl meduza-np update apply >/dev/null
	ph=""
	for _ in $(seq 1 60); do
		ph=$(pl meduza-np --json update status 2>/dev/null | jq -r '.lastJournal.groupVersion + " " + .lastJournal.phase')
		[ "$ph" = "$version done" ] && break
		sleep 2
	done
	echo "$B: $ph"
	sleep 60
	ping_report
	after=$(wg_index)
	if [ "$before" = "$after" ]; then echo "wg-wg0 kept (ifindex $after)"; else echo "wg-wg0 recreated ($before -> $after)"; fi
	forward_probe
	;;
recreate)
	# Replace the planner container (what an image change or a re-run of run.sh does): a new network namespace
	# without wg-wg0 and nft tables, a new container address behind the published ports.
	incus file push -q "$REPO/packages/meduza-np-base-container/deploy/podman/run.sh" $B/root/podman/run.sh
	lx $B sh -c 'sed -i "s/\r$//" /root/podman/run.sh'
	ping_start
	t0=$(date +%s)
	lx $B /root/podman/run.sh --env-file /etc/meduza-np/node.env --image localhost/meduza-np-base-container:e2e \
		--network bridge --publish 51820/udp --publish 8089/tcp --import /srv/import 2>&1 | tail -1
	until pl wg show wg-wg0 latest-handshakes 2>/dev/null | awk '$2 > 0 { up = 1 } END { exit !up }'; do
		[ $(($(date +%s) - t0)) -gt 180 ] && { echo "no handshake within 180 s"; break; }
		sleep 1
	done
	echo "handshake again $(($(date +%s) - t0)) s after run.sh started"
	lx $B podman logs --since 5m meduza-np-planner 2>&1 | grep -e 'recreating lost' -e 'restored last good' | cut -c1-140
	sleep 10
	ping_report
	lx $B sh -c 'command -v conntrack >/dev/null && conntrack -L -p udp --orig-port-dst 51820 2>/dev/null' | sed 's/^/conntrack: /' || true
	forward_probe
	;;
bgp)
	# lab-ext side: the neighbor (FRR from external.sh) and the LAN in the tunnel's allowed IPs
	if [ "$2" = on ]; then
		lx $B sh -c 'ip link add lan0 type dummy 2>/dev/null || true; ip addr replace 10.65.0.1/24 dev lan0; ip link set lan0 up'
		lx $X vtysh -c 'configure terminal' -c 'router bgp 65100' -c 'neighbor 10.96.0.2 remote-as 65020' \
			-c 'neighbor 10.96.0.2 description lab-br' -c 'neighbor 10.96.0.2 timers 10 30' -c 'end' -c 'write memory' >/dev/null
		lx $X wg set wg-br peer "$($L api pubkey wg-$B)" allowed-ips 10.96.0.2/32,10.65.0.0/24
		lx $X sed -i 's#^AllowedIPs = 10.96.0.2/32$#AllowedIPs = 10.96.0.2/32, 10.65.0.0/24#' /etc/wireguard/wg-br.conf
	else
		lx $X vtysh -c 'configure terminal' -c 'router bgp 65100' -c 'no neighbor 10.96.0.2' -c 'end' -c 'write memory' >/dev/null || true
		lx $X wg set wg-br peer "$($L api pubkey wg-$B)" allowed-ips 10.96.0.2/32
		lx $X sed -i 's#^AllowedIPs = 10.96.0.2/32, 10.65.0.0/24$#AllowedIPs = 10.96.0.2/32#' /etc/wireguard/wg-br.conf
		lx $B ip link del lan0 2>/dev/null || true
	fi
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	if [ "$2" = on ]; then
		jq '.nodes["lab-br"].routerId = "10.96.0.2" | .nodes["lab-br"].routing = { asn: 65020, originate: { lan: true } } |
			.nodes["lab-br"].networks.lan = ["10.65.0.0/24"] |
			.nodes["lab-br"].services.wireguard.instances[0].peers[0] += {
				allowedIps: ["10.96.0.1/32", "10.70.0.0/24"],
				bgp: { remoteAsn: 65100, address: "10.96.0.1", keepalive: 10, hold: 30, accept: ["10.70.0.0/24"] } }' \
			$S/doc.json >$S/doc2.json
	else
		jq 'del(.nodes["lab-br"].routerId) | del(.nodes["lab-br"].routing) | del(.nodes["lab-br"].networks.lan) |
			.nodes["lab-br"].services.wireguard.instances[0].peers[0] |= (del(.bgp) | .allowedIps = ["10.96.0.1/32"])' \
			$S/doc.json >$S/doc2.json
	fi
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | "\(.severity) \(.path): \(.message)"'
	$L api publish $S/doc2.json "bridge node bgp $2" | jq -c '{revision: (.revision // .id // .), changed: .changed?}' 2>/dev/null || true
	;;
bgp-check)
	echo "== session (lab-ext's view)"
	lx $X vtysh -c 'show bgp ipv4 unicast summary' | grep -e '^Neighbor' -e '^10\.96\.0\.2' || echo "no neighbor 10.96.0.2"
	echo "== learned 10.70.0.0/24: in the planner container, not on lab-br"
	pl ip -4 route show 10.70.0.0/24 | sed 's/^/container: /'
	lx $B sh -c 'ip -4 route show 10.70.0.0/24 | grep -q . && echo "UNEXPECTED on lab-br: $(ip -4 route show 10.70.0.0/24)" || echo "lab-br has no route to 10.70.0.0/24"'
	pl ping -c 3 -W 1 -q 10.70.0.1 | tail -2 | sed 's/^/container -> 10.70.0.1: /'
	lx $B ping -c 2 -W 1 -q 10.70.0.1 >/dev/null 2>&1 && echo "UNEXPECTED: lab-br reaches 10.70.0.1" || echo "lab-br itself cannot reach 10.70.0.1"
	echo "== the LAN 10.65.0.0/24 lives on lab-br's lan0, outside the container namespace"
	pl ip -4 route show 10.65.0.0/24 | grep -q . && echo "UNEXPECTED: container has a route" || echo "container has no route to 10.65.0.0/24"
	lx $X vtysh -c 'show bgp ipv4 unicast 10.65.0.0/24' 2>&1 | head -3 | sed 's/^/lab-ext: /'
	echo "== FRR feature and status"
	pl meduza-np --json status | jq -r '.features[] | select(.enabled and (.id | startswith("frr"))) |
		"\(.id): running=\(.running) healthy=\(.healthy) limitation=\(.limitationCode // "-")"'
	pl meduza-np --json status | jq -c '.instances[] | select(.id | startswith("frr/main/")) |
		{id, state, reason, prefixesSent, routes, notOriginated}'
	;;
*)
	sed -n '2,33p' "$0"
	exit 2
	;;
esac
