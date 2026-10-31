#!/bin/sh
# Route withdrawal/recovery measurement. Freezes a router (incus pause: no FIN, no link down — like a hang, so only
# BFD/hold timers can notice) or, with MODE=poweroff, kills it (incus stop --force) and cold-boots it; watches when
# PREFIX disappears from the observers' kernel tables and when it comes back, and pings across an affected and an
# unaffected path meanwhile. RECOVER_WAIT (default 50 s, 150 for poweroff) bounds the wait after recovery.
#
#   tools/lab/withdrawal.sh NODE PREFIX DOWN_SECONDS "OBSERVER..." AFFECTED_FROM AFFECTED_TO UNAFFECTED_FROM UNAFFECTED_TO
# e.g. tools/lab/withdrawal.sh lab-b 10.62.0.0/24 20 "lab-a lab-c" host-a 10.62.0.50 host-a 10.63.0.50
set -eu
node=$1 prefix=$2 down=$3 observers=$4 af=$5 at=$6 uf=$7 ut=$8
mode=${MODE:-pause}
[ "$mode" = poweroff ] && wait_default=150 || wait_default=50
recover=${RECOVER_WAIT:-$wait_default}

now() { date +%s.%N; }
for o in $observers; do
	# sample the route every 100 ms: "<epoch> present|absent"
	incus exec "$o" -- sh -c "rm -f /tmp/route.log; for i in \$(seq 1 $(((down + recover + 15) * 10))); do
		if ip -4 route show $prefix proto bgp | grep -q .; then s=present; else s=absent; fi
		echo \"\$(date +%s.%N) \$s\" >> /tmp/route.log; sleep 0.1; done" >/dev/null 2>&1 &
done
pings=$(((down + recover + 10) * 5))
incus exec "$af" -- sh -c "ping -i 0.2 -c $pings -W1 $at > /tmp/aff.log 2>&1 &"
incus exec "$uf" -- sh -c "ping -i 0.2 -c $pings -W1 $ut > /tmp/unaff.log 2>&1 &"
sleep 5

t_down=$(now)
if [ "$mode" = poweroff ]; then incus stop --force "$node"; else incus pause "$node"; fi
sleep "$down"
t_up=$(now)
incus start "$node" 2>/dev/null || incus resume "$node" 2>/dev/null || true
sleep "$recover"
# stop the samplers and pings now: left running they would write into the next run's logs
for o in $observers; do incus exec "$o" -- pkill -f /tmp/route.log || true; done
incus exec "$af" -- pkill ping || true
incus exec "$uf" -- pkill ping || true
sleep 1

echo "node $node down ($mode) for ${down}s; prefix $prefix"
for o in $observers; do
	incus exec "$o" -- cat /tmp/route.log | awk -v d="$t_down" -v u="$t_up" -v o="$o" '
		$1 >= d && !gone && $2 == "absent" { gone = $1; printf "  %s: withdrawn %.2f s after the failure\n", o, gone - d }
		$1 >= u && gone && !back && $2 == "present" { back = $1; printf "  %s: back %.2f s after recovery\n", o, back - u }
		END { if (!gone) printf "  %s: NOT withdrawn\n", o; else if (!back) printf "  %s: NOT back\n", o }'
done
# busybox ping prints "seq=N", iputils "icmp_seq=N"; a jump in N is a run of lost replies
gaps() {
	awk -F'seq=' '/seq=/ {split($2, a, " "); s = a[1]; if (p != "" && s != p + 1) printf "    gap %d -> %d (%.1f s lost)\n", p, s, (s - p - 1) * 0.2; p = s}
		END { if (p == "") print "    no replies at all" }'
}
echo "  affected path $af -> $at:"
incus exec "$af" -- cat /tmp/aff.log | gaps
echo "  unaffected path $uf -> $ut:"
incus exec "$uf" -- cat /tmp/unaff.log | gaps
incus exec "$uf" -- cat /tmp/unaff.log | tail -2 | head -1
