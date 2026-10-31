#!/bin/sh
# OpenWrt package acceptance (D29) in real OpenWrt userspace under rootful Podman, once per package manager:
#   apk  (apk-tools 3)  docker.io/openwrt/rootfs:x86-64-25.12.6   (+ the LuCI app)
#   opkg (.ipk)          docker.io/openwrt/rootfs:x86_64-v24.10.8
# Packages are built with the real release tool (ipk directly, apk through tools/openwrt-apk.sh) and a signed test
# group with a throwaway key. Checks per run:
#   1. a plain `apk add` / `opkg install` of planner, cli, wireguard, updater: procd service enabled + started by the
#      package script, plugin registered by the rescan, trust store in place
#   2. the updater (opkg/apk backend) upgrades planner + wireguard to the next group: installed versions, planner
#      restarted and registered at the new version, journal `done`, install marker removed
#   3. an updater-only group: the updater installs itself last, exits 75, the launcher starts the new binary and the
#      journal completes (`done`); a single updater process remains
#   4. removal stops and disables the services
# Also: building the same packages twice gives identical files (ipk and apk).
#
# Usage: tools/openwrt-pkg-e2e.sh [WORKDIR]   (needs: cargo + cargo-zigbuild for x86_64-unknown-linux-musl, podman, jq)
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-$(mktemp -d)}
TARGET=x86_64-unknown-linux-musl
BIN="$REPO/target/$TARGET/release"
APK_IMAGE=${OPENWRT_APK_ROOTFS:-docker.io/openwrt/rootfs:x86-64-25.12.6}
IPK_IMAGE=${OPENWRT_IPK_ROOTFS:-docker.io/openwrt/rootfs:x86_64-v24.10.8}
PKGS="meduza-np-planner meduza-np-cli meduza-np-wireguard meduza-np-updater"
C=mnp-openwrt-pkg

fail() { echo "FAIL: $*" >&2; podman exec $C logread 2>/dev/null | tail -40 >&2 || true; exit 1; }
step() { echo "== $*"; }
ox() { podman exec $C "$@"; }
oj() { podman exec $C meduza-np --json "$@"; }

step "build binaries and tools"
cd "$REPO"
cargo zigbuild --locked --release --target $TARGET -p meduza-np-planner -p meduza-np-updater -p meduza-np-cli -p meduza-np-wireguard
cargo build --locked --release -p meduza-np-release -p meduza-np-updater
REL="$REPO/target/release/meduza-np-release"
UPD="$REPO/target/release/meduza-np-updater"

step "signed test groups v0.1.0 and v0.2.0 (planner + wireguard changed)"
rm -rf "$W" && mkdir -p "$W/dist1" "$W/dist2" "$W/dist3" "$W/again"
"$UPD" keygen "$W/test.key" "openwrt-pkg-e2e throwaway key" >"$W/entry.json"
printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$W/entry.json")" >"$W/release-keys.json"
args=""; for p in $PKGS meduza-np-dashboard-openwrt; do args="$args --package $p"; done
# shellcheck disable=SC2086
"$REL" fingerprint --targets $TARGET $args >"$W/fp1.json"
"$REL" matrix --fingerprints "$W/fp1.json" --group-version v0.1.0 >"$W/m1.json"
build() { # version dist packages...
	v=$1 d=$2; shift 2
	for p in "$@"; do
		if [ "$p" = meduza-np-dashboard-openwrt ]; then
			"$REL" openwrt --package "$p" --version "$v" --noarch 1 --out "$d" >/dev/null
		else
			"$REL" openwrt --package "$p" --version "$v" --target $TARGET --bin-dir "$BIN" --trust "$W/release-keys.json" --out "$d" >/dev/null
		fi
	done
	"$REPO/tools/openwrt-apk.sh" "$d" "$REL" >/dev/null
}
build 0.1.0 "$W/dist1" $PKGS meduza-np-dashboard-openwrt
build 0.1.0 "$W/again" meduza-np-planner meduza-np-dashboard-openwrt
for f in meduza-np-planner_0.1.0_x86_64.ipk meduza-np-planner_0.1.0_x86_64.apk meduza-np-dashboard-openwrt_0.1.0_noarch.apk; do
	cmp -s "$W/dist1/$f" "$W/again/$f" || fail "$f is not reproducible"
done
echo "reproducible: ipk + apk"
"$REL" group --group-version v0.1.0 --sequence 1 --commit e2e1 --matrix "$W/m1.json" --artifacts "$W/dist1" \
	--url-base packages --site "$W/site1" --platforms openwrt/x86_64 --key "$W/test.key" >/dev/null
mkdir -p "$W/site1/packages" && cp "$W"/dist1/*.ipk "$W"/dist1/*.apk "$W/site1/packages/"
jq '."meduza-np-planner" = "changed" | ."meduza-np-wireguard" = "changed"' "$W/fp1.json" >"$W/fp2.json"
"$REL" matrix --fingerprints "$W/fp2.json" --group-version v0.2.0 --baseline "$W/site1/releases/v0.1.0/manifest.json" >"$W/m2.json"
build 0.2.0 "$W/dist2" meduza-np-planner meduza-np-wireguard
cp -r "$W/site1" "$W/site2"
"$REL" group --group-version v0.2.0 --sequence 2 --commit e2e2 --matrix "$W/m2.json" --artifacts "$W/dist2" \
	--baseline "$W/site1/releases/v0.1.0/manifest.json" --url-base packages --site "$W/site2" --platforms openwrt/x86_64 \
	--key "$W/test.key" >/dev/null
cp "$W"/dist2/*.ipk "$W"/dist2/*.apk "$W/site2/packages/"
# v0.3.0: only the updater changed -> self-update through the package manager
jq '."meduza-np-updater" = "changed"' "$W/fp2.json" >"$W/fp3.json"
"$REL" matrix --fingerprints "$W/fp3.json" --group-version v0.3.0 --baseline "$W/site2/releases/v0.2.0/manifest.json" >"$W/m3.json"
build 0.3.0 "$W/dist3" meduza-np-updater
cp -r "$W/site2" "$W/site3"
"$REL" group --group-version v0.3.0 --sequence 3 --commit e2e3 --matrix "$W/m3.json" --artifacts "$W/dist3" \
	--baseline "$W/site2/releases/v0.2.0/manifest.json" --url-base packages --site "$W/site3" --platforms openwrt/x86_64 \
	--key "$W/test.key" >/dev/null
cp "$W"/dist3/*.ipk "$W"/dist3/*.apk "$W/site3/packages/"

wait_for() { # description command...
	what=$1; shift
	i=0; until "$@" >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 60 ] && fail "$what"; sleep 1; done
}
version_of() { oj packages | jq -r --arg p "$1" '.packages[] | select(.name==$p) | "\(.version) \(.registration)"'; }

run() { # image format
	image=$1 fmt=$2
	case $fmt in
	apk) add="apk add --allow-untrusted" del="apk del" list="apk list --installed" ui=meduza-np-dashboard-openwrt ;;
	*) add="opkg install" del="opkg remove" list="opkg list-installed" ui="" ;;
	esac
	step "[$fmt] $image: native install with the package manager"
	podman rm -f $C >/dev/null 2>&1 || true
	# no NET_ADMIN: otherwise netifd applies its default br-lan config and cuts the container off its network
	podman run -d --name $C "$image" /sbin/init >/dev/null
	wait_for "ubus did not come up" ox ubus list
	ox sh -c '. /etc/openwrt_release; echo "OpenWrt $DISTRIB_RELEASE ($DISTRIB_ARCH)"'
	ox mkdir -p /tmp/pkgs
	for p in $PKGS $ui; do
		f=$(ls "$W/dist1/${p}_0.1.0_"*".$fmt")
		podman cp "$f" $C:/tmp/pkgs/
		# shellcheck disable=SC2086
		ox $add "/tmp/pkgs/$(basename "$f")" >/dev/null || fail "$add $p"
	done
	ox test -e /etc/rc.d/S95meduza-np-planner || fail "planner service not enabled by the package script"
	ox test -e /etc/rc.d/S96meduza-np-updater || fail "updater service not enabled by the package script"
	wait_for "planner not started by the package script" ox meduza-np status
	wait_for "wireguard not registered by the rescan" sh -c "podman exec $C meduza-np --json packages | jq -e '.packages[] | select(.name==\"meduza-np-wireguard\" and .registration==\"registered\")'"
	ox test -s /usr/share/meduza-np/trust/release-keys.json || fail "trust store missing"
	# shellcheck disable=SC2086
	ox $list | grep meduza-np- || true
	if [ -n "$ui" ]; then
		ox /etc/init.d/rpcd restart
		wait_for "rpcd object meduza-np" ox ubus -S call meduza-np status
		ox test -s /www/luci-static/resources/view/meduza-np/overview.js || fail "LuCI views not installed"
		echo "LuCI app: ubus meduza-np.status ok"
	fi

	step "[$fmt] updater upgrades planner + wireguard to v0.2.0"
	podman cp "$W/site2" $C:/site
	ox sh -c 'mkdir -p /etc/meduza-np && printf "{\"baseUrl\":\"file:///site\",\"autoApply\":false}\n" >/etc/meduza-np/updater.json'
	ox /etc/init.d/meduza-np-updater restart
	wait_for "updater not answering" ox meduza-np update status
	n=$(ox sh -c 'pidof meduza-np-updater | wc -w')
	[ "$n" = 1 ] || fail "$n updater processes after a service restart (launcher must forward the stop signal)"
	boot1=$(oj status | jq -r '.bootId // empty')
	oj update check >/dev/null || fail "update check"
	oj update plan | jq -c '[.actions[]? | {package, kind, to}]'
	oj update apply >"$W/apply-$fmt.json" 2>&1 || { cat "$W/apply-$fmt.json"; fail "update apply"; }
	# apply is scheduled on the updater's worker: wait for the journal of v0.2.0 to reach a final phase
	wait_for "upgrade to v0.2.0 did not finish" sh -c "podman exec $C meduza-np --json update status |
		jq -e '.lastJournal.groupVersion == \"v0.2.0\" and (.lastJournal.phase | IN(\"done\", \"failed\", \"rolled-back\"))'"
	wait_for "planner after upgrade" ox meduza-np status
	[ "$(version_of meduza-np-planner)" = "0.2.0 registered" ] || fail "planner: $(version_of meduza-np-planner)"
	[ "$(version_of meduza-np-wireguard)" = "0.2.0 registered" ] || fail "wireguard: $(version_of meduza-np-wireguard)"
	[ "$(version_of meduza-np-cli)" = "0.1.0 registered" ] || [ -z "$(version_of meduza-np-cli)" ] || fail "cli touched: $(version_of meduza-np-cli)"
	# shellcheck disable=SC2086
	ox $list | grep -q 'meduza-np-planner.*0\.2\.0' || fail "package manager does not list planner 0.2.0"
	boot2=$(oj status | jq -r '.bootId // empty')
	[ -z "$boot1" ] || [ "$boot1" != "$boot2" ] || fail "planner was not restarted"
	phase=$(oj update status | jq -r .lastJournal.phase)
	[ "$phase" = done ] || fail "journal phase $phase: $(oj update status | jq -c .lastJournal)"
	ox test ! -e /var/run/meduza-np/updater-installing || fail "install marker left behind"
	echo "upgraded: planner $(version_of meduza-np-planner), wireguard $(version_of meduza-np-wireguard)"

	step "[$fmt] updater self-update to v0.3.0 (installed last, exit 75, launcher starts the new binary)"
	# a newer release copied over the import directory in place (docs/operations.md §4)
	podman cp "$W/site3/." $C:/site/
	oj update check >/dev/null || fail "update check v0.3.0"
	oj update apply >/dev/null || fail "update apply v0.3.0"
	wait_for "self-update to v0.3.0 did not finish" sh -c "podman exec $C meduza-np --json update status |
		jq -e '.lastJournal.groupVersion == \"v0.3.0\" and (.lastJournal.phase | IN(\"done\", \"failed\", \"rolled-back\"))'"
	phase=$(oj update status | jq -r .lastJournal.phase)
	[ "$phase" = done ] || fail "self-update phase $phase: $(oj update status | jq -c .lastJournal)"
	[ "$(version_of meduza-np-updater)" = "0.3.0 registered" ] || fail "updater: $(version_of meduza-np-updater)"
	# shellcheck disable=SC2086
	ox $list | grep -q 'meduza-np-updater.*0\.3\.0' || fail "package manager does not list updater 0.3.0"
	n=$(ox sh -c 'pidof meduza-np-updater | wc -w')
	[ "$n" = 1 ] || fail "$n updater processes after the self-update"
	[ "$(version_of meduza-np-planner)" = "0.2.0 registered" ] || fail "planner changed by an updater-only group"
	# the new updater process has no plan yet: check again, nothing may be left to do
	oj update check >/dev/null || fail "update check after the self-update"
	oj update plan | jq -e '[.actions[] | select(.kind != "keep")] | length == 0' >/dev/null ||
		fail "installed set not updated after the self-update: $(oj update plan | jq -c '[.actions[] | {package, kind}]')"
	echo "self-update: updater $(version_of meduza-np-updater)"

	step "[$fmt] removal stops and disables the services"
	# shellcheck disable=SC2086
	ox $del $ui meduza-np-wireguard meduza-np-updater meduza-np-cli meduza-np-planner >/dev/null || fail "$del"
	ox test ! -e /etc/rc.d/S95meduza-np-planner || fail "planner still enabled"
	sleep 2
	ox sh -c 'pidof meduza-np-planner meduza-np-updater' && fail "services still running after removal"
	podman rm -f $C >/dev/null
	echo "PASS [$fmt]"
}

run "$APK_IMAGE" apk
run "$IPK_IMAGE" ipk
echo "PASS: OpenWrt packages (apk + opkg): install, updater upgrade, updater self-update, removal"
