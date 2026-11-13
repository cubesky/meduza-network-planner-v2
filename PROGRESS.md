# Implementation progress (AGENT.md)

This file is the hand-off log between work sessions. Every session (interactive or the
scheduled "continue" task) must:

1. Read `AGENT.md` sections relevant to the next unchecked item, then this file.
2. Work on the **first unchecked item** (or finish one marked `[~]` in progress).
3. Keep changes buildable; run the relevant checks (`cargo test --workspace`, `pnpm -r build`, `pnpm -r test`).
4. Update the checklist and append a short entry to the session log below before stopping.

Design decisions live in `docs/DECISIONS.md`.

### 2026-11-13 — D106: Access FRR push rejected by isolated preparation

Read-only inspect xuzhou-b1 on .9: rev-000032 is in-sync, but Access repeatedly reports FRR not
enabled and retains its old process/config without the FRR hook flag. Full configuration contains
FRR and Access frr/lan/privateLan pushes; registered planner/openvpn/frr are 0.4.2. The actual FRR
access-routes action returns 61 eligible routes, confirming the failure precedes client delivery.

Fix planner per-tunnel prepare to preserve other services' context while excluding them from repeat
preparation; replace only the current tunnel instances array. Keep sibling-error isolation, disabled
handling and survivor conflict validation. Declare services.frr in the OpenVPN manifest. Add real-
plugin regression for Access with FRR, healthy/broken Site2Site siblings, no duplicate other-service
planning, absent/disabled FRR rejection and option-off compatibility. Update AGENT.md, operations
and D106. All 110 relevant engine/library/hook/manifest tests, Clippy, formatting/diff checks pass;
evidence: target/check-access-frr-context-*.log. No production mutations, deployment, real client
reconnection or Git staging/commit. Publish the planner fix, reapply and reconnect for acceptance;
existing OpenVPN/FRR hook/export support does not require a system-binary replacement.

### 2026-11-13 — D105: native updater visibility and default update source

Diagnose 10.42.9.1 / roaming-cat1: updater 0.4.3 and its socket are running, planner 0.3.7 already
reports remoteControl=true after a check. Earlier empty-store reports lack update entirely because
snapshot ignores daemon activity before the first jittered check. Accept activity-only startup as
idle without claiming any successful/installed group, and extend the shared runtime regression.
Correct updater/base-image/example defaults from the obsolete 404 Pages URL to the actual repository
source; preserve explicit configuration priority. Document D105 and the operational recovery.

On the authorized node, persist baseUrl in a new 0600 updater.json, restart only updater, request
check and planner sync. Verify trusted stable v0.4.3 / aarch64_generic resolution with no blockers,
running service, error=null and in-sync rev-000013. Existing auto-apply policy stays enabled; no
manual apply, package upgrades or business-service restarts. Old system-installed versions have
rollback metadata warnings. Manager browser rendering remains unverified; local capability is true.
All 72 runtime/updater tests, all-target Clippy, formatting and diff checks pass. Evidence:
target/check-updater-source-{tests,clippy}.log. Source changes await release; no Git staging/commit.

### 2026-11-13 — D104: native OpenWrt single-package /tmp updates

Implement the requested download/verify/install/delete loop for native opkg/ipk and apk/apk, with
only one full archive in an owned private /tmp directory. Keep group/artifact verification, updater
takeover before planner, APK --allow-untrusted and add --no-cache. Persist source/plan and attempted/
completed mutations for recovery after losing /tmp; migrate legacy bulk-prepared updater handoffs.
Downloads release maintenance; each local mutation restarts/verifies its affected component before
continuing. Runtime progress follows temporary byte counts and download/retry/install phases.

Persist bounded artifact metadata instead of new rollback archives, restore old packages by verified
single-file re-download, and clean obsolete persistent cache/staging except unresolved legacy repair
data. Online rollback requires available source metadata/artifacts and reports restoration failures
explicitly. Restored download failures retain automatic backoff/retry. Preflight separately checks
largest single archive against /tmp/MemAvailable and each install's actual system partition space.

All 72 runtime/updater tests and one packaging/trust test pass; all-target Clippy with warnings denied,
shell syntax, rustfmt and diff checks pass. Fixtures cover both native managers, single-file cleanup,
updater-first/migration, corrupt/missing downloads, partial native failures, online rollback and reboot
recovery. Evidence: target/check-openwrt-stream-{tests,clippy,packaging}.log. Real OpenWrt userspace
end-to-end and hardware memory measurements remain pending (no local Podman/Docker). Update AGENT.md,
operations and D104; source only, no deployment or Git staging/commit. Release updater for this flow
and planner for the shared runtime's native download progress reporting.

### 2026-11-13 — D103: FRR route push for Access and Access-first console lists

Per user request, add default-off access.pushRoutes.frr to the OpenVPN model/schema and shared
Manager/Dashboard Access tab. Validate enabled local routing and include FRR in required packages
only when requested. After authentication, the client-connect hook asks planner's restricted socket
for selected, installed IPv4 BGP/OSPF/static routes from the owned FRR meduza pathspace. Exclude
defaults, special/blackhole routes and active Access pools; normalize covered prefixes. Cache five
seconds per revision, bound query/output/routes and reject the individual connection explicitly on
failure/overflow rather than truncate or reuse stale data. Preserve existing static pushes/session
lifetime; connected clients refresh by reconnecting. Document coordinated component upgrades.

Use presentation-only Access-first ordering for both consoles' OpenVPN settings/status, including
disabled Access entries and Dashboard applied/process tables. Preserve original config indexes and
stable IDs for edits/actions/errors. Reorder descriptor status sections and fix Access config rows
matching only Site2Site runtime kinds. No routing/NAT changes or forced client disconnections.

Validation passes: OpenVPN 16 library/two real helper socket tests; FRR 21 library/one owned-vtysh
action test; planner 50 library tests plus the new registered-plugin/cache integration; protocol 27
library/four manifest/one schema tests; eight targeted Manager tests. Chrome checks both actual
status pages plus translated FRR toggle/save, runtime match and correct-index disable/disabled
ordering. All affected Rust all-target Clippy, server compile, frontend/renderer typechecks, both
production frontend builds, rustfmt and git diff --check pass. Evidence: target/check-access-frr-*.
Updated AGENT.md §9.3, operations and D103. No deployment, live FRR/OpenVPN handshake, or Git
staging/commit; release updated planner/frr/openvpn and consoles, apply, then reconnect clients.

### 2026-11-13 — D102: fix Site2Site static-key direction compatibility

Per the user's follow-up authorization, add independent optional staticKeyDirection (0/1) to the
OpenVPN model, generated schema and shared Manager/Dashboard descriptor. Omission/null emits the
same undirected secret directive as v1 in either role; explicit values stay explicit. Match native
staged-key crypto preflight to runtime settings. Preserve tls-auth direction rules, compression,
static-mode provider opt-in and disabled-instance isolation. Correct v1 importer to retain static
key_direction under the new field and leave absent directions absent.

All 30 relevant tests pass: 15 OpenVPN, six Manager import/i18n, five renderer/schema and four package
manifest checks. All-target OpenVPN Clippy, Manager server compile, renderer typecheck, rustfmt and
git diff --check pass. Regressions cover omitted/null/0/1 for both roles, independent TLS fields,
invalid values, disabled instances, staged-key check arguments, explicit imported zero and nullable
UI editing/labels. Evidence: target/check-openvpn-static-{direction,import,ui,manifests}.log.
Use explicit directions for previously paired v2 peers during staggered upgrades; documented under
operations and D102. No production modifications, deployment or Git staging/commit. The diagnosed
lemon instance needs the updated meduza-np-openvpn package and reapply with staticKeyDirection unset;
live peer decryption/connection must be checked after deployment.

### 2026-11-13 — Read-only v1 Site2Site server comparison

Per the user's request to report differences before changing anything, compare v1 gen_openvpn.py,
the redacted XuZhou-B1/Lemon export and .9's active rev-000027 lemon config. TCP 20100, tuns1,
ifconfig 10.42.248.13/14, SHA256, AES-256-CBC, comp-lzo yes, allow-compression asym and keepalive
10/120 match. The significant incompatibility is static-key direction: v1 omits direction, while
the current renderer unconditionally appends server=0/client=1 and ignores tls.keyDirection for
static keys. OpenVPN documents omitted direction as bidirectional, distinct from complementary
0/1 operation. Live remote TCP connections reach .2:20100 and immediately repeat fatal decryption
errors; this matches the identified compatibility issue, though the peer's current configuration
and key equality have not been inspected. The provider is OpenVPN 2.7.8 with LZO/LZ4 support and
explicit static-mode compatibility opt-in. No service/source changes or production mutations;
report the finding and recommend configurable static-key direction with legacy omission support.

### 2026-11-13 — D101: corrected VNC target exposes side-router reply misrouting

User corrects the target to 10.42.10.4:5900. Its listener and RFB banner work from both 10.42.10.2
and 10.42.49.1, while the actual Android client .49.2 stalls in SYN_RECEIVED. Live lookups show the
unmarked client route uses tun_ovpn_access, but reply fwmark 0x4d50 forces it to .1 in table 4627.
The side-router reply jump precedes destination bypasses, so VPN-client SYN-ACKs take the wrong path.
The .49.1 probe had terminated locally and therefore did not exercise this faulty forwarded branch.

Fix meduza-np-mihomo's not_proxied chain to honor configured destination/routed-device bypasses before
setting its upstream mark. Preserve normal WAN replies, disabled bypass choices and cached TPROXY
ordering. No OpenVPN/profile or workstation route changes. Add native synthetic plan fixture example
and isolated Linux socket regression. Twenty Mihomo tests, all-target Clippy and native rustfmt pass.
Twenty-four real probes on authorized test container .45 reproduce prior VPN failures and validate
private/public VPN TCP/UDP recovery with conntrack on/off, retaining WAN replies. Test tools were
unpacked into an owned temporary directory; no services installed/started. Peer namespaces/processes
and owned temporary files cleaned, primary network unchanged. WSL lacks namespace permissions.
Evidence: target/check-access-return.log and target/check-access-return-network.log. Source fix needs
the normal Mihomo package update/reapply. No production mutation, workstation gateway edit or Git staging/commit.

### 2026-11-13 — Initial Access route diagnosis (target later corrected; see D101)

User reports missing 10.42.10.0/24 after enabling Access LAN/private-LAN pushes, using Android
OpenVPN Connect and repeated reconnects. Read-only SSH finds the Access server on 10.42.10.9's
Meduza planner, node xuzhou-b1, revision rev-000027. Both flags are true; its LANs are 10.42.10.0/24
and 10.42.49.0/24, privateLan is empty and the latter LAN equals the Access pool. Correct native
output therefore pushes only route 10.42.10.0 255.255.255.0, excluding its own pool.

Server PUSH_REPLY events at 21:48:56, 21:49:55 and 21:56:59 UTC on Nov 12 include that route.
Clients close/reset the TCP connections subsequently; the 21:58:46 status has no active clients.
IPv4 forwarding is 1, the LAN is on eth0 (10.42.10.2), and the pool route is on tun_ovpn_access
(10.42.49.1). These are server observations, not proof Android installed a route or of end-to-end
return-path reachability. No deployment, restart, configuration or workstation gateway change.

Checked Manager's Access profile export: it emits client and no route-nopull/pull-filter. OpenVPN's
official manual defines client as pull plus tls-client, so the missing separate pull line is not a
confirmed defect and no redundant directive/source patch is made. v1 aggregates all nodes' networks
where v2 currently uses local networks, but the user clarifies this complaint is the already-pushed
local 10.42.10.0/24, not other nodes; no route-scope expansion is made. At this point Android logs were
requested to distinguish route receipt from a data-path issue. Do not guess that Allow LAN Access is
responsible without evidence.
Source: https://openvpn.net/community-docs/community-articles/openvpn-2-6-manual/

Follow-up: user supplies C:/Users/max_3/Downloads/LocalSend/log_1794521025115.txt. Read it in place,
omitting sensitive auth lines. Its OPTIONS at lines 55/220/319/418 retain route 10.42.10.0/24, gateway
10.42.49.1 and client address 10.42.49.2; multiple CONNECTED events follow without route/TUN errors.
One separate attempt has AUTH_FAILED; the other sessions connect. The block-ipv4/block-ipv6 entries
are not proof of blocking the assigned IPv4 family: the core uses them for unused-family allowance,
and Android implicitly permits a family when its address/route is added. Do not recommend disabling
IPv6 protections based only on their names. Official core/Android references checked read-only.

While the user stays connected over mobile data, node netstat confirms ESTABLISHED TCP from
10.42.49.2 to 10.42.10.2:8443 and nonzero tunnel traffic. User clarifies the actual target is VNC on
10.42.10.5:5900. Two ICMP probes from each existing source 10.42.10.2 and 10.42.49.1 both succeed.
TCP connections to .5:5900 are immediately refused from both sources. Independently reproduce the
same refusal from this Windows LAN client 10.42.10.4. This applies only to the initially supplied .5;
the user subsequently corrects the VNC destination to .4. See D101 above for the actual reply-routing
defect and source fix. The earlier .5 refusal must not be used to diagnose the corrected target.

Additional bounded probes: .1 answers LAN-source ping but not pool-source ping; .45 answers both.
Do not infer a general pool return-path failure from the router's ping policy. SSH to .45 fails
public-key/password authentication and is not retried; it is unnecessary for the reproduced VNC
refusal. All probes used existing addresses and only read state/handshakes; no production service,
route, firewall, workstation gateway, VM or package change. No raw client-log copy or authentication
token persistence, Git staging or commit.

### 2026-11-13 — D100: LDAP save error details and transport hints

The user's ldap:// plus startTls=false is correctly rejected by the existing TLS-only password policy;
verifyCertificate=false does not allow plaintext. The UI incorrectly treated textual problems as
path/message objects, producing undefined. Extracted pure ApiError/errorText formatting supporting
both forms, malformed-entry filtering and useful fallback text, preserving existing API imports.

Added translated URL transport errors and blocked incompatible combinations before settings or bind
password writes. No automatic protocol changes and no backend TLS policy change. Authenticated API
regression confirms direct 422 requests preserve prior settings and both TLS combinations save.
All twelve API-error/Access/i18n tests, server/web typechecks and production build pass. Real Chrome
with synthetic responses verifies actual form validation, no invalid-transport writes, visible textual
422 details and successful corrected saves. Logs: target/check-ldap-save-tests.log,
target/check-ldap-save-browser.log and target/check-ldap-save-build.log. New source:
packages/meduza-np-manager/web/src/api-error.ts. No real LDAP/production changes, deployment or Git
staging/commit.

### 2026-11-13 — D99: AS headings stay near stable owners

Replaced distant candidate rings/global fallback with bounded local AS placement. Group headings now
follow a deterministic primary/ID member at its depth instead of a potentially huge bubble perimeter.
All AS placement uses stable identity order, independently of painter depth. Reserve hidden fixed
router panels to prevent hover-induced repacking and keep state across view operations/report order.
Significant persistent collisions must survive twelve draws before a slot switch, followed by at most
four pixels of local offset movement per draw. Long names cannot increase the title-center distance.
Dense views prefer bounded overlap to detached labels;
Node attachment, two-line names, translucent depth rendering and leader-free interaction are preserved.

Verification: all 29 graph tests, Manager server/web typechecks and production build pass. Regressions
cover forty crowded headings staying local and settling, long names/zoom, a wide cluster member anchor, depth ties,
hover/view persistence and bounded smooth collision recovery. Actual Chrome verifies the real Manager
page's label interactions, framing, polling, collapsed roots, four viewport sizes, three orbit cases and
forty dense AS headings stable for thirty additional draws. Evidence:
target/check-as-label-placement-tests.log, target/check-as-label-placement-browser.log,
target/check-as-label-placement-build.log and
target/as-label-attached-overview.png. No deployment, production mutation or Git staging/commit.

### 2026-11-13 — D98: full retained BGP paths and multi-hop topology

The user selects all FRR-retained post-policy paths, including non-best and invalid candidates;
pre-policy rejected routes and automatic soft-reconfiguration are out of scope. Read-only live
summaries identify known-AS traversal truncation as the immediate graph issue, with the old 32-path
sample, valid-only and IPv4-only collection also removed. No production routing changes are made.

FRR publishes a full IPv4/IPv6 snapshot with original AS_PATH, prefix, flags and next hops. Extract
each native table sequentially to reduce memory and retain bounded query/transport limits with
explicit incomplete/error reporting. Manager continues paths through known AS and managed/iBGP peers,
canonicalizes IPv6 anchors, retains ordered suffixes after sets, and avoids invented unordered links.
The new BGP paths tab and selected-object details search/filter/page all records, including AS names,
with raw notation preserved and old-agent/incomplete notices. Reset/direct and fit-all remain distinct.

Verification: 20 FRR tests, all-target Clippy -D warnings, Manager server/web typechecks, 90 Manager
tests passing (three skipped), production Vite build, native rustfmt and git diff --check. A real-plugin
synthetic probe retains all 286 candidates and verifies failed/oversized query diagnostics. Real Chrome
checks 124 synthetic records over three pages, invalid filter, IPv6/AS_SET, AS-name search, known-AS
transit, framing and responsive sizing. Logs: target/check-all-bgp-paths-final.log,
target/check-all-bgp-paths-manager-suite.log, target/check-all-bgp-browser.log and
target/check-all-bgp-paths-build.log. Automatic approval rejected optional local persistence of live
full-table topology metadata; no live-table files were created and synthetic acceptance completes
validation. Upgrade Manager and the node meduza-np-frr adapter together. No remote mutations,
deployment, Git staging or commit.

### 2026-11-13 — D97: optional OpenVPN management and independent liveness

Diagnosed 10.42.11.2 read-only: /usr/sbin/openvpn 2.7.4 rejects management and --help is unavailable.
Confirmed --ignore-unknown-option management makes its read-only --show-ciphers parser check exit 0;
without it the check exits 1. The persist-key warning is unrelated. Upstream source confirms the
prechecks exit before native PID writing, management socket binding and tunnel startup.

Made only management optional in generated configs, retaining native crypto/key checks. Generate
owned PID/status files for all instance modes, verify executable/config command lines before claiming
liveness, and accept only fresh status data from the running process lifetime. Added p2p transfer
counters and explicit unavailable session state, preserving process/device/session distinctions and
actionable errors for unsupported selective client disconnect. New source: openvpn/src/monitor.rs.

Verification: 92 tests pass across planner/OpenVPN/WireGuard (49 planner unit, 15 engine integration,
13 OpenVPN, 15 WireGuard), plus all-target Clippy -D warnings. The new real-adapter regression reproduces
management parse failures, recovers healthy instances, preserves another instance's native crypto
failure and verifies live PID observations without sockets plus stale-PID rejection after stop.
PID reuse/wrong executable/config, stale status, generator whitelist and conservative session status
tests pass. Evidence: target/check-openvpn-small.log, check-openvpn-small-engine.log and
check-openvpn-small-all.log. Native rustfmt and git diff --check pass. No device files/services changed,
no package installation/deployment, and no Git staging/commit.

### 2026-11-13 — D96: disabled tunnel entries across publish, runtime and status

Completed disabled-state handling for WireGuard/OpenVPN services/instances and WireGuard peers.
Manager policy, active-projection schema validation and OpenVPN compiler checks ignore dormant
operational fields while preserving drafts/snapshots and original error indices. Secret delivery
retains D94 semantics. SDK active_service filters disabled entries before typed parsing; the tunnel
validators also skip disabled typed entries. Dormant duplicate identities/keys/devices do not block
active entries. Runtime preparation, native generation and probing omit disabled entries.

Added planner tunnels.rs to resolve configured activation and supply disabled rows from raw config.
Parent disablement propagates to children, cached failures/retry/traffic details are cleared and
removed entries disappear. Package health ignores disabled peers and stale disabled failures.
Applied disables stop owned processes/resources; peers are removed from wg syncconf. Retry and local/
Manager start/restart guards cannot revive disabled instances even with stale persisted errors.
Manager status rows and shared Manager/Dashboard collection forms hide their restart actions while
retaining editing/enable controls. No production changes and no Git staging/commit.

Verification: 99 Rust tests pass across SDK/planner/WireGuard/OpenVPN (11 SDK, 49 planner unit,
14 engine integration, 15 WireGuard, 10 OpenVPN), plus all-target Clippy with -D warnings. Added both
tunnel types' run/fail→disable→re-enable lifecycles, stale retry prevention, cached status cleanup,
native peer removal, dormant parsing/resource conflicts and package-health regressions. The final
peer health assertion also passes in isolation. Manager's sync/config/import/install-progress files
pass all 29 tests, including dormant publish/secret exclusion and original schema-path checks.
Manager server/web and Dashboard web typechecks, both Vite production builds (Dashboard compression
included), native rustfmt and git diff --check pass. Evidence: target/check-disabled-entries-all.log,
check-disabled-peer-health.log, check-disabled-entries-manager-all.log and check-disabled-*-build.log.

### 2026-11-13 — D95: WireGuard/OpenVPN per-instance failure isolation

Completed the user's requirement that a failed tunnel must not prevent other instances from starting.
Prepare enabled instances independently, validate surviving groups for device/port collisions, and
retain full cross-service context (notably FRR Access pools). Missing active keys now hold back only
their owning tunnel. Commit isolates native checks, network resources, reload and exec failures;
preflight failures preserve prior instance files/processes, runtime cleanup touches only that owner.
Persist sanitized instance errors and merge them into Manager reports and Dashboard/local API rows.
Split tunnel probes by instance and include apply errors in package health.

Added bounded, jittered exponential retry of failed instances, maintenance/revision guards, draining of
queued revisions, and manual start/restart recovery. Scoped retries skip healthy checks/processes.
Same-revision retries stage fresh directories and prune bounded history after commit. CI/release test
builds include the actual OpenVPN adapter. Updated AGENT.md, decisions and operations documentation.

Verification: `CI=true cargo test --offline --locked -p meduza-np-planner -p meduza-np-wireguard
-p meduza-np-openvpn` passes all 82 tests (47 planner unit, 12 engine integration, 14 WireGuard,
9 OpenVPN). The engine tests cover both adapters' prepare/group conflicts, delivery failures, retained
old files, stable healthy PIDs, same-revision scoped retry, report/local API visibility and automatic/
manual missing-key recovery. Clippy for the same packages/all targets with `-D warnings`, native
rustfmt check and `git diff --check` pass. Evidence: target/check-instance-isolation-all.log.
The previously failing DNS integration passes with a twenty-second observation window; local WSL
NSS timeout cancellation/reaping was independently observed to publish after twelve seconds, with
the production resolver deadline unchanged. No production device changes and no Git staging/commit.

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

- 2026-11-13 (D106): Diagnose .9 Access FRR push: isolated planner validation erased FRR context,
  leaving the old Access config running. Actual FRR export returns 61 routes. Preserve service
  context without duplicate planning and declare OpenVPN's FRR consumption; 110 tests and Clippy/
  formatting/diff checks pass. Source-only fix, no production mutations or Git staging/commit;
  release planner/reapply/reconnect still required. See detailed D106 entry above.

- 2026-11-13 (D105): Diagnose and correct .1's obsolete 404 update source, restart only updater and
  request check/sync; signature/platform/plan, running socket and remoteControl=true verified.
  Fix source defaults and activity-only startup visibility in shared runtime. 72 tests, Clippy,
  formatting/diff checks pass. No manual upgrades, business restarts, publication or Git staging.
  Authenticated Manager browser acceptance remains with the operator; see D105 above.

- 2026-11-13 (D104, user request): Implement native single-package /tmp download/install/delete,
  verified online rollback metadata, interrupted-transaction recovery and old handoff migration.
  Preserve updater-first order and download retries; add APK --no-cache, per-package installation
  locks/progress and separate temporary/memory/system-space preflight. 72 tests plus packaging trust,
  Clippy, shell syntax, formatting and diff checks pass. Real OpenWrt userspace/RAM acceptance pending;
  no device changes or Git staging/commit. See detailed D104 entry above and operations.

- 2026-11-13 (read-only OpenWrt updater audit): Confirm automatic opkg/ipk vs apk/apk selection and
  apk add --allow-untrusted after project signature/digest verification. Current implementation downloads
  all changed archives sequentially into persistent /etc/meduza-np-state/store/staging before installing
  packages one by one, then clears staging after completion (except pending repair). Native cache retains
  current/rollback archives and installation copies downloads there. It does not yet implement a single
  /tmp archive download/install/delete loop; preflight still budgets total download bytes * 4 + 16 MiB.
  Report gaps relative to user's low-storage/RAM requirement; /tmp normally consumes RAM, so moving all
  staging there would not solve peak use. Static source audit only; no runtime tests or device changes.

- 2026-11-13 (D103, user request): Implement optional connection-time FRR route push for Access;
  preserve authorization/static pushes, bounded complete snapshots and per-connection failure isolation.
  Make Access first in both consoles' settings/status without changing stored indexes, including disabled
  entries; fix Access runtime matching. Rust/Manager tests, Clippy, typechecks, production builds and actual
  Chrome flows pass (target/check-access-frr-*). Source only; release coordinated planner/frr/openvpn and
  console updates, apply and reconnect clients. See the detailed D103 entry above and operations.

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
- 2026-11-03 (per the user) Investigate the node at 10.42.10.9: updater is enabled/unpaused but still pinned to
  v0.1.1 and the bootstrap file:// import source. The online stable feed advertises v0.1.3; a verified upgrade plan
  using the online source has no blockers. The running default gateway/DNS are already 10.42.10.1/119.29.29.29.
  Separately, add updater DHCP macvlan deployment support while keeping planner on its static network, and restore
  both explicit gateway/DNS after lease renewals without changing lease addresses. Add an optional DHCP Quadlet
  network, interval compatibility and idempotent route checks. All 19 network/deployment tests passed; real remote
  network/mount namespace validation retained a changed lease IP while restoring the gateway and mounted resolver.
  The user requested waiting for v0.1.4 and supervising that upgrade. Release run 41491203924 failed at archive URL
  validation after publishing the GitHub Release; OCI verification passed but Pages was not updated (stable remains
  v0.1.3). GitHub CLI keyring credentials are invalid and anonymous job-log downloads are denied; requested the
  final error lines or restored authentication. Deployment config/status backups are in the remote private directory
  /var/standalone/meduza/backups/pre-v0.1.4-20261102T180035Z. No upgrade applied; containers remain on v0.1.1.
  Baseline planner configuration is already apply-failed because the requested MosDNS binary package has not
  downloaded successfully. DHCP changes are source-only; the remote static/shared network deployment is unchanged.
- 2026-11-03 (per the user) v0.1.4 publish URL-check failure: the reported Dashboard 0.1.3 archive is present in
  the v0.1.4 Release at the expected 864001-byte size; reusing that component version/name is correct. The workflow
  used one silent HEAD request and incorrectly labelled every transport/HTTP failure as a missing file. Add bounded
  retries for all curl failures, connection/transfer/retry time limits and visible underlying errors for archive and
  SBOM URL checks. Both checks still fail publication after persistent errors. All 23 release Python tests passed,
  including real HTTP temporary-404/503/disconnect and permanent-missing cases for both workflow loops; actionlint
  and whitespace checks passed. Inline workflow logic also works for workflow_dispatch of an existing immutable
  source tag after the default-branch workflow is updated. Source fix only; remote upgrade remains pending release.
- 2026-11-03 (per the user) Supervised v0.1.5 release (run 41564561202) and upgraded 10.42.10.9 successfully.
  Verified signed stable sequence 3, the pinned image/platform digests and read-only store compatibility. Base
  runtime is 0.1.5; the installed package group is v0.1.5 with 14 completed updates and successful updater takeover.
  Unchanged node components correctly reuse 0.1.3; Mihomo/MosDNS provider packages are 0.1.5. Restored the live
  stable source with automatic package updates and no exact group pin. Both containers are healthy; Dashboard
  HTTPS at 10.42.10.12:8443 returns 200. Base-container now reports installed/registered version 0.1.5; MosDNS
  resource resolves its packaged v5.3.4 binary.
  Split the former shared/pod namespace: planner retains 10.42.10.12 and its MAC, updater uses a stable separate
  MAC and DHCP (currently 10.42.10.209). Enabled the Netavark DHCP proxy/network; retained gateway 10.42.10.1 and
  DNS 119.29.29.29 in both roles. Added the documented FRR/Avahi capabilities and placed forwarding sysctls in
  planner. A real DHCP preflight received the side-router gateway and confirmed the override; a live gateway/DNS
  overwrite test recovered both settings within seven seconds without changing the DHCP address.
  A stalled transfer required stopping updater and prefetching its remaining archives with bounded transport
  waits. All 14 files were verified against signed-plan sizes/SHA-256; updater independently reverified the cache
  and completed activation. No published artifacts changed. Private full pre-upgrade node/store snapshot and
  deployment backups: /var/standalone/meduza/backups/pre-v0.1.5-20261103T062223Z. Updated remote operations notes.
  Remaining pre-existing configuration blocker: Manager supplies services.dnsmasq.listenAddresses=[]; planner
  still reports apply-failed and the network services are not yet running. Asked the user to set the intended
  frontend DNS listener in Manager (10.42.10.12 for this LAN) and publish it. Did not modify authoritative cached
  configuration or advertise service health merely from container health.
- 2026-11-03 (per the user) Separate external connection health from configuration application (D60). Remote
  revision 8 now passes the DNS listener check, but openvpn/lemon exited 1 five times and caused the whole apply
  to roll back. Add an explicit, default-compatible process startup policy: reconnecting clients commit after
  local delivery/checks and successful exec, then retry all runtime exits indefinitely with jittered exponential
  waits capped at 60 seconds. OpenVPN outgoing Site2Site clients opt in and retry DNS indefinitely; local Access
  and Site2Site listeners retain stable startup checks. Add a required offline native options parse before
  activation; it does not test peer reachability. Local spawn/check/file/provider/network failures still fail
  apply and preserve last-good state. WireGuard interface configuration remains independent of handshake health.
  Verified 35 planner unit tests, 6 executable-backed engine integration tests, 27 protocol unit tests, 4 package
  manifest tests, 2 provider recipe tests, the schema freshness test, 8 OpenVPN tests and 8 WireGuard tests (91
  total); strict workspace/all-target Clippy passes. Coverage includes six consecutive alternating success/error
  exits followed by automatic recovery, successful/repeated apply during backoff, preserving unrelated services,
  and local OpenVPN-style/WireGuard resource failure rejection. Isolated the engine tests' process-wide native
  tool log environment variable to prevent parallel test interference. Generated generation-plan schema and
  documented operation/recovery behavior. Remote OpenVPN 2.7.7 accepted the staged lemon config in offline options
  mode and rejected an injected unknown option. Source fix only; remote v0.1.5 has not been replaced or patched.
- 2026-11-03 (per the user) Make Manager download/upgrade progress visible after completion. Confirmed the node
  and the Manager raw report both contain the completed v0.1.5 update (14/14, 14514061 bytes); the live web bundle
  already has the progress component. Its inactive view hid byte/count progress, and node summaries hid successful
  upgrades. Move a clearly labelled Packages and updates card above the node tabs, retain completed percentages,
  sizes/counts/time, accept both summary and raw-report progress, and show explicit missing-status diagnostics.
  Add dedicated update columns to node list/overview. Shared presentation also improves Dashboard; download 100%
  remains distinct from activation/verification completion. Extend runtime snapshots with installed/available group
  versions, activation/check times and last result; report rolled-back separately from generic failure. Existing
  v0.1.5 reports already support the main UI fix; extra metadata comes from a future node update.
  Verified actual authenticated report ingestion through detail/list/overview APIs (3 tests), 2 real-file runtime
  snapshot tests, strict runtime/planner/updater Clippy, UI-kit/Manager type checks and production Manager/Dashboard
  builds. New tools/test-manager-update-progress.mjs checks the complete pages in Chrome: the supplied completed
  report, downloading, automatic refresh to completion, retry details, verification, errors, rollback, optional
  metadata and absent status. Screenshot: target/manager-update-progress.png. Source fix only; the production
  Manager has not been redeployed.
- 2026-11-03 (per the user) Supervised the v0.1.6 rollout on 10.42.10.9. Verified stable sequence 4,
  source ab623052378afe8d612541a2b5422b8e49c52089, both signatures and the manifest SHA-256. Original curl stalled
  at 250610 bytes; stopped updater before switching, made a consistent store backup, and prefetched the 13 signed
  artifacts with bounded HTTP/1.1 transport. Updater independently verified and activated them; installed generation
  6 at 11:30:16Z, self-update takeover completed at 11:30:22Z, journal clear, 13/13 and 7144914 bytes complete.
  Automatic updates remain enabled/unpaused. Both containers healthy; Dashboard HTTPS 200; effective default
  gateway/DNS remain 10.42.10.1/119.29.29.29, updater DHCP address 10.42.10.209. Base image stays 0.1.5 (software
  groups update independently). Manager serves the tested new index-4MA-S11o.js; node reports the enriched status.
  Backup: /var/standalone/meduza/backups/pre-v0.1.6-20261103T112049Z, including store-v0.1.5.tgz. Remote audit:
  upgrade-v0.1.6.completed and the paired planner/updater status JSON.
  Functional acceptance found an omission in D60: lemon is a Site2Site server, which v0.1.6 still treats as a
  strict local daemon, so revision 8 still rolls back to degraded revision 3. Isolated native startup proved the
  actual exit reason is OpenVPN 2.7 rejecting the configured static-key mode without its compatibility opt-in.
  Source fix now covers both Site2Site roles (Access remains strict), emits the scoped legacy compatibility
  options and the proper static cipher directive, and adds a bounded static crypto preflight against the staged
  key instead of the previous current generation. Nine OpenVPN tests and strict Clippy pass. Real OpenVPN 2.7.7
  crypto preflight and a localhost-only/null-device listener pass; an invalid cipher is rejected. The native
  self-test does not fully validate key bytes on every provider, documented accordingly. These additional fixes
  are source-only and require a subsequent release; v0.1.6 upgrade itself is complete. Authoritative Manager
  configuration and published artifacts were not modified.
- 2026-11-03 (per the user) Fix default-open dnsmasq listeners and expose Mihomo binary preparation progress.
  Omitted/empty listenAddresses now use dnsmasq's native all-interface behavior; effective IPv4/IPv6 wildcard
  addresses still participate in listener ownership, loop prevention and probes. Manager DNS compilation uses
  the same defaults. Updated config schema/UI help and the specification; explicit address lists remain scoped.
  Remote diagnosis found revision 9 rejected by the old empty-list validation and the rollback stopping
  mihomo-pkg/prepare before its binary became ready. Provider download telemetry is now independent of its usable
  binary record: measured archive bytes, target/platform, attempt, validation/activation stage, retry time/error
  and update time. Exclusive kernel locks protect the transfer and preparer lifetime, detect termination even
  during retry backoff, prevent competing downloads from truncating an active file and preserve manual mode's
  no-network behavior. Download-now restarts the applied preparer through bounded planner IPC instead of doing
  a long transfer inside the action timeout. Failed validation preserves the previous binary/provider; successful
  activation is not invalidated by a final telemetry write failure. Curl has connect and low-speed deadlines.
  Manager and Dashboard show a separate binary-progress card; 100% downloaded does not imply binary readiness.
  Verified focused Rust tests (dnsmasq, Mihomo provider, shared fetch, updater retry and protocol schema/manifests),
  strict Clippy, 22 Manager API/compiler/localization/provider tests, UI-kit type checking and both production UI
  builds. Full-page Chrome tests cover transfer, retry, interruption, verification, completion and missing
  telemetry; preview target/manager-binary-progress.png. A temporary native dnsmasq in the remote planner network
  namespace answered the same test record over both 127.0.0.1 and 10.42.10.12 with no explicit listen address.
  Test daemon was stopped; production configuration and released artifacts were not changed. These fixes are
  source-only and require new node components plus the Manager UI to be released/deployed.
- 2026-11-03 (per the user) Implement updater-first mutual upgrades (D62). Read-only inspection of
  10.42.10.9 confirmed installed group v0.1.7/generation 8, with planner and updater 0.1.6; the public v0.1.7
  manifest explicitly reuses both from v0.1.6, and /proc confirmed planner runs the corresponding store object.
  The old implementation really did activate planner before updater, independently of that expected reuse.
  New updater plans now stage the complete verified target, journal a separate updater-only generation and ask
  planner's authorized updater.activate method to invoke a bounded activation helper. The independent owner
  starts the new updater after exit 75; it immediately resumes the saved transaction without another channel
  check/download and only then upgrades planner and remaining packages. The old group stays installed until
  completion. Failed takeover leaves planner untouched; subsequent planner/package rollback retains the new
  updater. Store, opkg and apk use the same boundary; native rollback still requires cached previous archives.
  Helper execution is constrained to the staged updater object (or the native updater), accepts no arbitrary
  request paths/commands, and kills its process group on timeout. Old journals and previous planner protocols
  remain supported; planner-down bootstrap/repair does not depend on a working planner.
  Verified all 33 updater tests, including real executable helper activation, simultaneous/one-sided upgrades,
  offline continuation, failures before activation/installed-record persistence, failed takeover, planner rollback,
  previous planner compatibility, bootstrap and native opkg/apk command ordering. Planner helper execution/path
  rejection test and strict planner/updater/runtime/protocol Clippy pass. Full planner (36 unit + 6 integration),
  protocol (34) and runtime (18) regressions also passed during this change. Final updater tests ran serially:
  WSL1 parallel runs intermittently rejected directory renames, including an unchanged store test, with EACCES;
  the same renames succeeded afterward. Logs: target/check-mutual-upgrade{,-final}.log. Updated specification,
  decisions and operations. Source-only: production artifacts/configuration were not replaced. The first
  transition initiated by an older updater still follows that older code until the new updater is installed.
- 2026-11-03 (per the user) Add Manager-triggered node upgrades (D63). The node detail's Packages and
  updates card now offers Check for updates and Upgrade now to operator/admin, with read-only status for viewer.
  The existing authenticated/CSRF-protected/audited job channel delivers update-check/update-apply to planner,
  which forwards only fixed updater socket methods and reports scheduling acceptance or the actual failure.
  Planner advertises update.remoteControl; previous nodes show disabled controls with an upgrade explanation.
  Pending requests deduplicate, conflicting requests reject, delivery expires after ten minutes, and bounded
  planner receipts survive replacement/HTTP-acknowledgement replay. Request acceptance is distinct from package
  completion; queued/delivered/accepted/failed/expired states sit alongside the existing real upgrade progress.
  Fixed an existing updater issue: explicit checks now remain read-only even with autoApply enabled; explicit
  apply bypasses pause/window once and can retry failed groups while preserving configured automatic policy.
  A check cannot overwrite an already queued apply. Requests accept no package/instance/source/command overrides.
  Verified 37 Rust tests (35 updater and 2 planner helper/relay tests), strict planner/updater Clippy, 27 Manager
  API/sync/Access/translation tests, Manager/UI-kit type checks, Manager and Dashboard production builds.
  Full-page Chrome tests click both controls, check request payloads, enforce viewer/unsupported/pending/active
  restrictions, show errors and distinguish accepted requests from completion through updater takeover. Preview:
  target/manager-update-progress.png. Updated specification/operations/decisions. Source-only: release/deploy
  Manager, planner and updater to enable these controls; no production node or Manager artifacts were replaced.
- 2026-11-03 (per the user) Diagnose the repeated dnsmasq empty-list error after explicit wildcard publish.
  Authenticated read-only inspection of 10.42.10.9 / xuzhou-b1 confirms Manager's desired rev-000010 has
  listenAddresses ["0.0.0.0"], port 53. A temporary native dnsmasq on isolated test port 55353 answered the
  same record on loopback and 10.42.10.12; it was stopped and its test PID file removed. The last-good
  rev-000003 still contains [] and the installed dnsmasq is reused 0.1.6 within group v0.1.7. Logs show
  rev-000010 first fails OpenVPN lemon preflight: configured lzo-asym is rejected by the compression-disabled
  provider. Last-good recovery then rejects the old DNS list and overwrites the original apply error.
  Source fix adds optional sync.recoveryError with the historical revision, preserving sync.lastError for
  the desired apply failure and clearing recovery diagnostics on a fresh successful restore/application.
  Manager and Dashboard display both separately; node overview exposes a recovery problem. Regenerated
  schemas/node-report.json. Executable-backed last-good recovery regression, 27 protocol unit tests, schema
  test, strict planner/protocol/Dashboard Clippy, 6 Manager API/i18n tests, full-page Chrome assertions,
  Manager type check and both production UI builds pass. Native credentials remained on the server; only
  non-secret configuration fields were printed. No production configuration/binary/snapshot was modified;
  OpenVPN compression compatibility remains unresolved, and the source diagnostic fix needs a release.
- 2026-11-04 (per the user) Add compression support to openvpn-pkg (D65). The provider now statically
  includes pinned LZO 2.10 and LZ4 1.10.0, ships their licenses and records both capabilities in build-info.
  Target builds verify static linkage, run real library compression/decompression round trips, and exercise
  OpenVPN crypto/options preflight for none, lzo-no, lzo-asym, lz4, lz4-v2, stub, stub-v2 and migrate.
  The pinned OpenVPN 2.7 retains incoming compression compatibility and sends uncompressed outgoing traffic;
  existing per-connection policy is preserved. No runtime compression libraries or base image changes are needed.
  Native linux/amd64 Podman build on 10.42.10.9 passed with two CPUs/1 GiB limits, all eight preflights,
  management binding, no ELF interpreter/DT_NEEDED entries, and the complete exported metadata/license tree.
  Validation artifacts: /var/standalone/meduza/validation/openvpn-compression-20261104/{build-final.log,out}.
  Provider recipe/manifests checks (6), OpenVPN tests (9), strict OpenVPN Clippy and shell syntax checks pass.
  Other architectures remain subject to the existing CI target matrix. Source-only: no production provider,
  configuration or service was replaced; release and upgrade openvpn-pkg to resolve the deployed lzo-asym error.
- 2026-11-04 (per the user) Add per-package disk/RSS reporting to Manager and Dashboard (D66).
  Optional resources.packages reports allocated package-file bytes, separate persistent service/provider data,
  RSS, sample times and per-instance/process details. Store measurements include retained versions; native
  opkg/APK inventories avoid scanning shared system directories. Walks deduplicate hard links and overlapping
  roots, do not follow symlinks, are bounded and cache for 60 seconds. OpenVPN and FRR process/child memory
  belongs to its service instance without duplicating it in the binary provider. Independent updater and
  Dashboard daemons publish namespace-local ten-second samples; planner rejects stale/wrong-boot samples.
  Unknown values, idle process RSS and shared base-image layers remain distinct in the UI. Regenerated the
  node-report schema and documented kernel/shared-layer accounting limits. Verified seven new Rust regressions
  (including two live supervised external instances), schema consistency, strict runtime/protocol/planner/
  updater/Dashboard Clippy, six Manager API/i18n tests with authenticated resource-report ingestion, all affected
  frontend type checks and both production UI builds. Full Manager and Dashboard pages pass desktop/mobile
  Chrome checks for instance totals, provider disk, independent updater PID namespaces, zero/unknown/legacy
  states and expandable details. Screenshot: target/package-resources.png. Source-only: release updated planner,
  updater, Dashboard and Manager artifacts; production node binaries/configuration were not replaced.
  Final protocol/runtime/planner regression passed all 102 tests (including six executable-backed engine cases),
  followed by strict Clippy for all five affected Rust packages. Log: target/check-package-resources-full.log.
- 2026-11-04 (per the user) Keep WireGuard endpoint DNS errors out of configuration application (D67).
  Required local syncconf now applies all keys/peers/AllowedIPs and numeric endpoints without resolving remote
  hostnames. Instances with hostname peers own a supervised resolver/updater with five-second DNS deadlines,
  three-second native-command deadlines and independent exponential retries capped at 60 seconds with jitter.
  It retains previous endpoints on failure, refreshes successful DNS, preserves learned endpoints when DNS is
  unchanged, repairs empty endpoints after link recreation, and replaces the helper on binary/spec changes.
  Per-peer status carries the bounded error, retry phase/attempt/time and desired/resolved endpoints; stale or
  superseded observations are ignored. Manager shows WireGuard/OpenVPN reasons directly on their connection
  rows; both packages' status descriptors expose a translated Connection error column for Dashboard/plugin
  views. OpenVPN retains its existing native reconnect and connection-process retry policy. Typed config,
  distribution, provider and local interface/reload failures still reject apply; no wg error-string bypass
  or suppression of required checks was added to planner. Verified 71 Rust tests, including seven executable
  engine cases and the real resolver-process scenario where invalid DNS commits alongside another service but
  a forced local wg rejection still fails. Strict planner/SDK/WireGuard/OpenVPN Clippy, frontend type checks,
  both production UI builds, six Manager API/i18n tests, final descriptor checks and full-page Chrome assertions
  pass. Screenshot: target/connection-errors.png; logs: target/check-wireguard-{final,engine,endpoints}.log.
  Source-only: release the changed WireGuard/OpenVPN package descriptors and Manager; no production node
  binary, configuration or service was replaced.
- 2026-11-04 (per the user) Automatically reclaim obsolete downloaded versions on space-constrained nodes (D68).
  Updater cleans before checks/space planning, after terminal attempts and during startup recovery, including
  no-change and failed-download runs. Keep current/one rollback generation plus running/adoption/view references
  and pending handoff/repair files. A persistent rollback checkpoint survives failed attempts replacing the
  last journal. Kept view targets remain protected if a full disk leaves installed.json behind; explicit native
  repair state retains archives even when its package database still reports old versions. Native opkg/APK
  caches prune obsolete owned archives while preserving unknown files. Cleanup and updater work share a process
  operation lock. Mihomo uses its transfer lock to clean obsolete/partial automatic archives and candidates,
  including old assets absent from the current manifest, after failure and periodically while ready. Capture
  final progress before deletion; preserve current/previous binaries, manual mode and unknown files. Internal
  directory symlinks are not traversed. Verified all 52 updater/mihomo-pkg tests and strict Clippy, including
  real truncated HTTP cleanup, failed attempts retaining rollback, no-change/startup collection, pending updater
  handoff survival, native repair, actual view references, adoption references, concurrent transfer protection
  and symlink/manual-file preservation. Log: target/check-cache-cleanup-final.log. Source-only: production
  volumes were not cleaned; updated updater and mihomo-pkg artifacts must be released. Git submissions remain
  with the user as requested; no index/commit/push changes were made for this task.
- 2026-11-04 (per the user) Fix updater activation rollback with a read-only planner store (D69).
  Confirmed the live helper could not create store/current.new because planner's deliberate store mount is
  read-only. Preserve that isolation and retry failed activation within the writable updater owner, validating
  the same updater-only journal and generation. Lost acknowledgements are idempotent; malformed handoffs still
  roll back. Planner captures at most 4 KiB of helper stderr so failures retain their actual filesystem/install
  reason; all waits remain bounded. All 44 updater/targeted planner tests and strict Clippy pass, including
  read-only helper failure, already-switched recovery, corrupted journal rejection and diagnostic propagation.
  Log: target/check-updater-owner-final.log. Recover the existing 10.42.10.9 node through its signed source:
  back up installation/control metadata, pause and stop only the idle old updater worker, activate v0.2.1 from
  its writable owner, then restart the independent updater container to finish the staged planner upgrade.
  Verified installed group v0.2.1/generation 11, both running executables at 0.2.1, terminal journal done/no
  error, healthy containers and restored automatic policy. Planner store remains read-only; gateway remains
  10.42.10.1 and DNS 119.29.29.29. Private repair backup is under /var/standalone/meduza/backups/.
  The source fix is not yet published or installed on this node: its next transition to a fixed updater may
  need the same owner-side takeover. Git index/commit/push and release remain with the user.
- 2026-11-05 (per the user) Complete the signed owner-side v0.2.2 transition on 10.42.10.9.
  GitHub connection/low-speed timeouts had exhausted retries. With metadata backups and restored automatic
  policy, both running planner/updater executables reached 0.2.2; normal selection installed the remaining 11
  service/provider packages and the final transaction completed at generation 14. Planner store stayed read-only.
  Existing rev-000011 recovery is separately degraded because Mihomo DNS is enabled without nameservers;
  manager requests also observed HTTP 429. No remote DNS policy was changed.
- 2026-11-05 (per the user) Publish non-OpenWrt package archives on Pages with resumable 500,000-byte parts (D70).
  Include reused packages, keep full-file compatibility, bind every ordered part to the signed group manifest,
  preserve and deduplicate historical Pages downloads, and retain verified partial downloads across failures.
  OpenWrt and legacy metadata continue full downloads. Remove the unpublished mirror implementation/settings.
  Source changes only; Git submission and publishing remain with the user.
  Verified 91 Rust tests (protocol schema, release CLI/package publication, runtime progress and updater flows),
  26 Python release/Pages tests and strict updater/runtime/release Clippy. Real HTTP failure resumes only the
  missing second part; failed transactions preserve earlier completed packages through housekeeping/checks.
  Tests cover corrupted cache repair, final whole-file digest, signed malformed layouts, successful cleanup,
  redirected paths, historical Pages preservation/content deduplication and the site capacity guard.
  Generated schemas/group-manifest.json. Log: target/check-pages-chunks-final.log.
- 2026-11-05 (per the user) Compare actual v1 Mihomo startup and fix DNS compatibility regression (D71).
  Restore subscription/core resolver inheritance for nodes with no custom nameservers, preserve explicit
  custom DNS policies and project-owned listeners, and stop emitting an empty bootstrap list. Validate the
  effective inherited DNS before caching/reloading, including wildcard front-end/local-address loop checks.
  Worker status now exposes downloading/results immediately; probe rows report blocked configuration and
  subscription reasons through existing Manager/Dashboard fields. Updated the schema, descriptor translations
  and Manager validation. Live subscription download succeeded and already provides its DNS resolvers;
  installed core v1.19.33 passed two temporary -t fixtures without changing the running configuration.
  Source-only: release updated Mihomo and Manager packages; existing node binary/revision was not replaced.
  Verified 19 Rust tests (normalization, real HTTP worker/last-good refresh, probe errors, schemas/descriptors),
  strict Mihomo Clippy and Manager's DNS-loop publishing test/type compilation. Log: target/check-mihomo-v1-final.log.
  Read the live subscription only in memory and printed sanitized DNS facts; no URL/token or proxy credentials
  were exposed or persisted. Native -t fixture directories were removed; running services were not replaced.
- 2026-11-05 (per the user) Fix CI's executable-bit false positive for Rust inner attributes.
  tools/fix-exec-bits.sh now requires a shebang interpreter path after optional spaces/tabs instead of treating
  every leading #! as a script. Rust #![cfg(unix)] remains ordinary 100644 source. Three regressions run the real
  helper in temporary Git repositories and verify both check/fix behavior, script paths with spaces, spaced/tab
  shebangs and unchanged Rust modes. All pass; the actual project --check and git diff --check pass as well.
  Log: target/check-exec-bits.log. The project's Git index was not modified; submission remains with the user.
- 2026-11-06 (per the user) Make GitHub Release publishing recover from HTTP 500 and ambiguous replies (D72).
  Add a bounded gh-based publisher for draft creation, file upload and finalization. Distinguish confirmed
  missing releases from transient query errors; re-read remote state after uncertain writes. Paginate assets,
  verify size/hash before reuse (including legacy no-digest downloads), and refuse mismatched/incomplete files
  without overwriting/deleting them. Six attempts, command deadlines, exponential jitter and rate-limit delays
  cover transient service/network faults; permission/validation errors stop. Workflow now uses the helper after
  SBOM inputs are prepared. All 47 release tests and executable-bit checks pass, including 18 publisher fault
  tests covering the reported v0.2.4 failure, lost responses, visibility delays, pagination and immutable reuse.
  Log: target/check-release-publish-final.log. Source-only; no remote release/run or Git index was modified.
- 2026-11-08 (per the user) Fix MosDNS rule-format regressions against v1 and its exported JSON (D73).
  Downloaded the four public sources: geoip-cn's CIDRs and the three geosite sources' regexp entries were
  incorrectly rejected by v2's simple-domain validator. Add bounded regexp syntax validation and homogeneous
  IPv4/IPv6/CIDR support for custom pipelines; generated domain sets remain domain-only. Keep imported config
  unchanged and preserve old files on failed refresh. All four complete sources pass (5,568 / 845 / 4,796 / 131
  rules). Fourteen plugin/manifest tests and strict Clippy pass, including real local HTTP download preservation.
  Also passed the actual Manager import of the v1 export: 19 files, generated path rewrites, geoip resp_ip
  binding and custom worker selection. Log: target/check-mosdns-rules-final.log. Source-only; release
  meduza-np-mosdns to update deployed nodes.
- 2026-11-08 (per the user) Fix false DNS timeouts for MosDNS wildcard listeners (D74).
  Reproduced 0.0.0.0 and :: healthy UDP replies being discarded because the shared probe expected a wildcard
  reply source. Query matching loopback instead and retain strict response validation/original listener config.
  Added IPv4/IPv6 regressions and a MosDNS probe-to-status test. All 45 SDK/MosDNS/Mihomo/dnsmasq tests and
  strict SDK/MosDNS Clippy pass; log: target/check-dns-probe-after.log. Initial live SSH verification was refused
  by the 1Password agent. Source-only; deployed binaries/configuration were not replaced.
- 2026-11-08 (per the user) Fix Manager showing applied rev-000011 as a configuration failure (D75).
  The supplied report confirms in-sync; its /jobs HTTP 429 is a separate transport diagnostic. Successful
  ACKs and accepted in-sync reports now reconcile active failure metadata/publish results while retaining ACK
  history, newer failures, real same-revision reapply failures and recovery warnings. Guard late intermediate,
  older-revision and retired-boot ACKs. Existing stale rows heal on the next healthy report; no new revision,
  node restart or production database edit is needed after updating Manager. Source changes remain unsubmitted.
  Manager TypeScript compilation and all server tests pass (51 passed; 3 environment-dependent tests skipped).
  Log: target/check-manager-apply-status-full.log; focused status regressions: target/check-manager-apply-status-final.log.
- 2026-11-08 (per the user) Replace repetitive flat status rows with a shared explorer-style tree (D76).
  Manager/Dashboard group services, interfaces/instances, peers, processes and probes by stable identity and
  explicit process owner. WireGuard reports interface/peer names; Manager also fills names absent from older
  reports using published config and shows them in connection rows. Preserve independent process/protocol
  health, original row restart targets and permissions. Collapsed branches retain problem counts and report
  reloads retain expansion choices. Three hierarchy regressions plus i18n checks pass; actual Manager/Dashboard
  Chrome checks verify both repeated helpers, names, expand/collapse, hidden errors, refresh, restart payload
  and viewer permissions. All 14 WireGuard tests, strict Clippy, type checks and production builds pass.
  Logs: target/check-status-tree-{rust,browser,build}.log; screenshots: target/status-tree-{manager,dashboard}.png.
  Source-only; release Manager, Dashboard and WireGuard for deployment. No Git submission or production changes.
- 2026-11-08 (per the user) Fix topology IP/label flicker and viewport overflow (D77).
  Keep deterministic per-vertex label placement through hover, selection and depth ties; crowded labels use
  additional slots/leader lines. Compare link identities rather than heartbeat array order so status refreshes
  do not reheat settled layouts. Bound the topology route to the actual viewport and give the graph remaining
  flex space; selection details stay in a bounded pane. Resize redraws immediately, refits the framed graph or
  proportionally preserves a manual camera, and ignores unchanged bitmap dimensions. Three regressions fail
  before and pass after; all 9 topology tests plus 3 i18n tests pass. Actual Manager Chrome verification passes
  six viewport transitions from 1500x900 down to 390x680 and back with no document overflow, visible framed
  vertices, matching canvas/DPR sizes, stable labels and bounded selected-router details. Shared/Manager/
  Dashboard type checks and Manager production build pass. Logs: target/check-topology-stability-{before,
  final,browser,types,build}.log; screenshot: target/topology-stability.png. Source-only; update Manager to deploy.
- 2026-11-09 (per the user) Prevent topology labels from covering rendered geometry (D78).
  Node/IP and AS labels now join the depth-sorted scene, with nodes/links winning equal-depth ties. Split
  depth-spanning links for foreground/background ordering while retaining dashes and traffic particles.
  Separate text from low-opacity panel painting (18% node / 16% AS maximum); consider link intersections,
  strongly penalize node/label overlap and add diagonal placement candidates. Keep hover/report placement
  stable and attach labels to their object's projection rather than the viewport edge. Two regressions fail
  before the fix; all 12 graph tests plus 3 i18n tests pass, including split-depth links and edge movement.
  Actual Manager Chrome checks pass for translucency, stable labels and six viewport transitions; front and
  rotated previews inspected. Type checks and production build pass. Logs: target/check-topology-occlusion-
  {before,final,browser,types,build}.log; previews: target/topology-occlusion.png and topology-occlusion-orbit.png.
  Source-only; updating Manager deploys the change. No Git submission or production modifications.
- 2026-11-09 (per the user) Give topology a true three-dimensional force layout (D79).
  Replace planar AS anchors/collision and member flattening with deterministic spherical placement and
  X/Y/Z repulsion, springs and collision. AS bodies separate in space as a whole; group members retain
  depth after settling. Preserve existing camera, spacing control, translucent depth-sorted labels and
  stable report refresh. New regressions fail before and pass after; all 14 graph tests plus 3 i18n tests
  pass, including rotation-independent volume, Z-only collision and deterministic coincident separation.
  Actual Manager Chrome checks pass for group/member depth, six viewport transitions, stable labels and
  bounded details. Front and rotated previews inspected; type checks and production build pass.
  Logs: target/check-topology-volume-{before,after,final,browser,types,build}.log; previews:
  target/topology-volume.png and topology-volume-orbit.png. Source-only; update Manager to deploy.
  No Git submission or production modifications.
- 2026-11-10 (per the user) Fix OpenWrt FRR startup directory/identity compatibility (D80).
  roaming-cat reports missing /var/lib/frr/meduza and denied PID/socket writes; its supplied FRR 10.4.1
  build uses root:root while the existing namespace is frr:frr. Inspect the actual provider's configure
  options with bounded --version and use its compiled identity for runtime/state/config ownership.
  Reject failed/unknown identity inspection instead of defaulting to root. Prepare both namespaces and
  missing parents with explicit modes, require ownership setup, preserve existing shared/sibling paths
  and secret modes. Capture supervisor stdout as well as stderr in the existing bounded log forwarder.
  All 18 FRR tests, 10 supervisor tests and strict FRR/planner Clippy pass, including root/custom/default
  builds, failed/missing providers, umask/reapply behavior and shared-directory/secret preservation.
  Logs: target/check-frr-pathspace-before.log, check-frr-output-before.log, check-frr-openwrt-tests.log,
  check-frr-output-after.log and check-frr-openwrt-clippy.log. Source-only; update meduza-np-frr then
  reapply; update planner for full log capture. No live device verification or Git submission.
- 2026-11-10 (per the user) Confirm official OpenWrt FRR uses network:network, not frr:frr (D80).
  Checked the official packages feed's master and openwrt-24.10 configure flags. Existing compiled-identity
  detection supports the official account as well as the reported custom root build; added network:network
  to the ownership-plan regression and clarified the specification. All 18 FRR tests and strict FRR/planner
  Clippy pass. Source-only; no additional runtime behavior change or Git submission.
- 2026-11-10 (per the user) Automatically merge learned BGP Router IDs in topology (D81).
  FRR already reports the remote ID. Manager now resolves same-AS identities to unique managed nodes,
  merges unmanaged multi-address/multi-observer peers and fills address-only observations only when
  unambiguous. Preserve all session status/routes/AS paths and managed labels/navigation. Reject zero,
  invalid or cross-AS identities; do not collapse distinct registrations with duplicate IDs. Retained IDs
  keep reconnecting peers merged. Peer details/tooltips list every address, and obsolete selection clears.
  Five regressions fail before; all seven new identity cases and the focused graph/sync/i18n suite pass
  (42 total). Type checks and production build pass. Actual Manager Chrome verifies automatic poll-time
  merging from server-generated fixtures, both session rows/routes, managed Router ID, stale selection
  and mobile/desktop bounds. Logs: target/check-topology-routerid-{before,final,types,build,browser}.log;
  preview: target/topology-routerid.png. Source-only; update Manager, no node package/config revision
  changes needed. No Git submission or production modifications.
- 2026-11-10 (per the user) Global AS address settings and peer ASN selection (D82).
  Added staged/published name–ASN mappings with duplicate/range/name validation and viewer permissions.
  Actual WireGuard nested peer and OpenVPN instance dialogs offer global choices alongside manual ASN
  input. Topology resolves published names for 3D AS clusters, collapsed/path vertices and details.
  Numeric peer configuration stays intact; renaming/removing names never causes a node network reapply.
  Manager API/topology/i18n: 14 tests; shared renderer: 4 tests. Manager/renderer/dashboard type checks,
  Manager production build and actual Chrome feature checks pass. Evidence: target/check-as-addresses-*
  logs and target/as-addresses.png. Source-only; no Git submission or production modifications.
- 2026-11-11 (per the user) Correct OpenWrt console feature and sync status (D83).
  Planner now enables registered CLI/UI entry-point capabilities without requiring services.* configuration
  or a supervised process. Preserve dependency, platform and registration failures, and all network service
  switches. LuCI displays in-sync green while unknown/unsynced remain neutral. Seven registry tests and two
  shipped-LuCI-module tests pass; LuCI regressions added to web CI. Update planner and dashboard-openwrt
  together. Evidence: target/check-console-features-tests.log and target/check-openwrt-status-tests.log.
  Source-only; no Git submission or production modifications.


- 2026-11-11 (per the user) Support Mihomo's native web panel (D84).
  Inherit subscription UI requests by default; add explicit enable/disable and archive URL controls in
  the shared Manager/Dashboard descriptor. Keep controller binding and per-node credentials owned.
  Let the core download into persistent URL-derived UI directories, restart it when the static route
  changes, and prune obsolete owned caches only after the replacement is ready. Failed downloads keep
  the proxy/API running and retain the previous cache. Actual Mihomo v1.19.33/worker checks pass for
  download, /ui/, auth, offline restart, refresh switching, cleanup and failed-download isolation.
  Plugin, protocol/manifest, translation, strict Clippy and browser form checks pass. Evidence:
  target/check-mihomo-ui-{tests,protocol,clippy,real,browser,i18n}.log. Source-only; no Git submission
  or production modifications.

- 2026-11-12 (per the user) Native Mihomo panel launch buttons in Dashboard and LuCI (D85).
  Gate entries on enabled native-panel status, including the ordinary dashboard's anonymous login page.
  Per the user's clarification, anonymous Dashboard clicks directly hand off endpoint/secret without login.
  A dedicated explicit POST and LuCI write ACL govern on-demand credentials; other management APIs keep
  authentication and public polling exposes only availability/address. Use
  native MetaCubeXD setup fragments, node hostname substitution, IPv6 and explicit loopback handling.
  The worker owns 0600 ephemeral credentials, deletes them when unavailable/stopped and the planner
  rejects expired/malformed state. Browser, URL/ACL, real HTTP/UDS, Rust, strict Clippy and production
  build checks are recorded in target/check-panel-launch-*.log. Source-only; no production changes or
  Git submission. Update planner/cli/mihomo and the selected dashboard together.

- 2026-11-12 (per the user) Sortable AS addresses, two-line topology labels and theme startup (D86).
  Add native drag handles and up/down controls to the AS catalog; preserve the existing Save/Apply flow,
  ASN/name pairs, validation and viewer restrictions. Split AS headings/names in clusters, collapsed nodes
  and paths; reserve both lines and revalidate cached router labels when they overlap AS text. Restore
  Manager/Dashboard theme before first paint using a shared same-origin CSP-compatible script.
  Graph/theme regressions, typechecks, both production builds and translations pass. Actual Chrome checks
  verify native drag, draft/published persistence, permissions, full theme reloads and the prior light-flash
  reproduction; actual topology and WireGuard/OpenVPN ASN forms pass. Evidence:
  target/check-settings-ui-browser.log, target/check-as-split-browser.log, target/check-as-labels-after.log.
  Source-only; no Git submission or production changes.

- 2026-11-12 (per the user) Node 24 for GitHub Actions themselves (D87).
  Upgrade all external Actions in CI/release/cache workflows and local release composites to versions
  declaring node24, including the Pages uploader's indirect dependency. Keep build Node at 24 and
  existing cache behavior by disabling setup-node's new automatic cache where no cache was requested.
  A read-only audit validates six YAML files, twelve upstream manifests and 86 action calls, including
  input compatibility and transitive runtimes; the existing actionlint 1.7.7 gate passes.
  Evidence: target/check-node24-actions.log.
  Source-only; no workflow execution, Git submission or production changes.

- 2026-11-12 (per the user's publish failure) Stable artifact staging after the Node 24 Action update (D88).
  Fix download-artifact's count-dependent wildcard extraction with explicit merged staging roots for
  Rust image inputs, published archives and runtime OCI metadata. Align updater lookup and image SBOM
  OS-data paths; retain existing integrity checks and Node 24 runtimes. Six Linux regression tests cover
  manager-only, base-only and multi-image builds, one/multiple bundles, actual find/cp consumers and the
  previous failure. All 53 release pipeline tests and actionlint pass.
  Evidence: target/check-release-artifact-layout.log, target/check-release-tests.log.
  Source-only; no Git submission or remote workflow/production changes.

- 2026-11-12 (per the user) Repair post-startup theme divergence, complete Dashboard i18n and consolidate
  upgrade/download details (D89). Confirm the published bootstrap is present; use the user's saved=dark /
  applied=light evidence to reconcile persisted preference with document/selectors after attribute changes,
  page restoration and cross-tab updates. Add English/Simplified/Traditional Dashboard messages, persistent
  language selection and coverage checks. Move full package/binary progress and history to one updates tab;
  other pages retain a compact active-only notice that vanishes on completion/failure. Keep permissions,
  anonymous Mihomo panel access and independent binary status when updater is unavailable. Actual Chrome
  checks cover both production builds, compressed assets/CSP, theme repair/reload/cross-tab changes,
  polling notice lifecycle, details/errors, all Dashboard languages and Manager request/viewer behavior.
  Evidence: target/check-node-ui-browser.log, target/check-update-progress-regressions.log.
  Source-only; no Git submission or production modifications.

- 2026-11-12 (per the user's gateway migration investigation) Restore missing owned policy tables (D90).
  Use only the authorized privileged Incus client for gateway tests; do not change the local workstation
  gateway or provision test resources on the production host. Actual gateway traffic fails while the same
  Mihomo SOCKS path succeeds; ordinary DNS forwarding works. Production nft/rules exist but tables
  4626/4627 do not, despite journal ownership. Fix LinuxBackend's known-absent query result so the existing
  apply reconciliation recreates those routes after namespace loss. Preserve unknown failures and unrelated
  routes. Regression: 44 planner + 17 Mihomo + 11 dnsmasq tests pass; an isolated real-kernel probe on the
  authorized client restores both route tables and validates marked lookups. Audit v1 exclusions, ordinary
  forwarding and DNS policies; document explicit custom-pipeline fallback and UDP/QUIC privacy boundaries.
  The initial investigation left production unchanged; the user's later upgrade and recreation are
  validated in D91 below. No Git submission.

- 2026-11-12 (per the user's post-upgrade retest) Fix the second transparent-proxy deployment blocker (D91).
  Confirm owned tables 4626/4627 recovered, but real gateway HTTPS still times out for five sites in two
  rounds while SOCKS succeeds. Identify the retained Compose all.src_valid_mark=1 setting. Isolated real
  TCP interception proves effective mark-based source validation is the blocker; disabling rp_filter
  alone fails, all/ingress src_valid_mark=0 succeeds. Correct run.sh/Quadlet defaults, preserve host policy,
  add actual-handshake and deployment regressions, and document planner-only Compose recreation.
  20 bootstrap/deployment tests pass; the five real-kernel cases pass on the authorized Incus client.
  During the retest, the user changes Compose to all.src_valid_mark=0 and recreates planner; observe
  all/eth0=0 and automatic restoration of both tables/rules. Final real gateway HTTPS passes 10/10,
  and SOCKS controls pass 10/10. Google 1.88–2.46s, Bing 1.52–2.01s; domestic HTTPS and front-end/ordinary
  UDP/TCP DNS also succeed. Evidence: target/meduza-gateway-retest.json, target/meduza-tproxy-regression.log,
  target/check-tproxy-deployment.log. The agent makes no production mutations, Git submission or
  workstation gateway change. Custom DNS fallback/privacy policy remains unchanged.

- 2026-11-13 (per the user) Clarify Manager topology labels, outward layout and framing (D92).
  Share centered translucent depth-sorted AS/node panel rendering; remove label leaders and reveal ordinary
  node labels on zoom/hover/selection with threshold hysteresis. Make panels hoverable/clickable, retain
  detail during pointer press, preserve label-drag offsets and keep clicks from reheating the simulation.
  Separate nearby AS title/name panels. Place force-body anchors in breadth-first XYZ hop layers from
  managed roots, continuing remote branches outward; adjust spring rest distances and default spacing.
  Default/reset frames managed nodes and configured direct endpoints, including down/unknown peers; fit-all
  includes remote paths. Preserve frame scope through resizing, manual navigation and metadata refresh.
  Retain collapsed managed roots and recover a vanished focus. Add Simplified/Traditional Chinese help.
  33 controller/topology/i18n regressions, Manager typechecks and production build pass. Actual Chrome tests
  verify both view buttons, real label hover/click/zoom, selection stability, four-hop branches and four
  viewport sizes without page scrollbars, collapsed managed roots and vanished-focus recovery. Evidence: target/check-topology-outward-tests.log,
  target/check-topology-outward-browser.log and target/topology-outward-*.png.
  Source-only; no Git submission or production deployment.


- 2026-11-13 (per the user's renewed Node-label jumping report) Fix Node labels beside their owners (D93).
  Replace ordinary node/router label avoidance slots with a fixed right-edge attachment and vertical
  centering. No side switching, collision displacement or viewport-edge pinning; retain zoom/hover/selection
  disclosure, depth-sorted translucent panels and label interaction/drag offsets. Reserve the fixed panels
  before AS layout so AS titles yield instead of moving Node labels. Supersede obsolete Node-avoidance tests
  with placement stability across moving/renamed AS headings, depth crossings, links, report order, orbit,
  zoom, resize and clipping. All 24 graph tests, typechecks and production build pass. Actual Chrome verifies
  nine AS/camera attachment cases and existing interaction/framing/responsive regressions.
  Evidence: target/check-node-label-anchor-tests.log, target/check-node-label-anchor-browser.log,
  target/check-node-label-anchor-build.log and target/node-label-anchor-selected.png.
  Source-only; no Git submission or production deployment.

- 2026-11-13 (per the user's OpenWrt OpenVPN investigation) Align planner secret scans with Manager (D94).
  Read-only SSH confirms the system OpenVPN binary/provider and planner PATH are correct. Eight active
  OpenVPN instances have their secrets, but three disabled entries retain intentionally undelivered refs;
  planner scans them and persists heldBack=[openvpn], with no native process/config ever scheduled.
  Pure device-side plugin plan returns eight plans without applying them. Fix the shared collector to
  skip explicit disabled objects, including instances/peers/services, while preserving active-key gating
  and node-held key recovery. Two unit tests and a real-plugin engine regression reproduce/fix the issue;
  nine OpenVPN library tests pass. The broader engine run is 7/8: an existing WireGuard DNS-observation
  timing test also fails in isolation locally and is left unchanged. Evidence: target/check-disabled-secret-refs-*.log,
  target/check-openwrt-openvpn-*.log and identifier/plan-only target/openwrt-openvpn-*.json.
  No remote mutations, Git submission or deployment. Actual OpenVPN startup/handshakes still need validation
  after deploying the corrected planner or a Manager configuration workaround.
