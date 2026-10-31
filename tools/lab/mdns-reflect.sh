#!/bin/sh
# mDNS reflector and IPv6 in the lab (AGENT §9.11, D10, D50): lab-b (host network) runs managed avahi on eth1 (lanb,
# host-b's LAN) and eth0 (mnpwan, where lab-mgr lives). With the reflector on, a multicast query from lab-mgr reaches
# host-b's avahi-daemon through lab-b and host-b's answer comes back onto mnpwan. lab-mgr runs no avahi; `probe` is a
# raw mDNS querier (python3, bound to 5353, joined to the group on eth0) that prints the A/AAAA records it receives
# and the sender. Builds on mdns.sh (host-b setup, bridge + mdnsForward stay as published there).
#
#   tools/lab/mdns-reflect.sh publish off|lan|reflect|reflect-ipv   eth1 only / eth0+eth1 without reflector /
#                                                                   reflector / reflector + reflect-ipv; IPv6 on
#   tools/lab/mdns-reflect.sh probe [NAME] [4|6] [A|AAAA]           lab-mgr: one query on eth0, answers for 3 s
#                                                                   (PROBE_HOST/PROBE_IF override lab-mgr/eth0)
#   tools/lab/mdns-reflect.sh conf                                  lab-b: rendered avahi-daemon.conf (server/reflector)
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
p() { lx lab-b podman exec meduza-np-planner "$@"; }

case "${1:-}" in
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$LAB/mdnsr-doc.json
	base='{ mode: "managed", bridge: { enabled: true }, useIpv6: true }'
	case "$2" in
	off) f=". + { allowInterfaces: [\"eth1\"], useIpv6: false }" ;;
	lan) f=". + { allowInterfaces: [\"eth1\", \"eth0\"] }" ;;
	reflect) f=". + { allowInterfaces: [\"eth1\", \"eth0\"], reflector: { enabled: true } }" ;;
	reflect-ipv) f=". + { allowInterfaces: [\"eth1\", \"eth0\"], reflector: { enabled: true, reflectIpv: true } }" ;;
	*) echo "publish off|lan|reflect|reflect-ipv" >&2; exit 2 ;;
	esac
	jq ".nodes[\"lab-b\"].services.avahi = ($base | $f) | .nodes[\"lab-b\"].services.dnsmasq.mdnsForward = true" \
		$LAB/mdnsr-doc.json >$LAB/mdnsr-doc2.json
	$L api publish $LAB/mdnsr-doc2.json "mdns reflector $2" | jq -c '{revision: (.revision // .id // .)}' 2>/dev/null || true
	;;
probe)
	lx "${PROBE_HOST:-lab-mgr}" env IF="${PROBE_IF:-eth0}" python3 - "${2:-host-b.local}" "${3:-4}" "${4:-A}" <<'EOF'
import socket, struct, sys, time
name, fam, qt = sys.argv[1], sys.argv[2], sys.argv[3]
QT = {"A": 1, "AAAA": 28}[qt]
IF = __import__("os").environ["IF"]
q = struct.pack(">6H", 0, 0, 1, 0, 0, 0) + b"".join(bytes([len(l)]) + l.encode() for l in name.split(".")) + b"\0"
q += struct.pack(">HH", QT, 1)
if fam == "4":
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BINDTODEVICE, IF.encode())
    s.bind(("", 5353))
    ip = [l.split()[3].split("/")[0] for l in __import__("subprocess").check_output(["ip", "-4", "-o", "addr", "show", IF]).decode().splitlines()][0]
    s.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, socket.inet_aton("224.0.0.251") + socket.inet_aton(ip))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_IF, socket.inet_aton(ip))
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, 255)
    s.setsockopt(socket.IPPROTO_IP, socket.IP_MULTICAST_LOOP, 0)
    dst = ("224.0.0.251", 5353)
else:
    idx = socket.if_nametoindex(IF)
    s = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    s.bind(("::", 5353, 0, 0))
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_JOIN_GROUP, socket.inet_pton(socket.AF_INET6, "ff02::fb") + struct.pack("@I", idx))
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_IF, idx)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_HOPS, 255)
    s.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_MULTICAST_LOOP, 0)
    dst = ("ff02::fb", 5353, 0, idx)

def rname(b, o):
    labels, jumped, end = [], False, o
    while True:
        n = b[o]
        if n & 0xC0 == 0xC0:
            if not jumped:
                end = o + 2
            o, jumped = ((n & 0x3F) << 8) | b[o + 1], True
            continue
        if n == 0:
            return ".".join(labels), (end if jumped else o + 1)
        labels.append(b[o + 1:o + 1 + n].decode(errors="replace"))
        o += 1 + n

s.sendto(q, dst)
s.settimeout(0.2)
seen, until = set(), time.time() + 3
while time.time() < until:
    try:
        b, src = s.recvfrom(9000)
    except socket.timeout:
        continue
    _, flags, qd, an, ns, ar = struct.unpack(">6H", b[:12])
    if not flags & 0x8000:
        continue
    o = 12
    for _ in range(qd):
        _, o = rname(b, o)
        o += 4
    for _ in range(an + ns + ar):
        n, o = rname(b, o)
        t, _, _, ln = struct.unpack(">HHIH", b[o:o + 10])
        d = b[o + 10:o + 10 + ln]
        o += 10 + ln
        if n.lower() != name.lower() or t not in (1, 28):
            continue
        a = socket.inet_ntop(socket.AF_INET if t == 1 else socket.AF_INET6, d)
        k = ("A" if t == 1 else "AAAA", a, src[0])
        if k not in seen:
            seen.add(k)
            print("%s %s from %s" % k)
if not seen:
    print("no answer")
EOF
	;;
conf)
	p sh -c 'f=$(ls -t /run/meduza-np/generations/*/avahi/main/avahi-daemon.conf | head -1); echo "# $f"
		sed -n "/^\[server\]/,/^\[wide-area\]/p;/^\[reflector\]/,/^$/p" "$f"'
	;;
*)
	sed -n '2,12p' "$0"
	exit 2
	;;
esac
