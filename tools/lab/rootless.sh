#!/bin/sh
# Rootless bridge-mode node in the lab (AGENT §14.3.3/§14.3.4, D57): lab-rl runs base-container as the unprivileged
# user `mnp` (uid 1000, subuids 100000-165535, lingering on) with the planner on a rootless Podman bridge network
# (`run.sh --network bridge`, netavark + pasta). Same shape as tools/lab/bridge.sh, on its own tunnel:
#
#   lab-ext 10.50.0.30 (wg-quick `wg-rl`, 10.97.0.1, :51831) ── udp 10.50.0.19:51820 (published) ──▶ lab-rl
#           lab-echo.py on :8089 (lab-echo-br.service)           planner container: wg-wg0 10.97.0.2/24
#   lab-vpn 10.50.0.40 ── tcp 10.50.0.19:8089 (published) ──▶ port forward `echo` (DNAT 10.97.0.1:8089 + SNAT)
#
#   tools/lab/rootless.sh setup SITE      create LXC lab-rl (10.50.0.19 on mnpwan), user mnp, passt, load the base
#                                         image as mnp, copy SITE; WireGuard on lab-ext (idempotent)
#   tools/lab/rootless.sh deploy          manager node + WireGuard key, enroll lab-rl rootless, 51820/udp + 8089/tcp
#   tools/lab/rootless.sh publish on|off  add/remove lab-rl in the live cluster document (warnings first)
#   tools/lab/rootless.sh check           who owns the containers, namespaces, ports, tunnel, port forward, report
#   tools/lab/rootless.sh recreate        re-run run.sh as mnp under a ping from lab-ext (new namespace)
#   tools/lab/rootless.sh reboot          restart the LXC; the user's podman-restart.service must bring both back
#   tools/lab/rootless.sh host-net        what a rootless planner in host network mode can do (expected: nothing)
#   tools/lab/rootless.sh quadlet         replace the run.sh containers by the shipped Quadlet units as user units
#   tools/lab/rootless.sh quadlet-reboot  restart the LXC; lingering + default.target must start both units
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/rootless
R=lab-rl
X=lab-ext
RL_IP=10.50.0.19
ENVF=/home/mnp/.config/meduza-np/node.env
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
# a command as mnp with the environment a login session would have (rootless Podman needs XDG_RUNTIME_DIR)
ru() {
	incus exec $R --user 1000 --group 1000 --cwd /home/mnp --env HOME=/home/mnp --env USER=mnp --env LOGNAME=mnp \
		--env XDG_RUNTIME_DIR=/run/user/1000 --env DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus -- "$@"
}
pl() { ru podman exec meduza-np-planner "$@"; }
run_sh() {
	ru /home/mnp/podman/run.sh --env-file $ENVF --image localhost/meduza-np-base-container:e2e \
		--network bridge --publish 51820/udp --publish 8089/tcp --import /srv/import
}
ping_start() {
	lx $X sh -c 'rm -f /tmp/rl-ping.log; ping -D -i 0.2 -W1 10.97.0.2 >/tmp/rl-ping.log 2>&1 & echo $! >/tmp/rl-ping.pid'
	sleep 2
}
ping_report() {
	lx $X sh -c 'kill -INT $(cat /tmp/rl-ping.pid); sleep 1; tail -2 /tmp/rl-ping.log'
	lx $X sh -c "grep -o 'seq=[0-9]*' /tmp/rl-ping.log | cut -d= -f2" |
		awk 'NR>1 && $1!=p+1 {printf "gap: seq %d -> %d (%.1f s)\n", p, $1, ($1-p-1)*0.2} {p=$1}'
}
forward_probe() {
	lx lab-vpn timeout 5 bash -c "exec 3<>/dev/tcp/$RL_IP/8089 && cat <&3" | sed 's/^/port forward answer: /' || echo "port forward: no answer"
}
wait_handshake() {
	t0=$1
	until pl wg show wg-wg0 latest-handshakes 2>/dev/null | awk '$2 > 0 { up = 1 } END { exit !up }'; do
		[ $(($(date +%s) - t0)) -gt 300 ] && { echo "no handshake within 300 s"; return 0; }
		sleep 1
	done
	echo "handshake $(($(date +%s) - t0)) s after start"
}
mkdir -p $S

case "${1:-}" in
setup)
	site=$2
	if ! incus info $R >/dev/null 2>&1; then
		incus init -q deb13-podman $R -p default -p mnplab
		incus config device add $R eth0 nic network=mnpwan ipv4.address=$RL_IP >/dev/null
		incus start $R
	fi
	i=0; until lx $R ip -4 route show default | grep -q via; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	lx $R sh -c 'command -v pasta >/dev/null || { apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq passt conntrack dbus-user-session >/dev/null; }'
	# Lab-only: in a nested (Incus) container the distro AppArmor profile of pasta denies mapping its own binary
	# (the file owner is a shifted uid) and pasta segfaults; a plain host does not need this.
	lx $R sh -c 'for p in usr.bin.pasta usr.bin.passt; do ln -sf /etc/apparmor.d/$p /etc/apparmor.d/disable/$p;
		apparmor_parser -R /etc/apparmor.d/$p 2>/dev/null || true; done'
	lx $R sh -c 'getent passwd mnp >/dev/null || useradd -m -u 1000 -s /bin/bash mnp; loginctl enable-linger mnp'
	# rootless Quadlet units wait (90 s, podman-user-wait-network-online) for the system network-online.target,
	# which only a system unit can pull in (D57)
	lx $R systemctl add-wants multi-user.target network-online.target 2>/dev/null || true
	i=0; until lx $R test -d /run/user/1000; do i=$((i + 1)); [ $i -gt 30 ] && exit 1; sleep 1; done
	ru podman image exists localhost/meduza-np-base-container:e2e || {
		incus file push -q $LAB/base.tar $R/home/mnp/base.tar
		lx $R chown mnp:mnp /home/mnp/base.tar
		ru podman load -q -i /home/mnp/base.tar
		ru rm -f /home/mnp/base.tar
	}
	incus file push -q -r "$REPO/packages/meduza-np-base-container/deploy/podman" $R/home/mnp/
	lx $R sh -c 'sed -i "s/\r$//" /home/mnp/podman/*.sh && chmod +x /home/mnp/podman/*.sh && chown -R mnp:mnp /home/mnp/podman'
	lx $R sh -c 'mkdir -p /srv/import /srv/incoming && rm -rf /srv/incoming/*'
	incus file push -q -r "$site" $R/srv/incoming/
	lx $R sh -c "find /srv/import -mindepth 1 -delete && cp -a /srv/incoming/$(basename "$site")/. /srv/import/"
	lx $X sh -c 'umask 077; mkdir -p /etc/wireguard; [ -s /etc/wireguard/rl.key ] || wg genkey >/etc/wireguard/rl.key'
	lx $X sh -c 'wg pubkey </etc/wireguard/rl.key' >$S/ext.pub
	lx $X systemctl is-active -q lab-echo-br.service || { echo "run tools/lab/bridge.sh setup first (echo service on lab-ext)"; exit 1; }
	echo "lab-rl ready; lab-ext WireGuard public key $(cat $S/ext.pub)"
	;;
deploy)
	$L api node $R static >/dev/null 2>&1 || true
	$L api pubkey wg-$R >/dev/null 2>&1 || $L api wgkey wg-$R $R >/dev/null
	token=$($L api token $R)
	ru sh -c "mkdir -p /home/mnp/.config/meduza-np && cat >$ENVF <<EOF
MEDUZA_NP_COMPONENTS=wireguard,cli
MEDUZA_NP_UPDATE_BASE_URL=file:///var/lib/meduza-np/import
MEDUZA_NP_UPDATE_AUTO=0
MEDUZA_NP_ENROLL_URL=http://10.50.0.10:8787
MEDUZA_NP_ENROLL_TOKEN_FILE=/var/lib/meduza-np/etc/enroll.token
MEDUZA_NP_ENROLL_INSECURE=1
EOF
chmod 0600 $ENVF"
	ru sh -c "podman volume exists meduza-np-node || podman volume create meduza-np-node >/dev/null
		d=\$(podman volume inspect meduza-np-node --format '{{.Mountpoint}}'); mkdir -p \$d/etc
		umask 077; printf '%s' '$token' >\$d/etc/enroll.token"
	run_sh
	pub=$($L api pubkey wg-$R)
	lx $X sh -c "umask 077; cat >/etc/wireguard/wg-rl.conf" <<EOF
[Interface]
Address = 10.97.0.1/24
ListenPort = 51831
PostUp = wg set %i private-key /etc/wireguard/rl.key

[Peer]
PublicKey = $pub
Endpoint = $RL_IP:51820
AllowedIPs = 10.97.0.2/32
PersistentKeepalive = 15
EOF
	lx $X systemctl enable wg-quick@wg-rl >/dev/null 2>&1
	lx $X systemctl restart wg-quick@wg-rl
	echo "lab-rl deployed (rootless bridge); lab-ext wg-quick@wg-rl up"
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	if [ "$2" = on ]; then
		jq --arg ext "$(cat $S/ext.pub)" '.nodes["lab-rl"] = {
			behavior: "static",
			networks: { portForwards: [ { id: "echo", proto: "tcp", listenPort: 8089,
				targetAddress: "10.97.0.1", targetPort: 8089, snat: true } ] },
			services: { wireguard: { instances: [ {
				id: "wg0", privateKeyRef: "wg-lab-rl", addresses: ["10.97.0.2/24"], listenPort: 51820,
				publicEndpoint: "10.50.0.19:51820",
				peers: [ { id: "lab-ext", publicKey: $ext, allowedIps: ["10.97.0.1/32"] } ] } ] } } }' $S/doc.json >$S/doc2.json
	else
		jq 'del(.nodes["lab-rl"])' $S/doc.json >$S/doc2.json
	fi
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | select(.severity == "warning") | "warning \(.path): \(.message)"'
	$L api publish $S/doc2.json "rootless node $2" | jq -c '{revision: (.revision // .id // .), changed: .changed?}' 2>/dev/null || true
	;;
check)
	echo "== lab-rl: root has no containers, mnp owns them"
	echo "root: $(lx $R podman ps -q | wc -l) containers"
	ru podman ps --format '{{.Names}} {{.Networks}} {{.Ports}}'
	ru podman info --format 'rootless={{.Host.Security.Rootless}} network={{.Host.NetworkBackend}} cmd={{.Host.RootlessNetworkCmd}}'
	echo "== planner process: uid inside, uid on lab-rl"
	pl sh -c 'grep -E "^(Uid|CapEff)" /proc/1/status'
	ru sh -c 'ps -o user= -p $(podman inspect meduza-np-planner --format "{{.State.Pid}}")' | sed 's/^/lab-rl user: /'
	echo "== WireGuard device: in the planner container, not on lab-rl"
	pl ip -br addr show wg-wg0 || echo "missing in the container"
	if lx $R ip link show wg-wg0 >/dev/null 2>&1; then echo "UNEXPECTED: wg-wg0 on lab-rl"; else echo "lab-rl has no wg-wg0"; fi
	pl sh -c 'nft list tables 2>/dev/null' | sed 's/^/container nft: /'
	lx $R sh -c 'nft list tables 2>/dev/null | grep meduza || echo "lab-rl has no meduza nft tables"'
	pl sh -c 'echo "container ip_forward=$(cat /proc/sys/net/ipv4/ip_forward)"'
	echo "== peer endpoint as the container sees it"
	pl wg show wg-wg0 endpoints
	echo "== tunnel lab-ext -> 10.97.0.2"
	lx $X sh -c 'echo "handshake $(( $(date +%s) - $(wg show wg-rl latest-handshakes | cut -f2) )) s ago"'
	lx $X ping -c 3 -W 1 -q 10.97.0.2 | tail -2
	echo "== port forward lab-vpn -> $RL_IP:8089 -> 10.97.0.1:8089 (answer = source seen by lab-ext)"
	forward_probe
	echo "== report + features"
	$L api status $R | jq -r '"networkMode: \([.. | objects | .networkMode? // empty][0] // "none")"'
	pl meduza-np --json status | jq -r '.features[] | select(.enabled) |
		"\(.id): running=\(.running) healthy=\(.healthy) limitation=\(.limitationCode // "-")"'
	pl meduza-np --json status | jq -c '.system | {networkMode, capabilities}'
	;;
recreate)
	incus file push -q "$REPO/packages/meduza-np-base-container/deploy/podman/run.sh" $R/home/mnp/podman/run.sh
	lx $R sh -c 'sed -i "s/\r$//" /home/mnp/podman/run.sh && chown mnp:mnp /home/mnp/podman/run.sh'
	ping_start
	t0=$(date +%s)
	run_sh 2>&1 | tail -3
	wait_handshake "$t0"
	ru podman logs --since 5m meduza-np-planner 2>&1 | grep -e 'recreating lost' -e 'restored last good' | cut -c1-140
	sleep 10
	ping_report
	forward_probe
	;;
reboot)
	t0=$(date +%s)
	incus restart $R
	i=0; until ru podman ps --format '{{.Names}}' 2>/dev/null | grep -q meduza-np-planner; do
		i=$((i + 1)); [ $i -gt 120 ] && { echo "planner container not running 120 s after the restart"; ru systemctl --user status podman-restart.service --no-pager | tail -5; exit 1; }
		sleep 1
	done
	echo "containers back $(($(date +%s) - t0)) s after the restart: $(ru podman ps --format '{{.Names}}' | tr '\n' ' ')"
	ru systemctl --user is-enabled podman-restart.service | sed 's/^/podman-restart.service (user): /'
	wait_handshake "$t0"
	lx $X ping -c 3 -W 1 -q 10.97.0.2 | tail -1
	forward_probe
	;;
quadlet)
	# The same node from the shipped Quadlet units as user units (~/.config/containers/systemd, systemctl --user):
	# only the lab specifics are edited (env file under %h, local image, import mount, the two published ports).
	ru /home/mnp/podman/run.sh --stop
	ru systemctl --user disable podman-restart.service >/dev/null 2>&1 || true
	incus file push -q -r "$REPO/packages/meduza-np-base-container/deploy/podman" $R/home/mnp/
	lx $R sh -c 'find /home/mnp/podman -type f -exec sed -i "s/\r$//" {} +; chown -R mnp:mnp /home/mnp/podman'
	ru sh -c 'q=/home/mnp/.config/containers/systemd; mkdir -p $q && cd /home/mnp/podman &&
		cp meduza-np-planner.container meduza-np-updater.container meduza-np-*.volume meduza-np.network $q/ && cd $q &&
		sed -i -e "s#^EnvironmentFile=.*#EnvironmentFile=%h/.config/meduza-np/node.env#" \
			-e "s#^Image=.*#Image=localhost/meduza-np-base-container:e2e#" meduza-np-*.container &&
		sed -i -e "s|^#Volume=/srv/meduza-np-import:|Volume=/srv/import:|" meduza-np-updater.container &&
		sed -i -e "s|^#PublishPort=51820:51820/udp|PublishPort=51820:51820/udp\nPublishPort=8089:8089/tcp|" meduza-np-planner.container &&
		grep -E "^(EnvironmentFile|Image|Volume=/srv|PublishPort)" meduza-np-*.container'
	ru systemctl --user daemon-reload
	ru /usr/lib/systemd/user-generators/podman-user-generator --dryrun 2>&1 | grep -E '^(ExecStart=|---)' | cut -c1-200 || true
	ping_start
	t0=$(date +%s)
	ru systemctl --user start meduza-np-updater.service meduza-np-planner.service
	wait_handshake "$t0"
	sleep 10
	ping_report
	forward_probe
	ru systemctl --user is-active meduza-np-updater.service meduza-np-planner.service | tr '\n' ' '; echo
	ru podman ps --format '{{.Names}} {{.Ports}}'
	;;
quadlet-reboot)
	t0=$(date +%s)
	incus restart $R
	i=0; until [ "$(ru systemctl --user is-active meduza-np-planner.service 2>/dev/null)" = active ]; do
		i=$((i + 1)); [ $i -gt 120 ] && { echo "planner unit not active 120 s after the restart"; exit 1; }
		sleep 1
	done
	echo "planner unit active $(($(date +%s) - t0)) s after the restart"
	wait_handshake "$t0"
	lx $X ping -c 3 -W 1 -q 10.97.0.2 | tail -1
	forward_probe
	ru journalctl --user -u meduza-np-planner.service -b --no-pager -o cat | grep -e 'starting:' -e 'restored last good' | cut -c1-140
	;;
host-net)
	# rootless + host network: the container shares lab-rl's network namespace but its user namespace does not own
	# it, so NET_ADMIN there is no CAP_NET_ADMIN over lab-rl's network (§14.3.4)
	ru podman run --rm --network host --cap-drop ALL --cap-add NET_ADMIN,NET_RAW --device /dev/net/tun \
		--entrypoint /bin/sh localhost/meduza-np-base-container:e2e -c '
		ip link add mnp-probe type dummy 2>&1 | sed "s/^/ip link add: /"
		ip link show mnp-probe >/dev/null 2>&1 && { echo "UNEXPECTED: link created"; ip link del mnp-probe; }
		ip route add 10.123.0.0/24 dev lo 2>&1 | sed "s/^/ip route add: /"
		if command -v nft >/dev/null; then nft add table inet mnp_probe 2>&1 | sed "s/^/nft add table: /"; else echo "nft: not in the base image"; fi
		sysctl -w net.ipv4.ip_forward=1 2>&1 | sed "s/^/sysctl: /"'
	;;
*)
	sed -n '2,21p' "$0"
	exit 2
	;;
esac
