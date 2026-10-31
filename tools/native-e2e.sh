#!/bin/sh
# Native (non-container) Linux install acceptance, following docs/operations.md §2.2 literally.
# WARNING: installs into the host (/usr/lib/meduza-np, /usr/bin links, systemd units, /var/lib/meduza-np).
# Run it only on a throwaway machine. Needs: cargo + cargo-zigbuild (x86_64 musl), systemd, jq.
set -eu

REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${1:-/tmp/mnp-native-e2e}
TARGET=x86_64-unknown-linux-musl
BIN="$REPO/target/$TARGET/release"
PKGS="meduza-np-planner meduza-np-updater meduza-np-cli meduza-np-wireguard"
fail() { echo "FAIL: $*" >&2; journalctl -u meduza-np-updater -u meduza-np-planner --no-pager -n 30 >&2 || true; exit 1; }
step() { echo "== $*"; }

[ "${I_UNDERSTAND_THIS_MODIFIES_THE_HOST:-}" = 1 ] || fail "set I_UNDERSTAND_THIS_MODIFIES_THE_HOST=1"

step "build + signed local release"
cd "$REPO"
cargo zigbuild --locked --release --target $TARGET -p meduza-np-planner -p meduza-np-updater -p meduza-np-cli -p meduza-np-wireguard
cargo build --locked --release -p meduza-np-release
REL="$REPO/target/release/meduza-np-release"
rm -rf "$W" && mkdir -p "$W/dist"
"$BIN/meduza-np-updater" keygen "$W/test.key" "native-e2e throwaway key" >"$W/entry.json"
printf '{"formatVersion":1,"threshold":1,"keys":[%s]}\n' "$(cat "$W/entry.json")" >"$W/release-keys.json"
args=""; for p in $PKGS; do args="$args --package $p"; done
# shellcheck disable=SC2086
"$REL" fingerprint --targets $TARGET $args >"$W/fp.json"
"$REL" matrix --fingerprints "$W/fp.json" --group-version v0.1.0 >"$W/m.json"
for p in $PKGS; do "$REL" package --package "$p" --version 0.1.0 --platform linux/amd64 --bin-dir "$BIN" --out "$W/dist" >/dev/null; done
"$REL" group --group-version v0.1.0 --sequence 1 --commit native --matrix "$W/m.json" --artifacts "$W/dist" \
	--url-base packages --site "$W/site" --platforms linux/amd64 --key "$W/test.key" >/dev/null
mkdir -p "$W/site/packages" && cp "$W"/dist/*.tar.gz "$W/site/packages/"

step "clean previous install"
systemctl disable --now meduza-np-updater meduza-np-planner >/dev/null 2>&1 || true
rm -rf /var/lib/meduza-np /run/meduza-np /usr/lib/meduza-np /usr/libexec/meduza-np /usr/share/meduza-np /etc/meduza-np \
	/usr/bin/meduza-np-planner /usr/bin/meduza-np /usr/bin/meduza-np-dashboard \
	/etc/systemd/system/meduza-np-updater.service /etc/systemd/system/meduza-np-planner.service

step "docs §2.2 step 1-2: baseline updater + trust store"
mkdir -p /tmp/u && rm -rf /tmp/u/* && tar -xzf "$W"/dist/meduza-np-updater_0.1.0_linux-amd64.tar.gz -C /tmp/u
install -D -m 0755 /tmp/u/usr/bin/meduza-np-updater /usr/lib/meduza-np/baseline/meduza-np-updater
install -D -m 0644 "$W/release-keys.json" /etc/meduza-np/trust/release-keys.json

step "docs §2.2 step 3: install components, link system paths"
export MEDUZA_NP_UPDATE_BASE_URL="file://$W/site"
# exit 75 = the updater package itself was installed; its first service start completes the takeover (journal)
rc=0; MEDUZA_NP_COMPONENTS=wireguard,cli /usr/lib/meduza-np/baseline/meduza-np-updater apply || rc=$?
[ $rc -eq 0 ] || [ $rc -eq 75 ] || fail "bootstrap apply exited $rc"
/usr/lib/meduza-np/baseline/meduza-np-updater system-links /
[ -x /usr/lib/meduza-np/updater-run ] || fail "launcher not linked"
[ -x /usr/bin/meduza-np-planner ] || fail "planner not linked"

step "docs §2.2 step 4: systemd units"
install -m 0644 /usr/share/meduza-np/packaging/updater/systemd/meduza-np-updater.service /etc/systemd/system/
install -m 0644 /usr/share/meduza-np/packaging/planner/systemd/meduza-np-planner.service /etc/systemd/system/
printf 'MEDUZA_NP_UPDATE_BASE_URL=file://%s/site\nMEDUZA_NP_UPDATE_AUTO=0\n' "$W" >/etc/meduza-np/updater.env
systemctl daemon-reload
systemctl enable --now meduza-np-updater meduza-np-planner
i=0; until meduza-np status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "planner not answering"; sleep 1; done
i=0; until meduza-np update status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "updater not answering"; sleep 1; done

step "checks"
[ "$(meduza-np --json packages | jq -r '.packages[] | select(.name=="meduza-np-wireguard") | .registration')" = registered ] || fail "wireguard not registered"
[ "$(meduza-np --json update status | jq -r .installed.groupVersion)" = v0.1.0 ] || fail "installed set"
systemctl stop meduza-np-updater
[ -S /run/meduza-np/planner.sock ] || fail "stopping the updater removed planner.sock (RuntimeDirectoryPreserve)"
meduza-np status >/dev/null || fail "planner must keep working while the updater is stopped"
systemctl start meduza-np-updater
systemctl restart meduza-np-planner
i=0; until meduza-np status >/dev/null 2>&1; do i=$((i + 1)); [ $i -gt 30 ] && fail "planner restart"; sleep 1; done
[ -S /run/meduza-np/updater.sock ] || fail "restarting planner removed updater.sock"
[ "$(meduza-np --json update status | jq -r .lastJournal.phase)" = done ] || fail "self-update takeover not completed: $(meduza-np --json update status | jq -c .lastJournal)"

echo "PASS: native install per docs/operations.md §2.2 (systemd)"
