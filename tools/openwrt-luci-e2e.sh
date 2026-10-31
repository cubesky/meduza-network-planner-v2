#!/bin/sh
# dashboard-openwrt acceptance in a real OpenWrt userspace (openwrt/rootfs image under rootful Podman): the static
# musl planner + CLI + wireguard plugin installed natively, planner started by its procd init script, the rpcd
# plugin + ACL installed, then every ubus method of `meduza-np` is called through ubus like LuCI does.
#
# KEEP=1 leaves the container running with LuCI installed (apk add luci) on 127.0.0.1:${LUCI_PORT:-8080} for manual
# inspection of the views.
#
# Usage: tools/openwrt-luci-e2e.sh   (after tools/container-e2e.sh or `cargo zigbuild --release --target
#        x86_64-unknown-linux-musl -p meduza-np-planner -p meduza-np-cli -p meduza-np-wireguard`)
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
BIN="$REPO/target/x86_64-unknown-linux-musl/release"
IMAGE=${OPENWRT_ROOTFS:-docker.io/openwrt/rootfs:x86-64-25.12.6}
C=mnp-openwrt

fail() { echo "FAIL: $*" >&2; podman exec $C logread 2>/dev/null | tail -30 >&2 || true; exit 1; }
ox() { podman exec $C "$@"; }

podman rm -f $C >/dev/null 2>&1 || true
publish=""; [ "${KEEP:-0}" = 1 ] && publish="-p 127.0.0.1:${LUCI_PORT:-8080}:80"
# shellcheck disable=SC2086
# no NET_ADMIN: otherwise netifd applies its default br-lan config and cuts the container off its network
podman run -d --name $C $publish "$IMAGE" /sbin/init >/dev/null
i=0; until ox ubus list >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "ubus did not come up"; sleep 1; done
ox sh -c '. /etc/openwrt_release; echo "OpenWrt $DISTRIB_RELEASE ($DISTRIB_ARCH)"'

echo "== native install (files as the OpenWrt packages lay them out)"
podman cp "$BIN/meduza-np-planner" $C:/usr/bin/meduza-np-planner
podman cp "$BIN/meduza-np" $C:/usr/bin/meduza-np
ox mkdir -p /usr/libexec/meduza-np/plugins /usr/share/meduza-np/packages
podman cp "$BIN/meduza-np-wireguard" $C:/usr/libexec/meduza-np/plugins/wireguard
for p in meduza-np-planner meduza-np-cli meduza-np-wireguard; do
	ox mkdir -p /usr/share/meduza-np/packages/$p
	podman cp "$REPO/packages/$p/manifest.json" $C:/usr/share/meduza-np/packages/$p/manifest.json
	for d in ui schema; do
		[ -d "$REPO/packages/$p/$d" ] && podman cp "$REPO/packages/$p/$d" $C:/usr/share/meduza-np/packages/$p/
	done
done
podman cp "$REPO/packages/meduza-np-planner/packaging/openwrt/meduza-np-planner.init" $C:/etc/init.d/meduza-np-planner
ox chmod 0755 /etc/init.d/meduza-np-planner /usr/bin/meduza-np-planner /usr/bin/meduza-np /usr/libexec/meduza-np/plugins/wireguard
ox /etc/init.d/meduza-np-planner enable
ox /etc/init.d/meduza-np-planner start
i=0; until ox meduza-np status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "planner did not start under procd"; sleep 1; done
echo "planner up under procd: $(ox meduza-np --json status | head -c 120)"

echo "== LuCI app (rpcd plugin, ACL, menu, views)"
D="$REPO/packages/meduza-np-dashboard-openwrt"
ox sh -c 'command -v rpcd >/dev/null' || fail "image has no rpcd"
ox mkdir -p /usr/libexec/rpcd
podman cp "$D/root/usr/libexec/rpcd/meduza-np" $C:/usr/libexec/rpcd/meduza-np
ox chmod 0755 /usr/libexec/rpcd/meduza-np
ox mkdir -p /usr/share/rpcd/acl.d /usr/share/luci/menu.d /www/luci-static/resources
podman cp "$D/root/usr/share/rpcd/acl.d/luci-app-meduza-np.json" $C:/usr/share/rpcd/acl.d/
podman cp "$D/root/usr/share/luci/menu.d/luci-app-meduza-np.json" $C:/usr/share/luci/menu.d/
podman cp "$D/htdocs/luci-static/resources/." $C:/www/luci-static/resources/
ox /etc/init.d/rpcd restart
i=0; until ox ubus -v list meduza-np >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 20 ] && fail "rpcd did not register meduza-np"; sleep 1; done
ox ubus -v list meduza-np

call() { ox ubus call meduza-np "$@"; }
for m in status features packages instances doctor config update_status; do
	out=$(call $m) || fail "ubus call $m"
	echo "$out" | grep -q '"ok": true' || fail "$m: $out"
	echo "ok   $m"
done
out=$(call logs '{"lines": 5}'); echo "$out" | grep -q '"ok": true' || fail "logs: $out"; echo "ok   logs"
call update_status | grep -q '"available": false' || fail "a node without updater must say so"
out=$(call instance '{"instance": "x; rm -rf /", "action": "restart"}'); echo "$out" | grep -q 'invalid instance' || fail "injection not rejected: $out"
echo "ok   argument validation"
out=$(call maintenance_start '{"reason": "e2e test", "minutes": 5, "pause_apply": true}'); echo "$out" | grep -q '"ok": true' || fail "maintenance_start: $out"
call status | grep -q '"active": true' || fail "maintenance not active"
out=$(call maintenance_stop); echo "$out" | grep -q '"ok": true' || fail "maintenance_stop: $out"
echo "ok   maintenance start/stop"
ox sh -c 'for f in /www/luci-static/resources/view/meduza-np/*.js /www/luci-static/resources/meduza-np/*.js; do [ -s "$f" ] || exit 1; done' || fail "views not installed"

if [ "${KEEP:-0}" = 1 ]; then
	ox apk update >/dev/null 2>&1 && ox apk add luci >/dev/null 2>&1 || fail "apk add luci"
	ox /etc/init.d/uhttpd restart
	ox /etc/init.d/rpcd restart
	echo "LuCI kept running on 127.0.0.1:${LUCI_PORT:-8080} (container $C)"
else
	podman rm -f $C >/dev/null
fi
echo "PASS: dashboard-openwrt over real procd/ubus/rpcd ($IMAGE)"
