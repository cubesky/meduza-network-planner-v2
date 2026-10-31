#!/bin/sh
# Rolls a release out to NODE while FROM pings TARGET across the tunnel; reports the outcome and every ping gap.
#   tools/lab/upgrade-under-ping.sh SITE VERSION NODE FROM TARGET
set -eu
site=$1 version=$2 node=$3 from=$4 target=$5
cd "$(dirname "$0")/../.."
tools/lab/lab.sh site "$site" 2>&1 | tr '\r' '\n' | grep -v Pushing | tail -1 || true
incus exec "$from" -- sh -c "ping -D -i 0.2 -c 350 -W1 $target > /tmp/upg.log 2>&1 &"
sleep 2
incus exec "$node" -- podman exec meduza-np-planner meduza-np update apply >/dev/null
ph=""
for i in $(seq 1 60); do
	ph=$(incus exec "$node" -- podman exec meduza-np-planner meduza-np --json update status 2>/dev/null | jq -r '.lastJournal.groupVersion + " " + .lastJournal.phase')
	[ "$ph" = "$version done" ] && break
	sleep 2
done
echo "$node: $ph"
sleep 75
incus exec "$from" -- tail -2 /tmp/upg.log
incus exec "$from" -- sh -c "grep -o 'seq=[0-9]*' /tmp/upg.log | cut -d= -f2" | awk 'NR>1 && $1!=p+1 {printf "gap: seq %d -> %d (%.1f s)\n", p, $1, ($1-p-1)*0.2} {p=$1}'
incus exec "$node" -- wg show wg-wg0 listen-port
