#!/bin/sh
# Blank-volume node with the avahi component (AGENT §9.11, §14.3.4, D48): lab-av is a new LXC on lab-b's LAN
# (eth0 10.50.0.16 on mnpwan, eth1 10.62.0.21 on lanb, host network mode). Its node volume is empty, node.env selects
# `avahi,cli`, so the updater has to install meduza-np-avahi (and its Alpine deps: dbus, avahi) at bootstrap, before
# any revision exists; run.sh adds SYS_CHROOT for the component, so avahi-daemon keeps its chroot. host-b (10.62.0.50,
# own avahi-daemon, `mdns.sh setup`) and lab-b (managed avahi since D45) announce on the same LAN.
#
#   tools/lab/mdns-fresh.sh setup SITE    create LXC lab-av, copy the base image from lab-b (D48 build) and SITE
#   tools/lab/mdns-fresh.sh deploy        manager node, wipe every meduza-np volume on lab-av, enroll; prints when the
#                                         node enrolled and when meduza-np-avahi was installed (s after run.sh)
#   tools/lab/mdns-fresh.sh publish on|off   avahi managed + bridge on eth1 for lab-av (on also adds the node)
#   tools/lab/mdns-fresh.sh check         installed packages, avahi instances, bus/daemon processes, chroot, static
#                                         service files, lab-av.local from host-b, host-b.local via lab-av's bridge
#   tools/lab/mdns-fresh.sh remove        delete lab-av (LXC + manager node stays; publish off first)
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/mdns-fresh
N=lab-av
IMAGE=localhost/meduza-np-base-container:e2e
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
pl() { lx $N podman exec meduza-np-planner "$@"; }
avahi_installed() { pl meduza-np --json update status 2>/dev/null | jq -e '.installed.packages | has("meduza-np-avahi")' >/dev/null 2>&1; }
mkdir -p $S

case "${1:-}" in
setup)
	site=$2
	if ! incus info $N >/dev/null 2>&1; then
		incus init -q deb13-podman $N -p default -p mnplab
		incus config device add $N eth0 nic network=mnpwan ipv4.address=10.50.0.16 >/dev/null
		incus config device add $N eth1 nic network=lanb ipv4.address=10.62.0.21 >/dev/null
		incus start $N
	fi
	i=0; until lx $N ip -4 route show default 2>/dev/null | grep -q via; do i=$((i + 1)); [ $i -gt 60 ] && exit 1; sleep 1; done
	# eth1 static like lab-b's (lanb's DHCP hands out lab-b as gateway; the default route stays on eth0)
	lx $N sh -c 'printf "[Match]\nName=eth1\n\n[Network]\nAddress=10.62.0.21/24\nDHCP=no\n" >/etc/systemd/network/20-eth1.network && networkctl reload'
	# the D48 base image (no Alpine sample services) exists only on lab-b
	lx lab-b podman save -q -o /root/base-d48.tar $IMAGE
	incus file pull -q lab-b/root/base-d48.tar $S/base.tar
	lx lab-b rm -f /root/base-d48.tar
	incus file push -q $S/base.tar $N/root/base.tar
	lx $N podman load -q -i /root/base.tar
	lx $N rm -f /root/base.tar
	incus file push -q -r "$REPO/packages/meduza-np-base-container/deploy/podman" $N/root/
	lx $N sh -c 'sed -i "s/\r$//" /root/podman/run.sh && chmod +x /root/podman/run.sh'
	lx $N sh -c 'mkdir -p /srv/import /srv/incoming && rm -rf /srv/incoming/*'
	incus file push -q -r "$site" $N/srv/incoming/
	lx $N sh -c "find /srv/import -mindepth 1 -delete && cp -a /srv/incoming/$(basename "$site")/. /srv/import/"
	lx $N ip -br addr
	;;
deploy)
	$L api node $N static >/dev/null 2>&1 || true
	lx $N sh -c '[ -x /root/podman/run.sh ] && /root/podman/run.sh --stop >/dev/null 2>&1 || true
		for v in meduza-np-node meduza-np-store meduza-np-ipc; do podman volume rm -f $v >/dev/null 2>&1 || true; done
		podman volume ls -q | grep meduza || echo "no meduza-np volumes left"'
	t0=$(date +%s)
	$L node $N avahi,cli host "$($L api token $N)"
	enrolled="" installed=""
	i=0
	while [ $i -lt 120 ]; do
		[ -z "$enrolled" ] && pl meduza-np --json status 2>/dev/null | jq -e '.nodeId | select(. != null and . != "")' >/dev/null 2>&1 &&
			enrolled=$(($(date +%s) - t0)) && echo "enrolled after $enrolled s"
		[ -z "$installed" ] && avahi_installed && installed=$(($(date +%s) - t0)) && echo "meduza-np-avahi installed after $installed s"
		[ -n "$installed" ] && [ -n "$enrolled" ] && break
		i=$((i + 1)); sleep 5
	done
	[ -n "$installed" ] || { echo "meduza-np-avahi NOT installed after 600 s"; lx $N podman logs --tail 30 meduza-np-updater; exit 1; }
	pl meduza-np --json update status | jq -c '{groupVersion: .installed.groupVersion, generation: .installed.generation, packages: (.installed.packages | keys)}'
	lx $N podman inspect meduza-np-planner --format 'planner caps: {{.EffectiveCaps}}'
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	case "$2" in
	on) jq '.nodes["lab-av"] = ((.nodes["lab-av"] // { behavior: "static" })
		| .services.avahi = { mode: "managed", allowInterfaces: ["eth1"], bridge: { enabled: true } })' $S/doc.json >$S/doc2.json ;;
	off) jq 'del(.nodes["lab-av"])' $S/doc.json >$S/doc2.json ;;
	*) echo "publish on|off" >&2; exit 2 ;;
	esac
	$L api call POST /api/v1/validate "{\"document\": $(cat $S/doc2.json)}" |
		jq -r '(.problems // .)[]? | "\(.severity) \(.path): \(.message)"'
	$L api publish $S/doc2.json "lab-av avahi $2" | jq -c '{revision: (.revision // .id // .)}' 2>/dev/null || true
	;;
check)
	echo "== packages"
	pl meduza-np --json update status | jq -c '{groupVersion: .installed.groupVersion, packages: (.installed.packages | keys)}'
	echo "== status"
	pl meduza-np --json status | jq -c '{applied: .sync.appliedRevision, stage: .sync.applyStage, err: .sync.lastError, instances: [.instances[]? | {id, state, reason}]}'
	echo "== processes"
	pl sh -c 'for d in /proc/[0-9]*; do
		c=$(tr "\0" " " <$d/cmdline 2>/dev/null | cut -c1-150); case "$c" in *avahi*|*dbus-daemon*) ;; *) continue ;; esac
		case "$c" in "sh -c"*) continue ;; esac
		printf "%6s uid=%-5s eff=%s  %s\n" "${d#/proc/}" "$(awk "/^Uid:/{print \$2}" $d/status)" "$(awk "/^CapEff:/{print \$2}" $d/status)" "$c"
	done'
	lx $N podman logs meduza-np-planner 2>&1 | grep -i 'chroot()\|service file\|Server startup complete' | tail -4
	pl sh -c 'ls /etc/avahi/services/ 2>/dev/null | sed "s/^/static service file: /"; true'
	echo "== host-b resolves lab-av.local (expect 10.62.0.21)"
	lx host-b sh -c 'timeout 8 avahi-resolve -4 -n lab-av.local 2>&1 || true'
	echo "== lab-av's bridge resolves host-b.local and lab-b.local"
	for n in host-b.local lab-b.local nosuch-host.local; do
		printf '%s: ' $n
		pl sh -c "timeout 8 nslookup -type=a -port=5354 $n 127.0.0.1 2>&1" | awk '/^Name:/{n=1} n&&/^Address/{print $NF; f=1; exit} /NXDOMAIN/{print "NXDOMAIN"; f=1; exit} END{if(!f)print "fail"}'
	done
	echo "== services lab-av announces (host-b, fresh cache)"
	lx host-b sh -c 'rc-service avahi-daemon restart >/dev/null 2>&1; sleep 3; timeout 6 avahi-browse -art 2>/dev/null | grep -i "lab-av" | sort -u; true'
	;;
remove)
	incus delete -f $N
	;;
*)
	sed -n '2,16p' "$0"
	exit 2
	;;
esac
