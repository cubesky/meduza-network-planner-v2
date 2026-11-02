# Implementation progress (AGENT.md)

This file is the hand-off log between work sessions. Every session (interactive or the
scheduled "continue" task) must:

1. Read `AGENT.md` sections relevant to the next unchecked item, then this file.
2. Work on the **first unchecked item** (or finish one marked `[~]` in progress).
3. Keep changes buildable; run the relevant checks (`cargo test --workspace`, `pnpm -r build`, `pnpm -r test`).
4. Update the checklist and append a short entry to the session log below before stopping.

Design decisions live in `docs/DECISIONS.md`.

## Local environment notes

- A user-provided Linux test VPS may be available (details in the agent's local project memory, not in the repo).
  Linux-only acceptance scripts: `tools/container-e2e.sh` (rootful Podman) and `tools/openwrt-luci-e2e.sh`
  (`docker.io/openwrt/rootfs:x86-64-25.12.6`; never give it NET_ADMIN, netifd then replaces its network with br-lan).
- Windows 10 VM, no MSVC, no WSL, no Podman. Rust uses the `x86_64-pc-windows-gnu` host toolchain
  (self-contained mingw linker). Linux-only code (UDS, signals, netlink, nft) is behind `cfg(unix)`
  so the platform-neutral logic (protocol, generators, backoff, planning) builds and tests on Windows.
  Real musl/ELF builds run in GitHub Actions (`.github/workflows/ci.yml`).
- Node 24 (built-in `node:sqlite`), pnpm 12.
- Rust installed by rustup into `~/.cargo/bin` (add to PATH: `export PATH="$HOME/.cargo/bin:$PATH"`).
- Heredocs containing a lone `'` can break the Bash tool; write files with the Write tool instead.
- In Python helper scripts ALWAYS pass `encoding='utf-8'` to open() (the default is cp1252 here and corrupts `§`/`—`).
- windows-gnu needs a working `dlltool`: a shim at `~/.cargo/dlltool-shim/dlltool.exe` forwards to `zig dlltool`.
  Zig 0.13 + cargo-zigbuild 0.19.8 are installed with `pip --user` (same versions as CI).
- Use this PATH for all cargo commands:
  `export PATH="$HOME/.cargo/dlltool-shim:$HOME/.cargo/bin:$APPDATA/Python/Python312/Scripts:$APPDATA/Python/Python312/site-packages/ziglang:$PATH"`
- Host tests: `cargo test --workspace -j 3` (more parallel rustc jobs exhaust the 8 GB RAM). `cargo fmt --all` uses rustfmt.toml (max_width 140); `cargo clippy --workspace --all-targets -- -D warnings` must stay clean. Linux type-check/cross-build of unix-only code:
  `cargo-zigbuild zigbuild -p <crate> --target x86_64-unknown-linux-musl` (target installed via rustup).

## Checklist

Legend: `[ ]` todo, `[~]` in progress, `[x]` done.

### Phase 1 — foundations (AGENT §4, §5, §7, §8, §12.1, §13.4)
- [x] Monorepo skeleton: Cargo workspace, rust-toolchain.toml, pnpm workspace, 19 package dirs, .gitignore
- [x] `crates/protocol`: revisions, config model (§8), node report (§12.1), sync events (§7.3), manifest (§5.1), plan (§5.2), group manifest (§13.4), access-auth DTOs
- [x] `crates/plugin-sdk`: plugin stdin/stdout JSON protocol, request/response envelope, helpers
- [x] `crates/runtime`: atomic files + fsync, ownership journal, backoff/jitter gate (§7.5) with injectable clock/rng, node paths (supervisor lives in planner)
- [x] `schemas/`: generated from Rust DTOs via schemars (`crates/protocol/tests/schemas.rs`, regenerate with MEDUZA_NP_UPDATE_SCHEMAS=1)
- [x] Package manifests for all 19 packages (each plugin adds its `schema/config.json`, `schema/status.json`, `ui/descriptor.json` when implemented)
- [x] CI skeleton `.github/workflows/ci.yml` (Rust 3 musl targets via cargo-zigbuild, ELF/QEMU checks in tools/, web build, tests)

### Phase 2 — manager core + planner sync (§6, §7, §11, §14.4)
- [x] manager server: Fastify + node:sqlite, migrations, config/revision store, drafts/validate/publish/rollback, audit (`server/src/{db,publish,cluster,diff,audit}.ts`)
- [x] manager Passkey (SimpleWebAuthn): bootstrap token, register/authenticate, sessions, CSRF, roles, credential mgmt (`auth.ts`; tests use a software authenticator in `server/test/helpers.ts`)
- [x] manager node API: enroll, self/config (ETag), stream (WSS), reports, acks, jobs, rate limit + Retry-After (`nodes.ts`, `stream.ts`, `limits.ts`)
- [x] manager secret store (AES-GCM with key file), secretRef envelopes per node (`secrets.ts`, snapshots keep secrets encrypted)
- [x] manager web (Vite + Preact) in `packages/meduza-np-manager/web`: overview, nodes (+detail/jobs/enrollment), config drafts (descriptor forms, networks, instant validation, redacted diff, publish, history/rollback), keys, Access LDAP/policies/log, accounts/passkeys, audit. Dev run: `.claude/launch.json` (manager-dev) + scratchpad dev-session script. PKI page done (Phase 3); redacted support bundle export done (D40: CLI `support-bundle`, dashboard 导出支持包, manager Collect diagnostics → Download)
- [x] web/ui-kit + web/descriptor-renderer (schema utils tested with `node --test`)
- [~] manager Containerfile, Quadlet (.container/.volume/.network), env/config examples, Caddy+nginx examples, healthz/readyz, admin tool (status/backup/recovery-invite), ops README — written but NOT verified (no Podman on this machine); `tests/load/fleet.mjs` fleet load test DONE (first measurement in tests/load/README.md: 300 nodes, 92 MiB steady RSS). Image smoke DONE: `tools/manager-podman-smoke.sh` + CI job `manager-image` (rootless) — hardened run like the Quadlet unit, fail-closed http origin, health, passkey bootstrap, node enrollment, admin tool, read-only root, SIGTERM exit 0, data kept across re-creation, Quadlet dry run; VERIFIED rootful on the VPS (Podman 5.4), rootless only in CI (not run yet). Load runs at 1000 and 2000 nodes on Linux (VPS, bare node process) DONE, sizing table + recommendation in deploy/podman/README.md. In-container load runs DONE (`fleet.mjs --container IMAGE --memory`, VPS rootful Podman 5.4: 1000 nodes / 512 MiB and 2000 / 768 MiB, peak cgroup 89 / 183 MiB, no OOM kill, graceful stop exit 0). TODO: the rootless CI run (job `manager-image`, needs the repo on GitHub)
- [x] planner: registry scan, sync client (WS + poll, shared backoff gate), apply lifecycle (§11.1), journal, generations, supervisor, local UDS API, reports — code written in `packages/meduza-np-planner/src` and builds for windows-gnu and x86_64 musl; engine integration test `tests/engine.rs` passes (real WireGuard plugin + `examples/fake_{plugin,tool}.rs`); e2e planner↔manager test `packages/meduza-np-manager/server/test/e2e.test.ts` passes (enroll via env, WS push, apply, ACK, report); procd init + systemd unit in `packaging/`. OpenWrt ipk/apk packages: see Phase 7 (D29)

### Phase 3 — VPN (§9.1–9.4)
- [x] wireguard plugin (keys, wg setconf render, status parse) in `packages/meduza-np-wireguard`; manager compiler `compilers/wireguard.ts` (peer node/instance refs → publicKey/endpoint/allowedIps). wg-quick export = plugin action `export-wg-quick`
- [x] tinc plugin (names, hosts, keys via tincd -K / tinc generate-ed25519-keys, tinc-up/down own the address, host-up/down reachability, SIGHUP reload for peer changes) + manager compiler `compilers/tinc.ts` (mesh by instance id, collision check). Engine now honours `reloadSignal`.
- [x] openvpn plugin (site2site p2p/subnet client/server, access server with via-file hook, push routes from networks with pool exclusion + aggregation, auth-gen-token, route-nopull default, management `state` + status v3 probe) + `openvpn-auth` helper bin + manager compiler `compilers/openvpn.ts` (peer → remotes, CA/tls-crypt consistency)
- [x] manager Access LDAP adapter (`access.ts`) + PKI (`pki.ts`: CA, server/client certs with EKU, CSR signing, revocation + CRL, tls-crypt keys, Access .ovpn export; UI in `web/src/pages/Pki.tsx`) DONE and unit-tested. Live LDAP acceptance (D27): `tools/access-ldap-e2e.sh` + `server/test/access-ldap.test.ts` + CI job `access-ldap` — OpenLDAP with ldaps/StartTLS/memberOf, AccessService matrix and the real openvpn-auth → planner → manager → LDAP chain; VERIFIED on the VPS (Podman), CI job not run yet

### Phase 4 — routing/network (§9.5, §9.6)
- [x] frr plugin (own `-N meduza` pathspace, per-daemon configs, large-community origin tagging, deny-first external export, anti-spoof import, BFD profiles/peers, Anycast OSPF limited to VIP stubs + carrier interfaces with `anycast-guard` health hysteresis, vtysh JSON status) + manager compiler `compilers/frr.ts` (meshInternal, node refs, ASN checks)
- [x] planner network features `netplan.rs`: owned `meduza_np_nat`/`meduza_np_nat6` tables (prefix netmap for 1:1 mappings, DNAT/SNAT port forwards), forwarding sysctls, health-port listeners (`planner-alive`, `feature:<id>`)

### Phase 5 — DNS/proxy (§9.7–9.11)
- [x] dnsmasq plugin: fallback-first front end, wrapper (`frontend` mode) probes MosDNS/Mihomo backends with real queries (rise/fall), servers-file + SIGHUP switching, fallback restore, hosts via addn-hosts, `.local` forwarding to the Avahi bridge, port-53 owner detection, loop checks; DNS helpers in `plugin-sdk::dns`
- [x] mosdns plugin (complete v5 template: domain sets, forwarders, cache, full sequence, UDP/TCP servers; `run` wrapper refreshes rule files atomically with validation/backoff, keeps previous rules; loop checks against the front end)
- [x] mihomo plugin (proxy/tproxy modes, subscription normalization replacing controlled keys with reporting, owned `inet meduza_np_mihomo` table referencing the proxy-server set, auto exclusions, reserved fwmark/table + policy route, API readiness/groups/connections, DNS probe, `preview-config` action); shared `plugin-sdk::fetch`
- [x] mihomo-openwrt (OpenClash adapter): managed `/etc/openclash/config/<name>.yaml` from the subscription (OpenClash-controlled keys dropped + shown), UCI takeover recorded before any change (`enable`/`config_path`/`en_mode` only), debounced controlled restart, status via OpenClash controller API, tested-version gate (luci-app-openclash 0.45–<0.48 from opkg/apk db), disable ⇒ one-shot `release` restores options and deletes only adapter files; dnsmasq accepts `mihomo-openwrt.dns`. NOT verified on a real OpenWrt/OpenClash device
- [x] avahi plugin + mdns bridge: managed (`avahi-daemon -f` generated conf, refuses when a system daemon runs) or system mode; reflector off by default and needs ≥2 allowed interfaces; Rust `.local` bridge (`bridge` mode, loopback UDP, A/AAAA only, everything else REFUSED, bounded in-flight/timeout/cache) over a minimal hand-written D-Bus client (both byte orders) calling Avahi `ResolveHostName`; probe uses D-Bus `GetState`/`GetVersionString` + a real bridge query. VERIFIED live in the lab container (`tools/lab/mdns.sh`, D45:
  4 defects fixed — container system bus, `--no-chroot` flag, rlimit-fsize, bridge timeout ⇒ NXDOMAIN); with the
  component selected at container creation (SYS_CHROOT, avahi's own chroot) VERIFIED too (D48: image no longer ships
  Alpine's sample ssh/sftp static services, which were announced on the host LAN); `system` mode VERIFIED on a
  native Debian node with the distribution's avahi-daemon (D51, `tools/lab/mdns-system.sh`) and on OpenWrt 25.12 with
  procd's avahi-dbus-daemon (D54, `tools/lab/mdns-openwrt.sh`)

### Phase 6 — providers, clients, updater, container (§3.3, §12.3, §13, §14.3)
- [x] openvpn-pkg / mosdns-pkg provider descriptors + build recipes (D11): pinned `upstream.lock.json` (OpenVPN 2.7.8 sig-verified,
  OpenSSL 3.5.9, libcap-ng 0.9.6 by commit; MosDNS v5.3.4 by commit, Go 1.26.10), `build/{Containerfile,build.sh}` with static
  ELF gate + smoke + license collection, consistency test `provider_recipes.rs`, CI job `provider-recipes` (amd64).
  VERIFIED on the test VPS (Debian 13, Podman 5.4): both build for linux/amd64, linux/arm64 and linux/arm/v7 (QEMU),
  static (no interpreter/NEEDED), smoke passes on each target (openvpn: versions + management socket; mosdns: `version`)
- [x] mihomo-pkg (auto-download of a pinned tested asset with trusted SHA-256 → staging → gunzip → ELF/arch check → bounded version probe → atomic activate + one previous copy; manual mode never downloads and never overwrites; provider.json state; supervised `prepare` loop with backoff + registry rescan). Tested assets pinned with `meduza-np-release mihomo-assets` (D31): v1.19.33 for amd64/arm64/armv7; auto-download `prepare` VERIFIED live on the VPS (amd64)
- [x] cli (`packages/meduza-np-cli`, binary `meduza-np`; UDS client of planner/updater)
- [x] dashboard (D15): Rust server (static UI, fixed API map onto planner/updater, manager-identity sign-in via planner
  relay + manager approval page/one-time code, break-glass `meduza-np dashboard-link`, CSRF, roles, security headers,
  TLS for non-loopback) + Vite/Preact web (`packages/meduza-np-dashboard/web`, 32 KB JS / 14 KB gzip). Manager side:
  `dashboard.ts`, migration 2, `#/dashboard-login/<id>` page, tests. VERIFIED in the base container on the VPS
  (container-e2e step 6). Manager sign-in round trip VERIFIED against the live lab manager (`tools/lab/dashboard-login.sh`, D52)
- [x] dashboard-openwrt: rpcd exec plugin (CLI wrapper, argument validation), ACL (read/write split), LuCI menu + 4 JS
  views, luci.mk Makefile. VERIFIED on OpenWrt 25.12.6 userspace (procd/ubus/rpcd, `tools/openwrt-luci-e2e.sh`) and all
  four views rendered in real LuCI in the browser. Packaged as `_all.ipk` / `_noarch.apk` (D29), installed with apk
  in `tools/openwrt-pkg-e2e.sh` (ubus object answers)
- [x] updater (D12): signed channel/manifest (Ed25519, rotation, expiry, anti-rollback), subset resolve, store backend
  (objects + symlink views + atomic switch + GC) and opkg/apk backend, maintenance lock/drain/planner.restart/verify/
  rollback, crash recovery, self-update takeover, daemon + updater.sock; e2e test `packages/meduza-np-updater/tests/flow.rs`.
  opkg + apk backends VERIFIED in OpenWrt 24.10.8 / 25.12.6 userspace (upgrade with planner restart, self-update)
- [x] base-container: Containerfile, role entrypoint (updater takeover loop + image fallback, planner wait + Alpine deps
  cache, dashboard loop), Quadlet bridge/host, run.sh, .env.example, README with ops/capability matrix. VERIFIED on the
  VPS with `tools/container-e2e.sh` (bridge mode): blank-volume bootstrap from a signed offline source, re-creation keeps
  state, planner-only upgrade with restart, broken release rolled back, dashboard. Host network mode verified in the
  lab; rootless bridge mode VERIFIED in the lab (D57, `tools/lab/rootless.sh`: run.sh + Quadlet user units, reboot,
  graceful stop); arm images only under QEMU. Base image replacement helper DONE (D53: `deploy/podman/base-image.sh`
  plan/replace/rollback with signed-group target, `store-check` by the candidate image, health check + automatic
  restore; VERIFIED on the VPS for run.sh and Quadlet with `tools/base-image-e2e.sh`, CI step added). Network mode awareness (D41, §14.3.3) DONE: deployment
  declares `MEDUZA_NP_NETWORK_MODE`, report `system.networkMode`, host-network capabilities flagged
  `bridge-network` in bridge mode, manager pre-publish warning. Bridge-mode node VERIFIED in the lab
  (`tools/lab/bridge.sh`, D42): published WireGuard + port forward to an external peer, warning, limitation,
  container re-creation (lost namespace state recreated, stale UDP conntrack dropped), planner-only upgrade with
  0 lost pings. Minimal planner capability set (D44): `--cap-drop ALL` + base set (+ SYS_ADMIN frr,
  SYS_CHROOT avahi) in run.sh/Quadlet; VERIFIED by re-creating every lab node (`tools/lab/caps.sh`)

### Phase 7 — release (§15)
- [~] tools: `tools/release` (crate meduza-np-release): closure-scoped fingerprints, change/reuse matrix, deterministic
  package archives, signed group + channel pointer, pointer re-sign, CycloneDX 1.5 SBOM per package (`sbom`: Rust
  closure, npm runtime + `meduzaNp.bundled` from pnpm-lock, provider upstream locks; tested). OpenWrt packages (D29):
  `openwrt` (deterministic ipk per OpenWrt arch + apk stage → `tools/openwrt-apk.sh` with pinned Alpine `apk mkpkg`
  → `artifact --stage`), `classify` (changed packages per build job), group completeness per platform family.
  VERIFIED by `tools/openwrt-pkg-e2e.sh` on the VPS (apk on 25.12.6 + LuCI app, opkg on 24.10.8: install,
  reproducible files, updater upgrade, updater self-update, removal). OCI (D30): `oci-layout` (deterministic
  distribution image per archive package), `oci-describe`/`oci-check`, `group --oci --require-oci`, runtime images
  manager + base-container as group packages (fingerprints: manager covers schemas + all package metadata, base
  embeds the updater fingerprint), `tools/release-image.sh`. VERIFIED on the VPS with a local registry (skopeo from
  a container): pushed digests = computed, arm/v7 pull, 3-arch base image with embedded updater + trust-check under
  QEMU, 2-arch manager image + full manager smoke, idempotent re-run. Runtime image SBOMs (D28): base image +
  Node.js + every Alpine package per image platform (`sbom --image-os`, apk dbs copied out of the pushed image by
  `tools/release-image.sh`); VERIFIED on the VPS (3-arch base image, reuse path, armv7 list = `apk info -v`,
  CycloneDX CLI 0.27.2 schema validation). TODO: first real GHCR run (release.yml)
- [~] release.yml (D13): plan/fingerprints/matrix → per-target Rust build + provider recipes → verify reused, draft
  Release, sign group + pointer, URL check → Pages merge/deploy. NOT run (needs the repo on GitHub + signing secret).
  SBOMs of changed packages are uploaded as release assets. Dashboard web built once in `plan` (artifact
  `dashboard-www`) and packaged per target; OpenWrt ipk/apk in build-rust/build-providers + `build-noarch` job.
  OCI (D30): `build-images` job (manager, base-container: build, smoke, push, describe) and in `publish` the
  distribution images of changed archive packages, group tags on reused images, `--require-oci`, `oci-check` of
  every pinned index before undrafting. NOT run on GitHub yet; one-time: commit
  `packages/meduza-np-base-container/trust/release-keys.json`, make GHCR packages public
- [x] docs: `docs/operations.md` (install for Podman / native Linux / OpenWrt, enroll, upgrade, recovery table).
  Native Linux path VERIFIED with `tools/native-e2e.sh` (systemd, follows the doc literally); OpenWrt install section
  (§2.3) rewritten for the ipk/apk files and verified by `tools/openwrt-pkg-e2e.sh` (x86_64 userspace, no real router)
- [x] multi-node lab (`tools/lab/`, Incus LXC + nested Podman): manager + 2 static nodes with private LANs + 1 roaming
  node behind NAT; WebAuthn bootstrap, env enrollment, WireGuard mesh, single-node change, manager outage, fleet
  upgrades under traffic (0 lost pings), roaming uplink outage. Found and fixed 6 bugs (see tools/lab/README.md, D16).
  Process adoption across planned planner restarts DONE (D32 + D33 fixes; re-measured in the lab: planner-only
  upgrade adopts tincd + all FRR daemons, 0 lost pings on the mesh and LAN-to-LAN).
  Mihomo TPROXY end to end DONE (`tools/lab/mihomo.sh`, D34: manager subscription → mihomo-pkg auto-download →
  TCP + UDP of lab-b's LAN through a SOCKS5 server on lab-ext, refresh, disable cleanup).
  DNS front end DONE (`tools/lab/dns.sh`, D36: dnsmasq + MosDNS (mosdns-pkg) + Mihomo DNS + fallback on lab-b,
  hang/upstream loss/crash/both-down switching measured, loop refused, disable cleanup; 5 bugs fixed).
  OpenVPN Access with LDAP DONE (`tools/lab/access.sh`, D38: OpenLDAP next to the manager, Access server on lab-a,
  remote client lab-vpn; logins/refusals/routing, session lifetime, revoked user cut off at the next renegotiation,
  manager and directory outages keep running sessions, planner upgrade adopts the server; 2 bugs fixed).
  Access online sessions + operator disconnect DONE (D39: manager list from node reports, `disconnect-client`
  plugin action via management `client-kill … HALT`, token refused afterwards; lab: kill 0.33 s, list clear 28 s).
  Already covered by the README tables: Tinc mesh, iBGP/BFD LAN-to-LAN routing, Anycast OSPF, OpenVPN Site2Site
  with eBGP. Bridge-mode node DONE (D42) incl. FRR eBGP over its tunnel (D43: routes stay in the container
  namespace, host LAN not announced → new `frr/main/origination` status, routerId pre-check, capability warning)

## Session log

- 2026-10-22: Read AGENT.md, created this checklist, set up hourly scheduled continuation task.
- 2026-10-22: Phase 1 complete (protocol/plugin-sdk/runtime crates, generated schemas, 19 manifests, CI). Manager server
  core done with 10 passing integration tests (`pnpm --filter @meduza-np/manager test`). Next: manager per-package
  compilers come with each plugin; then manager web UI + ui-kit/descriptor-renderer, manager Containerfile/Quadlet,
  then the planner.
- 2026-10-22 (later): planner implemented (registry, plugin runner, apply engine with generations/ownership/
  supervisor, sync loops WS+poll on the shared gate, reports, jobs, UDS API, enroll). WireGuard plugin done.
  Engine integration test passes on Windows. Next: wireguard manager compiler, CLI, manager web UI, packaging.
- 2026-10-22 (evening): CLI, manager web UI, ui-kit, descriptor renderer done and checked in the browser pane.
  Next: manager Containerfile/Quadlet/docs, planner packaging, then Phase 3 plugins (tinc, openvpn + PKI).
- 2026-10-23 (night): manager Podman deployment files + ops README, planner packaging + env enrollment, planner↔manager
  e2e test (passes), tinc + openvpn plugins with manager compilers, PKI. 17 manager tests + 71 Rust tests pass.
  Next: Phase 4 — FRR plugin and planner-owned network features (mappings, port forwards, health ports).
- 2026-10-23: Phase 4 done (FRR + planner netplan), Phase 5: dnsmasq, mosdns, mihomo, mihomo-pkg done (all tests pass).
  Session stopped at usage limit. NEXT: mihomo-openwrt (OpenClash adapter; dirs scaffolded), avahi + mDNS bridge
  (dirs scaffolded), then Phase 6 (openvpn-pkg/mosdns-pkg recipes, dashboard, dashboard-openwrt, updater,
  base-container) and Phase 7 (release tooling). Run `cargo fmt --all` + clippy (host and musl) before continuing.
- 2026-10-22 (scheduled run): Phase 5 complete. mihomo-openwrt (OpenClash adapter, D9) and avahi + `.local` bridge
  (D10) implemented with unit tests (116 Rust tests, 18 manager tests pass; clippy clean on host and
  x86_64-musl). Moved the local controller HTTP client into `plugin-sdk::localhttp` (proper chunked decoding),
  fixed two pre-existing clippy findings (mihomo-pkg clamp, dnsmasq closure). NEXT: Phase 6 — openvpn-pkg /
  mosdns-pkg provider descriptors + build recipes, then dashboard, dashboard-openwrt, updater, base-container.
  Device verification still owed: OpenClash UCI option names/version range, avahi D-Bus bridge on OpenWrt.
- 2026-10-23 (interactive, with test VPS): Phase 6 done except release-side packaging. openvpn-pkg/mosdns-pkg recipes
  pinned + built and smoke-tested on real Podman (amd64; mosdns also arm64/armv7, openvpn arm64 via QEMU; openvpn
  armv7 was still building). Updater (D12), release tool (D13), base-container (D14), dashboard + manager sign-in
  (D15), dashboard-openwrt. Real-environment bugs found and fixed on the VPS: planner `packages` envelope, OpenVPN 2.7
  `--version`/TLS-less smoke, hyper header timeout without timer (dashboard panic), OpenWrt run/state dir mismatch
  between planner and CLI (now defaults in `NodePaths`). Linux: 141 Rust tests pass, container e2e + OpenWrt LuCI e2e
  pass. NEXT: release.yml OCI/SBOM/web/ipk steps and first real tag run, docs (install/enroll/upgrade/recovery),
  host-mode + rootless container runs, live manager↔dashboard sign-in, 128 MiB measurements.
- 2026-10-23 (later): native install path: updater launcher `updater-run` + systemd/procd units (shared
  `/run/meduza-np` preserved), `system-links` also exposes packaging/dashboard dirs and skips absent binaries;
  `tools/native-e2e.sh` passes on the VPS; all six provider builds (2 packages × 3 platforms) pass; Linux: clippy clean,
  142 Rust tests, 20/20 manager tests incl. the real planner↔manager e2e. CI now runs container-e2e, openwrt-luci-e2e
  and native-e2e. NEXT: release.yml OCI images (per package + multi-arch base-container), SBOM, dashboard web build in
  release, OpenWrt ipk/apk packaging (then exercise the updater opkg/apk backend in the OpenWrt rootfs container),
  host-mode/rootless container runs, live manager↔dashboard sign-in, 128 MiB profile measurements.
- 2026-10-23 (lab): multi-node lab on the VPS (Incus 6.0.4, nested Podman 5.4, 4 LXC, isolated bridges). Fixes from
  it: WireGuard `syncconf` + stable derived ListenPort (tunnels survive reloads/restarts behind NAT), rescan
  re-applies on package/provider change, manager Containerfile Node image pin, bounded apk deps in the planner role,
  planner package version in reports/logs. NEXT: process adoption for supervised daemons across planner restarts;
  FRR (BGP over the mesh for LAN routes), OpenVPN Access with LDAP, tinc and DNS front-end scenarios in the lab;
  bridge-mode nodes with published ports; then the release.yml OCI/SBOM/ipk work.
- 2026-10-23 (lab, roles): per the user, the internal mesh is Tinc only and runs without NAT; WireGuard/OpenVPN are
  external connections only (D17). Manager: `compilers/policy.ts` rejects managed-node references in WireGuard/OpenVPN
  peers, Tinc compiler requires `publicAddresses` and keeps roaming nodes out; plugin schemas lost `node`/`instance`
  (WireGuard) and `peer` (OpenVPN). Lab rebuilt: Tinc mesh lab-a/b/c + external WireGuard lab-roam → lab-a, all
  verified. More fixes from it: Tinc tap MAC per member (cloned machine-ids), orphaned instances cleaned up at planner
  start, component changes applied immediately. NEXT: supervised daemons surviving planner restarts (Tinc mesh blip
  0.8 s), FRR iBGP over the Tinc mesh for LAN-to-LAN routes, OpenVPN Access/external Site2Site in the lab.

- 2026-10-23 (scheduled run): `tests/load/` fleet load test (real manager process + IPC memory probe, simulated
  planners with WS push, ACKs, reports, roaming ETag polls, Retry-After, shared-gate-like backoff, optional restart).
  Measured 300 nodes on this Windows VM (steady RSS 92 MiB, peak 137 MiB, fan-out p95 1.4 s, full reconnect after
  restart 27 s, no 429/503). Not touched: manager image Podman smoke CI job. NEXT: that CI job, 1000+/Linux load
  runs and sizing table in the manager ops README, then the LDAP CI job and release.yml OCI/SBOM work.
- 2026-10-23 (lab, routing): D18 routing roles per the user: iBGP+BFD over Tinc for internal LANs, eBGP only over
  WireGuard/OpenVPN for external routes with `noTransit`/`noForward` (renamed from acceptTransit/exportExternal, both
  on by default) and `exportPolicy.internal`; Anycast as a cluster catalog (`routing.anycast`: pool, a /30 per
  service, node-side health) with per-node `anycastServices`, OSPF on every mesh member over Tinc
  (point-to-multipoint, prefix suppression), guard owns the VIP addresses; roaming nodes: no BFD, transit
  de-preferred (LP 50, prepend), per-session BGP `timers`. Measured in the lab: member hang withdrawn in ~1 s, power
  loss recovered 7 s after boot, roaming peer withdrawn in 6.7 s, crashed Anycast service 4.5 s, frozen Anycast node
  0.7 s with no loss on the remaining path. Fixes from it: stale pidfiles after power loss (tincd refused to start),
  podman-restart.service for reboots, DR outage on broadcast OSPF, zebra filter needing mgmtd, VIP local routes on a
  down dummy. Not done: a manager web editor for the Anycast catalog (document/API only), BGP export of Anycast
  prefixes to external peers. NEXT: supervised daemons surviving planner restarts, OpenVPN Access/external Site2Site
  in the lab, release.yml OCI/SBOM/ipk work.
- 2026-10-23 (manager UI): cluster-level Anycast catalog editor on the configuration page (pool, area, /30 services
  with next-free suggestion, health probe, metric, per-node "runs on" that edits each node's `anycastServices`; rename
  and remove keep node selections consistent). Interface languages en / zh-Hans / zh-Hant (D19): ui-kit `t()`/`msg()`,
  account setting with browser fallback, descriptor `i18n` for all 10 packages, completeness test. Verified in a
  browser: browser-language default, saving to the account, restore after reload, plugin forms in all three
  languages. Lab manager now serves `http://localhost:8787` for `ssh -L` access (migration 3 applied on the real lab
  database). Not done: schema help texts and server error messages are English only; the node dashboard's own strings
  are not translated yet.
- 2026-10-23 (manager redesign, per the user): direct editing without drafts (D21) — node Settings tab and Global
  settings share a working copy, apply bar with per-node diff, `POST /api/v1/config/apply` with conflict check,
  History page; node page split into Status / Routing / Connections / Settings / Diagnostics with restart buttons
  next to each service and details only on problems; separate Add-node page with enrollment instructions; Keys/PKI
  pages removed, keys created inline in the field that needs them, CA on Global settings, LDAP bind password on
  Access; refreshed visual style. Features configurable before installation (D20): planner waits instead of
  rejecting and writes wanted-components.json, updater installs them. Stale pre-D17 drafts on the lab manager deleted.
- 2026-10-23 (manager, per the user): Tinc is one mesh without netname/network list (D22, both shapes read during the
  transition); FRR mesh members need neither ASN nor peering address; LuCI-style staged changes with per-account
  drafts, rebase on concurrent publishes, per-section revert (D23); collections and nested lists edit in dialogs with
  tabbed sections; System/Light/Dark theme in manager and dashboard; the configuration store retries a failed load
  instead of showing "Loading…" forever (likely what blocked pre-configuring); feature badges count errors only.
- 2026-10-23 (manager, per the user): FRR has no settings section any more (D24). eBGP is set on the WireGuard peer
  or OpenVPN instance that carries it (remote ASN, accepted/advertised prefixes, timers, BFD), iBGP follows from
  Tinc membership and the internal ASN, BFD timers and ECMP paths are global settings, a branch node's own AS is on
  its Network tab, Anycast hosts are chosen in Global settings. Shared `BgpSession` type in the protocol crate;
  optional object groups in the form renderer get a switch. Lab topology rewritten without any FRR settings.
- 2026-10-23 (manager, per the user): the iBGP neighbors are always all other Tinc mesh members and only them (D25):
  no `meshInternal` opt-out, FRR cannot be disabled on a member, written `internalNeighbors` only tune a member's
  session; a warning asks for the internal ASN when the mesh has members but none is set.
- 2026-10-23 (manager, per the user): Topology page (D26) — 3D force-directed, draggable, animated graph of the BGP
  sessions from the published config and the nodes' FRR reports; iBGP between every pair of mesh members, eBGP from
  a node to the remote AS only, in different colours; details list each session's state, prefix counts and learned
  routes. FRR 0.5.1 reports the routes accepted from each neighbor and the node of each iBGP neighbor.
- 2026-10-23 (scheduled run): manager image acceptance `tools/manager-podman-smoke.sh` (7 steps, see header) and CI job
  `manager-image`; passes on the VPS (rootful Podman 5.4). Found: Podman's default OCI build format drops the
  Containerfile HEALTHCHECK, so plain `podman run` had no health check — the Quadlet unit already sets HealthCmd, the
  smoke now passes `--health-cmd`, Containerfile comment says so. NEXT: 1000+ node / Linux load run and sizing table in
  deploy/podman/README.md, OpenLDAP CI job for Access, release.yml OCI/SBOM/ipk work.
- 2026-10-24 (scheduled run): manager load test on Linux (VPS, 2 vCPU): 1000 nodes steady 163 MiB / peak 227 MiB,
  2000 nodes 239 / 363 MiB, no 429s, 2000-node reconnect bounded by the handshake limit (503 + Retry-After); sizing
  table and recommendation in the manager Podman README. Live OpenLDAP acceptance for Access (D27, script + test + CI
  job) passes on the VPS and found a real bug: the default user filter said `inetOrPerson` (matches nothing), fixed
  with a read-time repair of stored settings. Release tool `sbom` command (CycloneDX 1.5) + release.yml step; release
  tests, clippy and fmt clean on Linux; 27/27 manager tests (2 LDAP tests skip without the environment). NEXT: OCI
  images in release.yml (manager, base-container multi-arch, per package), dashboard web build in release, OpenWrt
  ipk/apk, in-container load run, process adoption across planner restarts.
- 2026-10-24 (scheduled run): OpenWrt packages (D29). Release tool `openwrt`/`artifact`/`classify`, deterministic ipk
  written in Rust, apk via `apk mkpkg` in the pinned Alpine 3.24 image (OpenWrt's apk has no mkpkg), one file per
  OpenWrt arch (x86_64, 4× aarch64, 3× armv7 hf), LuCI app noarch, only `meduza-np-*` dependencies, maintainer
  scripts that defer to the updater via `/var/run/meduza-np/updater-installing`. release.yml: dashboard web built once
  in `plan`, OpenWrt packaging in the build jobs + `build-noarch`, `.ipk`/`.apk` uploads; CI runs the new
  `tools/openwrt-pkg-e2e.sh`. Real bugs found by it and fixed: the updater launcher did not know the OpenWrt layout
  (no store, `/usr/bin/meduza-np-updater`), procd stop/restart orphaned the updater child (launcher now forwards
  TERM), the opkg/apk self-update failed because staging was GC'd before the updater installed itself, and after a
  native self-update neither `installed.json` nor planner's registry knew the new updater. apk + opkg runs pass;
  native-e2e still passes with the new launcher; clippy/fmt clean, release + updater tests pass (Linux). NEXT: OCI
  images in release.yml (manager, base-container multi-arch, per package), first real tag run, in-container load
  run, process adoption across planner restarts, OpenWrt on a real (arm) router.
- 2026-10-24 (per the user): roaming nodes are Tinc mesh members in the internal AS (D27): no publicAddresses needed
  (they dial out), iBGP with them without BFD on 3/9 timers, their transit at local preference 50 (FRR 0.5.2 adds
  `roaming`/`timers` to iBGP neighbors); the topology graph has no vertex for the internal AS. Lab: lab-roam moved from
  WireGuard + eBGP (AS 65020) into the mesh.
- 2026-10-24 (per the user): joining the mesh from the console alone (D28): node keys can be requested before the
  package is on the node (pending secret), the configuration applies, the updater installs the package (and planner
  restarts for its Alpine dependencies), the manager queues the key generation when the package is registered,
  retries a premature attempt and publishes again to distribute the key; the planner waits per feature for a missing
  node-held key and re-applies once it exists.
- 2026-10-24 (lab, per the user): external router lab-ext (unmanaged Debian, OpenVPN 2.6 + FRR, AS 65100,
  tools/lab/external.sh) on an OpenVPN Site2Site TLS tunnel to lab-a, eBGP from the instance's bgp block: all LANs
  reach 10.70.0.0/24 and back, lab-ext receives only the internal public LANs, the import filter rejects other
  prefixes, the topology graph shows lab-a as edge router to AS 65100. lab-a got the OpenVPN package through the
  updater (configured first). Found and fixed: CRLs from the manager CA had the PEM label `BEGIN CRL`, which OpenSSL
  does not read (OpenVPN rejected the instance); now `BEGIN X509 CRL`, stored CRLs are repaired at manager start.
- 2026-10-24 (per the user: lab-a showed no OpenVPN in the console): the console edited a stale saved draft (left
  open by failed lab publishes, based on rev-000010), so newer revisions were hidden. The store now rebases staged
  changes onto the current revision when loading (drops a draft with nothing left), checks for revisions applied
  elsewhere every 15 s (reloads, or merges on the next save when there are unsaved edits); the lab publish discards
  its draft when the publish fails. OpenVPN 0.5.5: the status no longer overwrites its kind with a `kind` detail
  (flattened into the status, it hid the tunnel from the Connections tab), and a p2p Site2Site server reports its
  session (CONNECTED, remote) instead of an empty client list.
- 2026-10-24 (per the user): Mihomo subscriptions as global settings (D29): list + default + refresh interval in
  seconds under Global settings, node override in its Mihomo panel (default preselected), forced refresh per node
  (descriptor action, now shown for settings blocks) and for all nodes or one subscription's users; URL secrets with
  scope `*`, only the chosen URL reaches a node. Not run live in the lab: meduza-np-mihomo-pkg has no pinned Mihomo
  release yet (empty `versions`), so there is no binary to run; covered by manager and plugin tests.
- 2026-10-24 (scheduled run): OCI publishing (D30). Release tool `oci.rs`: deterministic distribution images (single
  layer = package archive + descriptor, OCI layout, index digest computed before push), runtime-image description
  from the pushed index, group integration (`Artifact::oci` = multi-platform index reference, `oci`-format
  artifacts for manager/base-container, completeness + registry check). manager and base-container are fingerprinted
  group packages now. `tools/release-image.sh` + release.yml `build-images` job and OCI steps in `publish`. VPS
  run against a local registry found and fixed one real problem: `podman run --platform linux/arm/v7` cannot
  resolve the variant in a local manifest list (smoke now uses `--arch/--variant`). 10/10 release tests, clippy +
  fmt clean (windows-gnu). NEXT: first real tag run of release.yml (needs the GitHub repo, signing secret, trust
  store), in-container manager load run, rootless CI runs, process adoption across planner restarts, base-container
  SBOM (Alpine packages are not listed yet), mihomo-pkg tested-assets script.
- 2026-10-24 (scheduled run): mihomo-pkg tested assets (D31). Release tool `mihomo-assets --tag vX.Y.Z`: GitHub
  release document → platformMapping assets, download, size + SHA-256 against GitHub's digest, URL shape, bounded
  gunzip, ELF platform check, version string in the binary, `mihomo -v` on a matching host, merge into assets.json
  (other tags kept, newest first, `--keep`). Pinned Mihomo v1.19.33 (all three platforms checked), mihomo-pkg 0.1.1;
  the real auto-download `prepare` on the VPS downloaded, verified, activated and registered the binary (first live
  run). 14/14 release tests, mihomo-pkg + protocol tests, clippy + fmt clean (Linux). NEXT: Mihomo in the lab now that
  a binary exists (TPROXY + subscription end to end), first real tag run of release.yml, in-container manager load
  run, rootless CI runs, process adoption across planner restarts, base-container SBOM (Alpine packages).
- 2026-10-24 (scheduled run): in-container manager load runs. `tests/load/fleet.mjs --container IMAGE [--memory]
  [--container-cpus]` runs the production image hardened like the Quadlet unit (Podman secrets, named volume,
  read-only root) and samples the container cgroup (memory.current/anon, cpu.stat, oom_kill) + VmRSS; each stop
  records exit code and OOMKilled. VPS (rootful, cgroup v2): 1000 nodes under 512 MiB and 2000 under 768 MiB (the
  recommended limits) peak at 89 / 183 MiB cgroup, no OOM kill, stops exit 0 in ~1 s; results in tests/load/README.md
  and deploy/podman/README.md. Found on the way (test harness, not manager bugs): a synchronous `podman stop`
  froze the simulated fleet so the WebSocket close handshake never completed (manager hit its 20 s shutdown
  timeout, exit 1) -> all Podman calls async; simulated planners now use the planner's 10 s handshake / 20 s
  request timeouts (conmon holds the host port without accepting while the container is stopped). Manager:
  the shutdown timeout now logs open connections + streams before exit 1. Manager tests 29/29 (+2 skipped) on the
  VPS. NEXT: everything left needs GitHub (first tag run of release.yml, rootless CI runs of manager-image /
  access-ldap); otherwise Mihomo in the lab (TPROXY + subscription end to end), process adoption across planner
  restarts, base-container SBOM (Alpine packages).
- 2026-10-24 (scheduled run): runtime image SBOMs list the OS layer (D28). `sbom --image-os DIR` (required for
  manager/base-container): digest-pinned base image of the Containerfile's final stage (`pkg:oci`), Node.js of the
  node base image, and per image platform every Alpine package of the built image (`pkg:apk/alpine/...?arch=&distro=`,
  license name, origin/commit/checksum, dependency edges via `D:` + `p:` provides); refuses missing platforms,
  non-Alpine images and wrong-architecture databases. `tools/release-image.sh` copies `/lib/apk/db/installed` +
  os-release out of a created (never started) container of the pushed index per platform, also when the image is
  reused; release.yml passes `--image-os in/oci-<p>/<p>.image-os` for image packages. VPS: 15/15 release tests,
  clippy + fmt clean; real 3-arch base image built and pushed to a local registry, reuse run extracted identical
  databases, SBOM deterministic, armv7 package list identical to `apk info -v` in the image (42 packages), CycloneDX
  CLI validates the document. NEXT: still GitHub-bound (first tag run of release.yml, rootless CI runs); otherwise
  Mihomo in the lab (TPROXY + subscription end to end), process adoption across planner restarts.
- 2026-10-24 (scheduled run): process adoption across planner restarts (D32). Supervisor records each process
  (pid, start time, boot id, canonical program, launch-spec digest) under `<RUN_DIR>/supervisor`; `planner.restart`
  detaches instead of stopping; the next planner adopts on `ensure` when the record proves the same process and the
  spec/binary are unchanged, otherwise stops the recorded group and starts anew; `reap_unclaimed` after the
  last-good restore; adopted exits seen by polling. Daemon stderr now goes to a per-process log file forwarded as
  `<id>| line` (a procd/journald pipe would SIGPIPE adopted daemons). Startup keeps pidfiles of live recorded
  processes. systemd unit: `KillMode=process` + `ExecStopPost=stop-orphans` (keeps only on exit 75). Found while
  testing: `ensure` stopped the recorded process before trying to adopt it (fixed). New tests: supervisor
  `detach_adopt_and_reap`, `stderr_goes_to_log_file`, engine `planner_restart_adopts_daemons` (adopt same config,
  restart after a provider binary switch). VPS: workspace tests, clippy, fmt clean; Windows clippy of the planner
  clean. NEXT: re-measure the Tinc mesh gap in the lab with a planner-only upgrade (expect 0), then the
  GitHub-bound items (first tag run of release.yml, rootless CI runs), Mihomo in the lab.
- 2026-10-24 (scheduled run): re-measured planner-only upgrades in the lab with D32 and found two bugs (D33).
  (1) FRR daemons were never adopted: they run as user `frr`, `/proc/<pid>/exe` is unreadable without
  CAP_SYS_PTRACE and counted as "binary replaced"; records now carry the program file identity (`<dev>:<inode>`),
  which decides when the exe link is unreadable. (2) The restarted FRR daemons came up in alphabetical order (zebra
  last), bgpd attached to the old zebra, and after zebra was replaced bgpd's own `network` route stayed invalid:
  host-a lost host-b's LAN for good (88 % loss, no recovery). New optional plan field `ProcessSpec.boundTo`
  (validated: an earlier process of the same plan; schema regenerated): the supervisor broadcasts fresh starts and
  restarts bound processes when their target starts a new process, and never adopts them when the target was started
  anew; FRR plans zebra first with bfdd/bgpd/ospfd bound to it (watchfrr semantics). Also fixed a race the new test
  found: `adopted` in the status is now set synchronously at spawn. New tests: protocol `bound_to_names_an_earlier_
  process`, supervisor `bound_process_restarts_with_target`, `bound_process_not_adopted_after_target_restart`,
  `replaced_binaries`, FRR order/binding asserts. VPS: workspace tests, clippy, fmt clean (planner tests 3x);
  Windows clippy of protocol/planner/frr clean. Lab (lab-b, releases v0.5.6–v0.5.9): old planner → D32 3.4 s mesh /
  5.2 s LAN gap (packages changed); D32 → D32 planner-only: tincd adopted, 0 loss on the mesh, FRR restarted + LAN
  broken (the bugs); v0.5.8 with the fixes restarted FRR once in order and the LAN recovered; v0.5.9 planner-only:
  all five daemons adopted (same pids), **0 lost pings on the mesh and LAN-to-LAN**. Whole lab on v0.5.9, all LAN
  pairs + the external 10.70.0.0/24 reachable. NEXT: GitHub-bound items (first tag run of release.yml, rootless CI
  runs of manager-image / access-ldap); otherwise Mihomo in the lab (TPROXY + subscription end to end).
- 2026-10-24 (scheduled run): Mihomo TPROXY end to end in the lab (`tools/lab/mihomo.sh` + `tools/lab/lab-echo.py`;
  results table in tools/lab/README.md). lab-ext serves a SOCKS5 server (mihomo, TCP+UDP), the subscription and a
  TEST-NET "internet" endpoint that answers with the peer address; the manager holds the subscription URL (`*`
  secret, global `mihomo.subscriptions`) and lab-b's API secret; `mihomo.sh publish on|off` patches the live
  document. Found and fixed a deadlock (D34): with `services.mihomo` + `services.mihomo-pkg` (auto-download) the
  planner rejected the whole revision because the binary was not prepared yet, but the preparer is planned from that
  same revision, so nothing ever downloaded and every other change to the node was blocked. Now a feature whose
  provider is prepared at runtime waits alone (warning), the rest applies, the preparer's rescan re-applies. New
  engine test `preparing_provider_holds_back_only_its_feature`; planner tests 15 + 3 pass, clippy + fmt clean (VPS).
  Lab releases v0.6.0 (mihomo + mihomo-pkg added) and v0.6.1 (planner fix) on lab-b: binary downloaded, verified
  and registered in ~2 s, Mihomo started; host-b TCP and UDP to 198.51.100.10 proxied via lab-ext (TCP did not work
  at all without it), private destinations bypassed (LAN-to-LAN unchanged), proxy-server set populated and
  referenced, controlled keys replaced and reported (`overriddenKeys`), manager-forced refresh switched DIRECT ↔
  proxy (18 s), disable removed table/rule/route/processes/packages and kept the binary, re-enable reused it.
  Lab-b is left on v0.6.1 with Mihomo enabled (other nodes still on v0.5.9). Small finding, not fixed: the feature
  list shows `mihomo.dns` enabled/healthy while `dns.enabled` is false (per-package capability status). NEXT:
  GitHub-bound items (first tag run of release.yml, rootless CI runs); otherwise DNS scenarios in the lab
  (dnsmasq front end with Mihomo DNS / MosDNS backends and fallback), bridge-mode nodes.
- 2026-10-24 (scheduled run): fixed the Mihomo feature-list finding (D35): manifests may declare
  `capabilitySwitches` (`{capability, pointer, equals?}` into `services.<key>`); a switched-off capability reports
  enabled/running/healthy false (so `feature:` health ports stay closed) and `systemDependencies[].when` uses the
  switched set (feature list + apply-time availability). Declared for mihomo (tproxy/dns/subscriptions),
  mihomo-openwrt (dns/subscriptions), frr (anycast-ospf), avahi (mdns.bridge); `schemas/package-manifest.json`
  regenerated; `Planner::enabled_services` now returns the enabled service configs. New tests protocol
  `capability_switches`, planner `switched_capability_follows_its_config`. VPS: clippy, workspace tests, fmt, pnpm
  typecheck/build/test clean. Lab release v0.6.2 on lab-b only (others still v0.5.9): `mihomo.dns` disabled, the
  rest healthy, TPROXY probe still proxied via lab-ext. NEXT: GitHub-bound items (first tag run of release.yml,
  rootless CI runs); otherwise DNS scenarios in the lab (dnsmasq front end with Mihomo DNS / MosDNS backends and
  fallback), bridge-mode nodes.
- 2026-10-24 (scheduled run): DNS front end in the lab (`tools/lab/dns.sh`, results table in tools/lab/README.md,
  D36). lab-b: dnsmasq on 10.62.0.12:53 in front of MosDNS (mosdns-pkg, packaged from the recipe output via the new
  `TREES=` option of `tools/lab/next-release.sh`) and Mihomo DNS, fallback on lab-ext; lab-ext marker resolvers make
  every answer name its path. Measured: single backend hang or upstream loss 0 failed queries (switch 0.3–2.5 s),
  both hung ~7 s / 3 failed queries until the fallback, backend-set change 0 failures. Five bugs found and fixed:
  port-53 owner check rejected any socket on the port (systemd-resolved, aardvark-dns) -> address-aware; the own
  running dnsmasq counted as "unidentified" (fds unreadable without CAP_SYS_PTRACE) so every later revision was
  refused; static providers (`mosdns.binary`) reported unhealthy; the supervisor left the orphaned daemon of a killed
  wrapper running (now stops the leader's process group, new test `exited_leader_takes_its_group_along`); stale
  run-dir files after disable. Also: DNS probe timeouts say "timeout", and the manager refuses DNS loops at publish
  (`compilers/dns.ts`, new test; before only the node refused them). Lab releases v0.6.3–v0.6.5 on lab-b (others
  still v0.5.9), lab manager image rebuilt; lab-b left with the front end + both backends enabled (rev-000028).
  VPS: workspace clippy + 182 Rust tests, pnpm typecheck/build/test (manager 30 + 2 skipped) clean. NEXT:
  GitHub-bound items (first tag run of release.yml, rootless CI runs); otherwise OpenVPN Access clients in the lab,
  bridge-mode nodes; possible improvement: change the backend set without restarting dnsmasq (wrapper re-reads its
  spec).
- 2026-10-24 (scheduled run): the dnsmasq front end reloads a changed backend set instead of restarting (D37). The
  wrapper spec moved into the generation (`dnsmasq/main/frontend.json`, `--spec-file` via `${GENERATION_ROOT}`),
  the restart digest covers only `dnsmasq.conf`, and the wrapper re-reads the spec on planner's SIGHUP, keeping the
  health of backends whose id + address are unchanged (`frontend::carry_states`). New tests dnsmasq
  `backend_set_changes_reload_listener_changes_restart`, `reload_keeps_unchanged_backends`; dnsmasq tests, clippy,
  fmt clean (VPS). Lab release v0.6.6 (only dnsmasq changed) on lab-b: both → MosDNS → Mihomo DNS → fallback →
  both under `dns.sh watch`, dnsmasq pid unchanged, 0 failed queries of 246 (lab README table). Lab-b left on
  v0.6.6 with both backends (rev-000032); other nodes still v0.5.9. NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap); otherwise OpenVPN Access (LDAP) clients in the
  lab, bridge-mode nodes.
- 2026-10-24 (scheduled run): OpenVPN Access with LDAP in the lab (`tools/lab/access.sh`, results table in
  tools/lab/README.md, D38). OpenLDAP (osixia, test-only) on lab-mgr's Podman gateway 10.88.0.1:636, manager LDAP
  settings + policy `lab-vpn`, Access server `access` on lab-a (pool 10.98.0.0/24 originated into iBGP), new LXC
  lab-vpn 10.50.0.40 as the remote user with the exported profile. Login, refusals (group, password, unknown,
  injection, policy off) and routing to all three LANs worked at once. Found: the policy's session lifetime was
  ignored and a revoked user kept the session for the 8 h token lifetime. Fixed (D38): `client-connect` hook writes
  `session-timeout` from the granted lifetime (planner hands the decision over in memory, `access.session` on the
  Access socket), `auth-gen-token … external-auth` + password-less `reauth` request, manager re-checks account,
  groups, policy and lifetime against the last password login (migration 5 index); planner keeps token-verified
  sessions when no decision is available (`kept-unverified`), the connect hook refuses new connections then. Also
  `capabilitySwitches.item` (`openvpn.access` follows its instances) and the manager refuses Access `renegSeconds`
  1–299 at publish. Measured: revoked user cut off 279 s after the removal (renegotiation 300 s), manager / LDAP
  down > 300 s with 0 lost pings, logins back ~63 s (manager, §7.5 gate) / 1 s (LDAP) after recovery, 60 s
  lifetime enforced. Tests: protocol `capability_switches`, planner `access::*`, openvpn render, live LDAP test
  (reauth matrix, connect hook, token states); VPS: workspace clippy + 186 Rust tests, pnpm typecheck/build/test,
  `tools/access-ldap-e2e.sh` PASS. Lab: releases v0.6.7/v0.6.8 on lab-a only (lab-b v0.6.6, others v0.5.9), lab
  manager image rebuilt (schema 5); Access left enabled (rev-000036). NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap); otherwise bridge-mode nodes in the lab, an
  Access sessions page with manager-driven disconnect (management `client-kill`).
- 2026-10-25 (scheduled run): Access online sessions with operator disconnect (D39). Manager:
  `GET /api/v1/access/sessions` (openvpn-client statuses of the latest reports, enabled Access instances only,
  `disconnecting` once a disconnect is newer than the report), `POST /api/v1/access/sessions/disconnect` (audited
  `access_log` row `end/disconnected` that makes token re-auth answer `disconnected` until the next password login,
  then a node job `run-action disconnect-client`), sessions card with filter + disconnect on the Access page
  (zh-Hans/zh-Hant). OpenVPN plugin: `clientId` in client statuses, `disconnect-client` action (live `status 3`,
  `client-kill <cid> HALT` for every client of the user, idempotent). Tests: openvpn
  `disconnect_kills_every_session_of_the_user`, manager `operators see online Access clients…`, live LDAP test
  step; VPS: workspace clippy clean + 187 Rust tests, pnpm typecheck/build/test, `tools/access-ldap-e2e.sh` PASS.
  Lab: release v0.6.9 (only openvpn changed) on lab-a, lab manager image rebuilt; `access.sh sessions|kick` added;
  kill 0.33 s, client halted, list clear 28 s later, password login again 0.36 s (lab README table). Lab state:
  lab-a v0.6.9, lab-b v0.6.6, others v0.5.9; alice connected. Not verified: the sessions card in a browser (the
  preview server start was declined in this unattended run). NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap); otherwise bridge-mode nodes in the lab.
- 2026-10-24 (scheduled run): redacted support bundle (D40, AGENT §6.2 诊断 "脱敏支持导出", the last open TODO of the
  manager web item). Planner `support.rs` (`support.bundle` local API method: report, doctor, packages, instances,
  maintenance, last config, dry-run validation with file list only, 500 log lines; whole-document redaction of
  secret-like keys, PEM private/static keys, Bearer/Basic, URL userinfo, key=value pairs; capped at 192 KiB with a
  `truncated` list); `config.current` uses the same redaction; `collect-diagnostics` job returns the bundle; CLI
  `meduza-np support-bundle [-o FILE]`; dashboard `GET /api/support-bundle` + 导出支持包 button; manager Download
  button for diagnostics jobs (zh-Hans/zh-Hant) and job results over 256 000 chars stored as a valid
  `{truncated, bytes}` marker instead of cut JSON; docs/operations.md §7. Checks (Windows): planner `support::*`
  4 tests, `cargo test --workspace` 0 failures, clippy clean on touched crates, musl build of planner/cli/dashboard,
  manager tests 32 pass / 2 skipped (incl. the new one), typecheck + build of manager and dashboard web. Not verified: the two download buttons
  in a browser and the bundle on a live node (VPS not used in this unattended run). NEXT: GitHub-bound items (first
  tag run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise bridge-mode nodes in the lab.
- 2026-10-24 (scheduled run): container network mode awareness (D41, AGENT §14.3.3 "缺少条件时在预检和界面显示具体
  限制"). Protocol: manifest `hostNetworkCapabilities` (validated ⊆ provides), feature `limitationCode`/`limitation`,
  `system.networkMode` (schemas regenerated). Manifests: frr (bgp/bfd/anycast-ospf), mihomo.tproxy, mdns.avahi,
  planner network.mappings/port-forwards. Planner: `platform::network_mode` (env `MEDUZA_NP_NETWORK_MODE`, containers
  only), `Registry::mark_network_scope` in the report and the `features` API. Deploy: run.sh `--env
  MEDUZA_NP_NETWORK_MODE=$NETMODE`, Quadlet planner units `Environment=` (bridge/host). Manager: mode from the last
  report, pre-publish warning per service with enabled host-network capabilities and for active mappings/port
  forwards (`switchOn`/`enabledCapabilities` mirror the Rust switch rule); node page subtitle shows the mode, feature
  badges "container network only" (zh-Hans/zh-Hant). Dashboard: mode in 系统, 正常（受限） + explanation; CLI prints
  the limitation. Checks (Windows): `cargo test --workspace` 0 failures, clippy clean on planner/cli/protocol, musl
  build of planner + cli, new test
  `host_network_capabilities_are_limited_in_bridge_mode`, manager 34 pass / 2 skipped incl. `network-mode.test.ts`,
  manager + dashboard web build. Not verified: a live bridge-mode node (VPS not used in this unattended run), the UI
  in a browser. NEXT: GitHub-bound items; otherwise a bridge-mode node in the lab (base-container `--network bridge`
  with a published WireGuard port, external peer through it, planner-only upgrade under ping, feature list showing
  `bridge-network` for FRR/TPROXY).
- 2026-10-25 (scheduled run): bridge-mode node in the lab (`tools/lab/bridge.sh`, D42; README table). New LXC lab-br
  (10.50.0.15) with base-container `--network bridge`, WireGuard to the unmanaged lab-ext through the published
  51820/udp, port forward 8089/tcp → tunnel → lab-ext (SNAT). Found and fixed: run.sh `--publish 51820/udp`
  published a random host port (now the same port); after re-creating the planner container the fresh namespace
  had no wg link / nft table but the ownership journal claimed them, so restore ran `wg syncconf` on a missing
  link — `NetBackend::exists` (links, nft tables, routes, fwmark rules) and the engine recreates lost owned
  resources; a stale UDP conntrack entry (peer keepalives during the replacement) kept the tunnel down — run.sh
  deletes the entries of published UDP ports (Quadlet: commented `ExecStartPost=`); planner's own
  `network.*` features were never enabled (manifest `capabilitySwitches`, `networks` under `planner`), so the
  D41 limitation never showed for port forwards; process-less instances (kernel WireGuard) now count as running.
  Measured: pre-publish warning for the port forward only, `limitation=bridge-network` on it, re-creation → tunnel
  back in 15 s, planner-only upgrade 0/317 pings lost with `wg-wg0` kept; host-mode regression on lab-a/lab-b
  (v0.7.3): no false recreation, adoption intact, 0/350 pings lost. Tests: registry
  `planner_network_features_follow_the_node_networks`, netbackend `rule_listing`, engine step 2b (lost link
  recreated + reconfigured); VPS: clippy -D warnings clean, 194 Rust tests, pnpm typecheck/build/test (manager 34).
  Lab: releases v0.7.0 (seq 423, all packages, D40/D41 now on nodes) – v0.7.3 (426); lab-a/lab-b/lab-br on v0.7.3,
  lab-c/lab-roam still v0.5.9; manager image rebuilt; lab-br published (rev-000039), conntrack installed in lab-br.
  Not verified: the "no conntrack" note path, rootless bridge mode, the UI badges in a browser. NEXT: GitHub-bound
  items (first tag run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise roll v0.7.3 to
  lab-c/lab-roam, FRR on a bridge-mode node (routes into the container namespace).
- 2026-10-25 (scheduled run, VPS lab): lab-c and lab-roam rolled to v0.7.3 (0/350 mesh pings lost each). FRR on the
  bridge-mode node (`tools/lab/bridge.sh bgp on|off`, `bgp-check`; README section; D43): eBGP AS 65020 ↔ lab-ext
  AS 65100 over WireGuard, learned 10.70.0.0/24 only in the container namespace, `limitation=bridge-network` on
  frr.bgp/bfd. Found and fixed: (1) FRR on a node without routerId was refused by the node (whole revision) —
  manager `compileFrr` now errors on `/nodes/<id>/routerId`; (2) FRR auto-installed (D20) into a container created
  without the frr component lacks CAP_SYS_ADMIN — planner reports `system.capabilities` (containers only, schema
  regenerated), manager warns before publishing (`containerCapabilityWarnings`), base-container README note;
  (3) a LAN outside the namespace (or with its interface down) was silently not announced (import-check) while
  every session was up — FRR probe adds `frr/main/origination` (degraded + `notOriginated`). Verified: warning and
  error live against the rebuilt lab manager, degraded on lab-br, up after routing the LAN into the container
  (lab-ext learned it and reached 10.65.0.1), up on all mesh nodes. Releases v0.7.4 (seq 427, manager image
  rebuilt) and v0.7.5 (428, FRR only, rest reused); all lab nodes on v0.7.5, lab-br re-created with
  `wireguard,frr,cli`, rev-000042 live. Upgrades: lab-br 0/318 and 0/319, lab-a 0/350 pings lost. Checks (VPS):
  cargo fmt clean, clippy -D warnings clean on planner/protocol/frr, protocol/planner/frr tests pass (new
  `effective_capabilities_are_named`, `networks_missing_from_the_routing_table_are_reported`), manager typecheck +
  35 pass / 2 skipped incl. `container-caps.test.ts`. Not verified: the new status/warnings in the manager and
  dashboard UI in a browser (shown by the generic instance/problem lists). Observation, not changed: the planner
  container keeps Podman's default capabilities besides the added ones (KILL, SETPCAP, SYS_CHROOT, SETFCAP,
  FSETID). NEXT: GitHub-bound items (first tag run of release.yml, rootless CI runs of manager-image /
  access-ldap); otherwise rootless bridge mode, or a minimal planner capability set (`--cap-drop ALL` + needs).
- 2026-10-25 (scheduled run, VPS lab): minimal planner capability set (D44). run.sh and both Quadlet planner units
  now drop all capabilities and add `CHOWN DAC_OVERRIDE FOWNER KILL SETUID SETGID NET_ADMIN NET_RAW NET_BIND_SERVICE`
  (+ `SYS_ADMIN` for frr, + `SYS_CHROOT` for avahi); Podman's `FSETID SETFCAP SETPCAP SYS_CHROOT` are gone. Probe
  container (base set only): all component apk deps install, dnsmasq/dbus drop privileges and take SIGHUP/SIGTERM
  from root, avahi-daemon dies in `cap_set_proc()` without SYS_CHROOT → avahi plugin renders `enable-chroot=no` when
  its effective set lacks SYS_CHROOT (new `has_effective_capability`/`CAP_*` in plugin-sdk, frr's check moved there).
  Lab: all nodes re-created with `tools/lab/caps.sh recreate` (new): lab-b 28/28 instances in 55 s, lab-a 24/24
  (OpenVPN Access reconnected) 82 s, lab-br 5/5, lab-c 18/18, lab-roam 10/10; DNS front-end failover (MosDNS frozen
  → Mihomo DNS → back, 0 failed queries), DNS off/on publish (rev-000043/44), Mihomo TPROXY probe, mesh pings OK.
  Docs: base-container README, docs/operations.md, lab README section, manager comment. Checks (VPS): fmt clean,
  clippy -D warnings on plugin-sdk/frr/avahi, their tests pass (new `capability_bits_come_from_cap_eff`, avahi
  chroot lines), `sh -n run.sh`, Quadlet dry run of both planner units. Not verified: avahi with `enable-chroot=no`
  on a live node (no avahi in the lab; same as `--no-chroot`, which ran in the probe); no new release built, so the
  plugin change reaches nodes with the next release. Lab state: lab-br origination `degraded` until the LAN is
  routed into its new container namespace again (D43 case). NEXT: GitHub-bound items (first tag run of release.yml,
  rootless CI runs of manager-image / access-ldap); otherwise rootless bridge mode, or an avahi/mDNS host-mode node in
  the lab.
- 2026-10-25 (scheduled run, VPS lab): first live run of the avahi component (`tools/lab/mdns.sh`, new; D45). lab-b
  (host network, no avahi component ⇒ no SYS_CHROOT) with managed avahi + `.local` bridge + dnsmasq mdnsForward,
  host-b running its own avahi-daemon. Four defects found and fixed, each fatal alone: (1) the base container never
  started the system D-Bus ⇒ entrypoint `dbus_loop` runs `dbus-daemon --system` while meduza-np-avahi is installed;
  (2) D44's `enable-chroot=no` is no avahi config key (exit 255) ⇒ `--no-chroot` argv via `daemon_argv`, plus a test of
  every rendered key against avahi 0.8's key list; (3) `rlimit-fsize=0` ⇒ SIGXFSZ at the pid file, dropped; (4) the
  bridge's 3 s budget is shorter than Avahi's ~5 s TimeoutError ⇒ unknown names were SERVFAIL and the bridge probe
  always `down`; a reply timeout on ResolveHostName is now "not found" (unix test with a silent socket pair).
  Lab releases v0.7.6–v0.7.9 (seq 429–432) rolled out to lab-b only; base image rebuilt (lab-b re-created; other
  nodes still on the old image and v0.7.5, unaffected). Result: host-b → lab-b front end `host-b.local` 10.62.0.50,
  unknown `.local` NXDOMAIN, MX NODATA, non-.local REFUSED, `lab-b.local` announced on the LAN, both avahi probes up,
  off/on publish clean (rev-000052 live with mDNS on). Checks (VPS): fmt clean, clippy -D warnings + 11 tests on
  meduza-np-avahi, `sh -n` on entrypoint and mdns.sh. Not verified: a fresh container where avahi is selected at
  creation (SYS_CHROOT, chroot on), reflector, IPv6, OpenWrt; the avahi package stayed installed after `publish off`
  (not investigated). NEXT: GitHub-bound items (first tag run of release.yml, rootless CI runs of manager-image /
  access-ldap); otherwise rootless bridge mode, or avahi with the component selected at creation.
- 2026-10-25 (scheduled run): followed up the D45 lead "avahi package stayed installed after publish off": from the
  lab-b updater/planner logs on the VPS it was in fact removed (rev-000047, rev-000049), but the logs showed a real
  defect (D46): while a revision's new feature failed, the node installed and removed its package every ~2 min,
  because restoring the last good configuration (and `validate`/`plan.preview`/support bundle, which all call
  `prepare`) rewrote `wanted-components.json` without it. Fix in the planner: `prepare` no longer writes the list;
  `Engine::publish_wanted_components(raw, merge)` replaces it for every received revision (before `prepare`) and only
  merges on `restore_last_good`. Engine test extended (prepare leaves the list alone, restore merges, received
  revision replaces). Checks (VPS): fmt clean, clippy -D warnings on meduza-np-planner, its 31 tests pass. Not
  verified live: no lab release built with the fix (next lab seq after 432). NEXT: GitHub-bound items (first tag
  run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise roll the fix into a lab release and
  repeat a failing-feature publish on lab-b, rootless bridge mode, or avahi with the component selected at creation.
- 2026-10-25 (scheduled run): D46 verified live. Lab release v0.8.0 (seq 433, only meduza-np-planner rebuilt, 0.8.0)
  rolled out to lab-b; checks on the VPS: fmt clean, clippy -D warnings on meduza-np-planner, its 31 tests pass.
  Failing-feature publish (rev-000054, avahi mode "system" without a system daemon): package installed once, then
  12 min with no install/remove churn (installed generation constant, wanted list stable); good/off/on afterwards
  normal, lab left with mDNS on (rev-000057). Observation recorded in D46: a revision accepted while a feature waits
  for its package becomes last good, so after the plugin rejects it the restore reports `degraded: invalid-config`
  (open question, no change made). NEXT: GitHub-bound items (first tag run of release.yml, rootless CI runs of
  manager-image / access-ldap); otherwise decide the D46 last-good question, rootless bridge mode, or avahi with the
  component selected at creation.
- 2026-10-25 (scheduled run): D47 — answered the D46 last-good question. A revision applied with held-back features
  (D20/D34/D28) stays last good and records them (`heldBack`); `Engine::prepare_last_good` restores it without
  exactly those features when they are rejected once installable, keeps the saved last good unchanged and reports
  `degraded: held-back-rejected` + `lastError`. New engine test; checks on the VPS: fmt clean, clippy -D warnings on
  meduza-np-planner, its 32 tests pass. Not verified live (no lab release with it). NEXT: GitHub-bound items (first
  tag run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise roll D47 into a lab release
  (next seq after 433) and repeat the rev-000054 failing-avahi publish on lab-b, rootless bridge mode, or avahi with
  the component selected at creation.
- 2026-10-25 (scheduled run): D47 VERIFIED in the lab. Planner-only release v0.8.1 (seq 434) on lab-b, repeated the
  D46 failing-avahi publish: avahi installed once, `degraded: held-back-rejected` within seconds, no install/remove
  churn for 8 min, other features up (mesh ping ok), state survives a planner restart, the next good publish
  (rev-000060) clears it. Found that `lastError` showed only `services.avahi is invalid`: new
  `Rejection::summary()` (message + validation details) used for acks, restore `lastError` and logs; v0.8.2
  (seq 435) shows the plugin reason on the node and in the manager. VPS checks: fmt, clippy -D warnings, 32 planner
  tests pass. Lab left with mDNS on. NEXT: GitHub-bound items (first tag run of release.yml, rootless CI runs of
  manager-image / access-ldap); otherwise rootless bridge mode, or avahi with the component selected at creation.
- 2026-10-25 (scheduled run): D48 — avahi selected at container creation, in the lab. lab-b's node.env lists
  `avahi`, re-created with run.sh (`caps.sh recreate`): container has SYS_CHROOT, avahi-daemon runs chrooted (no
  `--no-chroot`), all 32 instances up after 67 s, `.local` resolution ok, also after killing avahi-daemon, killing
  dbus-daemon and restarting the planner container. Defect: Alpine's avahi package ships `ssh.service`/
  `sftp-ssh.service`, which avahi-daemon always announces (also with `--no-chroot`), so lab-b advertised SSH/SFTP
  on its LAN with `publish.services` off. Fix: entrypoint `prepare_deps` removes those two files
  (`drop_sample_services`); VERIFIED with a rebuilt base image (host-b lists no lab-b services after a cache flush,
  `lab-b.local` still resolves). Also fixed `tools/lab/mdns.sh state` (version/hostName were always null). Checks:
  `sh -n` on entrypoint.sh and mdns.sh (no Rust/web code touched). NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap); otherwise a blank-volume node with avahi selected,
  rootless bridge mode, or avahi reflector/IPv6.
- 2026-10-25 (scheduled run): D49 — blank-volume node with avahi selected, in the lab. New `tools/lab/mdns-fresh.sh`
  (LXC `lab-av` on lab-b's LAN, host network, empty volumes, node.env `avahi,cli`, site v0.8.2): updater installed
  planner/updater/cli/avahi 3 s after run.sh before any revision, SYS_CHROOT + system bus present; `publish on`
  (rev-000064) → avahi chrooted and up in 1 s, no static services, host-b resolves `lab-av.local`, lab-av's bridge
  resolves host-b/lab-b; `publish off` (rev-000065) stops it, package kept. No avahi defect. Fixed: the CLI panicked
  (abort) when stdout's reader exited first (`meduza-np … | head`); now `out!`/`emit` exit 0 on BrokenPipe (checks:
  fmt, clippy -D warnings, build on Windows + Linux, `help | true` exit 0 on the VPS). Lab host inotify limit raised
  at runtime to its configured 1024. lab-av left running, not in the document. NEXT: GitHub-bound items (first tag
  run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise avahi reflector/IPv6 or rootless
  bridge mode.
- 2026-10-25 (scheduled run): D50 — avahi reflector + IPv6 in the lab (new `tools/lab/mdns-reflect.sh`: lab-b avahi
  on eth1+eth0, raw mDNS probe on lab-mgr). Found and fixed three defects: (1) every change to a running managed
  avahi configuration was rejected — `foreign_daemon` took planner's own daemon for a system one (avahi rewrites its
  process title); ownership now proven by the D32 supervisor record (pid + start ticks + boot id); (2) the bridge
  asked allowed interfaces one after another (miss = 3 s × N ⇒ probe down with the reflector's 2 interfaces), now
  concurrently; (3) a reflecting avahi-daemon never resolves its own name over D-Bus ⇒ plan passes
  `--self-answer v4[,v6]` and the bridge answers the own FQDN from local addresses. Sync rejection log now prints
  `Rejection::summary()`. Lab releases v0.8.3 (seq 436) and v0.8.4 (seq 437) on lab-b: reflection v4, reflect-ipv
  (A over IPv6), lan/off isolation, bridge up and front end resolves own + LAN names in every step. Checks on the
  VPS: fmt, clippy -D warnings (avahi + planner), avahi 15 tests, planner 32 tests; Windows clippy of the avahi crate.
  Not covered: reflection of global IPv6 (lab has link-local only). Lab left on `off` (rev-000078, eth1 only).
  NEXT: GitHub-bound items (first tag run of release.yml, rootless CI runs of manager-image / access-ldap);
  otherwise rootless bridge mode, or a node that also runs a system avahi-daemon (mode `system`).
- 2026-10-25 (scheduled run): D51 — avahi `system` mode on a native node. New `tools/lab/mdns-system.sh`: LXC
  `lab-sys` (Debian 13, distribution avahi-daemon, native install per operations §2.2 with `avahi,cli`, enrolled
  with `meduza-np enroll`). Worked at once (bridge resolves LAN + own name, host-b sees lab-sys, managed rejected).
  Fixed two defects: (1) every probe/bridge D-Bus call bus-activated a stopped system avahi-daemon (Debian's
  `SystemdService=`), now all calls set `NO_AUTO_START`; (2) system mode without a running daemon was a validation
  error, so a planner start before avahi-daemon rejected the last good revision and dropped the bridge for good —
  now a validate/plan warning + probe reason `no system avahi-daemon is running (dependency missing)`, bridge keeps
  running (SERVFAIL) and recovers by itself; planner systemd unit ordered `After=avahi-daemon.service`. Lab releases
  v0.8.5 (seq 438) and v0.8.6 (seq 439, planner + avahi) on lab-sys: outage down 7 s / up 29 s, planner restart
  with daemon masked keeps rev-000081 applied with no error, reboot ok. Checks: fmt, clippy -D warnings and tests
  of meduza-np-avahi on Windows and the VPS. lab-sys left running in the document (rev-000081, system mode). NEXT:
  GitHub-bound items (first tag run of release.yml, rootless CI runs of manager-image / access-ldap); otherwise
  rootless bridge mode, or avahi system mode on OpenWrt (procd).
- 2026-10-25 (scheduled run): D52 — dashboard sign-in with the manager identity against the live lab manager. New
  `tools/lab/dashboard-login.sh` (setup: lab-av gets the `dashboard` component + loopback public URL, containers
  re-created, updater installs the component from the selection; check: 9 scenarios — admin round trip 216 ms,
  replay, wrong code burns the login, deny final, callback bound to the starting browser, viewer read-only, current
  role and disabled flag at exchange, audit; expiry after 305 s refused by the dashboard's 300 s login cookie). All passed. Change: refused redemptions
  are now audited (`dashboard.login.refused` with reason) and a deny names the node (manager unit test extended; 35/35
  manager tests, typecheck clean). The lab manager still runs the previous build. VPS needed
  `kernel.keys.maxkeys`/`maxbytes` raised at runtime (crun keyring quota). NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap); otherwise rootless bridge mode, the base-image
  replacement helper, or avahi system mode on OpenWrt (procd).
- 2026-10-25 (scheduled run): D53 — base image replacement helper (§13.5/§14.3.2, the last open base-container
  item that needs no GitHub). Updater: new module `image.rs` + subcommands `base-image` (verified group's
  `meduza-np-base-container` oci reference/platform digest/updater version for this platform) and `store-check`
  (read-only: installed-set/journal formats, backend, platform, current view, no update half done; exit 3), 3 unit
  tests. Entrypoint roles `store-check` and `updater-cli`. `run.sh` records its options (`--again`). New host helper
  `deploy/podman/base-image.sh` (status/plan/replace/rollback/history, run.sh and Quadlet, refuses shadowed Quadlet
  units) and `tools/base-image-e2e.sh` (7 steps, PASS on the VPS incl. Quadlet; CI step in `container-e2e`). Docs:
  base-container README section, operations.md recovery row. Checks: updater fmt, clippy -D warnings, 19+3 tests on
  Windows and Linux. The VPS host's leftover container-e2e containers were removed by the run (volumes kept); the
  native units were moved aside during the Quadlet step and restored. NEXT: GitHub-bound items (first tag run of
  release.yml, rootless CI runs of manager-image / access-ldap / the new base-image step); otherwise rootless bridge
  mode, or avahi system mode on OpenWrt (procd).
- 2026-10-25 (scheduled run): D54 — avahi `system` mode on OpenWrt (procd), the last non-GitHub follow-up. New
  `tools/lab/mdns-openwrt.sh` (build apk packages of planner/cli/avahi from the tree, OpenWrt 25.12 LXC `lab-ow` on
  mnpwan + lanb with the feed's avahi-dbus-daemon, enroll, publish, check, outage, reboot). All scenarios passed with
  no code change: bridge resolves LAN names + own name, host-b resolves lab-ow.local, managed refused with the
  daemon's pid, stop ⇒ probe down with the dependency reason / SERVFAIL / no respawn, start ⇒ up in 25 s, LXC restart
  (S60dbus < S61avahi < S95planner) ⇒ up in 22 s, planner restart without the daemon keeps rev-000084. Setup trap
  (dbus must be restarted after installing avahi-dbus-daemon) documented in docs/operations.md §2.3. lab-ow left
  running with avahi system mode in the document (rev-000084). VPS: old lab sites v0.2-v0.6 pruned (4.9 GB free).
  NEXT: GitHub-bound items only (first tag run of release.yml, rootless CI runs of manager-image / access-ldap /
  base-image step); optional: rootless bridge mode, planner logging to stdout under procd.
- 2026-10-25 (scheduled run): D55 — planner log sink follows the service manager (the optional "planner logging to
  stdout under procd" follow-up). `log.rs`: `MEDUZA_NP_LOG_TARGET=split` (procd init script now sets it) sends
  INFO/DEBUG to stdout (`daemon.info`), WARN/ERROR to stderr (`daemon.err`); under systemd, when `JOURNAL_STREAM`
  matches fd 2, lines carry sd-daemon `<N>` priority prefixes; otherwise plain stderr; write errors no longer panic.
  docs/operations.md: where to read planner logs. Checks: fmt, 2 new unit tests, Windows clippy -D warnings, musl
  build. NOT observed yet on real `logread`/journald (try on lab-ow and a native node next lab session). NEXT:
  GitHub-bound items only (first tag run of release.yml, rootless CI runs of manager-image / access-ldap /
  base-image step); optional: rootless bridge mode, the D55 lab check.
- 2026-10-25 (scheduled run): D55 lab check + D56. New `tools/lab/log-sinks.sh` (`openwrt`, `systemd`), planner/cli/
  avahi rebuilt from the tree on the VPS (`mdns-openwrt.sh build 0.8.9`). procd: lab-ow logread shows planner INFO as
  `daemon.info`, WARN as `daemon.err`, no prefixes. journald: lab-sys transient units of the built binary (isolated
  dirs, dry-run network) give PRIORITY 6/4/3 for INFO/WARN/ERROR with the prefix stripped, `-p warning` hides INFO,
  `StandardError=file:` stays plain. Defect found and fixed (D56): the avahi bridge exited 1 (`Interrupted system
  call`) on SIGTERM because a timed `recvfrom` is never restarted; Interrupted now continues the loop, exit 0
  verified on lab-ow. Checks on the VPS: avahi fmt, clippy -D warnings, 15 tests. lab-ow now runs planner/avahi
  0.8.9 apks (cli 0.8.7), rev-000084, avahi instances up; lab-sys untouched. NEXT: GitHub-bound items only (first
  tag run of release.yml, rootless CI runs of manager-image / access-ldap / base-image step); optional: rootless
  bridge mode.
- 2026-10-26 (scheduled run): D57 — rootless bridge mode (the remaining optional follow-up). New
  `tools/lab/rootless.sh` and LXC `lab-rl` (user mnp, Podman 5.4.2 + pasta; lab trap: pasta's AppArmor profile
  segfaults it in nested Incus, unloaded there). Rootless run.sh deployment, WireGuard through the published port and
  the report work unchanged. Fixed: (1) port forwards also in a nat `output` chain (`fib daddr type local`, not
  loopback) — rootlessport delivers published ports from inside the namespace, prerouting DNAT never matched (lab:
  no answer with v0.8.6, answered with v0.8.7); (2) entrypoint forwards SIGTERM to planner/updater and waits — tini
  only signalled the shell, which died and took planner down with the container (no graceful stop anywhere, Quadlet
  units `Failed`); now `stopped (exit status 0)`, `Result=success`; (3) run.sh enables the *user*
  podman-restart.service rootless (+ lingering warning), skips conntrack cleanup rootless. Documented host
  prerequisite: `systemctl add-wants multi-user.target network-online.target` (rootless Quadlet starts waited 90 s;
  also in the manager README); capability matrix + Rootless section in the base-container README; operations.md.
  Verified on lab-rl: check/recreate/reboot (podman-restart user unit, back in 4 s)/host-net (EPERM)/Quadlet user
  units + reboot; lab release v0.8.7 (seq 440, planner/updater/avahi changed), lab base image rebuilt with the new
  entrypoint (loaded on lab-rl only; other nodes keep the old image until re-created). Checks: planner fmt/clippy
  -D warnings/tests (Windows + VPS), nft -c of the new chain, entrypoint run_child unit checks in busybox sh,
  shellcheck -S warning. NEXT: GitHub-bound items only (first tag run of release.yml, rootless CI runs of
  manager-image / access-ldap / base-image step); optional: roll the new base image to the other lab nodes.
- 2026-10-26 (scheduled run): every open item is GitHub-bound (first tag run of release.yml / GHCR, rootless CI runs
  of manager-image / access-ldap / base-image step), so this run did an offline pre-flight for them instead.
  actionlint 1.7.7 (container on the VPS) over ci.yml + release.yml: no syntax, expression, job-graph or action-input
  errors; the embedded shellcheck pass reports info/style notes only (deliberate word splitting of optional flags
  `$base`/`$os`/`$pkgs`, `A && B || { exit 1; }` inside a pipe that `bash -eo pipefail` still fails) — no defects,
  nothing changed in the scripts. New ci.yml job `workflows` runs the same actionlint image with shellcheck off
  (exit 0 verified on the VPS; it needs a git checkout, which actions/checkout provides). Lab untouched. NEXT:
  GitHub-bound items only; optional: roll the D57 base image to the other lab nodes.
- 2026-10-25 (scheduled run): no change. The three open items ([~] manager rootless CI run, [~] release tools first
  GHCR run, [~] release.yml first tag run) all need the repo on GitHub plus the signing secret, and the offline
  pre-flight for them (actionlint, ci.yml `workflows` job) was done in the previous run. Nothing was edited apart from
  this line. NEXT: push the repo to GitHub and run CI and a release tag (needs the user); optional: roll the D57 base
  image to the other lab nodes.
- 2026-10-25 (scheduled run, later): no change. The remaining items are still the three GitHub-bound [~] items
  (manager rootless CI run, first GHCR run of the release tools, first tag run of release.yml), which need the repo
  pushed to GitHub plus the signing secret. Nothing was edited apart from this line. NEXT: needs the user (push +
  CI + release tag); consider pausing the scheduled task until then.
- 2026-10-26 (scheduled run): no change. Repo still has no commits and no remote, so the three GitHub-bound [~] items
  (manager rootless CI run, first GHCR run of the release tools, first tag run of release.yml) stay blocked. Nothing
  was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag); pausing the scheduled
  task until then is recommended.
- 2026-10-26 (scheduled run, later): no change. Repo still has no commits and no remote; the three GitHub-bound [~]
  items stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, evening): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, late night): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, after midnight): no change. Still no commits and no remote; the three GitHub-bound [~]
  items stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, early morning): no change. Still no commits and no remote; the three GitHub-bound [~]
  items stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, morning): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, midday): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, afternoon): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, evening 2): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 2): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 3): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 4): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 5): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 6): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 7): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 8): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 9): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 10): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 11): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 12): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 13): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-26 (scheduled run, night 14): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-27 (scheduled run): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-27 (scheduled run, later): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-27 (scheduled run, evening): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-27 (scheduled run, night): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-27 (scheduled run, night 2): no change. Still no commits and no remote; the three GitHub-bound [~] items
  stay blocked. Nothing was edited apart from this line. NEXT: needs the user (commit + push + CI + release tag).
- 2026-10-28 (per the user): topology graph clusters the nodes of each AS (bubble labelled with the ASN, cohesion
  force); eBGP between two managed nodes links them directly (no "provides" links any more); "Collapse external
  ASes" shows each AS other than the internal one as one AS vertex.
- 2026-10-30 (per the user): etcd import (D58). Import page (admin) converts `etcd.redacted.json`
  (`meduza-etcd-export-v1`): preview with nodes, values to supply and skipped keys; staging creates nodes, stores
  secrets, requests Tinc keys on the nodes and saves the document as the account's changes. New Global settings
  cards: BGP prefix filters, DNS hosts, Tinc mesh, MosDNS (domain lists, rule files, custom plugin chain); node
  Mihomo refresh override. FRR prefix filters, OpenVPN legacy compression/p2p pull and MosDNS custom plugin chain in
  the plugins. Tests: server/test/import.test.ts, frr/openvpn/mosdns unit tests.
- 2026-10-30 (per the user): TPROXY rework (D59). Import maps use_conntrack; Mihomo proxies only connections that
  start at the LAN (SYN / new conntrack entry), conntrack or socket-lookup mode, destinations routed over
  mesh/VPN/Access devices bypass the proxy, side-router mode (port-forward answers to the main router by own table,
  no ICMP redirects). Fixed: the automatic device exclusion missed the single-object Tinc mesh.
- 2026-10-30 (per the user): form layout. Labels are linked to their controls (click focuses, help text is the
  control's description) and drawn stronger than help text; on/off settings are tiles with the label beside the
  tick; nested objects in descriptors (`tproxy.sources`, `tproxy.sideRouter`) are boxed groups (switchable when
  optional) instead of JSON text boxes; integer lists (`excludePorts`) edit as "22, 53"; unset fields show their
  default as placeholder. Fixed: problems of dotted descriptor fields (`tproxy.port`) never reached the field.
- 2026-10-30 Manager ease of use: Global settings split into tabs (#/settings/<tab>) with red error counts and errors shown under the routing fields; node list gets search (name/ID/tag) and state filters (all / need attention / offline / not at revision); node Settings tab shows its error count; every browser confirm()/prompt() replaced by in-page dialogs (ui-kit `ask`/`askText`, Escape closes, focus moves in); enrollment commands and invite links get copy buttons (`CopyBlock`). Checked on a mocked preview (light/dark), deployed to lab-mgr.
- 2026-10-31 (per the user) Topology: graph controls (iBGP/eBGP/remote ASes/collapse/rotate/reset) on their own row,
  legend on two rows below. An unmanaged router we peer with is now a `peer` vertex (triangle) inside its own AS
  cluster, named by its BGP hostname or router ID (one vertex per router ID, so sessions from several nodes meet),
  instead of a bare AS diamond. ASes further away come only from AS paths: `as` vertices (small diamonds) chained by
  dotted `path` links, stopping at an AS that is managed or directly peered. FRR plugin reports per neighbor
  `hostname` (summary), `routerId` (`show bgp neighbors json`, only with external neighbors) and, for external
  neighbors, `asPaths` (distinct, prepending collapsed, AS sets dropped, max 32). Fixed: a click on a graph vertex
  counted as a drag on any pointer move, so selecting nodes often did nothing. Manager deployed to lab-mgr. Lab release
  v0.8.10 (sequence 443; changed: frr, dashboard, mihomo) rolled out to lab-a/b/c/roam (`lab.sh site` + `update
  apply`) and lab-br (`bridge.sh upgrade`, 0 % ping loss, wg-wg0 kept); lab-a and lab-br both report lab-ext as
  hostname "lab-ext", router ID 10.70.0.1, AS paths [[65100]]. Lab trap: non-interactive ssh lacks /root/.local/bin
  (cargo-zigbuild via pipx) on PATH: export it before `next-release.sh`.
- 2026-10-31 (per the user) Topology demo of remote ASes: new `tools/lab/remote-as.sh` adds two unmanaged Alpine+FRR
  routers behind lab-ext (lab-net1 AS 65200 10.71.0.0/24, lab-net2 AS 65300 10.72.0.0/24) and publishes lab-a's `ext`
  session accepting both with `noTransit: false` (rev-000090/091). lab-a reports AS paths up to 65100 65200 65300;
  the manager's topology has lab-ext as one router in AS65100 and AS65200/AS65300 as ASN-only vertices. Lab
  README section added. VPS disk: 1.8 GB free.
- 2026-10-31 (per the user) VPS clean-up: dangling host Podman images and anonymous volumes, old manager images in
  lab-mgr, `target/debug`, lab sites v0.7.0-v0.8.7 (kept v0.8.8-v0.8.10), apt cache. Disk 97 % -> 58 % (20 GB free);
  all lab LXCs running, nodes in sync, manager answers. Old test leftovers in /tmp were left in place.
- 2026-10-31 (per the user) Source pack for a new GitHub repository: `meduza-network-planner-v2-src.zip` (all files
  .gitignore keeps, LF, scripts 0755). Added `.gitattributes` (`* text=auto eol=lf`), `tools/fix-exec-bits.sh` (git on
  Windows stages new files as 100644; the script sets +x on every `#!` file, `--check` lists missing ones), a CI step
  running that check in the actionlint job, and a root README with the publish steps. Tested on Windows git: unpacked,
  `git init` + `git add` gave 0 executables, the script 45; index all LF; `cargo metadata --locked`, `pnpm install
  --frozen-lockfile --lockfile-only` and `cargo fmt --check` pass on the unpacked tree. Not pushed or run on GitHub.
- 2026-11-02 (per the user) macvlan side-router bootstrap: both planner and updater have their own static LAN IP
  and go directly to the main router before package downloads or remote configuration. Added optional
  `MEDUZA_NP_DEFAULT_GW` / `_DEV`, `MEDUZA_NP_DNS_NAMESERVERS` / `_MONITOR_INTERVAL`, legacy image aliases,
  DNS overwrite recovery, run.sh macvlan selection/replay and rootful Quadlet examples. Planner/manager report
  macvlan without the bridge-only scope warning. Startup/deployment tests, planner scope and Mihomo tests,
  manager server compilation/network-mode tests, Quadlet 4.8/4.9 dry runs, Rust formatting and actionlint passed.
  Real macvlan route/bind-mounted DNS test added to CI; local WSL denies network/mount namespace creation even
  as root, so that check has not run here. Base image not published or deployed.
- 2026-11-02 (per the user) Package installation progress: updater records the current package and verified byte
  totals; a shared read-only snapshot measures the current staging file, exposes phases/errors/retry time and
  pending components through updater.status and planner reports. Manager node lists/status/settings and dashboard
  overview/updates render the same progress panel; download completion is separate from installation/registration.
  Planner requests FRR for Tinc and active external WireGuard/OpenVPN links, including configurations without an
  explicit FRR service; disabled and Access-only links are excluded. Rust installation/schema/dependency tests,
  Clippy, manager progress/FRR/i18n tests, frontend typechecks/builds and a Chrome progress/error/verification
  preview were checked. These source changes have not been released or deployed to the v0.1.1 node.
- 2026-11-02 (per the user) Updater file downloads automatically retry transient errors (four total attempts,
  exponential 2/4/8 s waits with jitter); metadata uses the same policy. Failed passes retain the existing daemon
  backoff. Verified staging files are reused across passes; partial or tampered files are replaced. Progress
  includes the attempt count, retry deadline and transport error. Verification failures and permanent errors
  remain failures. Tests cover finite retry/exhaustion, real HTTP partial-body/503 recovery, signed-hash cache
  reuse and retry progress; the changes are source-only pending a new release.
- 2026-11-02 (per the user) Compared Mihomo TPROXY/DNS with the previous planner. Added project-owned DNS fallback
  and filters, domain/proxy-server/direct resolver policies, respect-rules bootstrap validation, Fake-IP range and
  filters, plus corresponding schema and localized forms. Added optional dedicated SOCKS port (legacy import 7891),
  preserved directional/protocol port exclusions during import, ingress exclusions and conntrack original-source
  bypass. Exclusions precede cached decisions; side-router conntrack replies return through the main router.
  TPROXY declares IPv4 forwarding without depending on NAT configuration. No OUTPUT interception is added.
  Rust regressions and Clippy, manager import/publish/i18n checks and frontend builds passed. Both generated
  conntrack/socket nftables tables passed `nft --check` on 10.42.10.9; generated DNS YAML passed the existing
  Mihomo v1.19.17 `-t` check in an isolated temporary directory. No running configuration was applied or restarted;
  these source changes still need a release and node upgrade.
- 2026-11-02 (per the user) Front-end DNS upstreams are automatic. dnsmasq derives candidates from enabled,
  installed MosDNS / Mihomo DNS services and follows their listener changes; OpenClash integration reads its
  adapter's live DNS port. Manual backend entries are discarded and removed from the UI. Fallback defaults to
  119.29.29.29 + 1.0.0.1, including old empty lists. Readiness still requires actual DNS answers; failures remove
  a backend, recovery re-adds it, and a configuration reload removes disabled/moved endpoints before probing.
  The frontend stays running throughout; node processes retain independent main-router gateway/system DNS.
  Verified real UDP probe recovery and OpenClash port discovery, plus a wrapper process integration test that
  follows generated service candidates through SIGHUP, fallback, failure/recovery with one unchanged frontend
  child PID. Manager import/publish/i18n tests (20), frontend builds and strict Clippy passed. Lab helper updated
  to select service switches instead of manual backends. These changes are not released or deployed.
- 2026-11-02 (per the user) Pages discovery API: retain legacy index names and signed updater endpoints; add
  sequence-ordered release/channel summaries and one-request full channel inventories with actual package versions,
  download URLs/size/SHA-256, OCI digests/platforms, changed/reused origins, compatibility, package metadata, SBOMs
  and pinned upstream assets. Generate catalogs/checksums from verified release files; backfill old groups without
  inventing unknown SBOM hashes. Merge Pages history without GitHub API queries and fail closed on failed requests
  or altered published manifests. SBOM reuse reads the Pages catalog and direct Release downloads, with hash and
  package/version checks; legacy groups use deterministic download URLs. Release asset discovery is queried once
  for idempotent uploads. Pages explicitly runs after a successful publish even when upstream build jobs skipped.
  All 18 Python release/Pages tests (including the workflow CLI and digest/sequence mismatch cases), actionlint
  and patch whitespace checks passed. Source/workflow changes only; the deployed Pages site updates on a
  subsequent release workflow.
- 2026-11-02 (per the user) Topology navigation: remove continuously fighting camera fit, keep dragged vertex
  objects across report refreshes, bound orbit tilt and ignore geometry behind the camera. Default auto-rotation
  is off; add screen-plane pan, pointer-anchored zoom, touch pinch/pan, keyboard controls, fit/focus and persistent
  cluster spacing. Seed clusters at separated planar anchors, move them as whole bodies, weaken cross-AS springs,
  and avoid overlapping labels. Six camera/layout/interaction regressions and a real Chrome topology preview passed;
  native Chrome pointer input also verified dragging across report refresh, right-pan, wheel zoom and double-click
  cluster focus.
- 2026-11-02 (per the user) MosDNS/OpenVPN binary resource management: add schemas and localized descriptors for
  both static `-pkg` providers, making them configurable in Manager without extra entrypoints/daemons. Show
  installation requests/progress and actual provider/path/package/bundled-core versions, including preinstalled
  resources with no Manager settings. Register standalone provider slots and report unavailable binaries clearly;
  resources do not expose service restart actions. Existing deployment/installed-set retention is stated explicitly
  when stopping requests. All 36 planner tests, schemas/manifests, strict Clippy and 10 Manager topology/catalog/
  preview/translation tests passed; production frontend builds and Chrome checked both resource panels. These
  source changes are not released or deployed.
- 2026-11-02 (per the user) Connection controls and base-image diagnostics: move WireGuard/OpenVPN restart
  buttons to their own peer/session rows and keep confirmation/restart scoped to the owning instance. Report
  the running base container separately from store packages and the embedded updater; new images record their
  own version, while legacy runtime files identify an installed image with an unknown version. All three
  reporter tests, strict planner Clippy, four Manager API/translation tests, production frontend builds and
  release/E2E shell syntax checks passed. Native Chrome verified four instance targets, row/header placement,
  viewer permissions and the legacy image badge. Source changes only; not released or deployed.
- 2026-11-03 (per the user) Unify Mihomo/MosDNS/OpenVPN binary resources in Manager: classify all binary-provider
  packages as resources, with shared installation-request controls, package version badges, expandable installed
  status without configuration and matching status columns/labels. Separate Mihomo acquisition/preparation details
  and preserve manual/download actions; hide preparation actions until registered and enabled. Report Mihomo's
  package/binary versions and registry-selected source independently, with legacy report version compatibility.
  Binary resource status rows have no service restart action; real preparer processes keep their restart control.
  Five Mihomo adapter tests, four manifest tests, schema freshness, strict Clippy, four Manager catalog/i18n tests,
  workspace typechecks/builds and native Chrome resource comparisons passed. Chrome covered all three resources,
  unconfigured/configured/missing/viewer states, legacy versions, activation targeting and restart availability.
  Source changes only; not released or deployed.
