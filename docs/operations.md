# Operating Meduza NP nodes: install, enroll, upgrade, recover

This guide covers nodes. The manager has its own guide: `packages/meduza-np-manager/deploy/podman/README.md`.
Each statement below names what has been verified. Anything not yet verified is marked as such.

## 1. Choose a deployment

| Host | How it runs | Updates | Status |
| --- | --- | --- | --- |
| Linux with Podman | `meduza-np-base-container`: updater + planner containers | updater, store backend, atomic switch | verified (bridge rootful + rootless, host rootful; amd64) |
| Linux without containers | store layout under `/var/lib/meduza-np/store`, systemd units | updater, store backend | verified with `tools/native-e2e.sh` (systemd, amd64) |
| OpenWrt | native packages (`.apk` on 25.12+, `.ipk` on 24.10), procd, optional LuCI app | updater, native package manager (not atomic) | verified on OpenWrt 25.12.6 (apk) and 24.10.8 (opkg) userspace, x86_64: install, updater upgrade + self-update, removal (`tools/openwrt-pkg-e2e.sh`); no real router yet |

Install only what the node should do. A feature exists only when its package is installed. It runs only once the
manager publishes configuration for it.

## 2. Install

### 2.1 Podman (recommended for Linux)

See `packages/meduza-np-base-container/README.md`. In short:

```sh
install -m 0600 deploy/podman/meduza-np.env.example /etc/meduza-np/node.env   # MEDUZA_NP_COMPONENTS=wireguard,cli,...
cp deploy/podman/meduza-np*.{container,volume,network} /etc/containers/systemd/
systemctl daemon-reload && systemctl start meduza-np-updater meduza-np-planner
```

On blank volumes the updater checks the signed channel and installs planner + the selected components (+ required
dependencies). The planner container waits for that, installs the Alpine packages those components need and
starts. Watch it with `podman logs -f meduza-np-updater`.

Routers (`frr` component): the planner container needs CAP_SYS_ADMIN (run.sh adds it; with Quadlet uncomment
`AddCapability=SYS_ADMIN`) and `net.ipv4.ip_forward=1` (`--sysctl`/`Sysctl=` in bridge mode, on the host in host mode).
mDNS (`avahi` component): CAP_SYS_CHROOT the same way (`AddCapability=SYS_CHROOT`). The planner container otherwise
runs with a fixed minimal capability set (`DropCapability=ALL` + the list in the unit), not Podman's defaults.
Without systemd units, `run.sh` enables `podman-restart.service` so the containers return after a reboot (the user unit when
run as a rootless user, with lingering). Rootless (bridge mode only): see the base-container README, section Rootless.

### 2.2 Linux without containers

1. Download the `meduza-np-updater` archive for your platform from the release named by the signed channel and
   check its SHA-256 against `releases/<group>/manifest.json`. That first check is the trust anchor: verify the
   manifest signature against the published release key with any Ed25519 tool, or compare the digest on a second
   machine.
2. Install the baseline updater, the trust store and the launcher:
   ```sh
   mkdir -p /tmp/u && tar -xzf meduza-np-updater_*_linux-amd64.tar.gz -C /tmp/u
   install -D -m 0755 /tmp/u/usr/bin/meduza-np-updater /usr/lib/meduza-np/baseline/meduza-np-updater
   install -D -m 0644 release-keys.json /etc/meduza-np/trust/release-keys.json
   ```
3. Install the components and link the system paths:
   ```sh
   MEDUZA_NP_COMPONENTS=wireguard,cli /usr/lib/meduza-np/baseline/meduza-np-updater apply
   /usr/lib/meduza-np/baseline/meduza-np-updater system-links /
   ```
   `apply` exits with code 75 because it also installed the updater package; the first start of the service
   finishes that takeover (recorded in the journal). `system-links` links `/usr/libexec/meduza-np`, `/usr/share/meduza-np/{packages,packaging,dashboard}`,
   `/usr/lib/meduza-np/{providers,updater-run}` and the installed `/usr/bin/meduza-np*` binaries into `/var/lib/meduza-np/store/current`. It refuses to replace real files.
4. Install the units from `/usr/share/meduza-np/packaging/{planner,updater}/systemd/` into `/etc/systemd/system/`, then
   `systemctl enable --now meduza-np-updater meduza-np-planner`.
   Optional settings go in `/etc/meduza-np/updater.env` (channel, window, `MEDUZA_NP_UPDATE_AUTO`).

Service software (wg, openvpn, frr, dnsmasq, …) comes from the distribution unless a `-pkg` provider is installed;
`meduza-np doctor` lists what is missing.

### 2.3 OpenWrt

1. Find the package architecture: `. /etc/openwrt_release; echo $DISTRIB_ARCH`. Release assets exist for `x86_64`,
   `aarch64_generic`, `aarch64_cortex-a53`, `aarch64_cortex-a72`, `aarch64_cortex-a76`, `arm_cortex-a7_neon-vfpv4`,
   `arm_cortex-a9_vfpv3-d16` and `arm_cortex-a15_neon-vfpv4` (static binaries; MIPS and FPU-less ARM are not
   supported yet). The LuCI app is architecture-independent (`_all.ipk` / `_noarch.apk`).
2. Download from the GitHub Release of the group and check each file's SHA-256 against
   `releases/<group>/manifest.json` (same trust anchor as §2.2). The packages are not signed with an opkg/apk feed
   key; the updater verifies the signed group instead.
3. Install `meduza-np-planner` first, then `meduza-np-cli`, the wanted plugins, `meduza-np-updater` and optionally
   `meduza-np-dashboard-openwrt` (LuCI → Services → Meduza NP):
   * OpenWrt 25.12 and later: `apk add --allow-untrusted ./meduza-np-planner_<version>_<arch>.apk …`
   * OpenWrt 24.10: `opkg install ./meduza-np-planner_<version>_<arch>.ipk …`

   The planner and updater services are enabled and started by the packages; an installed plugin is registered at
   once. The updater package carries the release trust store (`/usr/share/meduza-np/trust/release-keys.json`).
4. Service software is not pulled in automatically (an update never installs system packages). Install what the
   features need from the OpenWrt feeds, e.g. `wireguard-tools kmod-wireguard`, `tinc`, `openvpn-openssl` (or
   `meduza-np-openvpn-pkg`), `frr frr-zebra frr-mgmtd frr-bgpd frr-bfdd frr-ospfd frr-vtysh`; `meduza-np doctor` lists what is
   missing.
   mDNS (`meduza-np-avahi`) needs the D-Bus build of Avahi: `apk add avahi-dbus-daemon` (or `opkg install
   avahi-dbus-daemon`), then `/etc/init.d/dbus restart; /etc/init.d/avahi-daemon restart`. The dbus package starts
   its daemon before Avahi's bus policy is installed, and until dbus is restarted avahi-daemon is refused the name
   `org.freedesktop.Avahi` and exits (logread: "not allowed to own the service"). With the distribution's daemon
   running use avahi mode `system`; `managed` is refused while it runs (D54).

State lives in `/etc/meduza-np-state` (overlay), runtime files in `/var/run/meduza-np`. Every component derives these
paths itself, so no environment variables are needed. Optional updater settings go to `/etc/meduza-np/updater.json`
(`baseUrl`, `channel`, `autoApply`, `window`, `checkIntervalSeconds`). Removing the packages stops and disables the
services; state under `/etc/meduza-np-state` and `/etc/meduza-np` is kept.

## 3. Enroll with the manager

1. In the manager: Nodes → create the node → "enrollment token" (single use, expires).
2. On the node, one of:
   * `meduza-np enroll --url https://manager.example.net --token <token> [--ca-file manager-ca.pem]`
   * unattended (containers, provisioning): `MEDUZA_NP_ENROLL_URL` and `MEDUZA_NP_ENROLL_TOKEN_FILE` (preferred over
     `MEDUZA_NP_ENROLL_TOKEN`) and optionally `MEDUZA_NP_MANAGER_CA_FILE` in the planner's environment. They are used
     only while the node is not enrolled yet.
3. The node creates its Ed25519 identity locally (`/etc/meduza-np/identity/` or the node volume). The private key never
   leaves the node. `meduza-np status` shows sync state, revisions and backoff.

Local dashboard (`dashboard` component): sign-in through the manager requires `MEDUZA_NP_DASHBOARD_PUBLIC_URL` (HTTPS,
TLS cert/key for non-loopback listening). Offline, `meduza-np dashboard-link` (root) prints a single-use link that is
valid for 10 minutes.

## 4. Upgrade

* The updater checks the channel on its own timer: 6 h by default, randomized, with backoff on failure. It verifies
  pointer and manifest signatures, the digest binding, expiry and sequence (no rollback to older metadata). Then it
  resolves only the installed or selected packages plus required dependencies.
* A normal update never adds packages. A new version that needs a package which is not installed is reported as
  blocked (`meduza-np update plan`).
* Apply: downloads are verified into staging without holding any lock. Then: planner maintenance lock → drain
  affected instances → switch → restart through the real owner (planner restart, or a rescan plus restart of affected
  instances only) → verify that every changed package is registered at its new version → unlock. Failure ⇒ previous
  generation and the same restarts; the group is remembered and not retried automatically (`meduza-np update apply`
  retries it explicitly).
* Offline import (`MEDUZA_NP_UPDATE_BASE_URL=file:///…`): copy a newer release site over the import directory **in
  place**; a replaced directory is not visible to an already running updater container until it restarts.
* Controls: `meduza-np update status|check|plan|apply|pause|resume`, `MEDUZA_NP_UPDATE_AUTO=0`,
  `MEDUZA_NP_UPDATE_WINDOW=02:00-05:00` (UTC), `MEDUZA_NP_GROUP_VERSION=vX` to pin a group.
* OpenWrt: packages are installed one at a time with the native package manager. That is **not** an atomic switch.
  Packages without a cached previous file cannot be rolled back automatically; the plan says so.
* Updater self-update: the new updater is activated last and started by its launcher (container entrypoint,
  `updater-run` under systemd/procd). If it fails three times within a minute, the baseline copy starts and rolls the
  generation back.

Verified on Podman (bridge): bootstrap, re-creation with state kept, a planner-only upgrade with restart, and a
broken release rolled back (`tools/container-e2e.sh`). Verified in the multi-node lab (`tools/lab/`): fleet upgrades
with planner restart, plugin upgrade and updater self-update cost 0 packets on WireGuard tunnels. Daemon-based
services (OpenVPN, FRR, dnsmasq, tinc) keep running across a planner-only upgrade and are adopted by the new
planner (D32); they restart when their own package, binary or configuration changed, and `systemctl stop`/`restart`
of the planner service (or the procd init script) stops them.

## 5. Recover

| Situation | What happens / what to do |
| --- | --- |
| Manager unreachable | planner keeps running the last good configuration and backs off; local status still works (CLI, dashboard via break-glass link, LuCI) |
| Power loss during an update | on start the updater reads its journal: before the switch ⇒ nothing changed; after ⇒ verify, else roll back |
| Power loss during a config apply | planner finds the pending-apply journal and reconciles to the last good configuration |
| Update keeps failing | `meduza-np update status` shows the last journal and reason; fix the cause, then `meduza-np update apply` |
| Expired channel metadata | nodes refuse it (freeze protection); the release side re-signs the pointer (`meduza-np-release sign-pointer`) |
| Release key rotation | releases are signed with old + new key while nodes receive the new trust store through the updater package |
| Container re-created / base image replaced | volumes keep identity, config, installed set, journal and provider data; nothing is re-downloaded |
| New base image (Podman) | `deploy/podman/base-image.sh plan`, then `replace`: verified digest from the signed group, store check by the new image, health check, automatic restore of the previous image; `rollback` returns to it later (base-container README) |
| Stop a service for maintenance | `meduza-np maintenance start --reason … --minutes 30 --pause-apply`, then `meduza-np instance stop …` |
| Remove a component (container) | drop it from `MEDUZA_NP_COMPONENTS` and restart the updater: it is deactivated; identity, configs and manual provider files stay |
| Uninstall | stop the services, remove the packages or units; delete `/var/lib/meduza-np` (or the volumes) only if the node identity should go too |

## 6. Routing roles (D17, D18, D24, D25)

- There are no FRR settings to write. Internal LANs: every Tinc mesh member always runs iBGP + BFD with all the
  other members, and only with them (D25), once Global settings has an internal ASN (BFD timers and ECMP paths are
  global too). External networks: set `bgp` on the WireGuard peer or OpenVPN Site2Site instance that carries the
  session — remote ASN, accepted prefixes, what to advertise (`exportInternal` advertises the whole internal AS);
  `noTransit` and `noForward` are on by default. A node outside the internal network sets its own AS (node
  `routing.asn`, Network tab).
- Anycast: declare the pool and services once in the cluster document (`routing.anycast`, a /30 each) and choose
  the nodes per service in Global settings (stored as each node's `routing.anycast`). The service must listen on a
  wildcard address (its /30 addresses exist only while it is healthy) and answer the health probe on 127.0.0.1 unless
  `health.host` says otherwise. Every mesh member routes the pool, also without local services.
- Roaming nodes join the Tinc mesh like the others (D27), without publicAddresses: they dial out to the static
  members. Their iBGP sessions use no BFD and the timers 3/9 automatically; what they only transit is the last
  resort. If a roaming node also has an eBGP session, give it short timers too (`keepalive` 3, `hold` 9): the hold
  timer is what detects a lost roaming peer.

## 7. Checks to run after changes

```sh
meduza-np status
meduza-np doctor
meduza-np update status
```

Planner logs: `podman logs meduza-np-planner` (container), `journalctl -u meduza-np-planner` (systemd; levels map
to journal priorities, so `journalctl -u meduza-np-planner -p warning` shows only warnings and errors), `logread -e
meduza-np-planner` (OpenWrt; INFO/DEBUG as `daemon.info`, WARN/ERROR and daemon output as `daemon.err`). Set
`MEDUZA_NP_LOG=debug` for more detail.

For a support request, attach the redacted support bundle (status report, doctor checks, packages, applied
instances, last applied configuration with a dry-run validation, recent planner logs; credentials removed, at most
192 KiB):

```sh
meduza-np support-bundle -o /tmp/meduza-np-support.json
```

The same bundle is the result of **Collect diagnostics** on the node's Diagnostics tab in the manager (with a
Download button under Jobs) and of **导出支持包** on the dashboard's diagnostics page. Read it before you send it:
redaction covers secret-like keys, PEM private keys and credentials in URLs/log text, not host names or addresses.
