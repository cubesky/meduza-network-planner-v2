#!/bin/sh
# Minimal planner capability set in the lab (D44, AGENT §14.3.4): re-create a node's containers with the current
# run.sh (same network mode and published ports) and check what the services run with.
#
#   tools/lab/caps.sh recreate NODE     push deploy/podman, re-run run.sh like the node was deployed, wait until every
#                                       instance the node reported before is up again (prints the time it took)
#   tools/lab/caps.sh check NODE        container + planner capabilities, managed processes with uid and CapEff,
#                                       instances that are not up
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
IMAGE=localhost/meduza-np-base-container:e2e
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
pl() { lx "$N" podman exec meduza-np-planner "$@"; }
instances() { pl meduza-np --json status 2>/dev/null | jq -r '.instances[] | "\(.id) \(.state)"' | sort; }

check() {
	echo "container: $(lx "$N" podman inspect meduza-np-planner --format '{{.EffectiveCaps}}' 2>/dev/null)"
	echo "planner reports: $(pl meduza-np --json status 2>/dev/null | jq -c '.system.capabilities // "n/a"' 2>/dev/null || echo n/a)"
	# every process but the container's tools: uid and effective capabilities (non-root daemons hold none or their own)
	pl sh -c 'for d in /proc/[0-9]*; do
		c=$(tr "\0" " " <$d/cmdline 2>/dev/null | cut -c1-60); [ -n "$c" ] || continue
		case "$c" in "sh -c"*|sleep*|"meduza-np --json"*) continue ;; esac
		printf "%6s uid=%-5s eff=%s  %s\n" "${d#/proc/}" "$(awk "/^Uid:/{print \$2}" $d/status)" "$(awk "/^CapEff:/{print \$2}" $d/status)" "$c"
	done' 2>/dev/null
	down=$(instances | awk '$2 != "up" && $2 != "running"')
	if [ -n "$down" ]; then echo "not up:"; echo "$down"; else echo "all $(instances | wc -l) instances up"; fi
}

N=${2:-}
[ -n "$N" ] || { sed -n '2,9p' "$0"; exit 2; }
case "${1:-}" in
check) check ;;
recreate)
	before=$(instances | awk '$2 == "up" || $2 == "running" { print $1 }')
	[ -n "$before" ] || { echo "$N reports no running instances; nothing to compare" >&2; exit 1; }
	net=$(lx "$N" podman inspect meduza-np-planner --format '{{.HostConfig.NetworkMode}}')
	[ "$net" = host ] || net=bridge
	pub=$(lx "$N" podman inspect meduza-np-planner --format '{{json .HostConfig.PortBindings}}' |
		jq -r 'to_entries[]? | "--publish \(.value[0].HostPort):\(.key)"' | tr '\n' ' ')
	incus file push -r "$REPO/packages/meduza-np-base-container/deploy/podman" "$N/root/"
	lx "$N" sh -c 'sed -i "s/\r$//" /root/podman/run.sh && chmod +x /root/podman/run.sh'
	t0=$(date +%s)
	# shellcheck disable=SC2086
	lx "$N" /root/podman/run.sh --env-file /etc/meduza-np/node.env --image $IMAGE --network "$net" $pub --import /srv/import
	i=0
	while :; do
		now=$(instances | awk '$2 == "up" || $2 == "running" { print $1 }')
		missing=$(echo "$before" | while read -r id; do echo "$now" | grep -qx "$id" || echo "$id"; done)
		[ -z "$missing" ] && break
		i=$((i + 1))
		[ $i -gt 120 ] && { echo "after 600 s still not up:"; echo "$missing"; check; exit 1; }
		sleep 5
	done
	echo "$N: all $(echo "$before" | wc -l) instances up again $(($(date +%s) - t0)) s after re-creation"
	check
	;;
*) sed -n '2,9p' "$0"; exit 2 ;;
esac
