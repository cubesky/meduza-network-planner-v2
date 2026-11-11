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

Manager node lists, node status/settings, and the dashboard overview/updates page show the updater's package
installation progress: current package, verified download count, actual downloaded bytes, install/activation and
registration phases, failure reason and next automatic check. Manager progress arrives with the node's normal
reports; the dashboard updates page polls every 3 seconds. An applied configuration can still have packages
waiting for installation. A 100% download is followed by installation, system dependency preparation and
registration checks before packages are ready. Older updaters expose phases/errors without invented byte counts.

Tinc mesh and active WireGuard/OpenVPN external connections automatically request `meduza-np-frr` in the planner's
component list, without requiring a handwritten FRR service. OpenVPN Access-only instances and disabled connections
do not add this routing prerequisite. The manager continues to derive FRR's actual routing configuration from the
internal ASN, peers, external BGP and Anycast settings. The base image installs the Alpine `frr` dependency when the
new adapter arrives; FRR still needs `SYS_ADMIN` in the planner container (fixed when creating the container).

* The updater checks the channel on its own timer: 6 h by default, randomized, with backoff on failure. It verifies
  pointer and manifest signatures, the digest binding, expiry and sequence (no rollback to older metadata). Then it
  resolves only the installed or selected packages plus required dependencies.
* Transient download errors (connection/DNS failures, TLS stream interruptions, timeouts, HTTP 408/425/429 and
  5xx) retry the individual file automatically: at most four attempts, with 2/4/8-second exponential waits plus
  up to 20% random jitter. The original timeout/size limit applies to each attempt. Metadata downloads use the
  same policy. The progress panel shows the attempt number, failure reason and next retry time. After all attempts
  fail, the daemon's longer automatic backoff still applies; a download failure does not blacklist the group.
  Previously completed staging files are reused only after their size and SHA-256 match the signed manifest;
  partial/unverified files are downloaded again. Invalid signatures/digests, certificate trust errors, local I/O
  failures, oversized responses and permanent HTTP errors are not blindly retried within a pass.
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
* In Manager, open a node's **Packages and updates** card and use **Check for updates** or **Upgrade now**.
  Admin/operator sessions may submit requests; viewers see progress and request results. Check refreshes
  available versions without installing, even if automatic updates are enabled. Upgrade now uses that node's
  configured signed source/channel or pinned group, bypasses pause/window for this execution and allows a
  previously failed group to be retried; it does not change automatic-update settings. Requests expire after
  ten minutes if not received. Fixed nodes get the existing job notification; roaming nodes receive it on sync.
  Pending/delivered/accepted/failed/expired request states appear in the card. Accepted means scheduled, not
  installed: follow the separate node-reported progress for activation, updater takeover, completion or rollback.
  Remote controls require a new planner reporting `update.remoteControl`; older nodes display a disabled control
  with an upgrade explanation. Duplicate pending requests reuse the same job; node receipts survive planner
  replacement to avoid repeating an acknowledged command after report delivery fails.
* Manager node details show **Packages and updates** above the tabs; node lists and overview have an update
  column. Completed upgrades retain their group, download size/count and completion time. Missing updater
  reports are labelled unavailable rather than disappearing. Download 100% can still mean activation or
  registration checks are running; configuration application and tunnel health are shown separately. Current
  v0.1.5 node reports work with this display; newer nodes additionally report available versions and the last
  upgrade/check result. Dashboard uses the same progress presentation.
  Configuration failure and recovery failure are distinct: `sync.lastError` retains the desired revision's
  apply failure, while optional `sync.recoveryError` identifies the historical revision whose restore degraded.
  Manager shows a separate recovery warning and Dashboard lists both; successful recovery clears the latter.
* **Mihomo binary download** is a separate card in node details and Dashboard: it shows bytes, checksum/
  extraction/executable checks, activation, attempts and retry time. Installing `mihomo-pkg` alone does not mean
  its binary is ready. A failed configuration can stop its supervised preparer; the card labels interruption
  and shows the configuration error. Fix/apply configuration to start preparation; Download now requests an
  immediate retry of the applied preparer. Manual mode continues to avoid network acquisition.
* dnsmasq `listenAddresses` may be omitted or empty to serve all local interfaces. Supply addresses to restrict
  the listener. The default still detects port conflicts and local resolution loops; no DHCP is enabled.
* OpenWrt: packages are installed one at a time with the native package manager. That is **not** an atomic switch.
  Packages without a cached previous file cannot be rolled back automatically; the plan says so.
* Old downloads are cleaned before update checks, after completed/failed upgrades and during startup recovery,
  including when no new version is installed. Store objects/views and native package archives keep the current
  set and one valid rollback baseline; running/adoptable executables and pending self-update files remain
  protected. Failed attempts do not erase the last successful rollback baseline. Mihomo auto-download cleans
  recognized obsolete archives and failed candidates, keeping its current and one previous binary. Manual
  binaries and unknown user files are preserved. Cleanup shares the download/transaction lock and does not
  traverse internal symlinks. No manual cache cleanup is needed after an ordinary failed transfer.
* The OpenVPN provider builds LZO and LZ4 into its static executable. Compression/framing options such as
  `lzo-asym` and `lzo-no` work with legacy peers without installing compression libraries on the host or changing
  the base image. Per-connection policy still controls use: the pinned OpenVPN 2.7 accepts compressed traffic
  asymmetrically and does not send compressed packets. Replacing an older compression-disabled provider needs
  a newly released `meduza-np-openvpn-pkg`; rebuilding only the service adapter does not add binary capabilities.
* Mutual updates: if updater changes, planner first invokes a short-lived helper that activates **only updater**.
  Its independent launcher (container entrypoint or `updater-run` under systemd/procd) starts the new updater,
  which immediately resumes the staged transaction and upgrades planner and the remaining packages. No second
  channel check or download is needed. Until then the installed group remains the previous group and progress
  says `self-restart`. If takeover fails, planner stays on its original version; if the subsequent planner/package
  update fails, rollback preserves the successfully started new updater. An unavailable or previous-version
  planner uses the independent bootstrap/compatibility helper. Unchanged package versions may be reused across
  group tags: for example v0.1.7 explicitly reuses planner/updater 0.1.6, so that is not a missed upgrade.

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
| External VPN peer unreachable | the configuration still applies; OpenVPN Site2Site instances (client and server roles) keep reconnecting, including after repeated process exits (jittered exponential waits, at most 60 seconds); WireGuard retries handshakes when traffic/keepalive requires it. Connection health stays down/starting/degraded until recovery; local file/provider/check/interface failures still fail apply |
| WireGuard endpoint DNS fails | local interface/keys/peers still apply; an instance-owned runner resolves each hostname separately and retries with jitter capped at 60 seconds. A failed peer keeps its old endpoint while other peers/services continue. Successful names refresh every 60 seconds. Manager Connections and Dashboard Instances show the failure on the corresponding row, without marking the revision failed; no republish is needed after DNS recovery |
| Power loss during an update | on start the updater reads its journal: before the switch ⇒ nothing changed; after ⇒ verify, else roll back |
| Power loss during a config apply | planner finds the pending-apply journal and reconciles to the last good configuration |
| Update keeps failing | `meduza-np update status` shows the last journal and reason; fix the cause, then `meduza-np update apply` |
| Expired channel metadata | nodes refuse it (freeze protection); the release side re-signs the pointer (`meduza-np-release sign-pointer`) |
| Release key rotation | releases are signed with old + new key while nodes receive the new trust store through the updater package |
| Container re-created / base image replaced | volumes keep identity, config, installed set, journal and provider data; nothing is re-downloaded |
| New base image (Podman) | `deploy/podman/base-image.sh plan`, then `replace`: verified digest from the signed group, store check by the new image, health check, automatic restore of the previous image; `rollback` returns to it later (base-container README) |
| Stop a service for maintenance | `meduza-np maintenance start --reason … --minutes 30 --pause-apply`, then `meduza-np instance stop …` |
| Remove a component (container) | disable/remove its services in the published manager configuration and drop it from `MEDUZA_NP_COMPONENTS`, then re-create containers with `run.sh --again` (or restart the Quadlet services); identity, configs and manual provider files stay |
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

### Mihomo DNS and TPROXY on a side router

Mihomo can serve its native panel at `http://<controller-address>:<controller-port>/ui/`.
In the node's Mihomo settings, **Web panel** offers `auto` (inherit the subscription's panel), `enabled`
(use the subscription/custom URL, or MetaCubeXD if none is provided) and `disabled`. The optional archive
URL overrides the subscription. For example, merge this into the existing service, preserving its API secret:

```json
{
  "webUi": { "mode": "enabled" },
  "api": { "address": "0.0.0.0", "port": 9090, "secretRef": "<existing-per-node-secret>" }
}
```

Set the API address explicitly when LAN access is wanted; its default remains loopback. After saving and
applying, open `/ui/` on that API address and configure the panel with the same API endpoint and node secret.
Mihomo downloads/unpacks the panel and keeps it in persistent storage for offline restarts. Subscription
paths are replaced with an owned cache directory. A source change switches to a new cache, removing the
previous one only after the replacement is ready. Download failures leave the proxy running; inspect the
Mihomo process log for its native UI downloader error. Updates of an unchanged source can use the panel's
native UI update function. The project does not bundle panel files into the adapter package.

Dashboard and LuCI show **Open Mihomo panel** only while Mihomo and its native panel are enabled.
The Dashboard login page also shows this entry and permits direct connection without signing in, as
requested: anyone who can access this enabled Dashboard entry can use the Mihomo controller credential.
Clicking it (or using LuCI with write access) opens the native panel and supplies its endpoint,
port and secret using MetaCubeXD's setup fragment. The secret is fetched on demand and never added
to the general report or public availability response. Wildcard listeners use the browser's node hostname,
including IPv6; loopback-only listeners require a local browser or an explicit LAN API address.
The panel must finish downloading before the credential connection becomes available. Upgrade planner,
cli, mihomo and the selected dashboard package together for this button. Custom panels need to understand
the MetaCubeXD setup-link format to auto-connect.

Mihomo DNS is configured explicitly in the node's Mihomo settings. Subscription DNS is replaced, including its
listener and upstreams. Enabling TPROXY alone does not enable Mihomo DNS or install/enable dnsmasq. A fragment
to merge into an existing Mihomo service (keep its API secret and subscription settings) is:

```json
{
  "mode": "tproxy",
  "socksPort": 7891,
  "dns": {
    "enabled": true,
    "listen": "127.0.0.1:1053",
    "nameservers": ["https://doh.pub/dns-query"],
    "defaultNameservers": ["119.29.29.29"],
    "proxyServerNameservers": ["119.29.29.29"],
    "fakeIp": true,
    "fakeIpFilter": ["*.lan", "+.local"]
  }
}
```

Enable the client-facing dnsmasq with its listener; no backend list is needed:

```json
{
  "enabled": true,
  "listenAddresses": ["10.42.10.12"]
}
```

The front end automatically discovers enabled, installed MosDNS and Mihomo DNS services and uses their actual
listener addresses/ports (wildcard backend listeners are queried over loopback). MosDNS is preferred, then Mihomo.
DNS queries must succeed before a backend is added. Failure removes it; recovery adds it again. Disabling/removing
a service removes it on reload, and moving its listener requires fresh successful probes. These changes update
the servers-file with SIGHUP and keep the front-end process running. The status panel shows the discovered
backends, actual addresses, readiness and current upstreams. OpenClash integration requires `dnsIntegration` and
uses the DNS port reported by its adapter, following port changes without taking control of OpenClash.

Fallback defaults to `119.29.29.29` and `1.0.0.1`, so it works before any backend is ready; set `fallbackServers`
only to override these defaults. An unset or empty list uses the defaults. With no healthy backend, fallback is
restored automatically. Legacy manual `backends` entries are ignored and no longer appear in the configuration
form. Enabling the front end does not install or enable any backend service.

The main router's DHCP advertises `10.42.10.12` as gateway and DNS to clients.
The container itself keeps its main-router gateway (`10.42.10.1`) and independent system DNS (`119.29.29.29`);
planner/updater and subscription downloads are never redirected through TPROXY. Mihomo only intercepts forwarded
IPv4 traffic from the selected LAN sources. It declares IPv4 forwarding even when no NAT rules are configured.

Leaving Mihomo DNS `nameservers` empty preserves the v1 startup behavior: inherit the subscription's DNS
resolution policy, or the core defaults for keys absent from that subscription. No duplicate node resolver
list is required before downloading. A nonempty node `nameservers` list selects custom DNS instead. The
project always controls DNS enable/listen/IPv6 and checks the effective inherited policy for DNS loops before
accepting an update. Failed subscription refreshes keep the previous usable subscription/configuration.

The DNS resolver-policy tab supports `fallback`, `fallbackFilter`, `nameserverPolicy`, `proxyServerNameservers`,
`proxyServerNameserverPolicy`, `directNameservers`, `directNameserverFollowPolicy` and `respectRules`; the Fake-IP
tab sets its range, filter and filter mode. Policy maps accept a resolver string or an array per domain. All
resolver groups are checked against the enabled dnsmasq front end and Mihomo's own DNS listener. `respectRules`
requires proxy-server resolvers in the effective subscription/custom policy to bootstrap proxy hostnames.
Empty node bootstrap lists are omitted so they do not suppress core defaults. These options map to the corresponding
[Mihomo DNS keys](https://wiki.metacubex.one/en/config/dns/).

For legacy MosDNS configurations referring to SOCKS port `7891`, set `socksPort: 7891`; the etcd importer does this
automatically. New configurations open the optional dedicated SOCKS port only when configured (the mixed listener
also supports SOCKS). `tproxy.excludePortRules` preserves the previous `[(in|out):][(tcp|udp):]port` syntax:
`in:tcp:80` excludes destination TCP port 80, `out:udp:123` excludes source UDP port 123, and bare `53` excludes
both directions of both selected protocols. Existing `excludePorts` keeps its destination-only meaning.
`excludeInterfaces` complements the automatic mesh/VPN device exclusions. Imported subscription DNS needs explicit
configuration because the etcd export does not include the downloaded subscription's DNS settings.

### Managing binary resources

Node Settings → Features presents **Mihomo binary resource**, **MosDNS binary resource** and **OpenVPN binary
resource** with the same installation-request controls, installed package version badge and resource status
columns. The component package version and the upstream binary version are separate. Installed resources can
be expanded even without Manager configuration; in that case the panel shows status without preparation actions.

Mihomo keeps its manual-file / pinned-download source settings and check/activate/download actions. Its extra
source, preparation, platform, verification and download-stage fields appear below the common status table.
These actions are offered once its package is registered and its configuration is enabled. The package remains
a lightweight acquisition adapter; it does not bundle Mihomo itself. Older reports' binary version fields remain
readable; the actual active provider requires the updated adapter's report and is not guessed from source metadata.

Node Settings → Features includes **MosDNS binary resource** and **OpenVPN binary resource**, independently of
the service adapter settings. Add/enable the installation request and apply the working copy to let the updater
install the corresponding `meduza-np-mosdns-pkg` / `meduza-np-openvpn-pkg` component. These packages contain pinned
binaries; they do not use Mihomo's separate manual-file/download preparation flow or start an extra daemon.

The resource panel shows the component package version, bundled upstream version, actual active provider, binary
path and any resolution failure. Installed resources can be inspected even without a Manager installation request.
They register their binary slots independently of whether the service adapter has been installed. A missing binary
is reported as unavailable, rather than silently advertised as a healthy system fallback.

Stopping/removing the request stops asking the updater for that component. It does not forcibly uninstall a package
retained by `MEDUZA_NP_COMPONENTS` or the updater's existing installed-set selection; change that deployment/selection
when removing such a package. The reported active provider remains the source of truth. Runtime restarts belong to
the Mihomo/MosDNS/OpenVPN service instances, not these binary resources. Mihomo's actual download-preparer
process retains its process restart control.

### Navigating the topology

The initial view keeps AS clusters apart with adjustable spacing; auto-rotation is off by default. Background
drag rotates with a fixed up direction and bounded tilt; Shift-drag or right/middle drag pans in screen directions.
The wheel zooms towards the pointer, and manual navigation is not corrected by automatic fitting. Dragging a node
continues across periodic report refreshes. Touch supports two-finger pan and pinch zoom.

Use **Fit graph** for the complete current topology, **Focus selection** (or double-click a cluster) for a local
view, and **Reset view** to restore orientation and framing. The canvas also supports arrows to rotate,
Shift+arrows to pan, `+`/`-` to zoom and `0` to reset. Cluster spacing is remembered in this browser. Selecting a
node/link/cluster emphasizes its connections; labels avoid overlapping one another where space permits.

On a node's **Connections** tab, WireGuard and OpenVPN restart controls appear at the end of each peer/session
row. Confirmation names the owning instance: restarting an instance briefly disconnects all of its peers or
sessions. Viewers cannot restart instances.

The **Diagnostics** package list includes the running `meduza-np-base-container` image even though updater does
not install it into the package store. Its image version is independent of the updater version. Older images
without their own version record show **unknown version** in the installed column; a new planner report is
required for this detection to reach Manager.

## 7. Checks to run after changes

Manager's node **Status** page and Dashboard's overview/packages show **Package resource usage**. Package disk
and persistent service/provider data are separate; disk measurements refresh every 60 seconds and include
retained store versions. Expand each package's process section for OpenVPN instances, FRR daemons, child PIDs
and RSS. Independent updater/Dashboard containers report their own samples through the shared runtime volume;
upgrade these components as well as planner and Manager. A sample expires after 30 seconds without refresh.
**Not measured** means unavailable, never zero. RSS includes shared pages; binary providers are not charged a
second copy of their service's RSS. Instances share package files. Kernel memory, shared system dependencies,
download archives and OCI layers are outside this per-package accounting. The base image shows an explanation.

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
