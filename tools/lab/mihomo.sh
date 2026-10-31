#!/bin/sh
# Mihomo TPROXY in the lab (AGENT §9.7, D29): lab-b proxies the forwarded traffic of its LAN (10.62.0.0/24) through
# a SOCKS5 server on lab-ext, configured only by a subscription that lab-ext also serves.
#
#   lab-ext 10.50.0.30   mihomo as plain SOCKS5 server (TCP + UDP, :1080), subscription over HTTP (:8088),
#                        "internet" endpoint 198.51.100.10:8081 (tools/lab/lab-echo.py: answers with the peer address)
#   lab host             route 198.51.100.10/32 via lab-ext (TEST-NET-2: not RFC1918, so TPROXY does not bypass it)
#   manager              text secrets mihomo-sub-lab (URL, scope *) and mihomo-api-lab-b (API secret, scope lab-b);
#                        topology.sh adds the global subscription and lab-b's mihomo + mihomo-pkg (auto-download)
#
# The echo answer names who opened the flow: 198.51.100.10 = lab-ext itself (through the proxy), 10.50.0.1 = lab-b
# (Mihomo went DIRECT; the Incus host masquerades mnpwan traffic), 10.62.0.50 = host-b (not intercepted; only UDP
# gets an answer, the reply returns over the eBGP/OpenVPN path and the asymmetric TCP handshake fails).
#
#   tools/lab/mihomo.sh setup              create/refresh all of the above (idempotent); writes /root/lab/mihomo.json
#   tools/lab/mihomo.sh publish on|off     add/remove Mihomo in the live cluster document and publish it
#   tools/lab/mihomo.sh sub proxy|direct   serve the subscription that sends everything to lab-ext / DIRECT
#   tools/lab/mihomo.sh probe              host-b: TCP + UDP to the endpoint, prints "tcp=<peer> udp=<peer>"
#   tools/lab/mihomo.sh state              lab-b: feature status, nft table, policy rule, proxy group, connections
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
X=lab-ext
ECHO=198.51.100.10
STATE=$LAB/mihomo.json
lx() { c=$1; shift; incus exec "$c" -- "$@"; }

subscription() { # proxy|direct
	if [ "$1" = proxy ]; then rule=out; else rule=DIRECT; fi
	# Controlled keys on purpose: the node must replace them (ports, controller, tun) and report what it dropped.
	cat <<EOF
mixed-port: 1234
external-controller: 0.0.0.0:9999
tun:
  enable: true
proxies:
  - { name: lab-ext, type: socks5, server: 10.50.0.30, port: 1080, udp: true }
proxy-groups:
  - { name: out, type: select, proxies: [lab-ext, DIRECT] }
rules:
  - MATCH,$rule
EOF
}

case "${1:-}" in
setup)
	[ -s $STATE ] || jq -n --arg t "$(head -c 18 /dev/urandom | base64 | tr -d '/+=')" \
		--arg s "$(head -c 24 /dev/urandom | base64 | tr -d '/+=')" '{token: $t, apiSecret: $s}' >$STATE
	chmod 600 $STATE
	token=$(jq -r .token $STATE)
	# Mihomo for lab-ext: the release pinned for mihomo-pkg (D31), checked against its SHA-256.
	if ! lx $X test -x /usr/local/bin/mihomo; then
		a=$(jq -c '.versions[0].assets[] | select(.platform == "linux/amd64")' "$REPO/packages/meduza-np-mihomo-pkg/assets.json")
		w=$(mktemp -d)
		curl -fsSL -o "$w/m.gz" "$(echo "$a" | jq -r .url)"
		echo "$(echo "$a" | jq -r .sha256)  $w/m.gz" | sha256sum -c --quiet
		gunzip "$w/m.gz" && chmod 755 "$w/m"
		incus file push -q "$w/m" $X/usr/local/bin/mihomo
		rm -rf "$w"
	fi
	lx $X sh -c 'command -v python3 >/dev/null || { export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq python3 >/dev/null; }'
	lx $X mkdir -p /etc/lab-socks /srv/sub /usr/local/lib/lab
	incus file push -q "$REPO/tools/lab/lab-echo.py" $X/usr/local/lib/lab/lab-echo.py
	lx $X sh -c 'cat >/etc/lab-socks/config.yaml' <<'EOF'
mode: direct
log-level: info
ipv6: false
listeners:
  - { name: s5, type: socks, listen: 10.50.0.30, port: 1080, udp: true }
EOF
	lx $X sh -c "cat >/etc/systemd/system/lab-socks.service" <<'EOF'
[Unit]
Description=lab SOCKS5 server (mihomo)
After=network-online.target
[Service]
ExecStart=/usr/local/bin/mihomo -d /etc/lab-socks
Restart=always
[Install]
WantedBy=multi-user.target
EOF
	lx $X sh -c "cat >/etc/systemd/system/lab-echo.service" <<EOF
[Unit]
Description=lab internet endpoint $ECHO:8081
After=network-online.target
[Service]
ExecStartPre=-/usr/sbin/ip addr add $ECHO/32 dev lo
ExecStart=/usr/bin/python3 /usr/local/lib/lab/lab-echo.py $ECHO 8081
Restart=always
[Install]
WantedBy=multi-user.target
EOF
	lx $X sh -c "cat >/etc/systemd/system/lab-sub.service" <<'EOF'
[Unit]
Description=lab subscription server
After=network-online.target
[Service]
ExecStart=/usr/bin/python3 -m http.server --bind 10.50.0.30 --directory /srv/sub 8088
Restart=always
[Install]
WantedBy=multi-user.target
EOF
	subscription proxy | lx $X sh -c "cat >/srv/sub/sub-$token.yaml"
	lx $X sh -c 'systemctl daemon-reload && systemctl enable -q --now lab-socks lab-echo lab-sub && systemctl restart lab-socks lab-echo lab-sub'
	ip route replace $ECHO/32 via 10.50.0.30 dev mnpwan
	# Manager secrets: the subscription URL is shared with every node that uses it (D29), the API secret is lab-b's own.
	jq -n --arg v "http://10.50.0.30:8088/sub-$token.yaml" '{id: "mihomo-sub-lab", kind: "text", value: $v, nodeScope: ["*"], description: "lab subscription"}' >$LAB/mihomo-sub-secret.json
	jq -n --arg v "$(jq -r .apiSecret $STATE)" '{id: "mihomo-api-lab-b", kind: "text", value: $v, nodeScope: ["lab-b"], description: "lab-b Mihomo API"}' >$LAB/mihomo-api-secret.json
	$L api call POST /api/v1/secrets @$LAB/mihomo-sub-secret.json >/dev/null
	$L api call POST /api/v1/secrets @$LAB/mihomo-api-secret.json >/dev/null
	rm -f $LAB/mihomo-sub-secret.json $LAB/mihomo-api-secret.json
	i=0; until curl -fs "http://10.50.0.30:8088/sub-$token.yaml" >/dev/null; do i=$((i + 1)); [ $i -gt 20 ] && exit 1; sleep 1; done
	echo "lab-ext ready: socks 10.50.0.30:1080, subscription served, endpoint $ECHO:8081"
	;;
publish)
	# Patches the published document (not topology.sh: keys rotated in the lab live only there).
	$L api call GET /api/v1/config | jq '.document // .' >$LAB/mihomo-doc.json
	if [ "$2" = on ]; then
		jq '.mihomo = { subscriptions: [ { id: "lab", name: "lab-ext", urlRef: "mihomo-sub-lab" } ], default: "lab", refreshSeconds: 300 }
			| .nodes["lab-b"].services.mihomo = { mode: "tproxy", api: { secretRef: "mihomo-api-lab-b" } }
			| .nodes["lab-b"].services["mihomo-pkg"] = { mode: "auto-download", targetVersion: "v1.19.33" }' \
			$LAB/mihomo-doc.json >$LAB/mihomo-doc2.json
	else
		jq 'del(.mihomo) | del(.nodes["lab-b"].services.mihomo) | del(.nodes["lab-b"].services["mihomo-pkg"])' \
			$LAB/mihomo-doc.json >$LAB/mihomo-doc2.json
	fi
	$L api publish $LAB/mihomo-doc2.json "mihomo $2" | jq -c '{revision: (.revision // .id // .), nodes: (.nodes // empty)}' 2>/dev/null || true
	;;
sub)
	subscription "$2" | lx $X sh -c "cat >/srv/sub/sub-$(jq -r .token $STATE).yaml.new && mv /srv/sub/sub-$(jq -r .token $STATE).yaml.new /srv/sub/sub-$(jq -r .token $STATE).yaml"
	echo "subscription: $2"
	;;
probe)
	t=$(lx host-b sh -c "nc -w 3 $ECHO 8081 </dev/null 2>/dev/null" | tr -d '\r\n' || true)
	u=$(lx host-b sh -c "echo x | nc -u -w 3 $ECHO 8081 2>/dev/null" | head -1 | tr -d '\r\n' || true)
	echo "tcp=${t:-none} udp=${u:-none}"
	;;
state)
	p() { lx lab-b podman exec meduza-np-planner "$@"; }
	auth="Authorization: Bearer $(jq -r .apiSecret $STATE)"
	p meduza-np --json status | jq -c '[.features[] | select(.id | test("mihomo")) | {id, enabled, healthy, running}]'
	p nft list table inet meduza_np_mihomo 2>&1 | grep -E 'elements|tproxy|@' || echo "no nft table"
	p ip rule | grep -E 'fwmark' || echo "no fwmark rule"
	p ip route show table all | grep -E '^local default' || echo "no local route"
	p curl -fsS -H "$auth" http://127.0.0.1:9090/proxies/out | jq -c '{now, all}' 2>/dev/null || echo "API not reachable"
	p curl -fsS -H "$auth" http://127.0.0.1:9090/connections | jq -c '[.connections[]? | {net: .metadata.network, dst: .metadata.destinationIP, src: .metadata.sourceIP, chains, rule}] | .[:4]' 2>/dev/null || true
	;;
*)
	sed -n '2,20p' "$0"
	exit 2
	;;
esac
