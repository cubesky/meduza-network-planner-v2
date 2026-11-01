#!/bin/sh
# DNS front end in the lab (AGENT §9.9, §9.10, §10.5): lab-b runs dnsmasq as the client-facing resolver of its LAN
# (10.62.0.12:53) in front of MosDNS (127.0.0.1:1153) and Mihomo DNS (127.0.0.1:1053), with an explicit fallback.
# lab-ext serves three marker resolvers, so every answer for who.lab.test names the path that produced it:
#
#   10.50.0.30:53    -> 192.0.2.1   MosDNS's upstream          (answer 192.0.2.1 = via MosDNS)
#   10.50.0.30:5353  -> 192.0.2.2   the front end's fallback   (answer 192.0.2.2 = fallback active)
#   10.50.0.30:5354  -> 192.0.2.3   Mihomo DNS's nameserver    (answer 192.0.2.3 = via Mihomo DNS)
#
# probe.lab.test (the front end's probe name) and blocked.lab.test resolve on all three, every other name is NXDOMAIN
# (so Mihomo's own probe of example.com gets a valid answer). nas.lab-b.test comes from the node's dns.hosts
# (10.62.0.50). host-b queries the front end; lab-b's systemd-resolved (127.0.0.53:53) and aardvark-dns
# (10.89.0.1:53) keep running next to it. MosDNS comes from mosdns-pkg: build the lab release with
# TREES="meduza-np-mosdns-pkg=<recipe output>" (tools/lab/next-release.sh).
#
#   tools/lab/dns.sh setup                         lab-ext marker resolvers (idempotent)
#   tools/lab/dns.sh publish fallback|mosdns|mihomo|both|off
#                                                  front end with no backend / one / both (MosDNS and Mihomo DNS
#                                                  run in every variant); off removes dnsmasq, MosDNS, mosdns-pkg
#                                                  and turns Mihomo DNS off
#   tools/lab/dns.sh q [NAME]                      host-b: one query to the front end, prints the answer or "fail"
#   tools/lab/dns.sh watch SECONDS [NAME]          host-b: query every 0.2 s, print each change of answer with the
#                                                  time since start, then the counts per answer (fail = no answer)
#   tools/lab/dns.sh freeze|thaw mosdns|mihomo     SIGSTOP/SIGCONT the backend process (a hang, not a crash)
#   tools/lab/dns.sh upstream stop|start 53|5353|5354   stop/start one lab-ext marker resolver
#   tools/lab/dns.sh state                         lab-b: front-end state file, DNS feature status, port 53 owners
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
X=lab-ext
FRONT=10.62.0.12
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
p() { lx lab-b podman exec meduza-np-planner "$@"; }

query() { # NAME -> answer address or "fail"
	a=$(lx host-b sh -c "timeout 3 nslookup -type=a $1 $FRONT 2>/dev/null" | awk '/^Name:/ { n = 1 } n && /^Address/ { print $NF; exit }' || true)
	case "$1" in blocked.*) [ -z "$a" ] && lx host-b sh -c "timeout 3 nslookup -type=a $1 $FRONT 2>&1" | grep -q NXDOMAIN && a=NXDOMAIN ;; esac
	echo "${a:-fail}"
}

case "${1:-}" in
setup)
	lx $X sh -c 'command -v dnsmasq >/dev/null || { export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq dnsmasq-base >/dev/null; }'
	lx $X sh -c 'cat >/etc/systemd/system/lab-dns@.service' <<'EOF'
[Unit]
Description=lab marker resolver on 10.50.0.30:%i
After=network-online.target
[Service]
ExecStart=/bin/sh -c 'case %i in 53) n=1 ;; 5353) n=2 ;; *) n=3 ;; esac; exec /usr/sbin/dnsmasq -k --conf-file=/dev/null --user=nobody --pid-file= --port=%i --listen-address=10.50.0.30 --bind-interfaces --no-resolv --no-hosts --log-facility=- --address=/who.lab.test/192.0.2.$n --address=/probe.lab.test/192.0.2.$n --address=/blocked.lab.test/192.0.2.$n --address=/#/'
Restart=always
[Install]
WantedBy=multi-user.target
EOF
	lx $X sh -c 'systemctl daemon-reload && for p in 53 5353 5354; do systemctl enable -q lab-dns@$p && systemctl restart lab-dns@$p; done'
	sleep 1
	for port in 53 5353 5354; do
		printf '10.50.0.30:%s who.lab.test -> %s\n' $port "$(lx host-b sh -c "nslookup -port=$port who.lab.test 10.50.0.30" 2>/dev/null | awk '/^Name:/ { n = 1 } n && /^Address/ { print $NF; exit }')"
	done
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$LAB/dns-doc.json
	front='{ listenAddresses: ["10.62.0.12"], port: 53, fallbackServers: ["10.50.0.30#5353"], probeName: "probe.lab.test", strategy: "ordered" }'
	case "$2" in
	fallback) use_mosdns=false; use_mihomo=false ;;
	mosdns) use_mosdns=true; use_mihomo=false ;;
	mihomo) use_mosdns=false; use_mihomo=true ;;
	both) use_mosdns=true; use_mihomo=true ;;
	off) use_mosdns=false; use_mihomo=false ;;
	*) echo "publish fallback|mosdns|mihomo|both|off" >&2; exit 2 ;;
	esac
	if [ "$2" != off ]; then
		# MosDNS runs from mosdns-pkg (the pinned static build); it blocks blocked.lab.test with NXDOMAIN.
		jq ".nodes[\"lab-b\"].services.dnsmasq = ($front)
			| .nodes[\"lab-b\"].dns = { hosts: [ { hostname: \"nas.lab-b.test\", ips: [\"10.62.0.50\"] } ] }
			| .nodes[\"lab-b\"].services.mosdns = { enabled: $use_mosdns, listen: \"127.0.0.1\", port: 1153, localUpstreams: [\"udp://10.50.0.30:53\"],
				probeName: \"probe.lab.test\", domains: { block: [\"full:blocked.lab.test\"] } }
			| .nodes[\"lab-b\"].services[\"mosdns-pkg\"] = {}
			| .nodes[\"lab-b\"].services.mihomo.dns = { enabled: $use_mihomo, listen: \"127.0.0.1:1053\",
				nameservers: [\"udp://10.50.0.30:5354\"], defaultNameservers: [\"10.50.0.30\"] }" \
			$LAB/dns-doc.json >$LAB/dns-doc2.json
	else
		jq 'del(.nodes["lab-b"].services.dnsmasq) | del(.nodes["lab-b"].services.mosdns) | del(.nodes["lab-b"].services["mosdns-pkg"])
			| del(.nodes["lab-b"].dns) | (if .nodes["lab-b"].services.mihomo then .nodes["lab-b"].services.mihomo.dns.enabled = false else . end)' \
			$LAB/dns-doc.json >$LAB/dns-doc2.json
	fi
	$L api publish $LAB/dns-doc2.json "dns $2" | jq -c '{revision: (.revision // .id // .), nodes: (.nodes // empty)}' 2>/dev/null || true
	;;
q)
	query "${2:-who.lab.test}"
	;;
watch)
	end=$(($(date +%s) + $2)) name=${3:-who.lab.test} start=$(date +%s.%N) last="" counts=""
	while [ "$(date +%s)" -lt $end ]; do
		a=$(query "$name")
		if [ "$a" != "$last" ]; then
			printf '%6.1fs %s\n' "$(awk -v a="$(date +%s.%N)" -v b="$start" 'BEGIN { print a - b }')" "$a"
			last=$a
		fi
		counts="$counts$a\n"
		sleep 0.2
	done
	printf "$counts" | sort | uniq -c | awk '{ printf "%s=%s ", $2, $1 } END { print "" }'
	;;
freeze | thaw)
	sig=STOP; [ "$1" = thaw ] && sig=CONT
	# the provider binary (.../providers/<name>/<name>), not its plugin wrapper (plugins/<name> run ..., same comm)
	pid=$(p ps -o pid,args | awk -v b="/providers/$2/$2" 'index($2, b) && substr($2, length($2) - length(b) + 1) == b { print $1; exit }')
	[ -n "$pid" ] || { echo "no $2 process" >&2; exit 1; }
	p kill -$sig "$pid"
	echo "$1 $2 (pid $pid)"
	;;
upstream)
	lx $X systemctl "$2" "lab-dns@$3"
	echo "lab-ext :$3 $2"
	;;
state)
	p sh -c 'cat /run/meduza-np/dnsmasq-main.state 2>/dev/null || cat /var/run/meduza-np/dnsmasq-main.state 2>/dev/null' | jq -c '{fallbackActive, fallbackVerified, upstreams, backends: (.backends | map_values(.healthy))}' 2>/dev/null || echo "no front-end state"
	p meduza-np --json status | jq -c '[.features[] | select(.id | test("dns|mosdns")) | {id, enabled, healthy}]'
	p meduza-np --json status | jq -c '[.instances[]? | select(.kind | test("dns")) | {id, state, reason}]' 2>/dev/null || true
	lx lab-b ss -lunp | awk '$4 ~ /:53$/ { print "port 53:", $4, $NF }'
	;;
*)
	sed -n '2,26p' "$0"
	exit 2
	;;
esac
