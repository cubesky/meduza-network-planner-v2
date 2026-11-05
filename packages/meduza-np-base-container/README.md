# meduza-np-base-container

Alpine-based node runtime for non-OpenWrt hosts (AGENT.md §14.3). One image, two roles, two containers:

| Role | Container | Network | Volumes | Capabilities |
| --- | --- | --- | --- | --- |
| `updater` (default) | `meduza-np-updater` | bridge (`meduza-np`) | `meduza-np-store` rw, `meduza-np-ipc` | none beyond file ownership |
| `planner` | `meduza-np-planner` | bridge (default) or host | `meduza-np-node` rw, `meduza-np-store` **ro**, `meduza-np-ipc` | only `CHOWN DAC_OVERRIDE FOWNER KILL SETUID SETGID NET_ADMIN NET_RAW NET_BIND_SERVICE` (+ per component), `/dev/net/tun` |

* The image contains Alpine, the verified updater (`/usr/lib/meduza-np/image/meduza-np-updater`, version in label
  `io.meduza-np.updater.version`), the release trust store (`/usr/share/meduza-np/trust/release-keys.json`) and the role
  entrypoint. planner, plugins and providers are **not** baked in.
* The updater installs planner + `MEDUZA_NP_COMPONENTS` (+ required dependencies) into the store volume, verifying
  signatures and digests, and keeps them updated. The planner role waits until planner is installed, installs the
  Alpine packages the installed components need (`rootfs/usr/lib/meduza-np/container/apk-deps`, cached in the node
  volume for offline restarts) and runs planner.
* Single writer per volume: only the updater writes the store, only planner writes the node volume. They talk over
  `planner.sock`/`updater.sock` in the shared IPC volume; no control port is published.
* The image's own version is recorded in `/usr/lib/meduza-np/image/base-container.json` and OCI label
  `org.opencontainers.image.version`, independently of the embedded updater version. Builds require both
  `BASE_CONTAINER_VERSION` and `UPDATER_VERSION`. Planner reports the running base image alongside installed
  packages; older images without this record are recognized by their runtime files and shown as installed with
  an unknown version, rather than borrowing the updater or catalog version.

Supported: Podman ≥ 4.4 (Quadlet), tested with Podman 5.4 on Debian 13; Alpine 3.24 base. Rootful is required for
host network mode; bridge mode works rootful and rootless (rootless: the VPN/routing resources live in the
container's own namespace only; see [Rootless](#rootless)).

## Files

| File | Purpose |
| --- | --- |
| `Containerfile` | image build (context prepared by release tooling: `bin/<arch>/meduza-np-updater`, `trust/release-keys.json`) |
| `deploy/podman/*.container, *.volume, *.network` | Quadlet units, bridge mode (copy to `/etc/containers/systemd/`) |
| `deploy/podman/host/meduza-np-planner.container` | planner in host network mode (replaces the bridge planner unit) |
| `deploy/podman/macvlan/` | rootful macvlan network and both roles with independent static LAN addresses |
| `deploy/podman/run.sh` | the same deployment with plain `podman run` (hosts without systemd) |
| `deploy/podman/base-image.sh` | host helper: replace / roll back the base image (verified digest, store check, health, restore) |
| `deploy/podman/meduza-np.env.example` | deployment parameters → `/etc/meduza-np/node.env` |

## Install (Quadlet)

```sh
install -d /etc/meduza-np /etc/containers/systemd
install -m 0600 deploy/podman/meduza-np.env.example /etc/meduza-np/node.env   # edit: components, enrollment
cp deploy/podman/meduza-np*.container deploy/podman/meduza-np*.volume deploy/podman/meduza-np.network /etc/containers/systemd/
systemctl daemon-reload
systemctl start meduza-np-updater meduza-np-planner
```

Pin the image by digest in both `.container` files (`Image=ghcr.io/meduza-np/meduza-np-base-container@sha256:…`):
the `oci` field of the `meduza-np-base-container` artifacts in `releases/<group>/manifest.json` is that multi-platform
reference (D30); the image's `io.meduza-np.updater.version` label names the embedded updater.
For host network mode copy `host/meduza-np-planner.container` instead of the bridge one.
Without systemd: `deploy/podman/run.sh --env-file /etc/meduza-np/node.env [--network host] [--publish 51820/udp]`.
`--publish PORT/PROTO` publishes the same host port (`HOST:PORT/PROTO` to choose another one). After replacing the
planner container run.sh deletes the conntrack entries of its published UDP ports (needs `conntrack-tools` on the
host): a tunnel peer that kept sending meanwhile otherwise stays on a stale entry without the new port mapping and
never reconnects (D42). With Quadlet, add the `ExecStartPost=` line the planner unit shows per published UDP port.

Stopping is graceful: the entrypoint forwards SIGTERM to planner (which stops its daemons) or the updater and exits
0 once it is gone, so `systemctl stop` / `podman stop` / a re-run of run.sh no longer kill planner with the
container (D57; images built before that do).

### macvlan side router

LAN clients receive the **planner's LAN IP** as their gateway and DNS from the main router's DHCP server.
The image's own software (planner, updater, package downloads and other local connections) uses the **main router**
as its upstream gateway and independent system DNS. This bootstrap path works before remote configuration arrives
and while the client-facing DNS/proxy services are stopped. Remote configuration still controls the LAN-facing
services and routes; Mihomo's TPROXY rules act on incoming LAN traffic, not the node's own outbound connections.

Use rootful Podman and a macvlan network. Reserve a fixed LAN address outside the main router's DHCP pool for
planner (the side router); updater can use another fixed address or its own DHCP lease, as described below. Both containers connect directly
to the main router, so updater does not depend on planner or host routing to fetch the first packages. Edit the NIC,
subnet and addresses below for your LAN:

```sh
podman network create --driver macvlan --opt parent=eth0 --ipam-driver host-local \
  --subnet 192.168.1.0/24 --gateway 192.168.1.1 meduza-np-lan
```

In `/etc/meduza-np/node.env`, set the main router's LAN IP and independent DNS servers:

```dotenv
MEDUZA_NP_DEFAULT_GW=192.168.1.1
MEDUZA_NP_DEFAULT_GW_DEV=eth0
MEDUZA_NP_DNS_NAMESERVERS=223.5.5.5,119.29.29.29
MEDUZA_NP_NETWORK_MONITOR_INTERVAL=5
```

The gateway and DNS may use different addresses. `DEFAULT_GW_DEV` names the interface **inside** each container,
not the host parent NIC; omit it to let the kernel find the interface to the gateway. DNS may point to the main
router if it resolves independently of this side router; never point bootstrap DNS at the planner's own IP or a
resolver that forwards back to it.

```sh
deploy/podman/run.sh --env-file /etc/meduza-np/node.env --network macvlan \
  --macvlan-network meduza-np-lan --ip 192.168.1.2 --updater-ip 192.168.1.3
```

`--macvlan-network` selects an existing network (default `meduza-np-lan`); the script verifies the driver before
replacing containers. Both static IP arguments are required and must differ. `run.sh --again` and the base-image
replacement helper retain these settings. A network change requires creating/selecting the appropriate network;
re-running `podman network create` with the same name does not update the existing network.

For Quadlet, edit `deploy/podman/macvlan/meduza-np-lan.network` (parent, subnet, gateway) and the `IP=` in both
macvlan `.container` files. Install these three files instead of the bridge network/container files, together with
the usual volume files, and use the same `node.env`. Pin the image digest in both container files as usual.

For a DHCP updater, leave planner on its static network and create a second macvlan network on the same parent
interface. Podman/Netavark obtains and renews the address; the image overrides the upstream gateway and DNS from
`node.env`, before the updater starts and again if lease renewal changes those settings:

```sh
systemctl enable --now netavark-dhcp-proxy.socket
podman network create --driver macvlan --opt parent=eth0 --ipam-driver dhcp meduza-np-updater-dhcp
deploy/podman/run.sh --env-file /etc/meduza-np/node.env --network macvlan \
  --macvlan-network meduza-np-lan --ip 192.168.1.2 \
  --updater-macvlan-network meduza-np-updater-dhcp --updater-ip dhcp
```

This requires Netavark with DHCP proxy support. The script checks the updater network's DHCP IPAM driver before
replacing containers, and `--again` retains both network choices. For Quadlet, install the optional
`macvlan/meduza-np-updater-dhcp.network`, edit its parent, change the updater's `Network=` to
`meduza-np-updater-dhcp.network` and remove its `IP=` line. Keep planner's static network and `IP=`.
With Compose, attach updater to that external DHCP network without `ipv4_address`; planner keeps its own static
network/address. Separate their network namespaces first if the deployment uses `network_mode: service:updater`,
otherwise moving updater to DHCP also moves the side-router IP used by LAN clients. Keep `NET_ADMIN` and the
gateway/DNS environment variables on updater. The image does not start a second DHCP client or change lease IPs.

| Environment variable | Behavior |
| --- | --- |
| `MEDUZA_NP_DEFAULT_GW` | IPv4 default gateway, applied to both roles before any download/dependency setup; requires declared `macvlan` mode and `NET_ADMIN` |
| `MEDUZA_NP_DEFAULT_GW_DEV` | Optional container interface for that gateway |
| `MEDUZA_NP_DNS_NAMESERVERS` | Comma/whitespace-separated IPv4/IPv6 system resolver addresses; applied to both roles in any mode |
| `MEDUZA_NP_NETWORK_MONITOR_INTERVAL` | Positive integer seconds between gateway/DNS restoration checks; default 5 |
| `MEDUZA_NP_DNS_MONITOR_INTERVAL` | Legacy interval fallback when `MEDUZA_NP_NETWORK_MONITOR_INTERVAL` is unset |

Old-image names remain supported: `DEFAULT_GW`, `DEFAULT_GW_DEV`, `DNS_NAMESERVERS` (or `DNS_NAMESERVER`),
`DNS_MONITOR_INTERVAL`. The corresponding `MEDUZA_NP_*` variable takes precedence, including an explicit empty
value. An unset/empty gateway or DNS leaves the runtime's corresponding setting alone; no public resolver is
forced by default. Invalid values or an unsuccessful initial route/DNS write stop the role with an error before
it starts. DNS preserves resolver search/options and writes through the runtime's mounted `resolv.conf`; it is
restored if another process overwrites its nameserver list. The monitor stops with its role.

The gateway override replaces the metric-zero default route (and takes precedence over defaults with higher
metrics). The monitor restores this explicit route and DNS within the configured interval after a runtime/DHCP
overwrite, preserving leased addresses, connected routes and resolver search/options. A gateway-only override
also starts the monitor. Steady-state checks do not rewrite the route or resolver. Both macvlan units grant updater
`NET_ADMIN` only to maintain this explicit route; updater remains without network administration privileges in the
bridge/host deployment. A gateway override in bridge/host mode is rejected, so it cannot change the host default
route by accident. Host mode continues to use planner's remotely configured host networking.

macvlan exposes listeners directly on each container's LAN IP, so do not use `--publish`/`PublishPort`.
The parent host and macvlan containers are isolated by Linux by default: if the manager/update source is on the
parent host, provide a host macvlan interface with a suitable route or another reachable address. The upstream
switch/NIC must allow multiple MAC addresses. Container routing affects clients that actually send traffic to
the planner IP; it does not take over the host's network namespace.

Validation: `python3 tools/container-network-test.py` covers both roles' startup order, legacy variables, malformed
parameters, DNS overwrite recovery, mode restrictions and deployment replay. On a Linux host with network/mount
namespace privileges, `sudo python3 tools/container-network-test.py --real-network` checks real macvlan routing
and a bind-mounted resolver in isolated namespaces. These checks are also in CI.

### Rootless

Bridge mode only (host network mode needs rootful Podman: a rootless container's NET_ADMIN does not reach the host's
network, D57). Prerequisites on the host: a user with subordinate ids (`/etc/subuid`, `/etc/subgid`), `passt`
(pasta, Podman 5's rootless network), lingering (`loginctl enable-linger USER`) so the containers start at boot
without a login, and readable `/dev/net/tun`. Then, as that user:

* run.sh: `run.sh --env-file ~/.config/meduza-np/node.env --publish 51820/udp …`; it enables the *user*
  `podman-restart.service` for the boot start and warns when lingering is off.
* Quadlet: copy the units to `~/.config/containers/systemd/`, set `EnvironmentFile=%h/.config/meduza-np/node.env`,
  `systemctl --user daemon-reload && systemctl --user start meduza-np-updater meduza-np-planner`. The units
  wait up to 90 s for the **system** `network-online.target` (Podman's `podman-user-wait-network-online.service`),
  which a user unit cannot pull in: on hosts where no system unit wants it, run once as root
  `systemctl add-wants multi-user.target network-online.target`, otherwise every start and restart waits 90 s.
* Published ports are forwarded in user space (rootlessport): the planner sees connections from its own container
  address, not from the client. Port forwards work (planner also forwards node-local connections, D57), but their
  `sourceCidrs` cannot match real clients, and WireGuard/OpenVPN endpoints of peers show the container address. No
  conntrack cleanup is needed after re-creating the planner container.
* `ip_unprivileged_port_start` limits published ports below 1024 (default 1024: DNS on :53 needs it lowered).

Verified in the lab with `tools/lab/rootless.sh` (WireGuard + port forward, re-creation, reboot, Quadlet user units).

First start on blank volumes: the updater fetches the signed channel, installs planner + the selected components and
records the runnable set (`podman logs meduza-np-updater`); the planner container waits for it and then starts.
Without network the updater keeps retrying with backoff and reports the bootstrap failure; planner keeps waiting —
nothing is reported as online that is not.

## Component selection

`MEDUZA_NP_COMPONENTS` selects optional packages to keep installed. Enabled services in a published manager
configuration add their required packages automatically: planner writes `wanted-components.json` in the shared
IPC volume, and updater checks for changes at most every 60 seconds. Both selections are combined; removing a
package from the environment does not remove it while the published configuration still needs it. Mihomo's
`mihomo-pkg` installs the source adapter; configure its `auto-download` mode and a supported pinned version to
obtain the Mihomo binary, or provide it in `manual` mode.

If updater says `NoChange` while planner's package list is missing an installed package, refresh it with
`podman exec meduza-np-planner meduza-np rescan`. The updater now checks planner registration even when package
versions/digests are unchanged, so a missed startup rescan is repaired without downloading the packages again.
The image-provided updater is also included in planner's package report even though it has no store manifest;
otherwise the manager can incorrectly display "install by hand".

## Enrollment

Set `MEDUZA_NP_ENROLL_URL` and `MEDUZA_NP_ENROLL_TOKEN_FILE` (mount the token file read-only into the planner
container) for the first start, or later: `podman exec meduza-np-planner meduza-np enroll …` (needs the `cli`
component). The identity is kept in the node volume and survives container re-creation.

## Pages package downloads and resuming

New release groups publish every non-OpenWrt archive to the configured Pages source, including unchanged
package versions reused by that group. Each archive also has ordered parts of **500,000 bytes**, except its
last shorter part. Full files and parts share content-addressed `packages/<sha256>/` paths across releases.
The signed group manifest binds every part's URL, size and SHA-256 as well as the full archive digest.
OCI runtime/distribution images keep their existing registry publication; OpenWrt `.ipk`/`.apk` files keep
their complete-file Release downloads.

The store updater reuses only verified parts in `<data>/store/downloads/<sha256>/`, retries missing/failed
parts independently, and checks the assembled archive again before installing it. Failed transactions and
restarts preserve verified parts for the current target; successful installation and a newer verified target
collect obsolete owned parts. Unknown files and redirected cache directories are preserved. Progress includes
verified assembled parts during downloads and retries. No mirror setting or GitHub API is required.

Old updaters ignore the additional part metadata and download the complete archive from Pages. Old groups
without part metadata and OpenWrt native backends still use full downloads. The first upgrade to a new updater
can therefore use the existing process. Keep the whole Pages layout when preparing an offline import.

The deployment preserves historical signed manifests and their package files, reuses identical content and
verifies both complete files and parts before deployment. It refuses a site exceeding GitHub Pages' published
1 GB size limit; it does not silently remove historical download files.

## Offline import

Copy a release site directory (`channels/`, `releases/`, `packages/` — the same layout as the Pages site, produced by
`tools/release`) to the host, mount it read-only into the updater container at `/var/lib/meduza-np/import`
(`run.sh --import DIR` or the commented `Volume=` line) and set `MEDUZA_NP_UPDATE_BASE_URL=file:///var/lib/meduza-np/import`.
Signatures are verified exactly as online. Refresh that directory in place for later imports (a replaced directory is not seen by the
running updater container until it restarts).

## Operations

| Task | How |
| --- | --- |
| Status | `podman exec meduza-np-updater /usr/lib/meduza-np/image/meduza-np-updater status` (or `meduza-np update status` with the `cli` component) |
| Update now / pause / defer | `meduza-np update apply`, `update pause`, `update resume` (cli component) — or set `MEDUZA_NP_UPDATE_AUTO=0` |
| Change components | edit `MEDUZA_NP_COMPONENTS`, then `run.sh --again` (or restart the Quadlet services) to re-create containers with the new environment; `podman restart` does not reload the env file. Packages still needed by the published configuration stay installed; identity, configs and manual provider files stay |
| Stop / start | `systemctl stop meduza-np-planner meduza-np-updater` (volumes keep all state) |
| Package upgrade | automatic per channel/window; planner is restarted through the container's restart loop; failure ⇒ previous generation restored and the group is not retried automatically |
| Updater self-update | new updater version is activated last; the container's entrypoint starts it; three quick failures ⇒ the image baseline updater starts and rolls the generation back |
| Base image replace | `deploy/podman/base-image.sh plan`, then `replace` (see below); `rollback` returns to the image that ran before. Volumes are reused; the persisted updater is used while it is newer and healthy |
| Uninstall | stop units, remove the unit files; `podman volume rm meduza-np-store meduza-np-ipc` (keep `meduza-np-node` if the identity should survive) |

## Base image replacement

If a node does not follow new group releases, inspect `updater-cli status` for `source`, `pinnedGroupVersion`,
`autoApply`, `control.paused` and `state.lastResult`. An offline `file://` source only sees the imported metadata;
`MEDUZA_NP_GROUP_VERSION` pins an exact group even when auto-apply is enabled. To resume channel updates, unset
that variable, set `MEDUZA_NP_UPDATE_BASE_URL` to the live release site and re-create the containers so they read
the changed environment. Changing the env file followed by `podman restart` does not reload container environment.

Package updates never need a new base image: the updater replaces planner, plugins and itself inside the store
volume. A new base image (Alpine fixes, a new baseline updater, entrypoint changes) is deployed by the host helper
`deploy/podman/base-image.sh`, run as the user that owns the containers (root, or the rootless user). It is part of
the deployment files, needs no planner/updater on the host and gives the containers no access to Podman.

```sh
deploy/podman/base-image.sh status                   # running image, previous image, last results
deploy/podman/base-image.sh plan [--group v0.9.0]    # verified target, pulled and checked; changes nothing
deploy/podman/base-image.sh replace [--group v0.9.0] [--timeout 600]
deploy/podman/base-image.sh rollback                 # back to the image recorded before the last replace
deploy/podman/base-image.sh history
```

* **Which image**: only the one the running updater names from the signed package group (`meduza-np-updater
  base-image` behind `entrypoint.sh updater-cli`): the `meduza-np-base-container` artifact for this platform,
  `<repo>@sha256:<index>` (D30), the channel's current group or `--group`. A reference typed by hand is not accepted;
  `rollback` uses only the image recorded as running before.
* **Before the switch**: pull by digest; the local image must be that group's manifest for this platform, carry the
  `meduza-np-base-container` title label and embed the group's updater version. Then the *new* image's baseline
  updater checks the store volume mounted read-only (`store-check`): installed-set and journal formats it reads,
  backend and platform match, a current view exists, and no package update is half done. Any problem ⇒ nothing changes.
* **Switch**: Quadlet units (`Image=` of both units rewritten, `daemon-reload`, restart) or the `run.sh` deployment
  (`run.sh --again --image …` repeats the recorded options). Then both containers must run the new image and pass
  their health checks within `--timeout` (default 600 s).
* **Failure**: the previous image is checked the same way (an older baseline must still read what a newer updater
  wrote) and restored, also with its own health check; if it cannot take the store over, the node stays on the new
  image for repair and the helper says why.
* **Record**: `<state>/base-image.history` (time, action, result, reference, image id, group, package versions) and
  `<state>/base-image.previous`; `<state>` is `/var/lib/meduza-np-deploy` (root) or
  `~/.local/share/meduza-np-deploy` (rootless), `MEDUZA_NP_DEPLOY_STATE` overrides it.
* The helper only touches `meduza-np-updater`, `meduza-np-planner` and the `meduza-np-store` volume (read-only).
  Images are not pruned, so `rollback` works offline; remove old ones with `podman image prune` when no longer needed.
* Deployments made before this helper: their updater image has no `updater-cli` role, so the first move to a new
  image is manual (pin the digest of the group's `meduza-np-base-container` artifact in both units, restart);
  `run.sh` deployments are recorded from their next `run.sh` call.

Verified on the VPS with `tools/base-image-e2e.sh` (rootful, run.sh deployment, local registry): see D53.

## Capability matrix

| Feature | bridge (rootful) | bridge (rootless) | host (rootful) |
| --- | --- | --- | --- |
| Sync, updates, CLI | yes | yes | yes |
| WireGuard / OpenVPN / Tinc endpoints | yes, published ports; tunnels live in the container namespace | yes (needs `/dev/net/tun`; kernel WireGuard needs the host module); peers appear with the container's address | yes, host namespace |
| Port forwards | for traffic arriving through published ports | yes (source address of clients not preserved: no `sourceCidrs`) | yes |
| Routing for the host/LAN (FRR, Anycast, TPROXY) | only for traffic routed into the container namespace — the host needs explicit routes | no | yes |
| DNS front end on :53 | published port | needs `net.ipv4.ip_unprivileged_port_start` ≤ 53 on the host | yes (port must be free) |
| mDNS (avahi) | no multicast across the bridge | no | yes |

Kernel modules (WireGuard, TPROXY) come from the host; the image cannot add them.

The deployment tells planner its mode (`MEDUZA_NP_NETWORK_MODE`, set by `run.sh` and the Quadlet units; D41). In
bridge mode the node reports routing, TPROXY, mDNS, 1:1 mappings and port forwards as "container network only"
(running, but not in effect for the host/LAN), and the manager warns before publishing such settings to the node.

The planner container drops Podman's default capabilities and gets only its base set (D44): `NET_ADMIN NET_RAW
NET_BIND_SERVICE` for the network, `CHOWN DAC_OVERRIDE FOWNER` for installing Alpine dependencies and preparing files of
daemons that run as their own users, `SETUID SETGID` for those daemons to drop privileges (dnsmasq, dbus, FRR, avahi)
and `KILL` for planner to signal them. Not granted: `FSETID SETFCAP SETPCAP SYS_CHROOT` (nothing installed or run
needs them; verified per component in the lab).

Per-component requirements: `frr` needs `SYS_ADMIN` in the planner container (run.sh adds it when the component is
selected; uncomment `AddCapability=SYS_ADMIN` in the Quadlet unit). `avahi` gets `SYS_CHROOT` the same way for
avahi-daemon's own chroot; without it planner turns that chroot off (`avahi-daemon --no-chroot`) instead of failing. While `avahi` is installed the planner
container runs its own system D-Bus (`dbus-daemon --system`, D45); do not also run avahi-daemon on a host-network
host. Routers (`frr`, mappings, port forwards) need
`net.ipv4.ip_forward=1`: the bridge-mode unit sets it for the container namespace; in host mode set it on the host
(`/etc/sysctl.d/`), since the container's `/proc/sys` is read-only and planner only verifies it.
Capabilities are fixed when the container is created: if the configuration later enables FRR on a node deployed
without the `frr` component, the updater installs the package but the node refuses the configuration until the
planner container is re-created with `frr` in `MEDUZA_NP_COMPONENTS` (or `AddCapability=SYS_ADMIN`). The planner
reports its capabilities (`system.capabilities`) and the manager warns about this before publishing (D43).
