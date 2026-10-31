#!/bin/sh
# Anycast scenario for the lab topology (topology.sh): service "web" (10.255.0.0/30, TCP 8080) on lab-a and lab-c,
# OSPF over the Tinc mesh on lab-a/b/c. Starts a tiny HTTP server per node that answers its node name, then checks
#   1. clients reach their nearest instance (host-a → lab-a, host-c → lab-c, host-b → either),
#   2. OSPF installs only pool routes (no Tinc subnet etc.),
#   3. a crashed service is withdrawn (guard: 3 failed checks) and even its own LAN is then served by the other node,
#   4. it is re-announced after recovery,
#   5. a frozen node's instance disappears from the others (OSPF + BFD over Tinc).
set -eu
VIP=10.255.0.1
PREFIX=10.255.0.0/30

now() { date +%s.%N; }
since() { echo "$(now) $1" | awk '{printf "%.1f", $1 - $2}'; }
web_start() { incus exec "$1" -- sh -c "mkdir -p /srv/web && echo $1 >/srv/web/index.html && systemctl stop lab-web 2>/dev/null; systemd-run -q --unit=lab-web python3 -m http.server 8080 --directory /srv/web"; }
web_stop() { incus exec "$1" -- systemctl stop lab-web; }
ask() { incus exec "$1" -- wget -q -T 2 -O - "http://$VIP:8080/" 2>/dev/null || echo "(no answer)"; }
# wait until CMD (run on the VPS) succeeds; prints the elapsed seconds since START
wait_for() { start=$1; shift; i=0; until sh -c "$*" >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 600 ] && { echo timeout; return 1; }; sleep 0.1; done; since "$start"; }

web_start lab-a
web_start lab-c
echo "waiting for both instances to be announced..."
wait_for "$(now)" "incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.1 && incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.4" >/dev/null
echo "1. nearest instance"
echo "  lab-b route: $(incus exec lab-b -- ip -4 route show $PREFIX | tr '\n' ' ')"
for h in host-a host-b host-c; do echo "  $h -> $(ask $h)"; done

echo "2. OSPF routes in lab-b's kernel (only the pool expected):"
incus exec lab-b -- ip -4 route show proto ospf | sed 's/^/  /'

echo "3. web on lab-a crashes"
t=$(now)
web_stop lab-a
echo "  withdrawn on lab-b after $(wait_for "$t" "! incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.1") s"
echo "  any-web on lab-a: $(incus exec lab-a -- ip -br addr show any-web | tr -s ' ')"
for h in host-a host-b; do echo "  $h -> $(ask $h)"; done

echo "4. web on lab-a recovers"
t=$(now)
web_start lab-a
echo "  re-announced on lab-b after $(wait_for "$t" "incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.1") s"
echo "  host-a -> $(ask host-a)"

echo "5. lab-c freezes"
t=$(now)
incus pause lab-c
echo "  lab-b drops lab-c's instance after $(wait_for "$t" "! incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.4") s"
echo "  host-b -> $(ask host-b)"
incus start lab-c 2>/dev/null || incus resume lab-c
t=$(now)
echo "  back after resume: $(wait_for "$t" "incus exec lab-b -- ip -4 route show $PREFIX proto ospf | grep -q 10.42.0.4") s"
