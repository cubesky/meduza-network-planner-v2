#!/bin/sh
# Multi-node lab on an Incus host (nested LXC + Podman), see tools/lab/README.md.
#
#   tools/lab/lab.sh images                      push manager + base-container images into the lab containers
#   tools/lab/lab.sh manager                     start the manager in lab-mgr (bootstrap token in /root/lab/bootstrap)
#   tools/lab/lab.sh site DIR                    copy a signed release site (tools/container-e2e.sh output) to the nodes
#   tools/lab/lab.sh node NAME COMPONENTS NET TOKEN [PORT/PROTO...]   deploy base-container on NAME (NET = host|bridge;
#                                                the ports are published in bridge mode)
#   tools/lab/lab.sh api ARGS...                 run tools/lab/manager-api.mjs against the lab manager
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
LAB=/root/lab
MGR_IP=10.50.0.10
MANAGER="http://$MGR_IP:8787"
# Origin the manager serves (WebAuthn RP ID + CSRF origin). localhost: an operator reaches the lab manager with
# `ssh -L 8787:10.50.0.10:8787 <lab host>` and http://localhost is a secure context for passkeys without TLS.
ORIGIN=${MANAGER_ORIGIN:-http://localhost:8787}
NODES="lab-a lab-b lab-c lab-roam"
# the bridge-mode node (tools/lab/bridge.sh) gets images and sites once it exists
incus info lab-br >/dev/null 2>&1 && NODES="$NODES lab-br"
mkdir -p $LAB

lx() { c=$1; shift; incus exec "$c" -- "$@"; }

case "${1:-}" in
images)
	rm -f $LAB/manager.tar $LAB/base.tar
	podman save -o $LAB/manager.tar localhost/meduza-np-manager:lab
	podman save -o $LAB/base.tar localhost/meduza-np-base-container:e2e
	incus file push $LAB/manager.tar lab-mgr/root/manager.tar
	lx lab-mgr podman load -q -i /root/manager.tar
	for n in $NODES; do
		incus file push $LAB/base.tar $n/root/base.tar
		lx $n podman load -q -i /root/base.tar
		incus file push -r "$REPO/packages/meduza-np-base-container/deploy/podman" $n/root/
		lx $n sh -c 'sed -i "s/\r$//" /root/podman/run.sh && chmod +x /root/podman/run.sh'
	done
	;;
manager)
	[ -f $LAB/bootstrap ] || head -c 24 /dev/urandom | base64 | tr -d '/+=' >$LAB/bootstrap
	lx lab-mgr sh -c 'mkdir -p /root/mgr && cd /root/mgr && for k in session.key secret.key; do [ -f $k ] || head -c 32 /dev/urandom >$k; done && chmod 0444 session.key secret.key'
	incus file push $LAB/bootstrap lab-mgr/root/mgr/bootstrap
	lx lab-mgr chmod 0444 /root/mgr/bootstrap
	lx lab-mgr podman rm -f mgr >/dev/null 2>&1 || true
	lx lab-mgr podman volume exists mgr-data || lx lab-mgr podman volume create mgr-data >/dev/null
	lx lab-mgr podman run -d --name mgr --restart always -p 8787:8787 \
		-v mgr-data:/var/lib/meduza-np-manager \
		-v /root/mgr/session.key:/run/secrets/session.key:ro -v /root/mgr/secret.key:/run/secrets/secret.key:ro \
		-v /root/mgr/bootstrap:/run/secrets/bootstrap:ro \
		-e PUBLIC_ORIGIN=$ORIGIN -e ALLOW_INSECURE_ORIGIN=1 -e BOOTSTRAP_TOKEN_FILE=/run/secrets/bootstrap \
		localhost/meduza-np-manager:lab >/dev/null
	i=0; until curl -fs $MANAGER/readyz >/dev/null; do i=$((i + 1)); [ $i -gt 60 ] && { lx lab-mgr podman logs mgr | tail -20; exit 1; }; sleep 1; done
	echo "manager ready at $MANAGER"
	;;
site)
	# refresh the content IN PLACE: the updater container bind-mounts /srv/import, a replaced directory would
	# leave it looking at the old (deleted) one
	for n in $NODES; do
		lx $n sh -c 'mkdir -p /srv/import /srv/incoming && rm -rf /srv/incoming/*'
		incus file push -r "$2" $n/srv/incoming/
		lx $n sh -c "find /srv/import -mindepth 1 -delete && cp -a /srv/incoming/$(basename "$2")/. /srv/import/"
	done
	;;
node)
	name=$2 components=$3 net=$4 token=$5
	shift 5
	pub=""
	for p in "$@"; do pub="$pub --publish $p"; done
	lx "$name" sh -c "mkdir -p /etc/meduza-np && umask 077 && printf '%s' '$token' >/etc/meduza-np/enroll.token"
	lx "$name" sh -c "cat >/etc/meduza-np/node.env <<EOF
MEDUZA_NP_COMPONENTS=$components
MEDUZA_NP_UPDATE_BASE_URL=file:///var/lib/meduza-np/import
MEDUZA_NP_UPDATE_AUTO=0
MEDUZA_NP_ENROLL_URL=$MANAGER
MEDUZA_NP_ENROLL_TOKEN_FILE=/var/lib/meduza-np/etc/enroll.token
MEDUZA_NP_ENROLL_INSECURE=1
EOF"
	# the planner container reads the token from its node volume (etc/ lives there)
	lx "$name" sh -c "podman volume exists meduza-np-node || podman volume create meduza-np-node >/dev/null; d=\$(podman volume inspect meduza-np-node --format '{{.Mountpoint}}'); mkdir -p \$d/etc && cp /etc/meduza-np/enroll.token \$d/etc/enroll.token && chmod 0600 \$d/etc/enroll.token"
	# shellcheck disable=SC2086
	lx "$name" /root/podman/run.sh --env-file /etc/meduza-np/node.env --image localhost/meduza-np-base-container:e2e --network "$net" $pub --import /srv/import
	;;
api)
	shift
	MANAGER=$MANAGER ORIGIN=$ORIGIN STATE=$LAB/admin.json node "$REPO/tools/lab/manager-api.mjs" "$@"
	;;
*)
	sed -n '2,10p' "$0"
	exit 2
	;;
esac
