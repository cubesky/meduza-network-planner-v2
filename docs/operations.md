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

The initial view frames managed nodes and all configured direct connections, including down or unreported
neighbors. Learned remote AS-path nodes remain in the graph without expanding that initial frame. AS clusters
spread outward in three dimensions by hop count from managed nodes; more distant ASes continue along their
upstream branches. Spacing defaults to 2× and retains any existing browser preference. Auto-rotation is off by default. Background
drag rotates with a fixed up direction and bounded tilt; Shift-drag or right/middle drag pans in screen directions.
The wheel zooms towards the pointer, and manual navigation is not corrected by automatic fitting. Dragging a node
continues across periodic report refreshes. Touch supports two-finger pan and pinch zoom.

Use **Fit graph** for the complete current topology, **Focus selection** (or double-click a cluster) for a local
view, and **Reset view** to restore the initial orientation and direct-connection frame. **Fit graph** includes
the learned remote AS paths and is intentionally a wider view. Window resizing preserves the chosen scope.
The canvas also supports arrows to rotate,
Shift+arrows to pan, `+`/`-` to zoom and `0` to reset. Cluster spacing is remembered in this browser. Selecting a
node/link/cluster emphasizes its connections without restarting the settled layout. Node labels use the same
centered translucent depth-sorted panels as AS labels, with no leader lines. Zoom in for node labels, or hover
or select a node/link/cluster to reveal its relevant labels immediately. Labels themselves remain hoverable
and clickable; dragging a node by its label preserves the pointer offset. Node/router labels attach directly
to the right of their projected glyph, vertically centered with a fixed gap. They never choose another side,
move away from a crossing link, or shift to make room for another label. AS panels avoid the already anchored
node labels instead. At the viewport edge the canvas clips the panel instead of moving its attachment.
AS numbers and names remain on separate lines and reserve their full bounds when avoiding other labels. Zoom thresholds use hysteresis to
keep detail labels from flickering around the boundary.

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

### OpenVPN reports `process not running` with no startup log

First distinguish missing software, native process failure and a plan that was never generated:

```sh
meduza-np --json doctor
meduza-np --json instances
meduza-np logs -n 100
```

On OpenWrt the system `openvpn-openssl` package normally supplies `/usr/sbin/openvpn`; use the actual
`provider:openvpn.binary` doctor result to verify which executable planner resolved. Compare the running
planner's PATH with the shell if necessary. A registered adapter and a working system provider do not
prove any OpenVPN instance was scheduled.

If `applied`/`processes` contain no OpenVPN instance and the current generation has no OpenVPN directory,
inspect only the held-back feature names in persistent state:

```sh
jsonfilter -i /etc/meduza-np-state/config/last-good.json -e '@.heldBack[*]'
```

D94 fixes an OpenWrt incident where eight enabled tunnels never start because three disabled instances
retain secret references. Manager correctly omits the disabled instances' secrets, but the old planner
collects those references and holds back the whole OpenVPN feature. Other services and the revision can
still be applied, while OpenVPN probes report `process not running`. This happens before OpenVPN is
executed, so replacing the system binary or retrying peer connections does not address that gate.

Upgrade planner with D94 and reapply/restore the configuration. If an immediate workaround is needed,
remove only the unused disabled instance entries in this node's Manager configuration and apply it;
preserve any configuration you intend to reuse. Do not enable those instances or deliver their private
keys just to bypass the old scanner. Genuine missing secrets in enabled instances still require repair.
Verify actual process startup and tunnel/peer state afterward; a successful read-only `plan` alone does
not verify the native binary's runtime compatibility or establish a tunnel.

## 8. Side-router cutover: real gateway traffic, recovery and DNS privacy

Test from a separate authorized client whose actual gateway is the planner's LAN address. A working
SOCKS listener verifies Mihomo's outbound path but does not verify transparent interception. Compare
real gateway HTTPS with SOCKS, and separately test front-end DNS, external UDP/TCP DNS and private
routed destinations. Keep the administrator's workstation gateway and production host routes intact.

If transparent TCP times out while SOCKS works, inspect all three parts inside the planner namespace:

```sh
podman exec meduza-np-planner nft list table inet meduza_np_mihomo
podman exec meduza-np-planner ip -4 rule show
podman exec meduza-np-planner ip -4 route show table 4626
podman exec meduza-np-planner ip -4 route show table 4627
podman exec meduza-np-planner ip -4 route get 1.1.1.1 mark 0x4d4e
```

The defaults are configurable; use the actual plan's marks and tables. TPROXY requires a local default
route in its policy table. Side-router replies require a separate default route to the main router.
`FIB table does not exist` is missing kernel state, even when persistent ownership and applied revision
look correct. D90 makes the next apply/startup restore those owned routes automatically.

For the diagnosed deployment, the following are the minimal recovery commands **after confirming the
ownership journal, current mark rules, gateway and deployment authorization**:

```sh
podman exec meduza-np-planner ip -4 route replace local 0.0.0.0/0 dev lo table 4626
podman exec meduza-np-planner ip -4 route replace default via 10.42.10.1 table 4627
```

These restore the two routes already declared by Meduza, without restarting a container. Verify that
the proxy mark resolves locally and the reply mark uses table 4627, then repeat HTTPS tests from the
separate client. For a different deployment, derive the values from its plan. Do not flush route tables.
For source verification on a disposable privileged Linux client, build the planner example and run
`unshare -n ./route_recovery_probe --isolated`; it refuses the initial network namespace and creates
only throwaway links/routes in the new namespace.

DNS privacy needs its own review:

- Front-end `no-resolv` prevents silently using the container resolver. `strict-order` enforces the
  generated backend priority. Health-based removal and plaintext fallback deliberately preserve DNS
  availability, but a healthy listener alone does not prove upstream policy or privacy.
- An operator-supplied MosDNS `pluginsYaml` pipeline controls its own upstreams and fallback. A remote
  primary with a domestic secondary can disclose the queried name to that secondary when fallback
  starts. For strict remote resolution, use remote encrypted upstreams through the intended proxy and
  keep domestic fallback out of that branch. Check every remote upstream, not just the first entry.
- The audited custom pipeline includes an Apple DoH entry without SOCKS and a 1000 ms domestic fallback
  for its `try-resolve@!cn` branch. Several foreign-domain rules bypass that branch and use remote
  resolution directly; the fallback risk does not mean every foreign query uses domestic DNS.
- Mihomo can inherit DNS policy from its subscription. Check the final DNS configuration, including
  nameserver policies, fallback filters and proxy-server bootstrap, rather than assuming the node's
  empty override removes subscription behavior. DoH encryption alone does not choose a proxy route.
- Distinguish queries for resolver/proxy hostnames from client domain queries. During this investigation
  two public plaintext resolvers returned loopback for a remote DoH hostname, while the front-end and
  main router returned a public address. Do not fix bootstrap by routing it back into a dependent
  front end or by sending all client names to an unrelated plaintext resolver.
- TPROXY's configured port 53 exclusion permits ordinary routed DNS. DHCP pointing clients at the
  front end does not prevent a client choosing another DNS/DoH provider. DNS interception, egress
  restrictions and browser secure-DNS policy require an explicit policy beyond DHCP configuration.
- TCP-only interception preserves the previous selected protocol scope; UDP/QUIC remains direct.
  Enable UDP interception only with the required UDP routing/proxy support and separate validation.

Custom policy changes belong in Manager configuration, not generated files under `/run`. Recheck
Google/Bing HTTPS, domestic HTTPS, DNS answers and latency after both routing and policy changes.

### Correct tables but transparent TCP still times out: marked source validation (D91)

The post-upgrade retest recovered tables 4626/4627 and their marked lookups, but the same separate
client still timed out on all five gateway HTTPS targets while SOCKS succeeded. The actual Compose
file retained `net.ipv4.conf.all.src_valid_mark: '1'`. A real transparent TCP listener in isolated
client/router namespaces reproduces this failure: all=1 blocks, all=0 with ingress=0 connects, and an
ingress override of 1 blocks again. Disabling `rp_filter` alone also remains blocked.

The mark is for local delivery in one direction. Linux's source validation must look up the reverse
route without that mark. Its effective `src_valid_mark` is the maximum of all and the ingress interface;
see the [kernel sysctl reference](https://docs.kernel.org/networking/ip-sysctl.html).

For the diagnosed `/var/standalone/meduza/compose.yml`, edit the existing `planner.sysctls` mapping,
preserving its other settings:

```yaml
services:
  planner:
    sysctls:
      net.ipv4.ip_forward: '1'
      net.ipv4.conf.all.src_valid_mark: '0'
      net.ipv4.conf.default.src_valid_mark: '0'
```

After reviewing the actual file, recreate only planner from that directory:

```sh
podman compose up -d --no-deps --force-recreate planner
podman exec meduza-np-planner sh -c 'for i in all default eth0; do echo "$i"; cat /proc/sys/net/ipv4/conf/$i/src_valid_mark; done'
```

The expected values for this deployment are all zero. Use the actual ingress name in other deployments;
an existing explicit per-interface override of 1 must also be corrected. Package upgrades, an ordinary
restart and editing generated Mihomo files do not replace container creation parameters. Recreating
planner briefly interrupts its network services; updater and host routes need no change. Check the
owned policy tables again after the namespace is recreated, then retest from the isolated client.

Updated `run.sh` and both independent-network Quadlets use zero for all/default. Host-network units
still leave host sysctls under administrator control. Run
`sudo python3 tools/container-network-test.py --real-tproxy` on an authorized disposable Linux test
device with nftables and TPROXY support to exercise real handshakes and both effective-value cases.
This command creates throwaway namespaces; it does not change the test device's primary network.

In the diagnosed deployment, after the user changes Compose and recreates planner, both owned tables
are automatically restored and all/ingress values read zero. Two rounds of real-gateway HTTPS to all
five targets and SOCKS controls pass, along with front-end and ordinary external UDP/TCP DNS. That
verifies the observed IPv4 TCP outage is repaired; it does not change the DNS privacy policy above.

### WireGuard and OpenVPN instance failures

A configuration can be `in-sync` while an individual tunnel is down or degraded. Inspect the named
instance in Manager or Dashboard: its reason identifies a missing dependency, invalid configuration,
native check, resource, reload or process-start failure. The same row includes `applyErrorCode`,
`attempt` and `retryAtUnix` in its diagnostic details. Other instances continue applying and running.

Missing keys and failed native checks retain that instance's previously applied configuration where
available. A failed runtime delivery cleans only that instance. Failed instances retry automatically
with backoff capped at sixty seconds; healthy instances keep their processes. Use the instance's
start/restart action to retry immediately after fixing a key, dependency or host condition. Maintenance
and a pending/failed global revision defer these retries. Peer availability errors remain runtime
health observations under connection supervision.

An explicitly disabled WireGuard/OpenVPN service, instance or peer is excluded from runtime validation,
secret delivery, probing and failure retries. Disabled drafts may retain unfinished settings; enabling
them restores validation. An applied disable stops the owned instance processes/resources or removes
the WireGuard peer from native configuration. Its status becomes `disabled` without stale errors or
retry details. Restart actions cannot override this configuration: enable and apply the entry first.

### System OpenVPN without management

Some system binaries omit management, and small builds may omit the full --help output too. A native
error naming `management` is independent of a `persist-key` deprecation warning. The adapter now makes
only management optional; crypto/key/config errors remain required, per-instance failures. After
updating meduza-np-openvpn, reapply the configuration (the normal package rescan/retry can also do so).
Do not edit a generated config as a persistent fix.

Without management, PID ownership, device presence and fresh native status counters remain observable.
Precise p2p session state is explicitly unavailable rather than guessed from a running process. For
full session observations and selective client disconnection, use a management-enabled provider;
meduza-np-openvpn-pkg already includes management support. No provider is automatically replaced just
because a system binary has reduced monitoring capabilities.

### Full retained BGP paths in Manager

Upgrade Manager and each observing node's meduza-np-frr adapter, then open Topology → BGP paths.
The default table includes every received IPv4/IPv6 candidate currently saved in FRR's post-policy
unicast RIB, including non-best and invalid paths. It excludes routes rejected before storage and
does not enable extra Adj-RIB-In retention. Search by node, peer, prefix, AS number/name or next hop;
use the state filter and page controls to inspect the complete report. The table preserves original
AS prepends, sets and confederation notation alongside prefix, family, flags and next hops.

The graph continues AS paths beyond already-known ASNs, including observations learned through
managed peers or iBGP. An AS-level inferred link does not establish which physical router inside
that AS forwards the route. Unordered AS sets do not imply ordered links. Reset still frames managed
nodes and configured direct peers; Fit all includes remote path nodes.

An older node displays an upgrade notice. Query failures, timeouts or output exceeding the existing
four-MiB/five-second query limits display incomplete collection; inspect the reported family/error
instead of treating missing records as an empty RIB. Existing plugin-response bounds also apply, so
full Internet tables may exceed this transport. Neither the view nor the adapter changes BGP policies,
neighbor configuration or forwarding state.

### LDAP settings rejected with HTTP 422

For ldap:// URLs, enable StartTLS and ensure the directory supports it. Alternatively use ldaps://
with StartTLS disabled, since TLS is already established by that protocol. Disabling certificate
verification does not permit plaintext LDAP. Prefer a trusted CA and certificate verification.
Saving settings does not itself prove that the directory connection succeeds; use the connection
test after saving the corrected configuration.

Manager now explains incompatible transport choices beside URL and blocks save before changing the
bind password. Older interfaces may display undefined for a textual validation problem; inspect the
response's problems array or update Manager to obtain the actual reason. Backend rejection leaves
the previous LDAP settings intact.

### Access clients reach the node but LAN TCP handshakes stall

Check the client OPTIONS for the pushed route, then check the actual LAN destination and port. A LAN
server in SYN_RECEIVED while the same client reaches the Meduza Dashboard can indicate misrouted
replies. With the affected Mihomo side-router rules, replies receive the upstream mark before private
or routing-system exclusions. Compare `ip route get CLIENT_IP` with
`ip route get CLIENT_IP mark 0x4d50` inside the planner container; adjust the mark to the configured
fwmark plus two. If only the marked lookup sends the VPN destination to the main router, update
meduza-np-mihomo to the D101 fix and reapply. This retains the original OpenVPN client profile.

For the specifically diagnosed .9 deployment, the operator can temporarily restore replies to its
configured bypass destinations without a service restart:

```sh
podman exec meduza-np-planner nft insert rule inet meduza_np_mihomo not_proxied ip daddr @bypass_v4 return
```

This command assumes the existing owned table, chain and bypass_v4 set observed on that deployment.
It restores its 10.42.49.0/24 client return path; a later Mihomo apply replaces this temporary rule.
The packaged fix also preserves public-prefix VPN routes when bypassRouted is enabled. No workstation
gateway, host routes, NAT or broader firewall access changes are needed for this defect.

The standalone real regression uses only synthetic generated tables and isolated namespaces:

```sh
cargo run --locked -q -p meduza-np-mihomo --example reply-routing-fixture > /tmp/reply-fixtures.json
sudo python3 tools/mihomo-reply-network-test.py /tmp/reply-fixtures.json
```
