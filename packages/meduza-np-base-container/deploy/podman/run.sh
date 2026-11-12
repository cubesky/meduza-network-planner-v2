#!/bin/sh
# Equivalent of the Quadlet files for hosts without systemd (§14.3.4). Idempotent: re-running replaces the two
# containers but keeps the volumes (identity, config, installed set, journal, provider data).
#
#   ./run.sh [--env-file FILE] [--image REF] [--network bridge|host|macvlan] [--publish PORT/PROTO]... [--import DIR]
#   ./run.sh --network macvlan --macvlan-network NAME --ip PLANNER_IP --updater-ip UPDATER_IP [other options]
#   ./run.sh --network macvlan --macvlan-network STATIC_NAME --ip PLANNER_IP \
#            --updater-macvlan-network DHCP_NAME --updater-ip dhcp [other options]
#            (--publish PORT/PROTO uses the same host port; HOST:PORT/PROTO or IP:HOST:PORT/PROTO are passed as given)
#   ./run.sh --again [--image REF]   repeat the last deployment's options (base-image.sh uses this to re-create the
#                                    containers from another image)
#   ./run.sh --stop        stop and remove both containers (volumes kept)
set -eu

# The options of the last deployment (one per line, without --image) and the image it ran are kept here.
if [ "$(id -u)" = 0 ]; then
	DEPLOY_STATE=${MEDUZA_NP_DEPLOY_STATE:-/var/lib/meduza-np-deploy}
else
	DEPLOY_STATE=${MEDUZA_NP_DEPLOY_STATE:-${XDG_DATA_HOME:-$HOME/.local/share}/meduza-np-deploy}
fi
if [ "${1:-}" = --again ]; then
	shift
	[ -f "$DEPLOY_STATE/run.args" ] || { echo "no recorded deployment in $DEPLOY_STATE/run.args" >&2; exit 1; }
	saved_image=$(cat "$DEPLOY_STATE/run.image" 2>/dev/null || true)
	n=$#
	while IFS= read -r a; do set -- "$@" "$a"; done <"$DEPLOY_STATE/run.args"
	# recorded options first, then the ones given now (a later --image wins)
	i=0
	while [ $i -lt $n ]; do set -- "$@" "$1"; shift; i=$((i + 1)); done
	if [ -n "$saved_image" ]; then set -- --image "$saved_image" "$@"; fi
fi
SAVED_ARGS=""
save_arg() { SAVED_ARGS="$SAVED_ARGS$1
"; }

ENV_FILE=/etc/meduza-np/node.env
IMAGE=ghcr.io/meduza-np/meduza-np-base-container:stable
NETMODE=bridge
MACVLAN_NETWORK=meduza-np-lan
UPDATER_MACVLAN_NETWORK=""
PLANNER_IP=""
UPDATER_IP=""
PUBLISH=""
IMPORT=""
UDP_PORTS=""

while [ $# -gt 0 ]; do
	case "$1" in
	--env-file) ENV_FILE=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--image) IMAGE=$2; shift 2 ;;
	--network) NETMODE=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--macvlan-network) MACVLAN_NETWORK=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--updater-macvlan-network) UPDATER_MACVLAN_NETWORK=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--ip) PLANNER_IP=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--updater-ip) UPDATER_IP=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--publish)
		save_arg "$1"
		save_arg "$2"
		# A bare PORT[/PROTO] is published on the same host port (like the Quadlet PublishPort=PORT:PORT lines):
		# Podman alone would pick a random host port, and peers dial the advertised endpoint port.
		case "$2" in
		*:*) PUBLISH="$PUBLISH --publish $2" ;;
		*) PUBLISH="$PUBLISH --publish ${2%%/*}:$2" ;;
		esac
		case "$2" in
		*/udp)
			# host port: PORT/udp, HOST:PORT/udp or IP:HOST:PORT/udp
			hp=$(echo "${2%/udp}" | awk -F: '{ print (NF > 1) ? $(NF - 1) : $1 }')
			UDP_PORTS="$UDP_PORTS $hp"
			;;
		esac
		shift 2
		;;
	--import) IMPORT=$2; save_arg "$1"; save_arg "$2"; shift 2 ;;
	--stop)
		podman rm -f -t 30 meduza-np-planner meduza-np-updater 2>/dev/null || true
		exit 0
		;;
	*) echo "unknown option $1" >&2; exit 2 ;;
	esac
done

[ -f "$ENV_FILE" ] || { echo "missing $ENV_FILE (copy meduza-np.env.example)" >&2; exit 1; }
# Capabilities follow the selected components (§14.3.4, D44). Base set: the network (NET_*), installing Alpine
# dependencies and preparing files for daemons that run as their own users (CHOWN, DAC_OVERRIDE, FOWNER), those
# daemons dropping privileges (SETUID, SETGID; dnsmasq, dbus, FRR, avahi) and planner signalling them (KILL).
# FRR daemons keep CAP_SYS_ADMIN (netns/VRF support); avahi-daemon chroots itself (SYS_CHROOT).
components=$(sed -n 's/^MEDUZA_NP_COMPONENTS=//p' "$ENV_FILE" | tail -1 | tr -d ' "')
caps="CHOWN,DAC_OVERRIDE,FOWNER,KILL,SETUID,SETGID,NET_ADMIN,NET_RAW,NET_BIND_SERVICE"
case ",$components," in *,frr,* | *,meduza-np-frr,*) caps="$caps,SYS_ADMIN" ;; esac
case ",$components," in *,avahi,* | *,meduza-np-avahi,*) caps="$caps,SYS_CHROOT" ;; esac
case "$NETMODE" in bridge | host | macvlan) ;; *) echo "--network must be bridge, host or macvlan" >&2; exit 2 ;; esac
if [ "$NETMODE" != bridge ] && [ -n "$PUBLISH" ]; then
	echo "--publish requires --network bridge (host/macvlan expose listeners directly)" >&2
	exit 2
fi
updater_net=meduza-np
updater_mode=bridge
updater_caps=CHOWN,DAC_OVERRIDE,FOWNER,SETUID,SETGID
if [ "$NETMODE" = macvlan ]; then
	[ "$(id -u)" = 0 ] || { echo "macvlan requires rootful Podman" >&2; exit 2; }
	[ -n "$PLANNER_IP" ] && [ -n "$UPDATER_IP" ] && [ "$PLANNER_IP" != "$UPDATER_IP" ] ||
		{ echo "macvlan requires --ip and a distinct --updater-ip (IPv4 or dhcp)" >&2; exit 2; }
	UPDATER_MACVLAN_NETWORK=${UPDATER_MACVLAN_NETWORK:-$MACVLAN_NETWORK}
	for network in "$MACVLAN_NETWORK" "$UPDATER_MACVLAN_NETWORK"; do
		case "$network" in '' | *[!a-zA-Z0-9_.-]*) echo "invalid macvlan network name" >&2; exit 2 ;; esac
		[ "$(podman network inspect --format '{{.Driver}}' "$network")" = macvlan ] ||
			{ echo "create the macvlan network $network first (see README)" >&2; exit 2; }
	done
	[ "$PLANNER_IP" != dhcp ] || { echo "planner requires a fixed IPv4 --ip" >&2; exit 2; }
	for address in "$PLANNER_IP" "$UPDATER_IP"; do
		[ "$address" != dhcp ] || continue
		# Full address/subnet validation is done by Podman; reject option/network-spec injection here.
		printf '%s\n' "$address" | awk -F. 'NF != 4 { exit 1 } { for (i=1; i<=4; i++) if ($i !~ /^[0-9]+$/ || $i ~ /^0[0-9]/ || $i+0 > 255) exit 1 }' ||
			{ echo "macvlan addresses must be IPv4 literals: $address" >&2; exit 2; }
	done
	if [ "$UPDATER_IP" = dhcp ]; then
		[ "$UPDATER_MACVLAN_NETWORK" != "$MACVLAN_NETWORK" ] ||
			{ echo "DHCP updater requires a separate network from the static planner" >&2; exit 2; }
		[ "$(podman network inspect --format '{{index .IPAMOptions "driver"}}' "$UPDATER_MACVLAN_NETWORK")" = dhcp ] ||
			{ echo "--updater-ip dhcp requires a macvlan network with --ipam-driver dhcp" >&2; exit 2; }
		updater_net="$UPDATER_MACVLAN_NETWORK"
	else
		updater_net="$UPDATER_MACVLAN_NETWORK:ip=$UPDATER_IP"
	fi
	updater_mode=macvlan
	# Only for this mode: the entrypoint applies the explicitly configured upstream gateway in its own namespace.
	updater_caps="$updater_caps,NET_ADMIN"
elif [ -n "$PLANNER_IP$UPDATER_IP$UPDATER_MACVLAN_NETWORK" ] || [ "$MACVLAN_NETWORK" != meduza-np-lan ]; then
	echo "--ip, --updater-ip and macvlan network options require --network macvlan" >&2
	exit 2
fi

for v in meduza-np-store meduza-np-node meduza-np-ipc; do
	podman volume exists "$v" || podman volume create "$v" >/dev/null
done
if [ "$NETMODE" != macvlan ]; then
	podman network exists meduza-np || podman network create --driver bridge meduza-np >/dev/null
fi

podman rm -f -t 30 meduza-np-planner meduza-np-updater >/dev/null 2>&1 || true

import_mount=""
[ -n "$IMPORT" ] && import_mount="--volume $IMPORT:/var/lib/meduza-np/import:ro"

# updater: bridge normally; macvlan gets its own LAN address and upstream route, independent of planner.
# shellcheck disable=SC2086
podman run -d --name meduza-np-updater --restart always \
	--env-file "$ENV_FILE" --env MEDUZA_NP_NETWORK_MODE="$updater_mode" --network "$updater_net" \
	--volume meduza-np-store:/var/lib/meduza-np/store \
	--volume meduza-np-ipc:/run/meduza-np \
	$import_mount \
	--cap-drop ALL --cap-add "$updater_caps" --security-opt no-new-privileges \
	--health-cmd "/usr/lib/meduza-np/container/entrypoint.sh health-updater" --health-interval 60s \
	--log-opt max-size=10m \
	"$IMAGE" updater

if [ "$NETMODE" = bridge ]; then
	net="--network meduza-np --sysctl net.ipv4.ip_forward=1 --sysctl net.ipv4.conf.all.src_valid_mark=0 --sysctl net.ipv4.conf.default.src_valid_mark=0 $PUBLISH"
elif [ "$NETMODE" = macvlan ]; then
	net="--network $MACVLAN_NETWORK:ip=$PLANNER_IP --sysctl net.ipv4.ip_forward=1 --sysctl net.ipv4.conf.all.src_valid_mark=0 --sysctl net.ipv4.conf.default.src_valid_mark=0"
else
	net="--network host"
fi
# planner: node volume read-write, store read-only, only the capabilities above (not --privileged, not Podman's
# default set: no FSETID, SETFCAP, SETPCAP, SYS_CHROOT unless avahi)
# shellcheck disable=SC2086
podman run -d --name meduza-np-planner --restart always \
	--env-file "$ENV_FILE" --env MEDUZA_NP_NETWORK_MODE="$NETMODE" $net \
	--volume meduza-np-node:/var/lib/meduza-np \
	--volume meduza-np-store:/var/lib/meduza-np/store:ro \
	--volume meduza-np-ipc:/run/meduza-np \
	--cap-drop ALL --cap-add "$caps" --device /dev/net/tun \
	--health-cmd "/usr/lib/meduza-np/container/entrypoint.sh health-planner" --health-interval 60s --health-start-period 10m \
	--log-opt max-size=10m \
	"$IMAGE" planner

# A peer that kept sending to a published UDP port while the planner container was replaced (WireGuard keepalives,
# OpenVPN/Tinc retries) holds a conntrack entry made without the new DNAT; its traffic keeps refreshing it, so it
# would never reach the new container. Drop those entries now that the new mapping exists. Rootless Podman forwards
# published ports in user space (rootlessport), with no host DNAT and so no such entry (D57).
if [ "$NETMODE" = bridge ] && [ -n "$UDP_PORTS" ] && [ "$(id -u)" = 0 ]; then
	if command -v conntrack >/dev/null 2>&1; then
		for p in $UDP_PORTS; do
			case "$p" in *[!0-9]*) continue ;; esac
			conntrack -D -p udp --orig-port-dst "$p" >/dev/null 2>&1 || true
		done
	else
		echo "note: conntrack (conntrack-tools) not found; if a tunnel peer does not reconnect after this, delete the stale UDP conntrack entries for:$UDP_PORTS" >&2
	fi
fi

# --restart always only covers crashes; after a reboot Podman restarts such containers through podman-restart.service:
# the system unit for root's containers, the user unit (systemd user instance, needs lingering) for rootless ones.
if [ "$(id -u)" = 0 ]; then sc=systemctl; else sc="systemctl --user"; fi
if command -v systemctl >/dev/null 2>&1 && $sc cat podman-restart.service >/dev/null 2>&1; then
	$sc enable podman-restart.service >/dev/null 2>&1 ||
		echo "warning: could not enable podman-restart.service ($sc); the containers will not start after a reboot" >&2
	if [ "$(id -u)" != 0 ] && [ "$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null)" != yes ]; then
		echo "warning: lingering is off for $(id -un); the containers start only after this user logs in (loginctl enable-linger $(id -un))" >&2
	fi
else
	echo "note: start the containers at boot from your init system: podman start meduza-np-updater meduza-np-planner" >&2
fi
mkdir -p "$DEPLOY_STATE"
printf '%s' "$SAVED_ARGS" >"$DEPLOY_STATE/run.args.tmp" && mv "$DEPLOY_STATE/run.args.tmp" "$DEPLOY_STATE/run.args"
printf '%s
' "$IMAGE" >"$DEPLOY_STATE/run.image.tmp" && mv "$DEPLOY_STATE/run.image.tmp" "$DEPLOY_STATE/run.image"
echo "started meduza-np-updater and meduza-np-planner ($NETMODE, $IMAGE)"
