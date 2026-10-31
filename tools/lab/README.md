# Multi-node lab (Incus LXC + Podman)

A realistic fleet on one Linux host: every machine is an unprivileged, nested LXC container running Podman, and the
networks between them are separate Incus bridges with host firewall rules. Connection roles follow D17/D27: the
internal mesh is Tinc (lab-a, lab-b, lab-c reach each other without NAT; the roaming lab-roam sits behind NAT and only
dials out), WireGuard/OpenVPN are external connections only. Routing follows D18/D25: iBGP between all mesh members
(BFD between the static ones, short timers with the roaming one), eBGP over an OpenVPN Site2Site tunnel to the
external router lab-ext (an unmanaged Debian box in AS 65100), OSPF over Tinc only for the Anycast pool.

```text
                 mnpwan 10.50.0.0/24 ("internet", NAT to the host uplink)
   lab-mgr 10.50.0.10 ── lab-a 10.50.0.11 ── lab-b 10.50.0.12 ── lab-c 10.50.0.14      lab-ext 10.50.0.30
   (manager, Podman)      │ lana 10.61.0.0/24   │ lanb 10.62.0.0/24   │ lanc 10.63.0.0/24   (unmanaged: OpenVPN
                          │ host-a .50          │ host-b .50          │ host-c .50           + FRR, AS 65100,
                          └──── Tinc mesh 10.42.0.0/24 (switch mode) ────┘                    net 10.70.0.0/24)
                                iBGP (AS 65010) · OSPF for Anycast 10.255.0.0/24                │
   mnpnat 10.52.0.0/24 ── lab-roam 10.52.0.13 (roaming, behind NAT) ── Tinc 10.42.0.5          │
                          │ LAN 10.64.0.0/24, iBGP without BFD (3/9)                            │
   lab-a ══ OpenVPN Site2Site TLS server `ext` (p2p 10.99.0.1 <-> 10.99.0.2) ════════════════ lab-ext
            eBGP 65010 <-> 65100: accepts 10.70.0.0/24, advertises the internal LANs
```

## Host setup (once)

```sh
apt-get install -y incus podman nodejs
incus admin init --minimal            # or a preseed with a dir pool
modprobe wireguard                    # kernel WireGuard comes from the host
incus network create mnpwan ipv4.address=10.50.0.1/24 ipv4.nat=true ipv6.address=none
incus network create mnpnat ipv4.address=10.52.0.1/24 ipv4.nat=true ipv6.address=none
incus network create lana ipv4.address=10.61.0.1/24 ipv4.nat=false ipv6.address=none
incus network create lanb ipv4.address=10.62.0.1/24 ipv4.nat=false ipv6.address=none
incus network create lanc ipv4.address=10.63.0.1/24 ipv4.nat=false ipv6.address=none
# LAN clients route through their lab node, not the Incus bridge:
incus network set lana ipv4.dhcp.gateway=10.61.0.11   # lanb → 10.62.0.12, lanc → 10.63.0.14
incus profile create mnplab
incus profile set mnplab security.nesting=true security.syscalls.intercept.mknod=true \
	security.syscalls.intercept.setxattr=true limits.memory=700MiB
incus profile device add mnplab tun unix-char source=/dev/net/tun path=/dev/net/tun
```

Isolation (host nftables):

```sh
nft -f - <<'EOF'
table inet mnplab {
	chain forward {
		type filter hook forward priority -10; policy accept;
		ct state established,related accept
		oifname { "lana", "lanb", "lanc" } drop
		oifname "mnpnat" drop
	}
}
EOF
```

Machines: a Debian 13 image with Podman (`incus launch images:debian/13 …`, `apt-get install podman curl jq
wireguard-tools iputils-ping`, `incus publish … --alias deb13-podman`), then one container per role with fixed
addresses (`incus config device add NAME eth0 nic network=mnpwan ipv4.address=10.50.0.11`, …). Routers: a second NIC
on their LAN bridge with a static address (lab-a 10.61.0.11, lab-b 10.62.0.12, lab-c 10.63.0.14) and
`net.ipv4.ip_forward=1` in `/etc/sysctl.d/` — in host network mode the planner container cannot write `/proc/sys`, it
only verifies the value. LAN clients host-a/b/c are small Alpine containers on lana/lanb/lanc.

## Run

```sh
tools/container-e2e.sh /root/e2e-run      # builds the base image (:e2e) and a signed test release (site1)
podman build -f packages/meduza-np-manager/Containerfile -t localhost/meduza-np-manager:lab .
tools/lab/lab.sh images
tools/lab/lab.sh manager
tools/lab/lab.sh api bootstrap /root/lab/bootstrap admin
tools/lab/lab.sh site /root/e2e-run/site1
for n in lab-a lab-b; do tools/lab/lab.sh api node $n static; done
tools/lab/lab.sh api node lab-roam roaming
for n in lab-a lab-b lab-roam; do tools/lab/lab.sh api wgkey wg-$n $n; done
for n in lab-a lab-b lab-roam; do tools/lab/lab.sh node $n wireguard,cli host "$(tools/lab/lab.sh api token $n)"; done
# Tinc keys are generated on the nodes (job), WireGuard keys by the manager:
for n in lab-a lab-b lab-c lab-roam; do tools/lab/lab.sh api nodekey $n tinc-$n tinc-rsa-private meduza-np-tinc; done
# External router (unmanaged Debian: OpenVPN client + FRR in AS 65100). Creates the lab CA in the manager, lab-a's
# server certificate, and signs lab-ext's CSR (its key stays there); topology.sh then adds lab-a's OpenVPN
# Site2Site instance with the eBGP session. lab-a's updater installs the OpenVPN package when the config needs it.
tools/lab/external.sh
tools/lab/lab.sh api wgkey wg-lab-a-ext lab-a
tools/lab/topology.sh > /root/lab/topology.json && tools/lab/lab.sh api publish /root/lab/topology.json "tinc mesh"
```

Components: lab-a `tinc,wireguard,frr,cli` (mesh member + external gateway; OpenVPN is added by its updater when the
configuration needs it, D20/D28), lab-b / lab-c `tinc,frr,cli`, lab-roam `wireguard,frr,cli` (Tinc likewise comes
with the configuration). With `frr` selected `run.sh` adds CAP_SYS_ADMIN to the planner container (the FRR
daemons keep it when dropping privileges and abort without it). (`tools/lab/mesh.json` is the earlier WireGuard full mesh; the manager now refuses it, D17.)

The manager serves the origin `http://localhost:8787` (WebAuthn RP ID `localhost`; override with
`MANAGER_ORIGIN`): open it from a workstation with `ssh -L 8787:10.50.0.10:8787 -p <port> root@<lab host>` and
browse to http://localhost:8787 — a secure context, so passkeys work without TLS. Accounts for people:
`tools/lab/lab.sh api invite-account <name> admin` prints a 72 h invitation link. After an origin change the lab's
scripted admin key no longer matches the RP ID; renew it with a host recovery invite:
`tools/lab/lab.sh api invite "$(incus exec lab-mgr -- podman exec mgr node /app/packages/meduza-np-manager/dist/server/src/admin.js recovery-invite admin | tail -1)"`.

`tools/lab/manager-api.mjs` signs in with a software passkey stored in `/root/lab/admin.json`. That file is a
real admin credential for the lab manager; keep it on the lab host only.

## Scenarios (all verified on 2026-10-23, Debian 13 host, Incus 6.0.4, Podman 5.4 nested)

| Scenario | How | Result |
| --- | --- | --- |
| Manager in LXC/Podman, first admin via WebAuthn | `lab.sh manager`, `lab.sh api bootstrap` | admin signed in through the passkey ceremony over HTTP |
| Blank nodes bootstrap + env enrollment | `lab.sh node …` (base-container, host network) | 3 nodes enrolled; static nodes on the WebSocket push channel, the roaming node polls from behind NAT |
| Feature configured before installation (D20) | publish right after enrollment | warning "has not reported its packages yet; these settings apply once … is installed"; the updater installs the package |
| WireGuard mesh incl. NAT'd node | `lab.sh api publish tools/lab/mesh.json` | all pairs ping across the tunnels; static nodes reach the roaming node only through its tunnel |
| Change for one node only | keepalive change on lab-roam | only lab-roam gets a new revision; lab-a/lab-b keep theirs (`changed: false`) |
| Manager outage (60 s) | `podman stop mgr` in lab-mgr | tunnels keep forwarding; nodes report `offline` + backoff; after restart the next publish reaches push and poll nodes |
| Fleet upgrade under traffic | `tools/lab/upgrade-under-ping.sh SITE VERSION NODE FROM TARGET` | planner restart, plugin upgrade and updater self-update with **0 lost pings** (after the fixes below) |
| Roaming node uplink outage + publish meanwhile | `ip link set eth0 down` for 40 s | revision applied 6 s after the link returned, tunnel recovered |

| Tinc internal mesh, no NAT (D17) | `topology.sh` + publish | all 6 member pairs ping over 10.42.0.0/24; the NAT'd node is not in the mesh and reaches only the gateway over WireGuard |
| Component change / package removal | `MEDUZA_NP_COMPONENTS` changed, containers recreated | tinc installed immediately; wireguard removed from lab-b *and* its interface cleaned up |
| Planner restart on a mesh member | `FORCE=meduza-np-planner tools/lab/next-release.sh …` + `upgrade-under-ping.sh` | before D32: 0.8 s gap on the Tinc mesh (tincd restarted with planner). Since D32/D33 (2026-10-24, lab-b, planner-only v0.5.9): tincd, zebra, bfdd, bgpd, ospfd all adopted (same pids), **0 lost pings** on the Tinc mesh (lab-a → 10.42.0.2) and LAN-to-LAN (host-a → host-b, 0.2 s pings); WireGuard (kernel) loses nothing |
| Release key rotation | v0.3.0 signed with a new key only | old nodes refuse it ("signed only by untrusted key"); trust rotated by recreating from the new base image |

Routing (D18), measured with `tools/lab/withdrawal.sh` (100 ms route sampling, 0.2 s pings) and `tools/lab/anycast.sh`:

| Scenario | How | Result |
| --- | --- | --- |
| LAN-to-LAN over iBGP+BFD on Tinc | publish `topology.sh` | all host pairs ping; host-b → 10.64.0.1 traceroute 10.62.0.12 → 10.42.0.1 → 10.64.0.1 (next-hop-self at the gateway) |
| Mesh member hangs | `withdrawal.sh lab-b 10.62.0.0/24 15 "lab-a lab-c" host-a 10.62.0.50 host-a 10.63.0.50` (incus pause) | 10.62.0.0/24 withdrawn on lab-a/lab-c after 0.8–1.0 s (BFD 300 ms × 3), back 1.5 s after resume; host-a ↔ host-c 0 loss |
| Mesh member loses power | `MODE=poweroff withdrawal.sh lab-b …` | withdrawn after 1.1 s; back 6.6–7.8 s after power-on (LXC boot, Podman, planner, Tinc, BGP) |
| Roaming branch hangs (eBGP, no BFD) | `withdrawal.sh lab-roam 10.64.0.0/24 20 "lab-a lab-b lab-c" host-a 10.64.0.1 host-b 10.63.0.50` | timers 3/9: withdrawn on all mesh members after 6.7 s, back 3.2 s after resume; no bfdd on the roaming node |
| Branch learns the internal AS | `exportPolicy.internal` on lab-a's branch neighbor | lab-roam has 10.61/62/63.0.0/24 via the tunnel; nothing else (noForward) |
| Anycast nearest instance | `anycast.sh` (web 10.255.0.0/30 on lab-a + lab-c) | host-a → lab-a, host-c → lab-c, lab-b ECMP over both; OSPF installs only the pool route |
| Anycast service crashes / recovers | `systemctl stop lab-web` on lab-a | withdrawn after 4.5 s (3 failed probes); host-a is then served by lab-c; re-announced 6 s after restart |
| Anycast node hangs | `incus pause lab-c` | lab-b drops lab-c's path after 0.7 s (OSPF BFD), host-b keeps getting answers from lab-a |

Mihomo TPROXY (§9.7, D29, D34), measured with `tools/lab/mihomo.sh` on 2026-10-24 (lab release v0.6.1): lab-b
proxies the forwarded traffic of its LAN through a SOCKS5 server on lab-ext; the only Mihomo input is a
subscription served by lab-ext (global setting `mihomo.subscriptions`, URL as a `*`-scoped secret); the binary
comes from mihomo-pkg in auto-download mode (pinned v1.19.33). Setup: `mihomo.sh setup`, `mihomo.sh publish on`.

| Scenario | How | Result |
| --- | --- | --- |
| Feature configured, nothing installed | `mihomo.sh publish on` | only lab-b got a new revision; updater installed mihomo + mihomo-pkg; `prepare` downloaded, verified and registered the binary, the rescan started Mihomo (~3 s after the planner came up) |
| TPROXY for the LAN, TCP + UDP | `mihomo.sh probe` (host-b → 198.51.100.10:8081) | both answered from lab-ext (proxied); before: TCP failed, UDP came from host-b itself |
| Exclusions | host-a ↔ host-b ping | private destinations bypass (LAN-to-LAN over the mesh unchanged, 0 loss); proxy-server set holds 10.50.0.30 and is referenced by the rule |
| Controlled keys in the subscription | subscription sets `mixed-port`, `external-controller: 0.0.0.0:9999`, `tun.enable` | replaced (7890, `127.0.0.1:9090`, tun off) and reported as `overriddenKeys`; `tproxy-port`, `routing-mark` 19791 set by the node |
| Subscription change + forced refresh | `mihomo.sh sub direct`, `POST /api/v1/mihomo/refresh` | flows switched to DIRECT (seen as the host's NAT address); back to the proxy 18 s after the next refresh request |
| Disable / re-enable | `mihomo.sh publish off` / `on` | off: nft table, fwmark rule, local route, processes and both packages removed, downloaded binary kept in the persistent provider dir; on: reinstalled, the kept binary registered without a new download |

DNS front end (§9.9, §9.10, D36), measured with `tools/lab/dns.sh` on 2026-10-24 (lab releases v0.6.3–v0.6.5 on
lab-b): dnsmasq answers lab-b's LAN on 10.62.0.12:53 in front of MosDNS (127.0.0.1:1153, from mosdns-pkg) and
Mihomo DNS (127.0.0.1:1053), fallback 10.50.0.30#5353. lab-ext's marker resolvers answer `who.lab.test` with the
path taken (192.0.2.1 MosDNS, .2 fallback, .3 Mihomo DNS); host-b queries every 0.2 s (`dns.sh watch`). Setup:
`dns.sh setup`, `dns.sh publish fallback|mosdns|both`.

| Scenario | How | Result |
| --- | --- | --- |
| Front end next to other DNS servers | systemd-resolved on 127.0.0.53/.54:53, aardvark-dns on 10.89.0.1:53 | dnsmasq binds 10.62.0.12:53 beside them (refused before, see below), nothing else touched |
| Fallback only, hosts | `publish fallback` (the updater installed dnsmasq, mosdns, mosdns-pkg) | `who` → .2 (fallback verified), `nas.lab-b.test` → 10.62.0.50 from `dns.hosts` |
| MosDNS backend | `publish mosdns` | `who` → .1, `blocked.lab.test` → NXDOMAIN (MosDNS block set); `mosdns.binary` healthy |
| Add Mihomo DNS (front end restarts) | `publish both` under `watch` | 0 failed queries; ~1 s on the fallback while the restarted wrapper probed again; then MosDNS first, Mihomo second |
| Backend set changes without restart (D37, v0.6.6) | `publish mosdns`, `mihomo`, `fallback`, `both` under `watch` | dnsmasq and the wrapper keep their pids (SIGHUP, spec re-read); **0 failed queries** of 246; both → MosDNS no gap, MosDNS → Mihomo ~1 s on the fallback until Mihomo DNS was probed healthy, fallback → both back on MosDNS after the rise |
| MosDNS hangs | `freeze mosdns` (SIGSTOP) / `thaw` | answers moved to Mihomo DNS 2.5 s after the freeze with **0 failed queries** (strict-order retries the next server); back on MosDNS ~3 s after the thaw |
| MosDNS's upstream dies | `upstream stop 53` | MosDNS answers SERVFAIL, dnsmasq moves on at once: Mihomo DNS answers within 0.3 s, 0 failures; MosDNS re-added ~4 s after the upstream returned |
| Both backends hang | `freeze mosdns` + `freeze mihomo` | fallback restored after ~7 s (fall 2 × 3 s probes): **3 failed queries** in that window, then .2; back to MosDNS after the thaw |
| MosDNS wrapper killed | SIGKILL to the supervised `plugins/mosdns run` | restarted in 1 s, the orphaned daemon stopped with its group (since v0.6.5); 1 query answered by Mihomo DNS, 0 failures |
| Resolution loop | MosDNS upstream `udp://10.62.0.12` | v0.6.4: the node refused the revision ("upstream points back at the dnsmasq front end") and kept serving the previous one; the manager now refuses the publish (422) |
| Disable | `publish off` | dnsmasq, MosDNS processes, run-dir files and the three packages gone; Mihomo DNS off, Mihomo TPROXY still proxied; port 53 owners as before |

OpenVPN Access (§9.3, D27, D38), measured with `tools/lab/access.sh` on 2026-10-24 (lab releases v0.6.7/v0.6.8 on
lab-a): Access server `access` on lab-a (udp 1195, pool 10.98.0.0/24 originated into iBGP, pushes the three LANs,
renegotiation every 300 s), OpenLDAP (osixia, test-only) next to the manager on lab-mgr's Podman gateway
10.88.0.1:636 (ldaps, CA pinned, not reachable from mnpwan), policy `lab-vpn` = group `vpn-users` on lab-a/access;
remote user machine lab-vpn 10.50.0.40 (Debian, OpenVPN 2.6.14) with the exported profile. Setup: `access.sh setup`,
`access.sh publish on`, `access.sh profile`.

| Scenario | How | Result |
| --- | --- | --- |
| Login + routing | `connect alice`, `probe` | connected in ~0.3 s, 10.98.0.2; host-a/b/c answer over the tunnel (0% loss); lab-b/lab-c route 10.98.0.0/24 via lab-a over the mesh (iBGP) |
| Refusals | `connect bob`, wrong password, unknown user, `alice)(uid=*` | AUTH_FAILED each; manager log `not-authorized`, `bad-credentials` (×3: wrong, unknown and the injection look alike) |
| Group / policy change, new login | `group carol off`, `policy off` | next login refused (`not-authorized`), allowed again after `on` |
| Session lifetime | `policy on 60` | server closed the session after 60 s (`session-timeout`); the UDP client noticed 54 s later (keepalive), its token was refused (`session-expired`), then it logged in again with the password from its file |
| Revoked while connected | `group carol off` during a session | cut off **279 s** after the removal, at the first renegotiation (token re-check `not-authorized`, password retry refused, client exits); before D38 the session ran until the 8 h token lifetime |
| Manager down while connected | `manager stop` > 300 s | renegotiation `kept-unverified`, **0 lost pings**; a new login meanwhile refused; after `manager start` logins refused for ~63 s more (planner's §7.5 backoff gate, by design), then allowed |
| Directory down while connected | `ldap stop` > 300 s | renegotiation `kept-unverified`, 0 lost pings; new login refused (`ldap-unavailable`); allowed 1 s after `ldap start` |
| Planner-only upgrade | v0.6.7 → v0.6.8 under a session | Access server and Site2Site adopted (same pids), alice stayed connected |
| Disable / re-enable | `publish off` / `on` | off: process gone, port closed, pool route withdrawn on lab-b/lab-c, client refused, `openvpn.access` disabled in the feature list (since v0.6.8); on: login again within 0.3 s |
| Online sessions + disconnect (D39, 2026-10-25, v0.6.9 on lab-a = only the OpenVPN plugin changed; alice's session survived that upgrade) | `sessions`, `kick alice` | listed by the manager ~30 s after the login (with `clientId`); kick: node job done in **0.33 s**, client got `HALT` (`server-pushed-halt`, exits, no retry); row marked `disconnecting` until the node's next report, gone **28 s** after the kick; manager log `end/disconnected`; password login allowed again at once (0.36 s), LANs reachable |

Bugs this lab found (fixed):

- dnsmasq refused port 53 whenever *any* socket used it (systemd-resolved on 127.0.0.53, aardvark-dns on a Podman
  bridge), although dnsmasq binds only its listen addresses: the front end could not run on a normal Debian host.
  The owner check now considers only sockets on a colliding address (same address, or a wildcard of the family; UDP
  and listening TCP) and names the address (D36).
- Once dnsmasq ran, every further revision for the node was refused: dnsmasq drops to its own user, so without
  CAP_SYS_PTRACE planner can list its fds but not read their links, and its own front end counted as "an
  unidentified process" on port 53. An unreadable dnsmasq of ours now counts as ours (D36).
- `mosdns.binary` (a provider package without a process) was reported enabled but not running/healthy. Static
  binary providers are up while their slot resolves to their binary (D36).
- After the MosDNS wrapper was killed, the old mosdns daemon kept running as an orphan next to the restarted one
  (both on 127.0.0.1:1153) and even outlived disabling the feature. The supervisor now stops what is left of a
  process group when its leader exits on its own (D36).
- Disabled front end / MosDNS left `dnsmasq-main.{pid,servers,state}` and `mosdns-main.state` in the run dir; the
  wrappers remove them when stopped. DNS probe timeouts were shown as "Resource temporarily unavailable (os error
  11)"; they say "timeout" now.

- OpenVPN Access ignored the policy's session lifetime, and a user removed from the directory group or the policy kept
  a running session for the full auth-token lifetime (8 h): OpenVPN answered renegotiations from its token alone.
  Now `session-timeout` from the granted lifetime (client-connect hook) and a password-less manager re-check at every
  renegotiation (`auth-gen-token … external-auth`); an unreachable manager or directory keeps running sessions
  instead of disconnecting everyone (D38).
- `openvpn.access` showed enabled/running in the feature list without any Access instance (only Site2Site):
  capability switches can match an instance in an array now (D38).

- A feature whose binary comes from a provider prepared at runtime (mihomo-pkg) made the planner reject the whole
  revision ("binary not prepared yet"), but the preparer that downloads the binary is part of that revision: the
  binary never arrived and every other change to the node was blocked. Now only that feature waits (D34).
- The feature list showed `mihomo.dns` enabled/healthy while `services.mihomo.dns.enabled` was false (all
  capabilities followed the service). Manifests now declare `capabilitySwitches` (D35); on lab-b v0.6.2 `mihomo.dns`
  reports disabled while proxy/tproxy/subscriptions stay healthy and the TPROXY probe is still proxied.

- WireGuard reload used `wg setconf`, which drops every peer session: ~15 s of tunnel outage on every planner restart
  and every config change. Now `wg syncconf`.
- Interfaces without `listenPort` got a new random port on every reload (both `setconf` and `syncconf` do that when
  the file has no `ListenPort`), breaking NAT mappings until the next keepalive. Now a stable port derived from the
  interface id is always written.
- A plugin package upgrade did not re-plan its instances until the next publish. `registry.rescan` now re-applies the
  last good configuration when a registered package or a resolved provider changed.
- Manager Containerfile pinned a non-existent `node:24.22.0-alpine3.22` image (the production image could not be built).
- The planner role waited forever for `apk add` when the mirror was unreachable; it is now bounded (120 s online,
  60 s offline cache) and planner starts regardless.
- Planner version shown by the manager/logs was the crate version instead of the installed package version.
- Tinc switch mode carried no traffic between cloned machines: systemd derives tap MACs from machine-id + name, so all
  members had the same MAC. tinc-up now sets a MAC derived from netname + member name (and tinc-up/-down are part of
  the restart digest so a change actually reaches the device).
- A package removed while planner was stopped left its resources behind (stale `wg-wg0`). Planner now deactivates
  instances of packages that are no longer registered at startup, by ownership.
- A changed `MEDUZA_NP_COMPONENTS` waited for the next randomized update check (up to an hour); it is applied at once.
- FRR daemons aborted in `privs_init` without CAP_SYS_ADMIN, and ran with an empty configuration because user `frr`
  could not read the generation files: the plugin now refuses the config without the capability (run.sh adds it with
  the frr component) and gives the daemon user its generation directory.
- `net.ipv4.ip_forward` could not be written from the container (read-only /proc/sys): the planner verifies a sysctl
  that already has the required value and otherwise names where to set it.
- A failed revision was never retried by a new planner (e.g. after the fix was released): it is retried once per
  planner start.
- After a power loss tincd refused to start ("already running with pid 51"): pidfiles live in the persistent
  RUN_DIR volume and named an unrelated new process. Planner removes stale pidfiles at startup.
- Containers did not come back after a reboot of the node: `--restart always` needs `podman-restart.service`, which
  run.sh now enables.
- Anycast on a broadcast Tinc segment lost every path for ~5 s when the OSPF DR failed (and again when it returned):
  carriers are point-to-multipoint now. A zebra OSPF install filter was silently ignored (FRR 10 wants mgmtd) and was
  dropped. An unhealthy node kept the service addresses on its down dummy, so its own LAN still reached the dead
  service: the guard removes them.
- Planner-only upgrade with D32 (process adoption) still restarted all FRR daemons: they run as user `frr`, so
  `/proc/<pid>/exe` is unreadable for planner and was taken as "binary replaced". Worse, the daemons were restarted
  in alphabetical order (zebra last): bgpd connected to the old zebra, which was then replaced, and its `network`
  route for the own LAN stayed invalid (never advertised again), so host-a lost host-b until FRR was restarted.
  Now records keep the program file identity, and the FRR daemons are bound to zebra (`boundTo`, started after it
  and restarted with it, like watchfrr) (D33).

### Bridge-mode node (`tools/lab/bridge.sh`, 2026-10-25, D41/D42)

lab-br (LXC 10.50.0.15 on mnpwan) runs base-container with the planner on the Podman bridge (`run.sh --network
bridge --publish 51820/udp --publish 8089/tcp`, components `wireguard,cli`). Its WireGuard `wg0` (10.96.0.2/24) has
the unmanaged lab-ext as the external peer (wg-quick `wg-br`, 10.96.0.1, dials lab-br's published port), and a port
forward `echo` (tcp 8089 → 10.96.0.1:8089, SNAT) leads from the published port through the tunnel to `lab-echo.py`
on lab-ext, which answers with the source it sees.

```sh
tools/lab/bridge.sh setup /root/lab/site-v0.7.0 && tools/lab/bridge.sh deploy && tools/lab/bridge.sh publish on
tools/lab/bridge.sh check                       # namespaces, ports, tunnel, port forward, mode, limitations
tools/lab/bridge.sh recreate                    # new planner container under ping
tools/lab/bridge.sh upgrade /root/lab/site-v0.7.3 v0.7.3
tools/lab/bridge.sh bgp on && tools/lab/bridge.sh bgp-check   # eBGP over the tunnel (D43)
```

| Scenario | How | Result |
| --- | --- | --- |
| Pre-publish warning | `publish on` | one warning: `network.port-forwards applies only to traffic that reaches its container namespace`; WireGuard none (tunnel endpoints work through published ports); published |
| Namespaces | `check` | `wg-wg0` and `table ip meduza_np_nat` exist only in the planner container; lab-br itself has neither |
| Tunnel + port forward | `check` | lab-ext ↔ 10.96.0.2 0% loss; lab-vpn → 10.50.0.15:8089 answered `10.96.0.2` (published port → container DNAT → tunnel → SNAT) |
| Reported mode + limitation | `check` | node report `networkMode: bridge`; `network.port-forwards` running/healthy with `limitation=bridge-network`, `wireguard.interfaces` without (since v0.7.2) |
| Planner container re-created | `recreate` | planner recreated `wg-wg0` and the nft table in the new namespace, run.sh dropped the stale UDP conntrack entry; tunnel back **15 s** after run.sh started (stop + start + restore + handshake; the stop was not graceful before D57), port forward answering |
| Planner-only upgrade | `upgrade … v0.7.3` (v0.7.2 → v0.7.3, planner only) | **0 lost pings of 317**, `wg-wg0` kept (same ifindex), port forward answering |
| Host-mode regression of D42 | v0.7.3 on lab-a (Tinc, FRR, OpenVPN Site2Site + Access) and lab-b (Mihomo TPROXY rule/route/nft, DNS front end) | no "recreating lost" (the new presence probes see the routes, the fwmark rule and the tables), FRR/OpenVPN/Tinc adopted on lab-a, 0 of 350 LAN-to-LAN pings lost on lab-b |

Found and fixed (D42):

- `run.sh --publish 51820/udp` (as documented) published a random host port; lab-ext's handshakes to :51820
  never arrived.
- After the planner container was re-created, its new network namespace had no `wg-wg0` and no nft table, but the
  ownership journal said they were owned: planner ran `wg syncconf` on the missing link (restore failed, tunnel
  down for good). Now lost links/nft tables/routes/rules are recreated.
- Even then the tunnel stayed down: lab-ext's keepalives had created a UDP conntrack entry on lab-br without the new
  DNAT while the container was being replaced and kept refreshing it. run.sh now deletes the entries of its
  published UDP ports after starting the planner.
- Planner's own features (`network.port-forwards`, …) were never "enabled", so the D41 limitation never showed for
  them; kernel WireGuard was `running: false` forever (no supervised process).

#### FRR on the bridge-mode node (`bridge.sh bgp on` / `bgp-check`, 2026-10-25, D43)

eBGP AS 65020 (lab-br, routerId 10.96.0.2) ↔ AS 65100 (lab-ext's FRR from external.sh) over the WireGuard tunnel
(the `bgp` block of the lab-ext peer, accept 10.70.0.0/24). lab-br declares and originates the LAN 10.65.0.0/24, which
exists only on lab-br's own dummy `lan0`, i.e. outside the planner container's namespace. FRR was not among the
deployment's components; the updater installed it from the configuration (D20).

| Scenario | How | Result |
| --- | --- | --- |
| FRR enabled on a container created without the frr component | `bgp on` (first try, v0.7.3) | published with only the D41 warnings; the updater installed meduza-np-frr within a minute, then the node **rejected the whole revision**: `routerId required` and `FRR daemons need CAP_SYS_ADMIN` (run.sh adds SYS_ADMIN only for the components it was started with) |
| The same before publishing (v0.7.4) | validate | **error** `/nodes/lab-br/routerId: this node runs FRR …`; **warning** `the planner container of lab-br lacks CAP_SYS_ADMIN, which frr needs …` from the reported `system.capabilities`; the mesh nodes get neither |
| Container re-created with `MEDUZA_NP_COMPONENTS=wireguard,frr,cli` | `recreate` | SYS_ADMIN warning gone; but the last good revision still had no routerId, so the fresh planner had **nothing applied** (tunnel down until the fixed revision rev-000041, ~4 min): one invalid service keeps the whole node unconfigured — the pre-publish error now prevents publishing such a revision |
| Session + learned route | `bgp-check` | Established at once over the tunnel (no published 179 needed); 10.70.0.0/24 `via 10.96.0.1 dev wg-wg0 proto bgp` **only in the container**, lab-br has no route; container → 10.70.0.1 0% loss, lab-br itself cannot reach it; `frr.bgp`/`frr.bfd` carry `limitation=bridge-network` |
| LAN outside the namespace | `bgp-check` (v0.7.5) | `network 10.65.0.0/24` never announced (import-check, no route in the container), lab-ext: `Network not in table`, session `up` with `prefixesSent: 0`. New `frr/main/origination` status: **degraded**, `not announced: 10.65.0.0/24 (no matching route in this node's routing table; …)` |
| LAN routed into the container on purpose | by hand: route 10.65.0.0/24 via the container gateway inside the container, 10.96.0.0/24 via the container on lab-br | origination **up**, lab-ext learns 10.65.0.0/24 via 10.96.0.2, lab-ext → 10.65.0.1 0% loss (routes removed again afterwards) |
| Upgrades | `upgrade` v0.7.4 / v0.7.5 (planner + all, then FRR only) | 0/318 and 0/319 pings lost, `wg-wg0` kept, port forward answering; host-mode lab-a on v0.7.5: origination up (LAN + Access pool), 4 sessions up, 0/350 mesh pings lost |

### Rootless bridge-mode node (`tools/lab/rootless.sh`, 2026-10-26, D57)

lab-rl (LXC 10.50.0.19 on mnpwan, Debian 13, Podman 5.4.2 + passt) runs base-container as the unprivileged user
`mnp` (uid 1000, subuids 100000-165535, lingering): rootless netavark bridge network `meduza-np` + pasta,
`--publish 51820/udp --publish 8089/tcp`, components `wireguard,cli`. Same shape as lab-br on its own tunnel: wg0
10.97.0.2/24 ↔ lab-ext `wg-rl` 10.97.0.1 (:51831), port forward `echo` tcp 8089 → 10.97.0.1:8089 (SNAT) to
lab-ext's `lab-echo-br.service`. Lab-only setup trap: inside a nested Incus container the distro AppArmor profile
of pasta denies mapping its own binary (shifted file owner) and pasta segfaults; `setup` unloads that profile.

```sh
tools/lab/rootless.sh setup /root/lab/site-v0.8.6 && tools/lab/rootless.sh deploy && tools/lab/rootless.sh publish on
tools/lab/rootless.sh check | recreate | reboot | host-net
tools/lab/rootless.sh quadlet && tools/lab/rootless.sh quadlet-reboot   # the shipped Quadlet units as user units
```

| Scenario | How | Result |
| --- | --- | --- |
| Rootless run.sh deployment | `deploy` | both containers start as mnp (root has none); `--sysctl net.ipv4.ip_forward=1`, `/dev/net/tun`, the D44 capability set and published ports all accepted; enrolled from the environment |
| Namespaces + tunnel | `check` | planner uid 0 in the container = mnp on lab-rl; `wg-wg0` and `meduza_np_nat` only in the container; lab-ext ↔ 10.97.0.2 0% loss; the container sees lab-ext as `10.89.0.x:port` (its own address: rootlessport) |
| Port forward | `check` | **failed with v0.8.6** (connection accepted, no answer): rootlessport-child delivers published ports from inside the container namespace to the container's own address, so prerouting DNAT never sees them; **v0.8.7** also forwards in the nat output hook ⇒ answered `10.97.0.2` |
| Report | `check` | `networkMode: bridge`, `network.port-forwards` with `limitation=bridge-network`, capabilities = D44 base set |
| Re-creation | `recreate` | lost link + nft table recreated, last good restored; handshake 17 s after run.sh started, port forward answering |
| LXC reboot (run.sh) | `reboot` | user `podman-restart.service` (enabled by run.sh, lingering) brought both containers back 4 s after boot; tunnel + forward up |
| Rootless host network | `host-net` | `ip link add`, `ip route add`, `nft add table`: Operation not permitted; `/proc/sys` read-only (§14.3.4: NET_ADMIN of a rootless container is not the host's) |
| Quadlet user units | `quadlet`, `quadlet-reboot` | units from `deploy/podman` with `EnvironmentFile=%h/…` run; same volumes ⇒ same identity and last good; active 4 s after an LXC restart |
| Unit restart time | `systemctl --user restart` | **91 s** on lab-rl: Podman's `podman-user-wait-network-online.service` waits for the system `network-online.target`, which no system unit wanted there; after `systemctl add-wants multi-user.target network-online.target`: **2 s** |
| Graceful stop | `systemctl --user stop` | **v0.8.6 image**: exit 143, unit `Failed`, no planner shutdown (tini signals only the entrypoint shell, which died at once and took planner down with the container — also rootful and run.sh); **new entrypoint**: `shutting down; stopping supervised processes`, `stopped (exit status 0)`, `Result=success` for planner and updater |
| Handshake after a restart | lab-ext pinging vs idle | ~10 s while lab-ext sends data (WireGuard re-handshakes when its data gets no answer); up to ~120 s for an idle peer that only sends keepalives (keeps its old session until the rekey; the node only responds, its peer has no endpoint) — WireGuard timers, same rootful |

### Minimal planner capabilities (`tools/lab/caps.sh`, 2026-10-25, D44)

`caps.sh recreate NODE` re-runs the current run.sh on a node with its network mode and published ports (planner now
gets `--cap-drop ALL` + `CHOWN DAC_OVERRIDE FOWNER KILL SETUID SETGID NET_ADMIN NET_RAW NET_BIND_SERVICE`, + `SYS_ADMIN`
with frr) and waits until every instance that was up before is up again; `caps.sh check NODE` lists the container and
planner capabilities, every process with uid and `CapEff`, and instances that are not up.

| Node | Services | Back up after re-creation |
| --- | --- | --- |
| lab-b (host) | Tinc, FRR BGP/BFD/OSPF, dnsmasq (own user) + MosDNS + Mihomo DNS, Mihomo TPROXY | 28/28 instances, 55 s |
| lab-a (host) | Tinc, OpenVPN ext + Access (LDAP, alice reconnected), FRR + Anycast | 24/24, 82 s |
| lab-br (bridge) | WireGuard, FRR (eBGP to lab-ext) | 5/5 (origination `degraded`: the manual LAN route into the old namespace is gone, D43) |
| lab-c, lab-roam (host) | Tinc, FRR, Anycast (lab-c) | 18/18 in 45 s, 10/10 in 13 s |

On lab-b afterwards: `dns.sh freeze mosdns` → the front end switched to Mihomo DNS after 8 s and back 10 s after
`thaw` with 0 failed queries (the root wrapper re-reads upstreams and SIGHUPs dnsmasq running as another uid: KILL);
`dns.sh publish off` stopped dnsmasq within 6 s, `publish both` answered again after 15 s; `mihomo.sh probe` still
`tcp=198.51.100.10 udp=198.51.100.10`. Mesh pings between all nodes fine. A probe container with only the base set
showed avahi-daemon failing in `cap_set_proc()` without `SYS_CHROOT` (run.sh adds it for the avahi component; the
plugin otherwise starts it with `--no-chroot`; the first version rendered `enable-chroot=no`, which avahi rejects, see D45) and zebra without `SYS_ADMIN` as before.

### mDNS / Avahi (`tools/lab/mdns.sh`, 2026-10-25, D10/D45)

lab-b (host network, deployed without the `avahi` component, so no `SYS_CHROOT`) gets `services.avahi = { mode:
"managed", allowInterfaces: ["eth1"], bridge: { enabled: true } }` plus `dnsmasq.mdnsForward` on top of the DNS front
end (`dns.sh publish both`); host-b (10.62.0.50, Alpine) runs its own avahi-daemon (`mdns.sh setup`). The package
arrives with the configuration, the container role starts the system D-Bus once it is installed, planner runs
avahi-daemon (uid avahi, no capabilities, `--no-chroot`) and the bridge. The first runs found four defects (D45; lab
releases v0.7.6–v0.7.9, sequences 429–432): no bus in the container, `enable-chroot=no` rejected by avahi (exit 255),
`rlimit-fsize=0` ⇒ SIGXFSZ, and unknown names ⇒ SERVFAIL after the bridge's 3 s budget (bridge probe `down`).

With v0.7.9: from host-b through lab-b's front end `host-b.local` → 10.62.0.50 and `lab-b.local` → 10.62.0.12,
`nosuch-host.local` → NXDOMAIN, `who.lab.test` unchanged (192.0.2.1 via MosDNS); on the bridge directly MX → NODATA,
`example.com` → REFUSED; host-b's `avahi-resolve -n lab-b.local` → 10.62.0.12 (lab-b announces on its LAN);
`avahi/main` and `avahi/bridge` both `up`. `mdns.sh publish off` stopped both processes within 40 s (`.local` then
NXDOMAIN, other DNS unaffected), `publish on` answered again within 60 s. Reflector and IPv6: see below (D50). Not covered: bridge
mode (no multicast across the bridge by design), a host that also runs avahi-daemon.

### Blank-volume node with avahi (`tools/lab/mdns-fresh.sh`, 2026-10-25, D49)

`lab-av` (eth0 10.50.0.16, eth1 10.62.0.21 on lanb, host network) starts from empty volumes with node.env
`MEDUZA_NP_COMPONENTS=avahi,cli` and the offline site: `setup /root/lab/site-v0.8.2`, `deploy`, `publish on`, `check`.

| Case | Result |
|---|---|
| First install at bootstrap | generation 1 (planner, updater, cli, avahi) 3 s after run.sh, before any revision; planner container has SYS_CHROOT; system bus running |
| `publish on` | 4 avahi instances up after 1 s, avahi-daemon chrooted (uid 86, no caps), no static service files; host-b → `lab-av.local` 10.62.0.21; lab-av's bridge → `host-b.local`, `lab-b.local`, NXDOMAIN for unknown names |
| `publish off` | no instances, avahi-daemon stopped, package kept (selected in node.env) |

Host note: a new LXC whose systemd-networkd fails with "Too many open files" means the host's
`fs.inotify.max_user_instances` is exhausted (`sysctl -w fs.inotify.max_user_instances=1024`).

Lab notes: an offline import directory that is *replaced* (new inode) is not seen by an already running updater
container (bind mount); refresh it in place (`lab.sh site` does). The updater checks on its own randomized schedule
(kept across restarts); roll a new site out at once with `podman exec meduza-np-planner meduza-np update apply` on
each node. LXC does not
virtualize `/proc/meminfo`, so node memory figures in the lab are the host's.

### mDNS reflector and IPv6 (`tools/lab/mdns-reflect.sh`, 2026-10-25, D50)

lab-b runs managed avahi on eth1 (lanb) and eth0 (mnpwan); `probe` is a raw mDNS querier on lab-mgr (mnpwan, no
avahi) that prints the A/AAAA records it receives and their sender (`PROBE_HOST=lab-av PROBE_IF=eth1` = control on
lanb). Sequence `publish lan | reflect | reflect-ipv | off` with v0.8.4 (seq 437), rev-000075..78:

| Step | host-b.local from lab-mgr | lab-b.local from lab-mgr | bridge / front end |
|---|---|---|---|
| `lan` (eth1+eth0, IPv6 on, no reflector) | no answer (v4 and v6) | A 10.50.0.12, AAAA fe80::…:2473 | up; both names resolve |
| `reflect` | A 10.62.0.50 from 10.50.0.12 (reflected); AAAA not reflected | as above | up; own name answered by the bridge (D50) |
| `reflect-ipv` | also A 10.62.0.50 over IPv6 (from lab-b's fe80 address) | as above | up |
| `off` (eth1 only, IPv4) | no answer | no answer | up; both names resolve |

The first run (v0.8.2/v0.8.3) found three defects: every change to the running managed configuration was rejected
(planner's own avahi-daemon taken for a system daemon: its process title is rewritten), bridge misses took one 3 s
budget per allowed interface (probe `down: timeout` with the reflector's two interfaces), and a reflecting
avahi-daemon never resolves its own name over D-Bus (`lab-b.local` NXDOMAIN through the front end). Not covered:
reflection of global IPv6 addresses (the lab LANs have link-local IPv6 only; host-b's AAAA was never reflected).

### Avahi system mode on a native node (`tools/lab/mdns-system.sh`, 2026-10-25, D51)

lab-sys (10.50.0.17 / lanb 10.62.0.22) is a plain Debian 13 LXC with the distribution's avahi-daemon and a native
Meduza NP install (docs/operations.md §2.2, components `avahi,cli`), enrolled with `meduza-np enroll`. Sequence
`setup SITE`, `deploy`, `publish system` (rev-000079), `publish managed` (rejected: system daemon running),
`outage`, `reboot`, then planner restart with the daemon masked. v0.8.4 found two defects: probe/bridge calls
bus-activated the stopped system daemon (`outage` never saw it down), and a planner start without the daemon
rejected the last good revision and dropped the bridge until the next publish. Fixed in v0.8.5/v0.8.6
(`NO_AUTO_START`, missing daemon = warning + probe reason, planner unit `After=avahi-daemon.service`).

### Avahi system mode on OpenWrt (`tools/lab/mdns-openwrt.sh`, 2026-10-25, D54)

lab-ow (10.50.0.18 wan / lanb 10.62.0.23 lan, DHCP/RA off) is an OpenWrt 25.12 LXC (`images:openwrt/25.12`) with
the feed's `avahi-dbus-daemon` 0.9-rc5 under procd and Meduza NP 0.8.7 installed from apk packages built from the
tree (`build`: planner, cli, avahi; release tool + `tools/openwrt-apk.sh`). Sequence `build`, `setup`, `deploy`,
`publish system` (rev-000082), `check`, `publish managed` (rev-000083, rejected with the daemon's pid, rev-000082
kept), `publish system` (rev-000084), `outage`, `reboot`, then a planner restart with the daemon stopped.

| Step | Result |
|---|---|
| system mode | bridge resolves host-b 10.62.0.50, lab-b 10.62.0.12, lab-ow 10.62.0.23, NXDOMAIN for unknown names; host-b resolves `lab-ow.local`; avahi/main, avahi/bridge up |
| managed mode | refused: `a system avahi-daemon is already running (pid 3128 …)` (pid file `/var/run/avahi-daemon/pid` despite OpenWrt's `rlimit-fsize=0`) |
| `/etc/init.d/avahi-daemon stop` | avahi/main down `no system avahi-daemon is running (dependency missing): …NameHasNoOwner` (34 s, probe interval), bridge SERVFAIL, daemon stays down (no bus activation); start ⇒ all up in 25 s |
| LXC restart | rc.d `S60dbus S61avahi-daemon S95meduza-np-planner`; rev-000084 restored, all up 22 s after the restart, bridge resolves |
| planner restart with the daemon stopped | rev-000084 stays applied, no `lastError`, probe down with the dependency reason; all up 31 s after the daemon starts |

No product defect. Setup trap: installing `avahi-dbus-daemon` starts dbus before Avahi's bus policy exists; until
`/etc/init.d/dbus restart` avahi-daemon is "not allowed to own the service org.freedesktop.Avahi" and procd's
respawn gives up (documented in docs/operations.md §2.3). Planner's log lines show up as `daemon.err` in logread
(procd maps stderr to err); fixed by D55, see the next section.

### Planner log sinks under procd and systemd (`tools/lab/log-sinks.sh`, 2026-10-25, D55/D56)

Planner, cli and avahi rebuilt from the tree (`mdns-openwrt.sh build 0.8.9`). `openwrt` upgrades lab-ow's planner +
avahi apks and restarts the planner; `systemd` runs the built binary on lab-sys as transient units with isolated
directories and the dry-run network backend (the installed node is not touched).

| Step | Result |
|---|---|
| procd (`MEDUZA_NP_LOG_TARGET=split`) | logread: every planner INFO as `daemon.info`, the WARN `avahi/bridge: exited …` as `daemon.err`, no `<N>` prefixes; avahi instances up again, rev-000084 kept |
| SIGTERM to the supervised avahi/bridge | 0.8.8: `exit status: 1` + `bridge: Interrupted system call (os error 4)` (defect, D56); 0.8.9: `exit status: 0` |
| systemd, not enrolled | journal PRIORITY 6 for INFO, 4 for both WARNs (dry run, manager not configured), MESSAGE without the prefix |
| systemd, env enrollment fails | ERROR `enrollment from environment failed` with PRIORITY 3 |
| `journalctl -p warning` | no INFO lines |
| systemd with `StandardError=file:` (JOURNAL_STREAM names stdout only) | plain lines in the file, no prefix |

### Dashboard sign-in with the manager identity (`tools/lab/dashboard-login.sh`, 2026-10-25, D52)

lab-av (host-network base-container node, v0.8.2, dashboard 0.7.4) got `dashboard` added to `MEDUZA_NP_COMPONENTS`
and `MEDUZA_NP_DASHBOARD_PUBLIC_URL=http://127.0.0.1:8443`; `setup` re-created its containers and the updater
installed the new component from the selection alone (`update apply`, no new release). The browser is curl in the
planner container, the manager users are software passkeys (`admin`, and a new account `dash-viewer`).

| Step | Result |
|---|---|
| `/auth/login` → manager approval → `/auth/callback` (admin) | 302 to `#/dashboard-login/<id>`; the approval view shows node `lab-av` and the return URL; callback → session `admin`/`via manager` in 216 ms; status read and `POST /api/sync` (200) through planner |
| Replay of the callback with the login cookie restored | 403, no session; login state `used` |
| Wrong code, then the right one | 403, 403 (one attempt per login) |
| Deny | login `used`, a later approve answers 409 `not-pending` |
| Callback without the login cookie / with another login's cookie | 400 / 400 (state mismatch); the starting browser still 302 |
| Viewer account approves | session `viewer`, reads ok, `POST /api/sync` 403 |
| Role raised to operator between approval and exchange | session `operator` (current role) |
| Account disabled between approval and exchange | 403, no session |
| Approved login redeemed after 305 s | 400, no session: the dashboard's login cookie (max-age 300 s) is gone first, so the manager's own 5-minute check is not reached here (unit-tested) |

Found: refused redemptions (wrong code, replay, other node, disabled account) left no audit entry and a deny had no
node target; the manager now records `dashboard.login.refused` with the reason (D52, unit-tested; the lab manager
still runs the earlier build).

### Remote ASes behind lab-ext (`tools/lab/remote-as.sh`, 2026-10-31, topology view)

Two unmanaged networks behind the external router, so the topology view shows ASes known only from AS paths:
AS 65010 (lab-a) – AS 65100 (lab-ext) – AS 65200 (lab-net1, 10.50.0.31, 10.71.0.0/24) – AS 65300 (lab-net2,
10.50.0.32, 10.72.0.0/24). Both are Alpine LXCs with FRR only (about 30 MB each; a Debian LXC like lab-ext is ~1 GB on
the `dir` pool). `setup` also publishes, for lab-a's OpenVPN session `ext`, the two prefixes in `accept` and
`noTransit: false`; `remove` undoes all of it.

| Check | Result |
|---|---|
| lab-ext ↔ lab-net1 ↔ lab-net2 sessions | Established; lab-ext's table: 10.71.0.0/24 `65200`, 10.72.0.0/24 `65200 65300` |
| lab-a with only `accept` extended (rev-000090) | still only 10.70.0.0/24: by default a session accepts only routes the peer AS originates (as-path `^65100(_65100)*$`) |
| lab-a with `noTransit: false` (rev-000091) | within 15 s, no session reset: 10.71/10.72 learned, report `asPaths` [[65100], [65100,65200], [65100,65200,65300]] |
| `/api/v1/topology` | `peer:65100:10.70.0.1` "lab-ext" (lab-a and lab-br both peer with it), `as:65200`, `as:65300`, path links lab-ext → AS65200 → AS65300 via lab-a |
