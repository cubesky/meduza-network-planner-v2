# Design decisions

Decisions taken while implementing `AGENT.md`. Each entry names the spec section it settles.
Later sessions append here instead of re-deciding.

## D1 — Repository layout (§4)

- Rust workspace at the root (`Cargo.toml`), shared `target/`. Members: `crates/{protocol,plugin-sdk,runtime}`
  and every package that ships Rust code (`planner`, `cli`, `updater`, `dashboard`, and the service plugins
  `openvpn`, `wireguard`, `tinc`, `frr`, `mihomo`, `mihomo-pkg`, `mihomo-openwrt`, `mosdns`, `dnsmasq`, `avahi`).
- `openvpn-pkg`, `mosdns-pkg`, `base-container`, `dashboard-openwrt` and `manager` contain no Rust crate:
  provider descriptors/build recipes, Containerfiles, LuCI assets and the TypeScript manager respectively.
- pnpm workspace (`pnpm-workspace.yaml`, single `pnpm-lock.yaml`): `web/ui-kit`, `web/descriptor-renderer`,
  `packages/meduza-np-manager`, `packages/meduza-np-dashboard/web`.
- Every package keeps `manifest.json`, `ui/descriptor.json`, `schema/config.json`, `schema/status.json`
  at its root; these are what ends up in `/usr/share/meduza-np/packages/<name>/`.

## D2 — Plugin protocol (§5.1, §5.2)

- A plugin is an executable at the manifest `entrypoint`. planner runs `<entrypoint> <operation>` with a
  JSON request on stdin (size-limited, default 1 MiB) and reads exactly one JSON response from stdout
  (limit 4 MiB). stderr is a redacted log, captured bounded. Timeout per operation (default 20 s), killed on expiry.
- Request envelope: `{protocolVersion, requestId, operation, node, instance?, config, secrets?, providers, context}`.
  Response envelope: `{protocolVersion, requestId, ok, result?, error?{code,message,path?}}`.
- Generators live in the plugin crates' `lib.rs` so they are unit-testable and are **not** linked into planner.
- Secrets reach plugins only in the request body (never argv/env). Plans mark secret-bearing files `sensitive`.

## D3 — Node machine identity (§7.1)

The spec allows an "explicitly implemented equivalent device authentication". Chosen scheme, chosen because it
survives TLS-terminating reverse proxies (§14.4.3) and does not depend on the node clock (OpenWrt boxes
without RTC):

1. Enrollment: node creates an Ed25519 key pair locally, `POST /api/v1/nodes/enroll` with the one-time,
   expiring enrollment token and its public key. Private key never leaves the node (`/etc/meduza-np/identity/`, 0600).
2. Session: `POST /api/v1/nodes/self/session/challenge` → random single-use nonce (60 s TTL);
   `POST /api/v1/nodes/self/session` with `{nodeId, nonce, signature}` → bearer token (15 min, revocable).
3. All node endpoints (incl. the WebSocket upgrade) take `Authorization: Bearer <token>`. 401 ⇒ re-auth once via
   the shared gate; repeated 401/403 ⇒ credential error state (no fast retry, §7.5.2).
4. Rotation: authenticated `POST /api/v1/nodes/self/keys/rotate` with the new public key signed by the old key.
   Revocation: manager marks the key revoked; outstanding tokens are invalidated.

## D4 — Manager stack (§6)

- Node 24 + TypeScript (compiled with `tsc`, run as plain ESM JS in production).
- HTTP: Fastify 5 (+ `@fastify/websocket`, `@fastify/cookie`, `@fastify/static`, `@fastify/rate-limit`).
- DB: built-in `node:sqlite` (`DatabaseSync`), WAL mode, versioned migrations; no native addon to compile.
- Passkey: `@simplewebauthn/server` + `@simplewebauthn/browser`.
- LDAP: `ldapts`. PKI: `@peculiar/x509` on WebCrypto. WireGuard keys: `node:crypto` X25519.
- Validation: JSON Schemas from `schemas/` (ajv, shared with the Rust side through the same files).
- Secret store: AES-256-GCM, key from `SECRET_ENCRYPTION_KEY_FILE`, per-record random nonce, AAD = secret id.

## D5 — Configuration model and compilation (§7.2, §8)

- Operators edit one typed **cluster document** (`schemas/cluster-config.json`): nodes, per-node service
  instances, meshes (tinc/wireguard), routing policy, DNS, Access policies, update policy.
- `publish` compiles it into one immutable **revision** (`rev-NNNNNN`) holding a per-node snapshot
  (`schemas/node-config.json`, the §8.2 shape) + digest. Peer public keys/addresses/neighbors are resolved
  by the compiler; only the node's own secrets are included as an envelope.
- A node's `desiredRevision` = latest revision whose snapshot for that node differs from the previous one
  (unchanged snapshots keep the old revision id so healthy instances are not restarted, §11.1).
- Rollback publishes the old cluster document again as a new revision.

## D6 — Rust runtime choices (§16)

- tokio `current_thread` runtime for planner/updater/dashboard; blocking work through a bounded pool.
- Pure-Rust crypto where possible: `ed25519-dalek`, `x25519-dalek`, `sha2`, `getrandom`.
- TLS via rustls; HTTP via hyper 1 (http1); WebSocket via tokio-tungstenite.
- JSON via serde_json. No dynamic plugin ABI.

## D7 — Push channel and node API details (§7.3, §7.5)

- First version implements WSS only (`GET /api/v1/nodes/self/stream`); the optional HTTP/2 event stream is not
  implemented. Frames: node sends `StreamHello` first, manager answers `StreamWelcome` (desired revision + whether the
  cursor resumed), then replays missed events and streams live ones. Event ids are per-node sequence numbers.
- One stream per node: a new connection replaces the old one. Slow consumers (>256 KiB buffered) are dropped.
- Manager capacity protection (`limits.ts`): global in-flight node requests, per-node token bucket, stream count and
  handshake rate. Rejections are 429/503 with a jittered `Retry-After`.
- Config `GET` answers 204 when nothing was published for the node, 304 on `If-None-Match`/`after` match.
- ACKs require the exact published digest; pointers never move backwards on out-of-order ACKs.
- Reports are ordered by (bootId, sequence); reports from the last 16 previous boots are rejected as stale.

## D8 — Contracts the plugins must follow for manager-side features

- Access instances: `services.openvpn.instances[]` items with `kind: "access"` and stable `id`; the manager only
  authorizes Access logins for enabled Access instances present in the node's desired snapshot.
- Secret references are any `*Ref` (string) / `*Refs` (array) field inside `services.*`. The compiler delivers
  manager-held secrets in scope; node-held secrets are resolved locally by planner from its key store.
- Node-side key generation: manager job `generate-keypair` with params `{secretId, keyKind, package, ...}`; the node
  stores the private key under `/etc/meduza-np/keys/<secretId>` (0600) and returns `{publicKey}`; the manager records a
  node-held secret with the public value.
- Package compilers (`packages/meduza-np-manager/server/src/compilers/<short>.ts`, registered in `compilers/index.ts`)
  resolve cross-node references: Tinc mesh members (public keys, addresses, subnets) and FRR internal neighbors.
  WireGuard/OpenVPN have no cross-node references (D17). Without a compiler, a service passes through.

## D9 — mihomo-openwrt / OpenClash integration (§9.8)

- Integration surface is limited to UCI section `openclash.config`: the adapter *writes* only `enable`,
  `config_path`, `en_mode` (`MANAGED_OPTIONS`, enforced in the UCI layer) and *reads* `cn_port`,
  `dashboard_password`, `dns_port`, `core_type`, `enable_redirect_dns` for status. Control is
  `/etc/init.d/openclash restart|stop` (bounded 180 s, at most one restart per 60 s). Status comes from the
  OpenClash-owned controller (`127.0.0.1:<cn_port>`, `/version`, `/proxies`, `/connections`).
- Version gate: luci-app-openclash `[0.45.0, 0.48.0)` read from the opkg control file or the apk database.
  Outside the range ⇒ incompatible (validation error + `down` status), never driven. Widen only after testing.
- Content: the adapter downloads its own subscription(s) and writes `/etc/openclash/config/<configName>.yaml`,
  dropping keys OpenClash rewrites from UCI (ports, controller, secret, DNS, TUN, ipv6...) and reporting them as
  overridden. OpenClash's own `config_subscribe` sections and other config files are never touched; a file with
  the managed name that the adapter did not create is never overwritten.
- Takeover record `<state>/mihomo-openwrt/takeover.json` (original value or "unset" per option, created files)
  is persisted *before* the first change and never overwritten by later takes. Planner has no "instance removed"
  hook, so hand-back is driven by config: `enabled: false` plans a one-shot `release` process
  (RestartPolicy::Never) that restores options, deletes adapter files and restarts/stops OpenClash according to
  the original `enable`. If the service entry is deleted outright, the `restore-takeover` action does the same.
  Stopping the adapter (upgrade, planner restart) leaves OpenClash running as configured.
- DNS: capability `mihomo-openwrt.dns`; dnsmasq treats it like `mihomo.dns` for a `kind: mihomo` backend pointed
  at OpenClash's `dns_port`. `enable_redirect_dns=2` (firewall DNS redirect) is reported as a plan warning.

## D10 — Avahi and the `.local` bridge (§9.11)

- `mode: managed` runs `avahi-daemon -f <generation>/avahi/main/avahi-daemon.conf` under planner; validation
  refuses it when `/run/avahi-daemon/pid` points at a live daemon whose command line is not ours (never a
  competing daemon). `mode: system` leaves the system daemon alone (bridge + status only).
- Defaults: IPv4 only, no wide-area, address publishing on, user-service publishing off, reflector off; the
  reflector requires managed mode and an explicit allow-list of ≥ 2 interfaces.
- Bridge = `meduza-np-avahi bridge` (same binary, separate supervised process `avahi/bridge`), UDP on
  `127.0.0.1:<bridge.port>` (default 5354, the port dnsmasq's `mdnsForward` uses). Only loopback clients, only
  names under `.local`, only A/AAAA (other types NODATA, other names REFUSED). One short-lived D-Bus connection
  per lookup, ≤ `maxInflight` lookups (excess ⇒ SERVFAIL), cache ≤ 512 entries (positive `cacheSeconds`,
  negative 5 s). Avahi `TimeoutError`/`NotFoundError` ⇒ NXDOMAIN, transport/D-Bus errors ⇒ SERVFAIL (not cached).
  Link-local IPv6 answers lose their `%scope` (DNS cannot carry it).
- D-Bus is a ~300-line client in the plugin (EXTERNAL auth, Hello, method calls, both byte orders) instead of
  libdbus/zbus: no C dependency, no async runtime, fits the §16 memory budget.
- Probes: daemon via D-Bus `GetState`/`GetVersionString`/`GetHostNameFqdn` (not process existence); bridge via a
  real query for `meduza-np-probe.local` (NXDOMAIN = path works, SERVFAIL = broken). `/etc/resolv.conf` and NSS
  are never changed.

## D11 — Built binary providers: openvpn-pkg, mosdns-pkg (§3.1, §14.1)

- Each package keeps `upstream.lock.json` (sources pinned by sha256 or full git commit, toolchain archives by sha256,
  builder image by digest, SPDX license + license file names) and `build/{Containerfile,build.sh}`. Release tooling
  runs `podman build --platform <oci platform> -f build/Containerfile --output type=local,dest=<dir> .` per platform
  and packages the exported tree (`/usr/lib/meduza-np/providers/<name>/…`, licenses, `build-info.json`).
- openvpn-pkg: OpenVPN 2.7.8 + OpenSSL 3.5.9 (static libs, no modules/engines/legacy) + libcap-ng 0.9.6 + LZO 2.10
  + LZ4 1.10.0 (D65), built natively on the target platform (QEMU for foreign arches) in Alpine/musl with
  `-all-static`. Disabled: DCO, .so plugins (PAM, down-root), PKCS#11, systemd. Kept: compression compatibility,
  management interface and script hooks (Access uses
  `auth-user-pass-verify … via-file`). The build fails unless the ELF has no interpreter/NEEDED entries and
  `--version` reports the pinned OpenVPN/OpenSSL versions and `enable_management=yes`.
- mosdns-pkg: MosDNS v5.3.4 at a pinned commit, Go 1.26.10 (pinned archive), `CGO_ENABLED=0 -trimpath`, modules
  verified by go.sum/GOSUMDB, cross-compiled on the build platform; a smoke stage runs `mosdns version` on the
  target platform. License files of MosDNS, Go and every linked module are shipped; a module without one fails.
- `crates/protocol/tests/provider_recipes.rs` enforces the pinning and that the Containerfile, lock and manifest
  agree. Refreshing a pin = edit the lock, re-verify (upstream checksum/signature), rerun CI's `provider-recipes`.

## D12 — Updater (§13, §14.3)

- Metadata: `channels/<ch>.json` → `releases/<group>/manifest.json`, each with `<file>.sig` = `SignatureFile` (Ed25519
  over the exact served bytes; several signatures for key rotation). Trust store `TrustStore` from
  `MEDUZA_NP_TRUST_FILE`, `/etc/meduza-np/trust/release-keys.json`, then `/usr/share/meduza-np/trust/release-keys.json`
  (threshold, `notAfter`, `revoked`). Checks: signature, pointer digest = manifest sha256, version/sequence agree,
  `expiresAt` (pointer and manifest) not passed, sequence ≥ highest accepted (`state.json`). A pinned group
  (`MEDUZA_NP_GROUP_VERSION`) skips the pointer and its expiry (explicit operator choice) but not the signature.
  Recovery from expired metadata = re-sign/republish the pointer (release tooling); nodes never accept expired data.
- Sources: `https://` (http only with `MEDUZA_NP_UPDATE_ALLOW_HTTP=1`), `file:///` or an absolute directory = offline
  import. Downloads use curl/wget through `plugin-sdk::fetch` (streamed to staging, bounded); verification is Rust.
- Resolve: `Selection::Installed` (native: update what is installed, a new required dependency *blocks*) or
  `Selection::Explicit` (`MEDUZA_NP_COMPONENTS`: fixed planner+updater + selection + requires closure, removals for
  deselected packages). Conflicts, missing platform artifact, unsupported platform block. D62 replaces the original
  planner-first/updater-last order: updater takeover precedes planner, `-pkg` and plugin activation. Same version + same sha256 = keep (unchanged packages are not downloaded); OpenWrt packages
  found installed by other means (`system`) at the same version are kept.
- Store backend (base-container, generic Linux): `objects/<pkg>/<ver>-<sha12>/usr/…` (archives may only contain
  regular files/dirs under `usr/`; the package's own manifest must match name+version), `views/g<N>` symlink farm
  (relative links; overlapping files between packages are refused), `current` replaced by rename. System/container
  paths link into `current` (`meduza-np-updater system-links`). GC keeps the current and previous generation only.
  Space preflight: 4× download size + 16 MiB free in the store, else blocked.
- OpenWrt backend: `opkg install --force-downgrade` / `apk add --allow-untrusted` of our verified files, one package at a
  time with `installed.json` updated after each; successfully installed files are cached for rollback; packages
  without a cached previous file are reported "rollback unavailable" in the plan. Not atomic — documented as such.
- Apply: journal phase written before each step (`downloading → staged → locked → switched → verifying →
  self-restart/done/rolled-back/failed`). No lock while downloading. When planner runs: `maintenance.lock`
  (pauseApply, 900 s), `drain` affected instances (unless planner itself restarts), switch, then `planner.restart`
  (new local method: planner exits 75 and its service manager restarts it; wait for a new `bootId`) or
  `registry.rescan` + `instance.restart` of affected instances; verify via `packages` that every changed package is
  `registered` at the new version; failure ⇒ switch back to the previous view (opkg: reinstall cached files), same
  restarts, `maintenance.unlock`, group remembered in `failedGroups` (not retried automatically; `apply --force` retries).
  planner `instances` now includes `applied: {instance: {package, processes}}` for this mapping.
- Self-update: the updater package is activated last; the process exits 75 and its owner (Podman restart policy /
  procd / systemd) starts the new binary. On start `recover()` sees phase `self-restart`: if the running executable is
  the new object (store) or the installed version (opkg/apk) ⇒ done; if the entrypoint fell back to the image copy
  (`MEDUZA_NP_UPDATER_SOURCE=image`) ⇒ roll back the whole generation.
- Daemon: own timer (`MEDUZA_NP_UPDATE_CHECK_SECONDS`, default 6 h, first check at a random phase ≤ 1 h, ±10 %;
  failures back off 5 min·2ⁿ up to the interval, ±30 %). Auto-apply needs `autoApply`, not paused/deferred, and the
  optional UTC window `MEDUZA_NP_UPDATE_WINDOW=HH:MM-HH:MM`; an explicit component change or bootstrap applies at
  once. `updater.sock` serves `update.*` (mutating methods: root or the updater's uid); pause/defer live in
  `control.json` so the socket thread never races the worker's `state.json`.

## D13 — Release tooling and workflow (§13.2, §15)

- `tools/release` (crate `meduza-np-release`, CI only). Fingerprint = SHA-256 over sorted input lines: package files
  (minus tests/examples/build output), files of every workspace crate in the package's resolved normal+build
  dependency closure (`cargo metadata --filter-platform`), external crates of that closure with version, source,
  Cargo.lock checksum and enabled features, toolchain file, `[profile.*]`, target list and the archive layout code.
  Web packages add the shared UI dirs and `pnpm-lock.yaml`. Paths are repo-relative and CRLF-normalized, so Windows and
  Linux checkouts agree.
- Matrix: same fingerprint as the baseline group ⇒ reuse version + artifacts (`reusedFromGroup` = original group);
  otherwise the package version = the group version without `v`. Users never bump 19 versions by hand.
- Archives: `tar.gz`, deterministic (sorted, mtime 0, uid/gid 0, fixed modes, gzip mtime 0), manifest version rewritten
  to the release version, binaries placed at the manifest entrypoint / `usr/bin`, provider recipe trees merged.
- `group` refuses an incomplete group (missing platform artifact, reused package absent from the baseline, version
  mismatch), never rewrites an existing immutable `releases/<v>/manifest.json` with different content, re-verifies
  every archive before publishing, and writes the channel pointer last. `sign-pointer` refreshes expiry.
- `release.yml`: plan (tests, baseline from Pages, fingerprints/matrix) → per-target Rust build of changed packages
  (one cargo invocation per target, ELF/QEMU checks) + provider recipe builds → publish (verify reused artifacts by
  digest, draft Release, upload only missing assets, sign, check every URL, undraft) → Pages (merge immutable
  history from the live site via `index.json`, replace pointer, deploy with the official actions).
  OCI images: D30; SBOMs: D28.

## D14 — base-container (§14.3)

- Volumes: `meduza-np-store` (updater rw, planner ro) at `/var/lib/meduza-np/store`; `meduza-np-node` (planner rw:
  `etc/`, state, providers, apk cache) at `/var/lib/meduza-np`; `meduza-np-ipc` at `/run/meduza-np` for the two
  sockets. Image paths `/usr/libexec/meduza-np`, `/usr/share/meduza-np/packages`, `/usr/lib/meduza-np/providers`,
  `/usr/bin/meduza-np*` are symlinks into `store/current`.
- The entrypoint stays parent of the role process: exit 75 ⇒ start the (new) current version in place (planner
  restart for updates, updater self-update takeover); a store updater failing 3× within 60 s ⇒ image baseline with
  `MEDUZA_NP_UPDATER_SOURCE=image`, whose recovery rolls the generation back.
- Alpine dependencies of installed components (`apk-deps`, `-pkg` providers suppress the system package) are installed
  at planner start with `--cache-dir` on the node volume; offline restarts use `--no-network` from that cache.

## D15 — Local dashboard (§12.3)

- `meduza-np-dashboard` (Rust, tokio current_thread + hyper http1, optional rustls) serves the prebuilt Vite + Preact
  UI from `/usr/share/meduza-np/dashboard/www` (`assets/*` immutable, `index.html` no-cache, `.gz` siblings) and a
  fixed API table onto planner/updater local methods (`READS`/`WRITES` in `server.rs`, checked against the protocol
  tables by a unit test) — never a generic socket proxy. Default listen `127.0.0.1:8443`; a non-loopback address is
  refused without TLS. CSP/nosniff/no-referrer/DENY headers on every response.
- Sign-in with the manager identity: `/auth/login` → planner `dashboard.login` → manager
  `POST /nodes/self/dashboard/login {nonceHash, returnUrl}` (returnUrl must be `<https origin>/auth/callback`,
  http only for loopback) → manager page `#/dashboard-login/<id>` shows node + return URL, the signed-in user approves
  → one-time code → `/auth/callback` → planner `dashboard.exchange {id, code, nonce}` → manager checks node, nonce
  hash, code (constant-time), expiry (5 min), single use, account not disabled, returns the *current* role. One
  failed exchange burns the login. Needs `MEDUZA_NP_DASHBOARD_PUBLIC_URL`.
- Break-glass offline sign-in: `meduza-np dashboard-link` (root, writes `<run>/dashboard/login-tokens/<sha256>`)
  prints a link valid once for 10 minutes → admin session marked `via: local`.
- Sessions in memory (≤ 32, ≤ 8 h from the manager, 1 h local), cookie holds a random token whose hash keys the map;
  CSRF token required on every POST; viewer reads, operator acts, admin applies updates. Config is read-only: changes
  still go through manager drafts (§12.3).
- Container: the planner role runs the dashboard next to planner in a loop that restarts it when it exits or when an
  update switched its binary.

## D16 — Data-plane continuity across reloads and upgrades (found in the multi-node lab, tools/lab)

- WireGuard keys/peers are loaded with `wg syncconf` (only differences applied; sessions survive). `wg setconf`
  replaced every peer and cost ~15 s of tunnel outage per planner restart or config change.
- The rendered WireGuard file always carries `ListenPort`. Without a configured port the plugin derives a stable one
  from the interface id (49152–65535; a collision is a validation error asking for an explicit port). A missing
  `ListenPort` makes both `setconf` and `syncconf` bind a new random port, which breaks NAT mappings and peers'
  learned endpoints until the next keepalive.
- `registry.rescan` re-applies the last good configuration when a registered package's manifest digest or a resolved
  provider (path/source) changed, so a plugin or provider upgrade takes effect immediately; commit only reloads or
  restarts plans whose output changed.
- Supervised daemons (OpenVPN, FRR, dnsmasq, tinc, mihomo, …) used to be stopped when planner exited for an
  upgrade; since D32 a planned planner restart leaves them running and the new planner adopts them. Kernel resources
  (WireGuard links, addresses, routes, nft tables) were always untouched.

## D17 — Connection roles: Tinc is the internal mesh, WireGuard/OpenVPN are external only

- Managed nodes reach each other only over the internal Tinc mesh (§9.1: the manager compiles members, keys and
  endpoints). The mesh runs **without NAT**: every member declares `publicAddresses` the other members reach
  directly, and roaming nodes (possibly behind NAT) do not join it. The Tinc compiler reports both as problems.
- WireGuard and OpenVPN (Site2Site and Access) describe **external** connections: third-party sites, remote clients,
  roaming nodes dialing a mesh gateway. Peers/remotes carry explicit public keys and endpoints; the `node`/`instance`
  peer fields (WireGuard) and `peer` (OpenVPN) were removed from the plugin schemas, and `compilers/policy.ts`
  rejects them before schema validation with the rule spelled out. The WireGuard compiler only fills the interface's
  own public key (display/export); the OpenVPN compiler was removed.
- Routing between LANs of mesh members is FRR iBGP over the Tinc addresses (§9.5); WireGuard/OpenVPN tunnels attach
  external prefixes at gateways (e.g. external BGP neighbors, Access pools).
- Tinc switch mode gives each member's tap a locally administered MAC derived from netname + member name (set in
  tinc-up); host MAC policies derive identical MACs on cloned machines. Lab result: full mesh between three members,
  a planner restart cost ~0.8 s on the mesh before supervised daemons survived planner restarts; with D32/D33 a
  planner-only upgrade loses no packets on the mesh or LAN-to-LAN (lab, 2026-10-24).


## D18 — Routing roles: iBGP+BFD over Tinc, eBGP over WireGuard/OpenVPN, OSPF only for Anycast, roaming last

- **Internal routes**: FRR iBGP between mesh members over their Tinc addresses (`peeringAddress` must be the node's
  Tinc address), BFD from `meshDefaults.bfdProfile`. Mesh neighbors take the per-neighbor defaults (accept/export
  external routes, next-hop-self); `meshDefaults` only carries the BFD profile.
- **External routes**: eBGP only over a WireGuard/OpenVPN `transport` of the node (manager and plugin reject Tinc or
  a missing transport). Two independent filters per eBGP neighbor, both on by default: `noTransit` (accept only
  routes the peer originated: AS path = its AS, prepends allowed) and `noForward` (advertise only our own AS: the
  node's export set plus, with `exportPolicy.internal`, every member's public prefixes; never routes learned from
  other external peers).
- **Anycast**: OSPF exists only for it. The cluster declares `routing.anycast` (one pool, one /30 per service, a
  node-side health check); nodes pick services with `services.frr.anycastServices`. Every FRR node on the Tinc mesh
  joins the OSPF domain over its Tinc device(s) even without local services, so traffic for the pool entering
  anywhere reaches the nearest healthy instance. The guard (`frr anycast-guard`) owns the service addresses: healthy →
  addresses on, dummy up, stub announced; unhealthy → dummy down **and** addresses removed (a down device keeps its
  local routes, so the node's own clients would otherwise still hit the dead local service). Services listen with a
  wildcard bind; the probe defaults to 127.0.0.1. Carriers are point-to-multipoint with prefix suppression: no DR
  (on a broadcast segment the DR's failure cut every path for ~5 s, twice) and no member /32s in OSPF — it carries
  the service stubs only. A zebra `ip protocol ospf route-map` filter was dropped: FRR 10 routes those commands
  through mgmtd, which this instance does not run.
- **Roaming nodes** (mobile, metered, moving) are a last resort: no BFD on any session involving one (manager and
  plugin refuse it); peers that are roaming nodes of the cluster are marked `roaming` by the manager, so routes they
  only transit get local-preference 50; a roaming node prepends its AS three times on what it forwards. Without BFD
  the hold timer detects a lost peer, so such sessions use short `timers` (keepalives cost far less than BFD).
- Deployment: FRR daemons need CAP_SYS_ADMIN (run.sh adds it with the frr component, Quadlet documents it) and own
  their generation directory (chown to frr); `net.ipv4.ip_forward` is verified, not written, where /proc/sys is
  read-only. Lab results (tools/lab/README.md): internal member failure withdrawn in ~1 s, roaming peer in ~7 s
  (timers 3/9), crashed Anycast service in ~4.5 s, frozen Anycast node in 0.7 s without loss on the remaining path.

## D19 — Interface languages: English, Simplified and Traditional Chinese

- The manager web UI (and the shared ui-kit / descriptor renderer) speaks `en`, `zh-Hans` and `zh-Hant`. Messages are
  keyed by their English text (`t("Save draft")`, `{name}` placeholders), so code stays readable and English needs no
  dictionary; zh-Hans uses mainland terms (配置, 密钥, 证书), zh-Hant Taiwan terms (設定, 金鑰, 憑證, 網路), both written
  by hand rather than converted. Constant tables mark their texts with `msg()`.
- Language choice: the account setting (`accounts.settings`, migration 3, `PUT /api/v1/account/settings`) wins, then a
  choice made in this browser before signing in (sign-in page picker), then `navigator.languages` (zh-TW/HK/MO/Hant →
  zh-Hant, other zh → zh-Hans, otherwise English). "Browser language" clears the account setting.
- Package UI descriptors carry their own translations in `i18n.<lang>.text` (keyed by the text as written) and
  `i18n.<lang>.fields` (labels by field name); missing entries fall back to the text and the humanized field name.
  Field help sentences come from the config schemas' doc comments and stay English for now.
- `server/test/i18n.test.ts` fails when a message or descriptor text lacks a translation or a translation drops a
  placeholder. Server error messages are not translated.

## D20 — Features can be configured before their package is installed

- Supersedes the §3.4 rule "enabled features for packages the node does not have are rejected before publishing"
  (per the user). The manager reports such a feature as a **warning** (`Problem.severity = "warning"`, never blocks a
  publish); the planner applies the rest of the revision and reports `services.<x>: waiting for meduza-np-<x>`
  instead of rejecting the whole revision. Installed but unusable packages (missing provider, failed registration)
  are still errors.
- The planner writes the components its configuration needs to `RUN_DIR/wanted-components.json` (rewritten only
  when the list changes). **With an updater** the node installs them automatically: its selection is
  `MEDUZA_NP_COMPONENTS` plus the wanted list (unchanged when nothing new is wanted, so no forced apply outside the
  update window), and a changed list is treated as an operator request and applied within a minute. A feature
  removed from the configuration leaves the wanted list, so its package is removed unless `MEDUZA_NP_COMPONENTS`
  keeps it (only a newly received revision shrinks the list; restoring the last good configuration merges, D46). **Without an updater** an operator installs the package; `registry.rescan` then re-applies the last
  good configuration and the feature starts.

## D21 — Direct editing in the manager; no drafts

- Node settings live on the node page (Settings tab) and cluster-wide settings on Global settings (internal ASN,
  Anycast catalog, node defaults, OpenVPN CA). Both edit one working copy of the cluster document in the browser; a
  bar shows unsaved changes on every page, "Review & apply" shows the validated diff of each node that changes and
  publishes it with `POST /api/v1/config/apply {document, baseRevision}` — 409 when someone published meanwhile.
  Every apply is a revision; History lists them with per-node results and rollback. The draft API remains for
  tooling but the console no longer uses it (stale drafts had shown pre-D17 configuration as if it were current).
- Keys live where they are used: key fields offer "New…" — generate on the node (WireGuard/Tinc, private key never
  leaves it), issue a certificate from the cluster CA (fills cert/key/CA/CRL), generate a static or preshared key,
  or enter a value; all scoped to that node. Key fields only list secrets that node may receive. The Keys and PKI
  pages are gone; the CA and issued certificates are on Global settings, the LDAP bind password on Access.
- Node page: Status (services with restart next to each process — the report now names a process's instance and
  `restart-instance` takes `params.process` — and details in a dialog only when something is off), Routing
  (BGP/BFD/OSPF/Anycast), Connections (Tinc/WireGuard/OpenVPN peers), Settings, Diagnostics (packages, jobs, raw
  report, enrollment token, revoke). Adding a node is a separate page that ends with the enrollment instructions.

## D22 — One Tinc mesh; nothing to type for iBGP

- There is exactly one internal mesh. `services.tinc` holds the node's member settings directly (address, public
  addresses, port, key, advanced options); there is no list of networks and no netname. Every node with Tinc enabled
  is a member. The plugin and the manager still read the earlier `instances[0]` shape (the manager normalizes it
  before validation, the console when loading), and the manager always emits the new one — upgrade the nodes' Tinc
  plugin before the manager publishes the new shape (an older plugin would see no mesh).
- FRR mesh members need no ASN and no peering address: `localAsn` defaults to the internal ASN from Global settings
  and `peeringAddress` to the node's Tinc address. The form shows both only under an advanced "own AS" section, meant
  for nodes outside the internal network.

## D23 — LuCI-style staged changes (supersedes the "no drafts" part of D21)

- Three layers like LuCI: published revision; the account's **staged** changes (its open draft, server-side, so they
  survive reloads and other tabs); the page's unsaved form edits. A bottom bar offers Reset / Save / Save & Apply for
  form edits; a "Changes: N" button on every page opens the review of everything staged — per node (or global
  section) with its diff, Revert per section, Revert all, Apply.
- Apply publishes the draft. If someone else published meanwhile, the draft is rebased first
  (`POST /api/v1/drafts/:id/rebase`): three-way per node and per cluster section (routing, defaults) — sections only
  this account changed keep its version, sections only the others changed take theirs, sections both changed keep
  this account's and are listed as conflicts to check before applying again.
- Staged changes never hide newer revisions: when the console loads, a draft based on an older revision is rebased
  onto the current one (same three-way merge), and the console notices revisions applied elsewhere (another account,
  the manager distributing a node key) within 15 s.
- Collections (WireGuard interfaces, OpenVPN instances…) are compact tables; editing opens a dialog with the
  sections as tabs (Save / Dismiss), nested lists such as WireGuard peers likewise. Manager and dashboard have a
  System / Light / Dark theme switch (per browser).

## D24 — BGP is set on the connection, FRR is compiled (supersedes the FRR form of D22)

- The node settings have no FRR section. The manager compiles `services.frr` from what the configuration already
  says: Tinc membership plus the internal ASN → iBGP with every other member (BFD with the cluster timers
  `routing.bfd`, on unless switched off, never for roaming nodes); `bgp` on a WireGuard peer or an OpenVPN Site2Site
  instance → an eBGP neighbor over that connection (transport implied, address defaulting to the peer's host route in
  `allowedIps` or the OpenVPN `ifconfig.remote`, neighbor id `<instance>-<peer>` or `<instance>`); the node's
  `routing.anycast` → its Anycast services; `routing.maximumPaths` → ECMP. A node outside the internal network sets
  its own AS as node `routing.asn` (refused on Tinc members).
- The session type (`meduza_np_protocol::bgp::BgpSession`) is part of the WireGuard and OpenVPN schemas, so their
  forms show it (an optional group behind a switch), but the manager strips it from what those plugins receive: the
  VPN plugins never act on routing. OpenVPN's informational `bgpNeighbor` string is replaced by it.
- A written `services.frr` (raw JSON, documents from before D24) still applies on top of the derived settings, so
  existing documents compile as before; the Network tab says when a node has one.

## D25 — iBGP neighbors are always the Tinc mesh members

- Every Tinc mesh member runs iBGP with every other member, on their Tinc addresses, and has no other iBGP neighbor.
  There is no opt-out: a written `meshInternal: false` is ignored, disabling FRR on a member is refused, and an own AS
  on a member is refused (D24). A written `internalNeighbors` entry may only name another member, and then only tunes
  that session (weight, local preference, next-hop-self…); address-only entries or non-members are refused.
- Without an internal ASN the members cannot peer yet: a warning (never blocking) asks for it in Global settings.

## D26 — Routing topology graph

- `GET /api/v1/topology` builds the graph from the published configuration (each node's compiled FRR neighbors:
  which sessions should exist) and the latest reports (`bgp-neighbor` instances: FRR state, prefix counts and, since
  FRR 0.5.1, the routes accepted from each neighbor, at most 64 listed). Every managed node is a vertex carrying
  its AS; the console groups the nodes of one AS into a cluster (a translucent bubble labelled with the ASN, members
  pulled together; vertices repel only within their cluster and clusters collide as whole bodies, so one AS never
  rings another), so the ASN is shown once per AS instead of on every node; nodes without BGP form a "No BGP"
  cluster. iBGP links two
  mesh members (one link per pair, both sides' sessions on it). eBGP links the two nodes when both ends are managed
  (a branch in its own AS and the edge router it peers with), otherwise the node running it (an edge router, drawn
  as a square) and a vertex for the unmanaged external AS, never its routers. "Collapse external ASes" draws every AS
  other than the internal one as a single AS vertex (its nodes hidden, their links merged onto it), remembered per
  browser. A link is up when every session on it is Established, partly up when some are, down when
  none are, and "no report" while no side reported it.
- The console draws it as a 3D force-directed graph on a 2D canvas (no WebGL dependency; the graph has tens of
  vertices): iBGP in the accent colour, eBGP in orange, down sessions dashed red, particles flowing on live links;
  vertices can be dragged, the scene rotated, zoomed and reset (the camera keeps the whole graph in view); clicking shows the sessions with their routes.

## D27 — Live LDAP acceptance for OpenVPN Access

- `tools/access-ldap-e2e.sh` (CI job `access-ldap`, Docker on the runner, Podman elsewhere) runs the pinned
  `docker.io/osixia/openldap:1.5.0` with a CA and server certificate generated per run (ldaps and StartTLS on
  loopback ports, memberOf overlay, its read-only account as the service bind). The image is test-only and never
  shipped; it is used because it brings TLS, the memberOf overlay and a read-only account through environment
  variables, without a hand-written cn=config bootstrap. `--ulimit nofile=1024` is required (slapd sizes its tables by
  the file limit), DH parameters are the named ffdhe2048 group (GnuTLS in slapd rejects X9.42 parameters).
- The test seeds a fresh `ou=run-<random>` subtree per run, so runs never clean up or collide.
- The hook chain part runs the real `openvpn-auth` and planner. OpenVPN itself is not installed in that planner
  tree, so the test writes the applied-state record for the Access instance that the engine writes after a
  successful apply; the manager side still checks the node's published configuration. The hook is spawned
  asynchronously because the manager answering it runs in the test process.
- Found by it: the default user filter named the object class `inetOrPerson` (matches nothing); now
  `inetOrgPerson`. Settings saved with the misspelt default unchanged are read as the corrected default; custom
  filters are kept as written.

## D28 — SBOM per package version

- `meduza-np-release sbom` writes CycloneDX 1.5 JSON from the same inputs as the fingerprint (§15.2), so an SBOM
  changes exactly when the package does: the Rust normal + build closure for the release targets (registry crates as
  `pkg:cargo` with the Cargo.lock SHA-256 and their license expression, workspace crates without purl, the
  dependency graph), the provider's `upstream.lock.json` (sources as `pkg:github`/`pkg:generic` with digest or commit,
  the Go standard library for Go builds), and for the manager/dashboard the npm packages from `pnpm-lock.yaml`.
- npm scope: the importer's `dependencies` and their transitive closure (what ships in node_modules), plus the
  packages listed in `meduzaNp.bundled` of its package.json. Browser libraries stay devDependencies (the production
  image installs `--prod`, so they must not land in node_modules), but they are compiled into the shipped bundle and
  have to appear in the SBOM; listing them explicitly is the only reliable signal without instrumenting Vite.
- Deterministic: `SOURCE_DATE_EPOCH` timestamp (release.yml uses the tag's commit time), serial number derived from
  package, version and fingerprint. release.yml uploads `<package>_<version>.cdx.json` for changed packages; a reused
  package keeps the SBOM already published with its unchanged version.
- Runtime images (manager, base-container) also list what the image adds below our code: the final stage's base
  image (from the Containerfile, must be pinned by digest; `pkg:oci/...`), Node.js of the node base image (version
  from its tag), and per image platform every Alpine package of the built image as
  `pkg:apk/alpine/<name>@<version>?arch=<arch>&distro=alpine-<VERSION_ID>` with license (as a license name: apk
  license fields are not always valid SPDX), origin/commit/apk checksum and apk dependency edges (`D:` resolved
  through names and `p:` provides). The source is the image itself, not the Containerfile: `tools/release-image.sh`
  copies `/lib/apk/db/installed` and `os-release` out of a created, never started container of the pushed index
  (also on the reuse path), and `sbom --image-os DIR` refuses an image package without them, a missing platform,
  a non-Alpine image or a database whose architecture does not match the platform. The fingerprint is unchanged
  (apk packages are resolved at build time, so a rebuild of an unchanged image would only happen on a version
  bump); the SBOM describes the image that was actually published.

## D29 — OpenWrt packages (ipk + apk) from the release binaries

- The OpenWrt packages reuse the static musl binaries and metadata of the generic archives; no OpenWrt SDK build.
  `meduza-np-release openwrt` adds the OpenWrt layout (procd init scripts in `/etc/init.d`, the updater launcher and
  the release trust store for the updater, the LuCI app's `htdocs/` → `/www` and `root/` → `/`).
- One file per OpenWrt package architecture and format, because opkg and apk refuse foreign architectures. The table
  in `tools/release/src/openwrt.rs`: x86_64 → `x86_64`; aarch64 → `aarch64_generic`, `aarch64_cortex-a53/-a72/-a76`;
  armv7 (VFPv3-D16 hard float) → `arm_cortex-a7_neon-vfpv4`, `arm_cortex-a9_vfpv3-d16`, `arm_cortex-a15_neon-vfpv4`.
  FPU-less ARM and MIPS are not supported yet (no build target). The group platform is `openwrt/<arch>`
  (= the updater's `DISTRIB_ARCH` platform); the LuCI app is `all`/`noarch` with platform `any`.
- `.ipk` (opkg, OpenWrt ≤ 24.10) is written by the release tool itself: gzipped tar of `./debian-binary`,
  `./data.tar.gz`, `./control.tar.gz`, names written raw (opkg looks members up with the `./` prefix), fixed
  mtime/owner/mode ⇒ byte-identical. `.apk` (apk-tools 3 ADB, OpenWrt ≥ 25.12) is built by `apk mkpkg` from a staged
  tree (fixed modes, mtimes touched to 0) in the base container's pinned Alpine image (`tools/openwrt-apk.sh`), because
  OpenWrt's own apk is compiled without `mkpkg`. Both builders and the arch table are part of the OpenWrt packages'
  fingerprints.
- Dependencies: only other `meduza-np-*` packages (`requires`; `conflicts` as opkg `Conflicts` / apk `!name`). System
  software (wireguard-tools, kmods, frr, …) is not declared: the planner reports missing dependencies, and an update
  can never pull system packages from the feeds (D12). Exception: the LuCI app depends on rpcd, jsonfilter, luci-base.
- Maintainer scripts: planner/updater `enable` + `start` on install (an unchanged procd instance is not restarted),
  `stop` + `disable` on removal; plugins/providers ask a running planner to `rescan`; the LuCI app clears LuCI caches
  and HUPs rpcd. All of them do nothing while `/var/run/meduza-np/updater-installing` exists: the updater holds that
  marker around every opkg/apk call (install, rollback, self-install) and does drain/restart/verify itself; a stale
  marker is removed when the updater starts.
- Group completeness: a package needs a `tar.gz` for each `linux/*` platform when it is a generic archive package and
  an `ipk` and an `apk` for each `openwrt/*` platform when it is an OpenWrt package. The Rust dashboard is not built
  for OpenWrt (LuCI, §16); mihomo-openwrt and the LuCI app exist only there.

## D27 — Roaming nodes are mesh members (supersedes "roaming nodes stay outside the Tinc mesh")

- A roaming node (mobile, metered, behind NAT) joins the internal Tinc mesh and the internal AS like any other node:
  it needs no publicAddresses and only dials out to the static members (Tinc lists it as a host without an address,
  so nobody connects to it; the static members still reach each other directly). It needs at least one static
  member with publicAddresses.
- Its iBGP sessions (to and from it) run without BFD on short timers (keepalive 3 s, hold 9 s), so a vanished
  roaming member is withdrawn within the hold time. What it learned outside the mesh and passes on is the last
  resort: receivers give it local preference 50, and the roaming member sets the same on what it sends.
- It stays outside the Anycast OSPF domain (no OSPF over a metered link). WireGuard/OpenVPN eBGP remains for real
  external ASes; a roaming node may still use it, as before.
- The topology graph draws no vertex for the internal AS (D26).

## D28 — Node-held keys before the package is installed

- A key the node generates itself (Tinc, WireGuard) needs the node's package, but before D28 a configuration that
  referenced a key not generated yet could not be applied, so the package that would generate it was never
  installed. Requesting the key from the console (`POST /api/v1/nodes/:id/keys`) now records it as pending: a
  node-held secret without a public value.
- The configuration applies with a pending key: the node receives its settings (its updater installs the package),
  the other members skip it with a warning ("joins once its key is generated"). On the node, the planner holds back
  only the feature whose key is missing (a warning), the rest of the revision applies.
- The manager queues the generation job as soon as the node reports the package registered, and again after a
  failed attempt (up to 5). When the public key arrives it publishes the current document again (author `system`):
  the document is unchanged, the compiled snapshots now carry the key. The node re-applies its last good revision
  after generating a key, so the waiting feature starts.
- A package added at runtime whose system software the base container installs at planner start (Alpine packages
  in `apk-deps`, e.g. `tinc`) makes the updater restart planner, so the binary is there when the key is generated.

## D29 — Mihomo subscriptions are global settings

- The cluster document holds one list of Mihomo subscriptions (`mihomo.subscriptions`: id, name, `urlRef`), a
  `default` and `refreshSeconds` (default 43200). A node that enables Mihomo (meduza-np-mihomo, or the OpenClash
  adapter meduza-np-mihomo-openwrt) uses the default; `services.<mihomo>.subscription` chooses another entry of the
  list. The manager compiles the choice into the plugin's own `subscriptions` (only the chosen entry, so a node
  receives only the URL it uses) and `activeSubscription`. Documents without a global list keep per-node
  subscriptions.
- Subscription URLs are secrets with node scope `*`: delivered to any node whose compiled configuration references
  them.
- Refreshing: the wrappers download their subscription every `refreshSeconds` (plugins accept `refreshSeconds`, older
  configurations `refreshHours`). The `refresh-subscription` action forces a download now (it leaves a trigger the
  running wrapper picks up within 10 s): per node from its Mihomo panel, for all nodes or the users of one
  subscription from Global settings (`POST /api/v1/mihomo/refresh`).
- Settings-block packages show their descriptor actions in the block header (they were only reachable for
  collections before).

## D30 — OCI images of a package group (§15.1 step 6, §15.4)

- Every generic archive package (`ARCHIVE_PACKAGES`) gets a distribution image `ghcr.io/<owner>/<package>`: one
  image per `linux/*` platform whose single layer is `meduza-np/<package>/<archive>.tar.gz` + `artifact.json`, tied
  by an image index. `meduza-np-release oci-layout` writes it as an OCI image layout with no timestamps, sorted
  entries and gzip mtime 0, so the same archives give the same digests; `skopeo copy --all --preserve-digests`
  pushes it unchanged (verified on Podman 5.4 with a local registry: pushed index digest = computed digest, an
  `arm/v7` pull selects the arm image). The image only wraps the archive: nodes still install from the Release asset,
  the image is a registry copy for mirrors and air-gapped imports. OpenWrt-only packages (LuCI app, OpenClash
  adapter) get no image, since OpenWrt never pulls from a registry (§15.4).
- Runtime images: `meduza-np-base-container` (amd64, arm64, armv7) and `meduza-np-manager` (amd64, arm64; the
  manager is a server and its Node.js base image is not tested on armv7) are group packages now (fingerprinted,
  in the matrix and the group manifest). `tools/release-image.sh` builds them with `podman build --platform …
  --manifest`, smoke-tests them (base: the embedded updater runs `trust-check` under QEMU on each platform; manager:
  `tools/manager-podman-smoke.sh` on the native platform), pushes them with `podman manifest push --all` and
  describes the pushed index (`oci-describe`). A version tag that already exists is reused as it is (re-runs never
  rebuild or move a tag).
- The base image embeds the group's updater: the base-container fingerprint contains the updater's fingerprint, and
  the image is built from the updater archive of this run or, when the updater was reused, from its published
  archive (downloaded and checked against the baseline's SHA-256). The manager fingerprint includes `schemas/` and
  every package's manifest/UI/schema, which its Containerfile copies.
- Group manifest: `Artifact::oci` = `<repo>@sha256:<image index digest>` (the multi-platform reference deployments
  pin) on every generic archive; runtime image packages have one `format: "oci"` artifact per platform whose
  `sha256`/`size` describe that platform's image manifest and whose `url` is the registry manifest URL. The updater
  never selects `oci` artifacts (it asks for `tar.zst`/`tar.gz`/`ipk`/`apk`). `group --oci DIR --require-oci 1`
  refuses a group with an archive or image platform not pinned to an image.
- Tags: `<package version>` (immutable; an existing tag with other content fails the release) and the group version
  (`v0.3.0`) on every image of the group, including reused ones (their digest is kept and verified by the copy).
  Before the release is undrafted `oci-check` compares every pinned index from the registry with the manifest
  (digest and each listed platform).
- One-time setup: GHCR packages pushed with the workflow token start private; make them public (or give nodes and
  the manager host registry credentials).

## D31 — Pinning Mihomo releases for mihomo-pkg (§3.3, §15)
- `meduza-np-release mihomo-assets --tag vX.Y.Z` (release preparation, a maintainer run, not part of release.yml)
  writes `packages/meduza-np-mihomo-pkg/assets.json`. The tag is always named; `vX.Y.Z` only, published and not a
  prerelease (`latest`/`Prerelease-Alpha` are refused). Asset names come from `platformMapping` (amd64 uses the
  `compatible` build: no AVX requirement on small x86 routers/VMs).
- Every asset is downloaded and checked before it is recorded: size and SHA-256 equal GitHub's release metadata
  (`digest`; a release without digests is still accepted on our own computation), the download URL is exactly
  `https://github.com/<upstream>/releases/download/<tag>/<name>`, it decompresses (bounded) to an ELF for that
  platform, the binary contains the tag string, and on a host of that platform `mihomo -v` reports the tag. The
  recorded digest is the one computed from the downloaded bytes.
- Older tags stay (nodes may have `targetVersion` pinned to them; validation rejects unknown tags) unless `--keep N`;
  newest first. `assets.json` is compiled into the plugin, so pinning a version is a mihomo-pkg version bump and a
  fingerprint change. `--release FILE` takes a saved `releases/tags/<tag>` document for re-runs without the API.
- First pin: v1.19.33 (mihomo-pkg 0.1.1). Verified on the VPS: the tool's checks on all three platforms, then the
  real `mihomo-pkg prepare` in auto mode downloaded, verified, activated and registered the amd64 binary.

## D32 — Supervised daemons survive a planned planner restart (process adoption, §11.2, D16)

- `Supervisor::persistent(<RUN_DIR>/supervisor)` (Linux) writes a record per started process: pid, kernel start time
  (`/proc/<pid>/stat` field 22), boot id, canonical program path and a digest of the whole launch spec (program, argv,
  env, restart policy, stop timeout, restart digest). Processes keep their own process group as before.
- `planner.restart` (the updater after switching binaries, exit 75) calls `detach_all`: the run tasks return without
  signalling, records stay. SIGTERM (service stop/restart by the operator) still stops everything and removes the
  records, so stopping the service stops the data plane as before.
- The next planner adopts in `ensure` (driven by the normal last-good restore) only when the record proves it is the
  same process (same boot, pid with the same start time, still leading its group, not a zombie), the spec digest is
  equal, the program still canonicalizes to the same path (a switched store view = package upgrade) and
  `/proc/<pid>/exe` is not `(deleted)` (replaced in place by opkg/apk). Otherwise the recorded group is stopped
  (SIGTERM, deadline, SIGKILL) and a new process started. Groups are only ever signalled after that identity check,
  never by a bare pid/pgid from a file. After the restore, `reap_unclaimed` stops recorded processes nothing claimed
  (instance removed or package deregistered while planner was down); `stop(id)` without an entry stops a recorded one.
- An adopted process is not our child, so its exit is seen by polling (500 ms) and its exit status is unknown: it
  counts as a failure for the restart policy. Status reports carry `adopted: true` while it runs; `running_since` is
  derived from the process age so startup verification does not wait again.
- Daemon stderr goes to `<RUN_DIR>/supervisor/<sha>.log` (0600, append) instead of planner's stderr: a pipe from
  procd/journald/conmon would be closed when planner restarts and the adopted daemon would die of SIGPIPE on its next
  line. Planner forwards the file line by line to its own stderr as `<process id>| <line>` (so `logread`, the journal
  and `podman logs` still show it), truncates it after 64 KiB were forwarded and empties it on detach; output in the
  short window between reading to EOF and truncating can be lost. Daemon lines are not put into planner's in-memory
  log ring (unchanged: they never were).
- Pidfiles in RUN_DIR are removed at start only when they do not name a live recorded process (they are valid for
  adopted daemons).
- systemd: `KillMode=process` (otherwise systemd kills the cgroup when the main process exits) plus
  `ExecStopPost=-meduza-np-planner stop-orphans`, which stops recorded processes unless the exit was the planned
  restart (`EXIT_CODE=exited`, `EXIT_STATUS=75`). A planner crash under systemd therefore still restarts the daemons;
  under procd and in the container (tini as PID 1 reaps adopted processes) a crash leaves them for adoption too.
- Not adoptable, restarted as before: processes on non-Linux builds, `Supervisor::new()` (tests), interpreted
  wrappers whose exec target changed under the same path.

## D33 — Process binding (`boundTo`) and adoption of privilege-dropping daemons (§11.2, D32)

Found in the lab when re-measuring a planner-only upgrade with D32: Tinc was adopted (0 lost pings), but every FRR
daemon was restarted, and LAN-to-LAN traffic over iBGP stopped and did not recover.

- **Adoption check without /proc/<pid>/exe**: FRR daemons drop privileges to user `frr`; planner has no
  CAP_SYS_PTRACE, so the exe link is unreadable and D32 treated that as "binary replaced". Records now carry the
  program file identity (`programFile`, `<dev>:<inode>` of the canonical path at start). With a readable exe link a
  `(deleted)` image or a different file identity means replaced; with an unreadable one the file identity decides,
  and a record without it (older planner) is not adopted. Package managers replace binaries by rename (new inode),
  and a running executable cannot be written in place (ETXTBSY), so the inode is sufficient evidence.
- **`ProcessSpec.boundTo`** (plan protocol, optional): id of an *earlier* process of the same plan
  (`GenerationPlan::validate` rejects anything else). The bound process is restarted whenever its target starts a
  new process (crash restart by the policy, spec change, manual restart, not adopted after a planner restart) and
  is not adopted when its target was started anew by this planner. Adopting the target does not count as a start.
  Implemented with a broadcast of fresh starts in the supervisor; events that arrive while the dependent itself is
  restarting are dropped (it reconnects to the new target anyway); a lagged receiver restarts conservatively.
  The field only enters the launch-spec digest when set, so records of unbound processes stay adoptable.
- **FRR**: zebra is planned first and bfdd/bgpd/ospfd are bound to it — what FRR's own watchfrr does. A daemon that
  outlives zebra keeps stale interface/connected state: bgpd's `network 10.62.0.0/24` stayed invalid (not `*>`,
  PfxSnt 0) after zebra was replaced under it, so the other members withdrew the LAN until FRR was restarted.
  Previously the daemons were ordered alphabetically (zebra last), so a fresh bgpd first connected to the old zebra.
  The Anycast guards are not bound (they manage dummy devices, not zebra state).


## D34 — A feature waits for a provider that is prepared at runtime (§3.3, §11.1, D20, D28)

Found in the lab with the first Mihomo deployment through the manager: lab-b received `services.mihomo` +
`services.mihomo-pkg` (auto-download), the updater installed both packages, and the planner then rejected the whole
revision ("provider-missing: meduza-np-mihomo-pkg: binary not prepared yet (waiting-binary)"). The `prepare` process
that downloads the binary is planned by mihomo-pkg from that same revision, so the binary could never arrive: a
deadlock, and every other change to the node was blocked with it.

- When an enabled feature's provider slot is unresolved and a registered binary-provider package declares that slot
  with `requiresPreparation` (download or manual file, mihomo-pkg), only that feature is held back, with the warning
  `services.<key>: waiting for its binary (<provider state>); applied once it is prepared`. The rest of the
  revision applies, including the preparer, and the revision becomes the last good one — the same shape as a
  missing package (D20) or a node-held key not generated yet (D28).
- When the preparer registers the binary it asks for a registry rescan; a newly resolved provider counts as a change
  and re-applies the last good revision, which now plans the feature. This also covers manual mode (the operator
  copies the file and triggers "register").
- No silent fallback: while the prepared provider is not ready the feature does not use a system binary (§3.1); the
  feature status keeps `provider-missing` with the provider's state as reason. A provider that is installed and
  needs no preparation but whose binary is missing is still an error that rejects the revision.

## D35 — Capabilities switched on by the service configuration (`capabilitySwitches`, §12.1)

Found in the lab with Mihomo: the feature list showed `mihomo.dns` enabled and healthy on lab-b although
`services.mihomo.dns.enabled` was false, because a package's capabilities were all enabled together with its service.
A health port `feature:mihomo.dns` would then have listened without a DNS listener behind it.

- A manifest may list `capabilitySwitches: [{capability, pointer, equals?}]`. `pointer` is a JSON pointer into
  `services.<short name>`; the capability is enabled only while the service is enabled and the value equals `equals`,
  or, without `equals`, is `true` / a non-empty string, array or object. A missing value means off, so a switch must
  point at a field whose schema default is off. Capabilities without a switch follow the service as before.
- A switched-off capability is reported `enabled: false, running: false, healthy: false` (so `feature:` health ports
  stay closed); `systemDependencies[].when` is evaluated against the switched set, both in the feature list and in
  the apply-time availability check.
- Installed capabilities (`has_capability`, used by validation of cross-package references such as a dnsmasq
  backend) are unchanged: they mean "an installed package provides it", not "it is switched on".
- Declared now: mihomo (`tproxy` ↔ `/mode` = `tproxy`, `dns` ↔ `/dns/enabled`, `subscriptions` ↔ `/subscriptions`),
  mihomo-openwrt (`dns` ↔ `/dnsIntegration`, `subscriptions`), frr (`anycast-ospf` ↔ `/anycast/enabled`), avahi
  (`mdns.bridge` ↔ `/bridge/enabled`).

## D36 — DNS front end findings from the lab: port owners, own processes, static providers, orphaned groups (§9.10, §11.2)

Found with `tools/lab/dns.sh` (dnsmasq front end + MosDNS + Mihomo DNS on lab-b, a Debian host with
systemd-resolved and Podman's aardvark-dns).

- **Port owner check is address-aware.** dnsmasq runs with `bind-interfaces` and binds only its listen addresses, so a
  socket on another address (127.0.0.53, a Podman bridge) does not collide. Validation reports an owner only for a
  socket on the same address or a wildcard of the family (`::` counts for both: dual stack), or for any address when
  dnsmasq itself would listen on a wildcard; UDP sockets and listening TCP sockets count. The message names the
  owner's address. Owners are still never killed (§9.10).
- **Own front end without CAP_SYS_PTRACE.** dnsmasq drops to its own user and becomes non-dumpable; planner (root
  without CAP_SYS_PTRACE, like D33) can list its `/proc/<pid>/fd` but not read the links, so its sockets cannot be
  attributed. When no visible process holds the colliding socket and a process of ours (`comm` dnsmasq, command line
  with the `meduza-np` marker) is among the unreadable ones, the socket is taken as ours. Granting planner
  CAP_SYS_PTRACE was rejected (too broad for an ownership check).
- **Static binary providers report their slot.** A `binary-provider` package whose provider needs no preparation
  (openvpn-pkg, mosdns-pkg) has no process or probe; its slot capability is running/healthy while the slot resolves to
  that package's binary. Prepared providers (mihomo-pkg) keep reporting their preparer process.
- **A leader that exits takes its process group along.** Supervised processes start in their own process group
  (§11.2); when the leader exits on its own (crash, SIGKILL, a wrapper dying) the kernel does not signal the rest
  of the group, and the restarted process ran next to the orphaned daemon (both MosDNS on 127.0.0.1:1153, the
  orphan even outlived disabling the feature). The supervisor now sends the group SIGTERM, then SIGKILL after the
  stop timeout, right after reaping the leader (a pid is not reused while a group of that id has members). This
  also covers adopted processes (D32). A daemon that forks into the background was already not supported.
- **Wrappers leave nothing behind when stopped.** The dnsmasq front-end wrapper removes its servers file, state file
  and dnsmasq's pid file (dnsmasq is killed and keeps it); the MosDNS wrapper removes its state file. A restart
  writes them again.
- **DNS loops are checked by the manager too.** `compilers/dns.ts` applies the plugins' loop rules per node at
  publish time (dnsmasq fallback/backends, MosDNS listener/upstreams/bootstrap, Mihomo DNS listener/nameservers/
  default nameservers against the front end's listeners; a wildcard listener is reached over loopback; DoT/DoH/QUIC
  upstreams cannot reach the plain listener). Before, a loop was only refused on the node: the revision failed
  there and the previous one kept running, while the operator saw a successful publish.
- Not changed, measured instead: with both backends hung, clients get no answers until the wrapper's fall
  hysteresis (2 probes, 3 s apart) restores the fallback (~7 s, 3 failed queries in the lab); a single hung or
  failing backend costs no queries because `strict-order` retries the next listed backend. Changing the backend set
  restarted the wrapper and with it dnsmasq (0 failed queries, ~1 s on the fallback while re-probing); since D37 it
  is a reload.

## D37 — The dnsmasq front end reloads its backend set instead of restarting (§9.10, §11.1)

- The wrapper spec (fallback servers, enabled backends, probe name, listeners) is a generation file
  `dnsmasq/main/frontend.json`; the wrapper gets `--spec-file ${GENERATION_ROOT}/...` (the stable `current` link)
  instead of the spec as an argument. The restart digest covers only `dnsmasq.conf` (port, listen addresses, cache,
  strategy, mDNS forward), so a changed backend set, fallback or probe name only changes files and planner sends the
  process its `reloadSignal` (SIGHUP), as for hosts changes.
- On SIGHUP the wrapper forwards the signal to dnsmasq (hosts) and re-reads the spec. A backend whose id and address
  are unchanged keeps its health state (stays listed, no gap); new or moved backends start unprobed and need the
  usual RISE good probes; removed backends drop out at once. A changed fallback is verified again. An unreadable or
  invalid spec file keeps the running spec (warning). The next probe round runs immediately and rewrites the
  servers-file + SIGHUPs dnsmasq when the upstream list changed.
- Measured in the lab (v0.6.6, lab-b, `dns.sh watch` at 0.2 s): both → MosDNS → Mihomo DNS → fallback only → both,
  dnsmasq kept its pid throughout, **0 failed queries** of 246. Switching to a backend set that shares no backend
  with the old one answers from the fallback until the new backend is probed healthy (~1 s); removing one of two
  backends has no gap.
- Upgrading from a wrapper that took `--spec` restarts the front end once (argv and digest change).

## D38 — OpenVPN Access sessions: lifetime, re-check on renegotiation, outage behaviour (§9.3, §7.5.2)

Found with `tools/lab/access.sh` (Access server on lab-a, OpenLDAP next to the manager, a remote client lab-vpn):
logins, refusals and routing worked, but the policy's session lifetime (`sessionSeconds`, returned as
`validForSeconds`) was ignored by the node, and a user removed from the directory or the policy kept the session for
the whole auth-token lifetime (8 h), because renegotiations were answered by OpenVPN's token check alone. AGENT §9.3
requires disconnecting disabled users.

- **Lifetime.** The Access server runs the helper also as `client-connect` (`openvpn-auth --instance <id> --connect`).
  Planner keeps each allowed auth decision in memory for 120 s, keyed by instance, username and the client's
  address/port (no password, no token); the connect hook takes it once and writes `session-timeout <validForSeconds>`
  into OpenVPN's dynamic client config. No matching decision → the connect hook fails → OpenVPN drops the client.
- **Re-check on renegotiation.** `auth-gen-token <lifetime> <renegSeconds> external-auth`: OpenVPN still verifies the
  token's HMAC and then calls the hook with `session_state`. `Authenticated` → planner asks the manager with
  `reauth: true` and no password (the token is never read past the username line or sent). The manager requires the
  user's last password login on that node and instance (`access_log`, reason `ok`; migration 5 indexes it), finds the
  account with the service bind (no user bind: there is no password), matches groups and policy again and answers
  `ok-reauth` with the remaining lifetime, or `user-disabled` (account gone), `not-authorized`, `no-session`,
  `session-expired`. `Expired` / `Invalid` / `*EmptyUser` are refused by the hook without asking. A user removed from
  the granting group therefore loses the session at the next renegotiation (≤ `renegSeconds`, minimum 300).
- **Outages.** New logins fail closed (unchanged). A token renegotiation that gets no decision (`manager-unavailable`,
  `timeout`, `rate-limited`, `ldap-unavailable`) is answered by planner as allow + `kept-unverified`: a manager or
  directory outage must not disconnect every VPN user at their next renegotiation, and the session still ends at its
  `session-timeout`. A new connection presenting a token (client restart) during the outage is refused by the connect
  hook (`unverified`), so "kept" never starts a session. The manager-unavailable gate is the shared §7.5 backoff
  gate on purpose (§7.5.2: Access obeys it and is denied fast rather than queued); after a 5-minute manager outage
  logins resumed ~63 s after the manager was back, bounded by the backoff window.
- **Feature list.** `capabilitySwitches` items may use `item` (an array of instances, on while one enabled element
  has the given fields): `openvpn.access` ↔ `/instances` holding `{"kind": "access"}`. Before, the feature showed
  enabled/running without any Access instance.
- Requires OpenVPN ≥ 2.6 on the server (`session-timeout` in client-connect configs, `external-auth`); openvpn-pkg
  ships 2.7.8, OpenWrt 24.10/25.12 ship 2.6. On UDP OpenVPN does not tell the client that `session-timeout` closed
  its instance; the client notices by its keepalive (`ping-restart` 60 s pushed), then reconnects with its token,
  which the manager refuses (`session-expired`): it has to log in with the password again. Measured in the lab with
  a 60 s policy: closed after 60 s, client noticed 54 s later, token refused, password login allowed again.
- The manager refuses an Access `renegSeconds` of 1–299 at publish (`compilers/index.ts`, the plugin's rule); before,
  only the node refused that revision and kept the previous one.
- Not done here (done in D39): immediate disconnect from the UI (management `client-kill` driven by the manager)
  and an online-sessions page; the node already reports connected clients (`openvpn-client` resources with username, virtual address,
  connected since).

## D39 — Access online sessions and operator disconnect (§9.3, §6.5)

D38 left "immediate disconnect from the UI" open. The Access page now lists online clients and ends a user's
sessions on demand.

- **List.** `GET /api/v1/access/sessions` (operator) reads the `openvpn-client` statuses of the nodes' latest
  reports, only for enabled Access instances of the node's desired configuration; each row says when it was
  reported. No new node API: reports already carry username, addresses, traffic and since; the plugin now also
  reports the management `clientId` (status v3 column 11).
- **Disconnect** (`POST /api/v1/access/sessions/disconnect {node, instance, username}`, operator, audited
  `access.session.disconnect`) does two things, in this order:
  1. an `access_log` row `decision = 'end', reason_code = 'disconnected'`. Re-authorization (D38) looks for the
     newest password login *or* end row of that user on that node and instance; an end row answers `disconnected`.
     So a token renegotiation or a token reconnect is refused even if the kill below is late or the node offline
     (then the session ends at its next renegotiation, ≤ `renegSeconds`). A new password login supersedes it.
  2. a node job `run-action` → plugin action `disconnect-client` (TTL 600 s; triggered by this route, not
     listed in the descriptor, like Mihomo `refresh-subscription`). The plugin opens the instance's management
     socket, takes the live `status 3` client list (not the status file, which lags by its write interval), and
     sends `client-kill <cid> HALT` for every client of the user (case-insensitive; CN when no username), so
     clients stop instead of retrying with their token. Nobody connected is a success with an empty list.
- The unit is user × node × instance, not one client: the manager cannot tell a user's token sessions apart (the
  token never reaches it), so killing only one would leave the others to be refused at their next renegotiation
  anyway. Disconnect is not a ban: the user can log in again with the password; keeping someone out stays the job
  of the LDAP group / policy (the UI says so).
- The management command input is only a number (`clientId` from OpenVPN itself); usernames never go into a
  management command.
- The list lags the kill by the status file interval (10 s) + probe + report (lab: gone 28 s after the kill, the
  kill itself took 0.33 s). Rows whose user was disconnected after the report are marked `disconnecting` instead
  of offering the button again.

## D40 — Redacted support bundle (§6.2 诊断, §12.3)

- One planner function (`support::bundle`, local API method `support.bundle`, read-only like `logs` and
  `config.current`) builds the export; the CLI (`meduza-np support-bundle [-o FILE]`, 0600, never overwrites),
  the dashboard (`GET /api/support-bundle`, download in the browser) and the manager `collect-diagnostics` job all
  return exactly that document (`kind: meduza-np-support-bundle`, `formatVersion: 1`), so support always gets the
  same shape whichever way it was taken.
- Contents: status report, doctor checks, packages/providers, supervised processes + applied instances,
  maintenance, the last applied configuration, a dry-run validation with the generated *file list* (paths, modes,
  sizes — no generated contents, which can carry credentials no key name gives away) and the last 500 log lines.
- Redaction runs over the whole document, not per section: string values under keys that contain password/
  passphrase/secret/token/psk/presharedKey/privateKey/apiKey/authorization/cookie/credential (normalized, so
  `bind_password`, `auth-token` match) unless the key only names or describes one (`…Ref`, `…Id`, `…File`, `…Path`,
  `…At`, …); `secrets.*.value` envelopes; inside every string, PEM private key and OpenVPN static key blocks,
  `Bearer`/`Basic` credentials, URL userinfo and `key=value` / `key: value` / `"key":"value"` pairs with such keys.
  Public keys, certificates, host names and addresses stay: they are what support needs. `config.current` now uses
  the same redaction (it previously only blanked `secrets.*.value`).
- Size: at most 192 KiB serialized, below the manager's 256 000 character job result limit. Over the limit the
  oldest logs are halved first (down to 20 lines), then the plan file list, the configuration body (its digest
  stays), the report; what was cut is listed in `truncated`. The manager now stores an oversized job result as
  `{truncated: true, bytes}` instead of cutting the JSON text, which used to leave an unparseable result.

## D41 — Container network mode and host-network capabilities (§14.3.3)

- Planner cannot tell a Podman bridge namespace from the host's on its own, so the deployment declares it:
  `run.sh` passes `MEDUZA_NP_NETWORK_MODE=<bridge|host>` to the planner container and the two Quadlet planner units
  set it (`Environment=`). Planner reads it only inside a container and reports it as `system.networkMode`.
- Manifests list `hostNetworkCapabilities` (a subset of `provides`, checked by manifest validation): capabilities
  whose purpose is the host's/LAN's traffic. Now: `frr.bgp`, `frr.bfd`, `frr.anycast-ospf`, `mihomo.tproxy`,
  `mdns.avahi`, `network.mappings`, `network.port-forwards`. Not listed: tunnel endpoints (WireGuard/OpenVPN/Tinc
  work through published ports), DNS front end (published :53), Mihomo proxy/DNS, the `.local` bridge (loopback).
- Bridge mode does not disable or fail those capabilities (traffic may be routed into the container on purpose,
  and §14.3.3 forbids marking everything unsupported). Instead an enabled one carries
  `limitationCode: "bridge-network"` + `limitation` in the feature status: still running/healthy, but shown as
  "container network only" (manager badge, dashboard 正常（受限）, CLI text), so "process started" never reads as
  "in effect for the host/LAN".
- Preflight: the manager compiles with the node's last reported mode and adds a *warning* (never blocks) per
  service whose enabled capabilities include host-network ones, and for active `networks.mappings` /
  `networks.portForwards`. The capability switch rule is shared with planner (`switchOn` mirrors
  `CapabilitySwitch::is_on`). Nodes that never reported, or report no mode (native installs, older deployments),
  get no warning.

## D42 — Bridge-mode node: lost namespace state, published ports, planner's own feature status (§14.3.3, §11.1)

Found with a bridge-mode node in the lab (`tools/lab/bridge.sh`, lab-br behind published ports, external
WireGuard peer lab-ext):

- **Lost owned resources are recreated.** Re-creating the planner container (run.sh again, image change, Quadlet
  restart) gives a fresh network namespace; the node volume keeps the ownership journal, so the engine diffed
  "nothing to add" and ran `wg syncconf` on a link that no longer existed (restore failed, tunnel down for good). A
  host-mode node after a reboot or a hand-made `ip link del` has the same gap. `NetBackend::exists` now answers for
  owned resources whose loss can be detected cheaply: links (`/sys/class/net`), owned nft tables (`nft list
  table`), routes (`ip route show table … exact …`) and fwmark rules (`ip rule show`); anything else is "cannot
  tell" and trusted as before (addresses and link settings are re-asserted on every apply anyway). In commit, an
  unchanged resource reported gone is added again in plan order and a failure fails the apply like a new resource.
  Present resources are never re-added, so nft tables are not replaced and policy rules are not deleted/re-added
  on every planner restart (verified on lab-a/lab-b: no false "recreating lost", 0 lost pings).
- **run.sh `--publish PORT/PROTO` publishes the same host port** (Podman alone picks a random one, so peers dialling
  the advertised endpoint never arrived). `HOST:PORT/PROTO` and `IP:HOST:PORT/PROTO` pass through.
- **Stale UDP conntrack after container re-creation.** A peer that keeps sending (WireGuard keepalive every 15 s)
  while the container is replaced leaves a conntrack entry created without the new DNAT; its own traffic keeps it
  alive, so the tunnel never returns although the new port mapping exists (Netavark 1.14 did not flush it in the
  lab). run.sh deletes the UDP conntrack entries of its published UDP ports after starting the planner when
  `conntrack` is installed and otherwise prints a note; the Quadlet unit carries a commented `ExecStartPost=`
  line. Measured: tunnel back 15 s after run.sh started (graceful stop + start + restore + handshake).
- **Planner's own features follow the node's `networks`.** Only `config.services` counted, so `network.mappings`,
  `network.port-forwards` and `network.health-ports` were never "enabled" and the D41 `bridge-network` limitation
  could not appear for them. The planner manifest now has `capabilitySwitches` (`/mappings`, `/portForwards`,
  `/healthPorts`, `item: {}` = on while one entry is not `enabled: false`) and `enabled_services` adds the node's
  `networks` under the key `planner` (`registry::PLANNER_SERVICE`). Features of the controller package are running
  and healthy while enabled: they are in place once their revision applied (a failure rejects the apply). The
  `nft` dependency `when: network.port-forwards` is now actually evaluated.
- **Process-less instances count as running** once applied while their probes are up (kernel WireGuard had
  `running: false` forever because "running" required at least one supervised process).

## D43 — Container capabilities in the report, FRR router id pre-check, origination status (§14.3.4, §9.5)

Found with FRR on the bridge-mode lab node (`tools/lab/bridge.sh bgp on`):

- **Capabilities are fixed at container creation.** run.sh / Quadlet add `SYS_ADMIN` only for the components the
  container was started with. When the configuration enables FRR later, D20 lets the updater install the package,
  but the plugin then refuses the configuration (`FRR daemons need CAP_SYS_ADMIN`) and with it the node's whole
  revision. Planner now reports its effective capabilities inside containers as `system.capabilities` (names
  without `CAP_`, from `CapEff`; absent on native installs, where planner runs with full root capabilities), and
  the manager warns before publishing an enabled service whose container lacks a capability it needs. The table
  of such needs is small and lives in the manager (`CONTAINER_CAPABILITIES`, now `frr: SYS_ADMIN`), mirroring
  run.sh; it is a warning, not an error, because the operator fixes it on the host (re-create the container with
  the component) and may do so before the node applies. Containers are not changed automatically: re-creating
  the planner container is a host operation outside planner/updater.
- **FRR without a router id is a pre-publish error.** §9.5 requires a router id on every routing node and the
  plugin refuses the revision without one, but the manager did not check it (mesh nodes in the lab always had
  one). `compileFrr` now reports `/nodes/<id>/routerId` (missing, or not an IPv4 address) whenever the node's
  configuration implies FRR. No default is derived: §9.5 says the router id is only an identifier and must be
  configured, and silently picking a tunnel address would change it when the tunnel is renumbered.
- **Origination status.** FRR runs with `bgp network import-check`, so a network is announced only while a
  matching route is in the node's own routing table. A LAN whose interface is down, or one outside a
  bridge-network container's namespace, was silently missing while every session was up (only `prefixesSent: 0`
  hinted at it). The probe now adds `frr/main/origination` (`bgp-origination`): the node's IPv4 `network`
  prefixes (LAN, private LAN, mappings, Access pools; Anycast VIPs excluded because the guard withdraws them on
  purpose) compared with the locally originated paths of `show bgp ipv4 unicast json` (`peerId` "(unspec)");
  `degraded` with `notOriginated` and a reason when any is missing, `up` otherwise. Verified both ways in the lab
  (bridge node: degraded, then up once the LAN was routed into the container; mesh nodes: up).
- Observed, unchanged: one invalid service rejects the node's whole revision, so a re-created container whose
  last good revision became invalid comes up with nothing applied (lab: tunnel down ~4 min until a fixed revision
  arrived). This follows the atomic revision rule of §11.1; the pre-publish checks above are what prevent it.

## D44 — Minimal planner container capability set (§14.3.4)

The planner container used to get `NET_ADMIN NET_RAW NET_BIND_SERVICE` (+ `SYS_ADMIN` with frr) on top of Podman's
default set, i.e. also `FSETID SETFCAP SETPCAP SYS_CHROOT`, which nothing was shown to need. run.sh and both Quadlet
units now drop everything (`--cap-drop ALL`, `DropCapability=ALL`) and add a fixed base set:

| Capability | Why |
| --- | --- |
| `NET_ADMIN NET_RAW NET_BIND_SERVICE` | links, routes, nft, TPROXY, raw sockets (BFD/OSPF), DNS on :53 |
| `CHOWN DAC_OVERRIDE FOWNER` | `apk add` of the component dependencies (package scripts create users and files), planner preparing files of daemons that run as their own users (FRR generation dirs, `/var/run/frr`) |
| `SETUID SETGID` | daemons dropping privileges: dnsmasq, dbus-daemon, FRR (`frr`), avahi-daemon |
| `KILL` | planner and the DNS front end signalling those non-root daemons (SIGHUP/SIGTERM across uids) |

Per component: `frr` adds `SYS_ADMIN` (unchanged), `avahi` adds `SYS_CHROOT`: avahi-daemon chroots itself by default
and exits in `cap_set_proc()` when the capability is missing. Unlike FRR's `SYS_ADMIN` this is not refused: the
avahi plugin starts avahi-daemon with `--no-chroot` (D45: not `enable-chroot=no`, which is no config key) when its effective set lacks `SYS_CHROOT` (only when that is known, i.e. a
readable `/proc/self/status`), so avahi installed later by the updater (D20) still runs, just without its own
chroot. The manager table of D43 therefore lists only `frr`. The capability test moved to the plugin SDK
(`has_effective_capability`, used by frr and avahi).

How the set was established: a probe container with only the base set installed every component's Alpine
dependencies and started dnsmasq (drops to its user, SIGHUP/SIGTERM from root), dbus, avahi (with and without
`SYS_CHROOT`) and zebra (with and without `SYS_ADMIN`); then every lab node was re-created with the new run.sh
(`tools/lab/caps.sh`) and all previously running instances came back: Tinc, WireGuard, OpenVPN incl. Access/LDAP,
FRR BGP/BFD/OSPF + Anycast, dnsmasq + MosDNS + Mihomo DNS (front-end failover and an off/on publish), Mihomo TPROXY,
bridge mode. Existing containers keep their old (larger) set until re-created; nothing depends on the difference.
`no-new-privileges` is not set on the planner: the daemons drop privileges themselves and gain none, but provider
binaries are not ours to vet for setuid use; the updater keeps it.

Observed, unchanged: system users created by `apk add` in a fresh container get uids in install order, so `frr` and
`dnsmasq` swapped uids (100/101) between re-created lab containers. Harmless today (planner re-chowns FRR's directories
on every apply and no daemon keeps uid-owned state in the node volume), but a future component that persists files
as its own user must chown them at start rather than rely on a stable uid.

## D45 — Avahi in the base container: own system bus, daemon flags, bridge timeout (§9.11, §14.3)

First live run of the avahi component (`tools/lab/mdns.sh`, lab-b in host network mode, host-b on its LAN running
its own avahi-daemon). Four defects, each of which alone kept mDNS from working in a container:

- **No system bus.** The container has no init; `apk-deps` installed `dbus` but nothing started it, so avahi's
  validation ("the system D-Bus is not running") refused every configuration. The planner role of the entrypoint now
  runs `dbus-daemon --system --nofork` in a restart loop while `meduza-np-avahi` is installed (rechecked every 15 s,
  so a package installed later by the updater is picked up; the first planner start waits up to 5 s for the socket).
  It is the container's own bus, never the host's: with host networking avahi-daemon still announces on the host's
  interfaces, and a host avahi-daemon is not visible to the plugin's conflict check (operators must not run both).
- **`enable-chroot=no` is no config key** (D44 assumed it was): avahi-daemon rejects the whole file ("Invalid
  configuration key", exit 255). The chroot switch is the command-line flag `--no-chroot`; the plugin adds it to the
  process argv when `SYS_CHROOT` is known to be missing. A test now checks every rendered key against avahi 0.8's
  `load_config_file` key list.
- **`rlimit-fsize=0` kills avahi-daemon** with SIGXFSZ: the limits are applied before the pid file is written.
  Dropped; the other rlimits (core 0, data/stack 8 MiB, nofile 768, nproc 3) stay.
- **Bridge timeout vs. Avahi's.** Avahi answers `TimeoutError` for a silent name only after ~5 s, later than the
  bridge's default 3 s budget, so every unknown name became a D-Bus read timeout ⇒ SERVFAIL, and the bridge probe
  (expects NXDOMAIN for `meduza-np-probe.local`) always reported `down`. A reply timeout on `ResolveHostName` (the
  connection, auth and Hello already succeeded) now means "not found" ⇒ NXDOMAIN (negative cache 5 s); failures to
  connect/authenticate stay SERVFAIL. Consequence: a frozen avahi-daemon shows as NXDOMAIN answers from the bridge,
  but `avahi/main` goes down (its probe needs `GetState` answered).

## D46 — Only a received revision shrinks the wanted components (D20)

Found in the updater log of lab-b during the D45 run (08:53–09:05 UTC, 2026-10-25): while the avahi defects made
`avahi/main` exit, the node installed and removed `meduza-np-avahi` every ~2 minutes. Cycle: rev-000045 (avahi on)
is received ⇒ wanted list +avahi ⇒ updater installs it (new Alpine deps ⇒ planner restart) ⇒ the restarted planner
restores last good rev-000044 ⇒ `prepare` rewrote the list from rev-000044 ⇒ −avahi ⇒ rev-000045 fails, restore
again ⇒ the updater removes the package ⇒ rescan, rev-000045 retried ⇒ +avahi … The same rewrite happened on every
`meduza-np validate`, `plan.preview` and support bundle, since they also call `prepare` on the last good config.

- `prepare` no longer writes `wanted-components.json`. `Engine::publish_wanted_components(raw, merge)` is called by
  the sync path for every newly received revision (`merge = false`, before `prepare`, so a rejected or failing
  revision still gets its packages — the dnsmasq `mdnsForward` rejection of rev-000045 before avahi existed was
  resolved exactly this way) and by `restore_last_good` with `merge = true` (adds, never removes).
- Consequence: a package is removed only when the operator publishes a revision without its feature (or drops it
  from `MEDUZA_NP_COMPONENTS`). A failing feature keeps its package installed until then, which is what the operator
  needs to fix it; nothing churns. A wanted list that only grew through restores (the persisted IPC volume keeps it
  across planner restarts) is corrected by the next received revision.
- `publish off` did remove the package (rev-000047 at 09:11, rev-000049 at 09:20); the D45 note that it stayed
  installed was a misreading.
- VERIFIED in the lab (2026-10-25, release v0.8.0 = planner 0.8.0, seq 433, lab-b only): off (rev-000053) removed
  the package in 30 s; rev-000054 with avahi `mode: "system"` and no system daemon (rejected by the plugin once it
  is installed) installed it in 30 s, then 12 minutes with no install/remove (installed generation stayed 63,
  wanted list unchanged, the planner restart from the new Alpine deps restored with merge); good on (rev-000055),
  off (rev-000056, removed in 40 s), on (rev-000057, `host-b.local` → 10.62.0.50).
- Seen in that run, left as is: a revision whose feature waits for its package (D20) is applied without the
  feature and recorded as last good; once the plugin is installed and rejects the feature, the restore of that
  last good fails too (`last good configuration … no longer valid here`, stage `degraded: invalid-config`), while
  the running state of the other features is kept and the received revision is reported rejected. Correct as a
  signal; the open question is whether such a "pending-package" revision should become last good at all
  (it would need the previous last good to be kept until every pending feature has validated). Answered by D47.

## D47 — Last good with held-back features: restore without them when they are rejected later (D20, D46)

The open question of D46. A revision that applies while enabled features are held back (package not installed,
D20; binary still being prepared, D34; node key not generated yet, D28) still becomes last good, with the held-back
service keys recorded next to it (`heldBack` in the last good file, `Prepared.held_back`).

- Keeping the previous last good instead was rejected: the held-back feature is applied exactly by re-applying the
  last good once its package/binary/key arrives (rescan, `reapply_after_key`); the received revision is already
  marked applied and is not retried, so the feature would never come up. It would also roll the node's other
  features back to an older revision on every planner restart.
- `Engine::prepare_last_good` (used by `restore_last_good`): if the last good is rejected and it has held-back
  features, it is prepared again with exactly those services removed — the state that was applied and good. That
  commit keeps the saved last good unchanged (`keep_last_good`, so the stored revision/digest stay the ones received
  and the next restore tries the feature again, e.g. after a provider switch). The node reports
  `applyStage: "degraded: held-back-rejected"` and `lastError` names the features and the plugin's rejection;
  the operator fixes it by publishing a corrected revision. A rejection of a last good without held-back features,
  or one that persists without them, is reported as before (`degraded: <code>`).
- `meduza-np validate`, `plan.preview` and the support bundle still prepare the full last good, so they show the
  rejection itself.
- Tested in `tests/engine.rs` (`restore_drops_held_back_feature_rejected_later`).
- `lastError` (and the restore log lines) carry the plugin's validation details, like the ack of a rejected
  received revision: `Rejection::summary()` = message + `path: message; …`. Before, the restore reported only
  `services.avahi is invalid`.
- VERIFIED in the lab (2026-10-25, lab-b, planner-only releases v0.8.1 seq 434 and v0.8.2 seq 435): repeating the
  D46 publish (avahi mode `system`, no system daemon) → package installed once, then `degraded: held-back-rejected`
  within seconds, `lastError` = `avahi: services.avahi is invalid: /services/avahi/mode: no system avahi-daemon is
  running; …` on the node and in the manager node view; applied revision is the received one, every other
  instance up, mesh ping to lab-a ok; 8 min with no install/remove churn (generation constant); a planner restart
  comes back in the same degraded state; the next good publish clears stage and error and brings avahi up.

## D48 — Avahi selected at container creation; no sample static services in the image (§9.11, §14.3.4)

Lab run of the case D44/D45 left open: lab-b's `node.env` now lists `avahi` in `MEDUZA_NP_COMPONENTS`, and
`tools/lab/caps.sh recreate lab-b` re-created the containers with run.sh, which adds `SYS_CHROOT` for that
component. Result: the plugin leaves out `--no-chroot`, avahi-daemon logs "Successfully called chroot()" and runs as
`avahi` (uid 86, no capabilities) with its chroot helper. All 32 instances were up again 67 s after re-creation;
`.local` resolution (host-b → dnsmasq front end → bridge → D-Bus → avahi) and `lab-b.local` from host-b worked; it
also worked after killing avahi-daemon (supervisor restart), after killing dbus-daemon (avahi exits 255 when it loses
the bus, the entrypoint loop restarts the bus and the supervisor restarts avahi) and after `podman restart` of the
planner container.

Defect found: avahi-daemon always loads the static service files in `/etc/avahi/services` (`/services` in its
chroot), whatever the publish settings say (`disable-user-service-publishing` only covers D-Bus clients). Alpine's
avahi package ships `ssh.service` and `sftp-ssh.service`, so a planner container with the avahi component announced
SSH and SFTP for the node on the host's LAN (host network mode) although `publish.services` was off and nobody had
configured them. This also happened in the D45 runs with `--no-chroot`.

- Fix in the base container: `prepare_deps` removes exactly those two files after installing the Alpine
  dependencies (`drop_sample_services`). They belong to the image, not to the operator; the container rootfs is
  rebuilt with every re-creation and `prepare_deps` runs again at every start and update takeover, so they cannot
  come back with a reinstalled package.
- Not removed: anything else an operator might mount into `/etc/avahi/services`, and on native Linux managed mode
  the system's `/etc/avahi/services` (that directory is the administrator's).
- VERIFIED in the lab (rebuilt base image, lab-b re-created): avahi logs "No service file found in
  /etc/avahi/services"; after flushing host-b's avahi cache it no longer lists SSH/SFTP for lab-b, and `lab-b.local`
  still resolves to 10.62.0.12.
- `tools/lab/mdns.sh state` read `version`/`hostName` from a non-existent `detail` object (always null); fixed, and it
  now lists the static service files in the container.

Still not verified: reflector, IPv6, OpenWrt. (The blank-volume node with avahi selected: D49.)

## D49 — Blank-volume node with avahi selected; CLI output to a closed pipe (§9.11, §12.3, §14.3.4)

Lab run of the last open D48 case (`tools/lab/mdns-fresh.sh`, 2026-10-25): a new LXC `lab-av` on lab-b's LAN (eth0
10.50.0.16 on mnpwan, eth1 10.62.0.21 on lanb, host network mode), D48 base image, offline site v0.8.2, every
meduza-np volume removed before `run.sh`, node.env `MEDUZA_NP_COMPONENTS=avahi,cli`.

- Bootstrap: the updater installed generation 1 = planner, updater, cli, avahi from the offline import 3 s after
  `run.sh`, before the node had any revision; the planner container had `SYS_CHROOT` (run.sh adds it for the
  component, D44), `prepare_deps` installed dbus + avahi and the entrypoint started the container's system bus
  before the first planner start (D45). The node enrolled with the env token; until a revision names lab-av it runs
  no avahi instance (the package only provides the feature).
- `publish on` (rev-000064: `services.avahi = { mode: managed, allowInterfaces: [eth1], bridge.enabled }`): all four
  avahi instances up 1 s after the publish; avahi-daemon chrooted ("Successfully called chroot()", uid 86, no
  capabilities, chroot helper), "No service file found" (D48 fix in effect, host-b lists no lab-av services);
  host-b `avahi-resolve -n lab-av.local` → 10.62.0.21; lab-av's bridge → `host-b.local` 10.62.0.50, `lab-b.local`
  10.62.0.12 (two managed nodes on one LAN), `nosuch-host.local` NXDOMAIN.
- `publish off` (rev-000065, lab-av removed from the document): applied with no instances, avahi-daemon stopped,
  the package stays installed (it is selected in node.env, D46), the bus loop keeps running (harmless, no clients).
- No product defect in the avahi path. Found on the way: `meduza-np … | reader` panicked ("failed printing to
  stdout: Broken pipe", abort) when the reader exited first (`| jq` with jq missing in the container, `| head`).
  The CLI now writes through `out!`/`emit`, which end the process with status 0 on `BrokenPipe` (other write errors
  exit 1); checked on Linux with `meduza-np help | true` (old binary: Aborted; new: exit 0).
- Lab note: the 13th LXC hit the host's inotify limit (`fs.inotify.max_user_instances` was 128 at runtime although
  its sysctl.d file sets 1024): systemd-networkd in the new container failed with EMFILE and left the links
  unconfigured. `sysctl -w` to the configured 1024 fixed it.

## D50 — Avahi reflector and IPv6 in the lab: own daemon, multi-interface bridge, own name (§9.11, D10, D32)

Lab run of the reflector/IPv6 case D45 left open (`tools/lab/mdns-reflect.sh`, 2026-10-25): lab-b (host network)
runs managed avahi on eth1 (lanb, host-b's LAN) and eth0 (mnpwan); lab-mgr on mnpwan runs no avahi and queries with
a raw mDNS probe (python3, bound to 5353, group joined on eth0). Three defects, all fixed:

- **Every change to a running managed configuration was rejected.** `foreign_daemon` took the daemon behind
  `/run/avahi-daemon/pid` for a system daemon unless its command line contained `/meduza-np/`; avahi-daemon rewrites
  its process title to `avahi-daemon: running [<host>]` right after start, so planner's own daemon was "a system
  avahi-daemon is already running" and rev-000066..69 (even a plain interface change) were rejected. Earlier runs
  never noticed: they only switched avahi on from nothing and off again. Ownership is now proven by a supervisor
  record (D32) in `<run_dir>/supervisor/*.json` with the same pid, kernel start time and boot id (no
  `/proc/<pid>/exe` needed: planner lacks CAP_SYS_PTRACE for the avahi uid); a reused pid or another boot never
  matches. The path check stays for the moment before the title is rewritten.
- **Bridge misses took one timeout per allowed interface.** The bridge asked Avahi on each allowed interface in turn
  (3 s budget each), so with two interfaces (the minimum for the reflector) an unknown name took ~6 s: past the
  plugin probe's `timeoutMs + 1 s` (`avahi/bridge` down: timeout) and nslookup's 5 s. The interfaces are now asked
  concurrently; the first address wins, a miss costs one budget.
- **A reflecting avahi-daemon (0.9-rc4) never answers ResolveHostName for its own name** (TimeoutError on every
  interface and on -1, while it answers the same name on the wire and LAN names resolve). With
  `reflector.enabled` + `publish.addresses` in managed mode the plan passes `--self-answer v4[,v6]` (the families the
  daemon announces); the bridge compares the name with `GetHostNameFqdn` (cached 10 s, so a conflict rename is
  followed) and answers from the local addresses of the allowed interfaces (interface order, link-local last); a
  family not announced is NODATA. Without the reflector Avahi is asked as before.

Results with v0.8.4 (seq 437): without the reflector lab-mgr sees only lab-b on eth0 (A and link-local AAAA), not
host-b; with it host-b's A arrives reflected from 10.50.0.12; with `reflectIpv` a query over IPv6 gets host-b's A
(IPv4 answer reflected into IPv6); `off` (eth1 only, IPv4) shows nothing on mnpwan. Bridge probe up in every step,
`lab-b.local` and `host-b.local` resolve through the DNS front end in every step. Not covered: host-b's AAAA was
never reflected (the lab has only link-local IPv6, which the probe never saw crossing), so reflection of global
IPv6 addresses is untested. Also: the sync log line for a rejected revision now prints `Rejection::summary()`
(it printed only "services.avahi is invalid" while the ack and `lastError` had the details).

## D51 — Avahi `system` mode on a native node: no bus activation, a missing daemon is a runtime state (§9.11)

Lab run of system mode (`tools/lab/mdns-system.sh`, 2026-10-25): lab-sys is a plain Debian 13 LXC on lab-b's LAN with
the distribution's avahi-daemon (systemd, bus-activatable) and Meduza NP installed natively per docs/operations.md
§2.2 (components `avahi,cli`, `meduza-np enroll --insecure` against the lab manager). The basic case worked at once
with v0.8.4: bridge on the system daemon resolves host-b, lab-b, its own name and NXDOMAIN for unknown names, host-b
resolves `lab-sys.local`, mode `managed` is rejected with the system daemon's pid. Two defects, both fixed:

- **Our D-Bus calls restarted a stopped system daemon.** Debian's `org.freedesktop.Avahi.service` names
  `SystemdService=dbus-org.freedesktop.Avahi.service`, so every probe or bridge call bus-activated avahi-daemon
  within seconds of `systemctl stop`: the probe never saw it down and planner silently overrode the operator. In
  managed mode on a systemd host the same activation could start a system daemon next to planner's own while that
  one restarts (exactly the competing process §9.11 forbids). Every method call now carries the
  `NO_AUTO_START` header flag (`dbus::NO_AUTO_START`); an absent daemon is `NameHasNoOwner`.
- **System mode without a running daemon was a validation error.** At boot planner may start before avahi-daemon,
  and an operator may restart it: planner restart while the daemon was down ⇒ "last good configuration rev-000081 no
  longer valid here", the bridge stopped, and nothing retried after the daemon came back (the revision stayed
  rejected). The missing daemon is now a warning on validate/plan and a probe state: `avahi/main` down with
  `no system avahi-daemon is running (dependency missing): …`, the bridge keeps running and answers SERVFAIL
  (bridge probe down) until the daemon answers, then everything is up without a publish. Managed mode next to a
  system daemon stays a validation error (unsafe coexistence, §14 preflight). D47's lab check used this rejection
  as its trigger; that behaviour is now covered by `tests/engine.rs` only.
- The planner systemd unit is ordered `After=avahi-daemon.service` (ordering only; nothing is pulled in and a
  missing unit is ignored), so a normal boot validates against a running system daemon.

Results with v0.8.6 (seq 439, planner + avahi 0.8.6): daemon stopped ⇒ probe down in 7 s, started ⇒ all up in
29 s; planner restart with the daemon masked ⇒ rev-000081 stays applied, no `lastError`, bridge running, all up 25 s
after the daemon starts; reboot ⇒ avahi-daemon active before planner, rev-000081 applied, bridge resolves. Not
covered: OpenWrt's avahi (procd, no bus activation) in system mode.

## D52 — Dashboard sign-in against a live manager; refused redemptions are audited (§12.3, D15)

Lab run (`tools/lab/dashboard-login.sh`, 2026-10-25) of the D15 flow end to end: dashboard on lab-av (loopback
public URL, which the manager accepts as the only http return URL), planner relay over the node-authenticated manager API (plain http in the
lab), lab manager approval with software passkeys, `/auth/callback` exchange. Every rule held on the first run:
node and return URL shown on the approval page, single use, one attempt per login, deny is final, the login cookie
binds the callback to the browser that started it, the *current* role and disabled flag are read at exchange time,
5-minute expiry (in the lab the 300 s login cookie ends it first; the manager check stays unit-tested). Adding a component to `MEDUZA_NP_COMPONENTS` and re-creating the containers was enough for the
updater to install it from the current group (no release needed).

- Change: a failed redemption of a known login (`other-node`, `used`, `expired`, `not-approved`, `invalid-code`,
  `account-disabled`) is now recorded as `dashboard.login.refused` (actor `node:<id>`, target the redeeming node,
  detail `{loginId, reason, approvedBy}`). Before, only successes were audited, so a replayed or guessed code — the
  events an operator most needs to see — left no trace. Unknown login ids are not recorded (no row to attribute;
  the node API rate limit bounds them). `dashboard.login.deny` now names the node.
- Lab environment note: many nested containers exhausted the kernel key quota of the LXC root uid
  (`kernel.keys.maxkeys=200`, crun: `create keyring … Disk quota exceeded`); raised at runtime on the VPS. Hosts that
  run many rootless or nested Podman containers may need the same (not a product change).

## D53 — Base image replacement by a restricted host helper (§13.5, §14.3.2)

- `packages/meduza-np-base-container/deploy/podman/base-image.sh` (POSIX sh, runs on the host as the container owner)
  is the "受限宿主 helper": `status`, `plan`, `replace`, `rollback`, `history`. Package updates stay inside the
  containers; the helper is only for a new base image and never exposes the Podman socket to node services.
- Target = signed metadata only. The running updater answers `meduza-np-updater base-image` (via the new entrypoint
  role `updater-cli`, which runs the store updater or the baseline): verified channel group (or `--group`, i.e.
  `MEDUZA_NP_GROUP_VERSION`), `meduza-np-base-container` `oci` artifact for its platform → index reference,
  platform manifest digest, the group's updater version. The helper accepts only
  `<repo>/meduza-np-base-container@sha256:<hex>`; a hand-typed reference is not an input. `rollback` uses only the
  image recorded as running before the last successful replace (`base-image.previous`).
- Pull by digest, then the local image must match the platform digest (`.Digest` or `RepoDigests`), carry the title
  label and embed the group's updater version (label).
- Compatibility = the candidate image's baseline updater: `store-check` runs in a throwaway container
  (`--network none --cap-drop ALL`, store volume read-only) and exits 3 on a problem: installed set / journal format
  (`INSTALLED_FORMAT_VERSION`, `JOURNAL_FORMAT_VERSION` = 1) it cannot read, wrong backend or platform, installed set
  without current view, or a journal in a non-terminal phase (an update half done). The baseline matters because it
  is the entrypoint's fallback and recovery after the switch; the same check guards the restore and `rollback` to an
  older image ("基础镜像回滚也必须验证包和状态格式兼容"). Planner state formats are versioned and migrated by the
  planner package itself (store volume), which a base image change does not touch.
- Re-creation: Quadlet (rewrite both `Image=` lines, keep `.prev` copies, `daemon-reload`, restart) or `run.sh`
  (`run.sh` now records its options in `<state>/run.args` and the image in `run.image`; `--again --image REF`
  repeats them). Success = both containers on the new image id and `podman healthcheck run` passing for both within
  `--timeout`. Otherwise the previous image is store-checked and restored (units from the `.prev` copies), and the
  result is recorded; if the previous image refuses the store the node stays on the new image for repair.
- History is a TSV file (time, action, result, reference, image id, group, `pkg@version` list from the store check),
  readable without jq. A `mkdir` lock prevents two runs at once. Images are not pruned so rollback works offline.
- Quadlet mode refuses to act when `meduza-np-updater.service`/`meduza-np-planner.service` do not resolve to the
  generated units (`FragmentPath` under `…/generator/`): a native install's units of the same name in
  `/etc/systemd/system` take precedence, and restarting them would start the host updater instead (found on the VPS).
- Not covered: an update the updater starts between the store check and the re-creation is interrupted and finished
  or rolled back by the new updater's journal recovery (D12), not prevented. No timer is shipped: replacing the base
  image stays an operator action.
- Verified 2026-10-25 on the VPS (Debian 13, rootful Podman 5.4) with `tools/base-image-e2e.sh` (also a CI step of
  `container-e2e`): three 3-platform images in a local registry pinned by three signed groups; plan (no-op on the
  same image, target from the group, nothing changed), refusal while the journal says `switched`, run.sh replace
  A→B (installed set kept, planner restarted, history + previous), B→C with a failing planner health check restored
  to B after the 45 s timeout (previous record kept), rollback to A, a tampered group refused by the updater
  (`digest-mismatch`), and with Quadlet units replace via `--group v0.2.0` (the channel's sequence 2 was refused as a
  rollback first: anti-rollback holds for the helper too) and rollback with the units' `Image=` restored.

## D54 — Avahi `system` mode on OpenWrt: procd's daemon needs no special case (§9.11, D51)

Lab run (`tools/lab/mdns-openwrt.sh`, 2026-10-25) on an OpenWrt 25.12 LXC with the feed's `avahi-dbus-daemon`
(0.9-rc5, procd, `avahi-daemon -s`, no D-Bus service activation file) and Meduza NP 0.8.7 apk packages. Everything
D51 established on Debian holds unchanged, so there is no OpenWrt-specific code path:

- Detection of the system daemon reads `/var/run/avahi-daemon/pid` (`/var/run` = `/tmp/run`); avahi writes it even
  with OpenWrt's default `rlimit-fsize=0`, so `managed` is refused next to it. The bus socket is
  `/var/run/dbus/system_bus_socket` (already in `system_bus_path()`).
- Boot order needs no change: rc.d gives `S60dbus S61avahi-daemon S95meduza-np-planner`; a start of planner before
  the daemon finishes is the D51 runtime state (warning + probe reason), not a rejection.
- A stopped daemon stays stopped (no bus activation on OpenWrt; `NO_AUTO_START` is harmless there).
- Operator note, not a product change: `avahi-dbus-daemon` installed while dbus runs fails to own
  `org.freedesktop.Avahi` until `/etc/init.d/dbus restart` (docs/operations.md §2.3). Planner does not restart dbus
  for the operator: dbus is a system service planner does not own.
- Planner logs went to stderr only, which procd records as `daemon.err`; superseded by D55.

## D55 — Planner log sink follows the service manager (§16, D54)

- No logging framework still (§16); `log.rs` picks a sink once at start. `MEDUZA_NP_LOG_TARGET=split` (set by the
  procd init script) writes INFO/DEBUG to stdout and WARN/ERROR to stderr: procd's `stdout 1`/`stderr 1` log them as
  `daemon.info` / `daemon.err`, so `logread` no longer shows every line as an error. Two streams cannot express four
  levels; warnings go with errors.
- Under systemd nothing is configured: when `JOURNAL_STREAM` names fd 2 (device:inode compared, an inherited variable
  with a redirected stderr does not count) every line gets the sd-daemon `<3>`/`<4>`/`<6>`/`<7>` prefix, which
  journald strips and stores as the priority (`journalctl -p warning` works). `stderr`/`journal` force a sink.
- Podman and a shell keep plain stderr (unchanged). Supervised daemons' forwarded `<id>| line` output stays on stderr
  without a prefix (journal priority info by default). The in-memory ring (support bundle, `logs`) never has prefixes.
- Log writes ignore write errors instead of `eprintln!` panicking on a closed pipe.
- Checked: unit tests of the sink choice and routing, Windows clippy, x86_64-musl build. Observed in the lab with
  `tools/lab/log-sinks.sh` (2026-10-25): procd logread on lab-ow (INFO `daemon.info`, WARN `daemon.err`) and journald
  on lab-sys (PRIORITY 6/4/3, prefix stripped, `-p warning` filters, redirected stderr stays plain).

## D56 — A SIGTERM-interrupted socket read is a stop, not an error (avahi bridge)

- Found by the D55 lab run: the avahi bridge exited 1 with `Interrupted system call` when the planner stopped it.
  Linux never restarts `recvfrom` on a socket with `SO_RCVTIMEO` after a signal handler ran, even with SA_RESTART,
  so the handler's EINTR reached the loop. `ErrorKind::Interrupted` now joins WouldBlock/TimedOut: the loop re-checks
  the stop flag and exits 0.
- The other helpers with a SIGTERM handler (dnsmasq frontend, mosdns rules, mihomo runners, frr) wait with
  `thread::sleep`, which resumes after EINTR; no change there. New helpers that block in a timed socket read under a
  handler must treat Interrupted the same way.

## D57 — Rootless bridge mode: local-origin port forwards, graceful container stop, user-unit boot start (§14.3.3, §14.3.4)

First rootless run (`tools/lab/rootless.sh`, lab-rl: user `mnp`, Podman 5.4.2, netavark bridge + pasta). WireGuard
through a published port worked unchanged; three defects and two host prerequisites were found:

- **Port forwards in the nat output hook.** Rootless Podman forwards published ports in user space: rootlessport-child
  dials the container's own address from inside its namespace, so the connection is locally originated and never
  passes prerouting, where the forward's DNAT sat (lab: connection accepted, no answer). netplan now renders each
  forward a second time in an `output` chain (`type nat hook output priority dstnat`), restricted to
  `fib daddr type local` and excluding loopback (`ip daddr != 127.0.0.0/8` / `ip6 daddr != ::1`; DNAT from a loopback
  source cannot leave the host, and localhost clients keep "connection refused"). Same sourceCidrs/listenAddress
  matches as prerouting. Side effect accepted for every mode: a process on the node that connects to one of the
  node's own addresses on a forwarded port is forwarded like an external client (the port belongs to the forward
  anyway). Not rendered per mode: planner cannot tell rootless from rootful (a nested rootful container's uid map
  looks the same), and the rule is inert where Podman uses kernel DNAT.
- **Client source addresses are not preserved rootless.** Peers and forwarded clients appear with the container's
  own address, so `sourceCidrs` cannot match real clients and WireGuard endpoints show `10.89.0.x`. Documented in
  the capability matrix; no workaround in the product (pasta networking would keep sources, but it is not the
  bridge mode §14.3.3 asks for).
- **Graceful stop.** tini (PID 1) forwards SIGTERM only to the entrypoint shell. Without a trap the shell exited at
  once (status 143), the container ended and planner was SIGKILLed with it: no `stop_all`, no clean daemon stop, and
  Quadlet units ended `Failed` on every `systemctl stop`. This hit rootful and run.sh deployments too (the earlier
  "graceful" claims were wrong). The entrypoint now runs the role process as a child, forwards TERM/INT to it, waits
  (repeating `wait` after the trap interrupts it), and exits with its status; death by the forwarded SIGTERM (the
  updater has no handler) counts as the requested stop, exit 0. The takeover status 75 path is unchanged. Wait loops
  check the stop flag. Needs a new base image (package updates do not change the entrypoint).
- **Boot start of run.sh containers.** run.sh enabled the *system* `podman-restart.service`, which restarts only
  root's containers. As non-root it now enables the user unit (`systemctl --user`) and warns when lingering is off;
  the conntrack cleanup after re-creation is skipped rootless (no host DNAT, no stale entry, no permission).
- **Host prerequisite: network-online.target.** Rootless Quadlet units get `podman-user-wait-network-online.service`,
  which waits up to 90 s for the system `network-online.target`; a user unit cannot pull it in, so on a host where no
  system unit wants it every start/restart waited 90 s (lab: 91 s ⇒ 2 s after `systemctl add-wants
  multi-user.target network-online.target`). Documented for the base container and the manager (its recommended
  deployment is rootless Quadlet); not changeable from the units.
- Observed, unchanged: rootless host network mode cannot touch the host's network (link/route/nft EPERM, read-only
  `/proc/sys`), so host mode stays rootful-only. After a planner restart an idle WireGuard peer that only sends
  keepalives keeps its old session until its rekey (~120 s) because the node only responds (no peer endpoint);
  with data flowing the peer re-handshakes in ~10 s. Same rootful.

## D58 — Import of the previous planner's etcd export; cluster-wide DNS hosts, Tinc, MosDNS and BGP filters

The previous service planner kept its configuration in etcd; its export (`meduza-etcd-export-v1`, a flat key/value
dump with secrets replaced by `[REDACTED]`) is imported from the Import page (admins). The manager converts it and
shows what it found before anything is written; staging creates the missing nodes (they enroll later with their own
tokens), stores the supplied secrets, asks the nodes to generate their Tinc keys (D28) and puts the document into the
account's saved changes. Nothing is published by the import: it is reviewed and applied like any other change.

- **Taken over:** `/dns/hosts`, `/global/bgp/filter` and `max_paths`, `/global/clash/subscriptions` (as the D29
  Mihomo list), `/global/mosdns`, `/global/tinc`, and `/nodes/<id>/*` except `ospf` and `easytier`. Not taken over:
  `/update`/`/updated` (status the old nodes uploaded), `/commit`, OSPF, EasyTier, `internal_routing_system` and
  `mesh_type` (fixed here: BGP over one Tinc mesh), `edge_broadcast`. The page lists every skipped key family.
- **Redacted values.** Each is a "value to supply" (OpenVPN static keys and TLS material, WireGuard private keys,
  subscription URLs) or, for WireGuard peer public keys, a document field. A tunnel whose value is not supplied is
  imported switched off; disabled objects no longer pull their secret references into the node's snapshot (the
  plugins skip them already). Tinc private keys are never asked for: new ones are generated on the nodes. Mihomo API
  secrets are generated. Values are checked before anything is written.
- **New cluster-wide sections** (Global settings), all merged under the node's own values: `dns.hosts` (a node's
  entry for the same hostname wins), `tinc.cipher`/`digest` (mesh-wide defaults; the export's `SHA512` is stored
  lowercase as the plugin spells it), `mosdns` (the shared MosDNS configuration of every node that enables it,
  including a custom `pluginsYaml` chain whose `/etc/mosdns/<file>` references resolve to the domain lists and rule
  files), and `routing.bgpFilter.in`/`out` (prefix rules for every eBGP session without its own filter; rendered as
  an inverted prefix list in a `deny` route-map entry, since a route-map entry matches one prefix list).
- **Node Mihomo refresh interval** may override the cluster one (the export had per-node intervals).
- OpenVPN gained the legacy framing the export uses: `comp-lzo no`, `comp-lzo yes` + `allow-compression asym`, and
  `pull` on p2p TLS clients (`p2pPull`).
- A roaming member whose static members still wait for their generated Tinc key gets a warning, not an error.

## D59 — TPROXY: connections that start at the LAN, routed destinations stay routed, side-router mode (§9.7)

The previous planner's TPROXY settings map onto `services.mihomo.tproxy`: `tproxy_protocol` → `protocols`
(tcp/udp/both), `exclude_tproxy_port` → `excludePorts` (destination ports, TCP and UDP), `exclude_rfc1918` →
`excludePrivateDestinations`, `use_conntrack` → `useConntrack`. The rule set was reworked around three requirements:

- **Only connections that start at this node's LAN are proxied.** Sources are the node's LAN (`sources`); a flow is
  handed to Mihomo on its first packet only (TCP: a SYN). With `useConntrack` (default; needs nf_conntrack) the
  decision is stored in the connection's ct mark and later packets go straight to Mihomo; reply-direction packets
  are never proxied. Without conntrack every packet is classified again and established proxied TCP connections are
  found with `socket transparent` (needs nft_socket); validation checks the module the chosen mode needs.
- **The routing system keeps its destinations** (`bypassRouted`, default on): a destination whose route leaves
  through a mesh, VPN or Access device (`fib daddr oifname`) — routes from BGP, OSPF, Tinc or a tunnel — is not
  proxied, besides traffic arriving on those devices (already excluded). The automatic device list now also covers
  the single-object Tinc mesh of D22 (`tnc-mesh` by default), which it missed before.
- **Side router** (`sideRouter.gateway`, the main router's LAN address): clients get this node as their gateway from
  the main router's DHCP, the main router keeps the WAN and its port forwards. A forward (WAN → lan_a:9009) reaches
  lan_a directly and lan_a answers through this node. TCP answers are recognized because this node never saw their
  connection start (not a SYN, no proxied connection); UDP answers only by the forwarded service ports
  (`sideRouter.replyPorts`, source ports). Answers are marked (fwmark+2) and routed by an own table (routeTable+1,
  `default via gateway`), so they leave towards the main router whatever BGP/OSPF installed in the main table. ICMP
  redirects from the node are dropped (a redirect would teach LAN hosts to skip the side router).

Not covered: UDP answers of forwarded services without `replyPorts` look like new flows from the LAN and are
proxied. The OpenWrt adapter (OpenClash) keeps OpenClash's own TPROXY rules.

## D60 — Applying external connections does not wait for a reachable peer

An unavailable external peer must not fail a configuration revision or roll back unrelated DNS, routing or mesh
services. Generation plans explicitly mark external connection processes with `startup: connection` and
`restart: always`; planner does not infer this from process names. After files, provider resolution, required
checks and local network operations succeed, a successful exec is enough to commit such a connection. Local spawn
errors still fail apply. Normal local daemons retain stable-start verification and the five-quick-failure limit.
The default startup field is omitted from serialized plans and process identities, preserving unchanged legacy
daemon hashes/adoption.

OpenVPN Site2Site instances opt in in both client and server roles; Access listeners retain stable-start checks.
Client DNS resolution retries indefinitely and native connection waits grow up to 60 seconds. If a connection exits (including
exit 0), supervisor retries indefinitely with exponential waits capped at 60 seconds and random jitter. A
connection must run at least 60 seconds before its supervisor backoff resets. Runtime failure/backoff, exit reason,
restart counts and actual tunnel/session health remain visible in node reports; applied does not mean connected.
No manual restart or republish is needed when the peer returns. Native `--config … --show-ciphers` parses staged
OpenVPN options without contacting the peer or opening a TUN device; it is a syntax check, not a full certificate
or connection test. Static-key instances instead run a required `--test-crypto` self-test without a peer or TUN,
checking crypto options and the staged key path (overridden in argv while `current` still names the old
generation). This does not fully validate supplied key contents on every provider. Typed validation continues
to check configuration and secret references.
Static-key generation uses `cipher` (TLS uses `data-ciphers-fallback`) and opts into the explicitly configured
legacy mode with `allow-deprecated-insecure-static-crypto`, required by OpenVPN 2.7. A narrowly scoped
`ignore-unknown-option` keeps that same configuration usable with earlier providers; TLS configurations never
carry this compatibility opt-in. See the [OpenVPN manual](https://openvpn.net/community-docs/community-articles/openvpn-2-7-manual/).

WireGuard applies local interface/configuration independently of handshakes; D67 also separates endpoint DNS
resolution from required local reloads. Kernel handshakes retry
when traffic or a configured persistent keepalive needs the peer. Missing/stale handshakes stay in peer health,
while failed local link/configuration operations still reject apply. Tinc and BGP retain their native reconnect
loops; peer health is not an application gate.

## D61 — Default front-end listeners and observable binary acquisition

At the user's request, omitted or empty dnsmasq `listenAddresses` means all available local interfaces. Native
configuration omits `listen-address`; probes and conflict/loop checks use effective IPv4/IPv6 wildcard listeners.
Explicit addresses still restrict listening. The Manager schema/compiler and node validation use the same
default, so a legacy empty list cannot reject a whole revision or its last-good restore. Existing port owners
and local DNS loops are still rejected; fallback and dynamic upstream health behavior are unchanged.

Mihomo adapter installation and Mihomo binary acquisition are separate operations. The provider reports
`downloadProgress` independently of the usable `provider.json`: target version/platform, real compressed-file
bytes, phase, attempt, retry time, error and observation time. A short-lived thread samples only while acquiring
the binary. Kernel file locks exclude duplicate transfer writers and identify a terminated preparer, including
one stopped during retry backoff, without a stale PID or misleading live status. Failed preparation keeps the previous usable provider. File bytes
reaching 100% does not mean checksum, decompression, executable inspection or activation finished. Final progress
logging cannot turn successful activation into a failed installation.

Manager and Dashboard show a dedicated binary card, including configuration failure/interruption reasons, rather
than treating package-group progress as binary progress. The existing Download now action restarts the supervised
preparer through the planner; it does not block a plugin job for a long download or launch another controller.
The preparer reports its bounded exponential retry schedule. Curl downloads also have connection and low-speed
timeouts, so a stalled transfer reaches automatic retry instead of waiting for the full artifact timeout.

## D62 ? Planner/updater mutual upgrades with updater-first takeover

When updater changes, resolve/download/preflight still cover the complete signed target subset. Before planner
or services change, planner handles the authorized `updater.activate` local method and invokes a short-lived
activation helper. For store installs it executes the staged updater inside its validated object directory;
for native installs the installed updater helper uses the detected package manager. The helper reads the pending
journal, accepts no arbitrary command/path parameters, and activates only updater. It is bounded at 120 seconds;
timeout kills its process group, including a native installer. The daemon's independent launcher remains the
owner and starts the new updater after exit 75. Planner does not supervise an updater daemon.

The optional `handoff` journal checkpoint is persisted before activation. Its intermediate installed set changes
only updater, retains the previous group identity, and has its own store view; the final target is fully staged
separately. The new updater proves its running object (or native installed package), then immediately continues
the saved plan, acquires maintenance, switches/installs planner and remaining packages, and verifies them. It
never waits for a new scheduled check, refetches a moving channel, or marks the whole group done at takeover.
If new updater startup fails, the old set is restored without restarting planner. After successful takeover,
planner/package rollback retains the new updater and restores the previous planner and business packages.
Native rollback still depends on cached previous archives and is not atomic.

Old `self-restart` journals without a handoff keep their existing completion behavior. When planner is down or
its previous release lacks this method, the independent updater helper preserves bootstrap/repair capability.
The first transition initiated by an older updater necessarily uses that updater's existing ordering; subsequent
transactions use D62 once the new updater is installed. Group versions and package versions remain independent:
on 2026-11-03 the v0.1.7 signed manifest and remote process/install records confirmed both planner and updater were
intentionally reused from v0.1.6, rather than skipped during upgrade.

## D63 ? Manager-triggered node upgrades

The node detail's Packages and updates card exposes Check for updates and Upgrade now to operator/admin users.
Both use the existing authenticated, CSRF-protected, audited node job channel (`update-check` / `update-apply`)
and its push notification or roaming sync. Planner advertises `update.remoteControl` and relays only these two
fixed methods to the independent updater socket with a ten-second timeout and bounded response. No caller may
supply a package, instance, command, version or source override. Old planners are labelled unsupported. Pending
requests deduplicate by node and kind, conflicting pending requests reject, and undelivered requests expire in
ten minutes. A bounded persistent receipt cache on planner preserves accepted/error results across replacement
and HTTP-acknowledgement retries; socket errors and updater busy responses are shown instead of false success.

Explicit update.check is strictly read-only, including when automatic apply or component-selection changes would
otherwise authorize installation. Explicit update.apply retains the updater's existing manual semantics: use
the node's configured channel/pin, bypass pause/window once, and allow a failed group retry. It follows D62's
updater-first takeover whenever updater changes. A queued apply cannot be overwritten by a later check before
the worker starts. Request acknowledgement is displayed as acceptance for scheduling, never as installation
completion. The existing package progress remains authoritative for actual checks, downloads, verification,
activation, takeover and rollback. Viewer pages retain read-only request status and older-node progress.

## D64 ? Preserve the desired revision's failure when last-good recovery also degrades

On 2026-11-03, the authenticated configuration endpoint for xuzhou-b1 returned rev-000010 with dnsmasq
listenAddresses ["0.0.0.0"]. A temporary dnsmasq on port 55353 in its network namespace answered a test record
on both 127.0.0.1 and 10.42.10.12 with that explicit wildcard. The apparent empty-list rejection came from
restoring rev-000003, whose exact saved configuration still contains []; the node's dnsmasq is reused 0.1.6.
The primary rev-000010 failure is local OpenVPN preflight: lemon requests lzo-asym but the packaged provider
is built with compression disabled. Recovery dropped the rejected held-back services and overwrote lastError,
misattributing an old DNS validation error to the corrected current configuration.

Keep sync.lastError for the desired apply error and report optional sync.recoveryError separately, including
the historical revision. Include held-back validation, restore commit/prepare and unreadable last-good errors;
clear recoveryError on the next recovery or new application attempt, leaving the primary failure intact until
successful desired application. Manager shows a separate historical recovery warning/problem; Dashboard
lists it independently. The optional field is backward compatible and node-report schema is regenerated.
Cached configurations remain exact for digest verification: do not rewrite old snapshots as a diagnostic fix.
The native compression incompatibility remains an actual local blocker; no production peer configuration or
provider binary was changed during this diagnosis.

## D65 — OpenVPN provider includes compression libraries

At the user's request, openvpn-pkg statically links pinned LZO 2.10 and LZ4 1.10.0 alongside its existing OpenSSL
and libcap-ng inputs. Explicit --enable-lzo/--enable-lz4 replace the old disable flags; the static ELF gates and
management/script-hook support remain. No runtime system compression package or base image change is required.
LZO's tarball was checked against the upstream-published SHA-1 and Alpine aports SHA-512 before pinning SHA-256.
LZ4's official v1.10.0 tag is pinned to a full verified commit. Both licenses ship with the provider, including
LZ4's nested lib/LICENSE. OpenVPN's official GitHub release tarball matches the existing signed-source SHA-256
and replaces an unreachable swupdate redirect. Source downloads use bounded HTTPS/HTTP1.1 retries.

Each target build checks both compile-time compression capabilities, runs actual LZO/LZ4 compressed round trips,
and runs OpenVPN's local crypto/options preflight for none, lzo-no, lzo-asym, lz4, lz4-v2, stub, stub-v2 and migrate.
The executable still must have no interpreter or DT_NEEDED libraries. build-info.json records the compression
capabilities and all pinned library inputs. Build tools are cached separately from source compilation; BUILD_JOBS
can bound validation concurrency without changing default CI architecture selection. An optional read-only
/archives cache supplies tarballs and bare Git repositories; cached tarballs still pass the locked SHA-256 gate,
and fresh Git checkouts must match the locked commit and pass full object integrity checking before use.

This provides compatibility with configured compressed peers and existing lzo-asym settings. It retains the
pinned OpenVPN 2.7 upstream semantics: compressed incoming traffic is supported, while outgoing traffic remains
uncompressed. Per-connection compression policy is unchanged. See the upstream
[configure options](https://github.com/OpenVPN/openvpn/blob/v2.7.8/configure.ac) and
[compression option handling](https://github.com/OpenVPN/openvpn/blob/v2.7.8/src/openvpn/options.c).

## D66 — Package disk and instance memory accounting

Manager node status and Dashboard overview/packages share a resource table from optional resources.packages
in the node report. Each installed package reports allocated package-file bytes, separate known persistent
service/provider data, RSS, measurement times and owned processes with instance IDs and PID namespace labels.
OpenVPN instances and multi-process FRR instances aggregate their own processes/descendants. Running daemon
memory belongs to its service adapter, not again to the binary-provider package; instances share binary files.
No process is attributed from its command-line arguments. Idle plugin/provider packages have zero process RSS;
kernel allocations (notably WireGuard), shared libraries, download archives and OCI layer sizes are excluded.
The base container is explicitly shared/unmeasured instead of claiming the entire root filesystem or zero bytes.

Disk walks use filesystem blocks, deduplicate inodes/hardlinks/overlapping roots, and never follow symlinks.
Store package files include all retained object versions, excluding symlink views. Native opkg/APK inventories
identify owned files without walking shared system directories. Known service state, provider state and custom
active/preparation/previous binaries are measured separately. Permission/scan failures remain unknown. Per-planner
disk snapshots cache for 60 seconds and invalidate when the installed set or resolved providers change; each
tree has entry/time bounds and a report has a bounded disk sampling budget. Process scans have PID/time bounds.
RSS is sampled from procfs VmRSS, includes shared pages, and is not a PSS/unique-physical-memory estimate.

Independent updater and Dashboard processes publish small atomic namespace-local samples every ten seconds
in the shared runtime volume. Planner prefers visible local processes and otherwise accepts only matching
package/host-boot samples no older than 30 seconds. Stale, missing, unreadable or incomplete measurements are
unknown, not zero. Report time and disk/memory sample times remain visible. Older reports parse unchanged and
the UI explains when upgrading planner is needed. Source-only delivery requires updated planner, updater,
Dashboard and Manager artifacts; no production package/configuration is replaced by this change.

## D67 — WireGuard endpoint DNS failures are per-peer runtime health

`wg syncconf` resolves configured endpoint names while parsing, so an external DNS failure used to abort the
required reload and roll back unrelated configuration. Generated local wg.conf now omits hostname endpoints
while retaining every peer, key, AllowedIPs, listen port and numeric IPv4/IPv6 endpoint. Local syncconf remains
required: malformed local configuration, missing providers and failed interface operations still reject apply.
Exports retain the operator's original endpoint names. Omitting an endpoint leaves an existing kernel endpoint
untouched; see upstream [parser](https://git.zx2c4.com/wireguard-tools/plain/src/config.c),
[syncconf](https://git.zx2c4.com/wireguard-tools/plain/src/setconf.c) and
[netlink endpoint writes](https://git.zx2c4.com/wireguard-tools/plain/src/ipc-linux.h).

An instance with hostname peers owns a small supervised endpoint runner. It resolves each peer independently,
checks that local syncconf has loaded that peer, and updates only its numeric endpoint through wg set. Resolver
subprocesses time out after five seconds; native observation/updates after three seconds. Failures use per-peer
bounded exponential retries with jitter capped at 60 seconds, retain the last working endpoint and never raise
sync.apply-failed. Successful DNS is refreshed every 60 seconds. Unchanged DNS preserves learned endpoints;
an empty endpoint after link recreation is repaired even if DNS did not change. Runner/spec identity changes
replace the helper on upgrades or configuration changes; instances without hostname endpoints need no helper.
Mutations occur only in this generation-owned runner, never in plugin plan/validate/probe operations.

Per-peer status exposes desired/resolved endpoints, phase, attempt, retry time and bounded error text. Only the
matching current peer is degraded; its actual connection state remains separately visible. Stale observations
and observations for old endpoint/key configurations are ignored. Manager connection rows show WireGuard and
OpenVPN reasons directly, with automatic endpoint retry labels; Dashboard's existing per-instance reason column
consumes the same report. External connection errors do not become revision application errors. OpenVPN retains
D60's native reconnect plus supervised process retries. No arbitrary wg error-string exception was added to
planner, and no required local checks were suppressed. Release the WireGuard plugin and Manager changes to
enable the new behavior on deployed nodes.

## D68 — Reclaim obsolete download versions automatically

Successful-install-only cleanup leaves interrupted or failed downloads on small devices indefinitely. Updater
now collects before metadata checks (before space planning), after terminal transactions and on startup even
without an active journal. A process-wide filesystem operation lock serializes metadata/download/install/recover
and collection across updater processes; it does not hold planner's configuration maintenance lock during a
download. The short-lived updater activation helper participates in its parent's transaction without retaking
that lock. Pending journals, especially the updater-first handoff, suppress collection until recovery finishes.

Current installed sets/views and one accepted rollback baseline remain. A separate rollback.json checkpoint
survives failed attempts replacing journal.last.json; compatible legacy successful journals seed retention.
Failed candidates are discarded after an unchanged/rolled-back installation, while failed rollback/repair
references remain protected. Store executable references visible in procfs and same-boot planner adoption
records protect binaries still running in another PID namespace. Native caches retain only the selected
current/rollback package archives. This bounds normal version history without deleting running/recovery files.
Kept views also protect their actual object targets when a full disk interrupts installed-record persistence.
An optional repairRequired journal flag preserves native repair downloads even when an interrupted native
installation has changed files without advancing its package database; old journals still parse.

Mihomo auto mode uses the existing transfer lock to remove known asset archives and mihomo.new before/after
each attempt, including failed HTTP bodies, and periodically when the installed binary is already ready.
Legacy Linux archive names with bounded release version syntax remain recognized after leaving assets.json.
Its previous/mihomo path continues to hold only one previous binary. Manual mode and unknown files remain
untouched. Final byte progress is captured before deleting failed files. Store/native/Mihomo cleanup refuses
to traverse redirected internal directories; archive names must be single normal path components.

This is runtime package storage housekeeping. It does not change service configuration, discard pending
handoffs, remove user-managed binaries/data or run global container/image pruning. Updated updater and
mihomo-pkg artifacts are required; source validation does not clean production volumes.

## D69 — Recover updater activation within its writable owner

The reference Podman deployment intentionally mounts the package store read-only in planner and writable in
updater. A staged activation helper launched by planner inherits that read-only mount and cannot create
store/current.new, causing a generic exit-status error and rollback before the new updater can take over.
Keep that mount isolation. When planner's activation RPC fails, the independent updater repeats the same
constrained activation in its writable namespace. It revalidates the persisted self-restart journal, target
updater, generation, backend, unchanged other packages and expected current view. A lost acknowledgement after
a successful switch is idempotent; invalid handoffs still fail and roll back. The independent entrypoint then
starts the new updater, which resumes the verified staged plan and upgrades planner afterwards.

Planner drains helper stderr while retaining at most 4 KiB, and includes the diagnostic in failed RPCs.
Execution and capture waits stay bounded; timeout still kills the helper's process group. If owner activation
also fails, report both errors. This fix requires an updated updater binary. Older deployed updaters need one
owner-side takeover through their signed source before the new behavior is available; changing planner's
store mount to writable is not required.

## D70 ? Pages archives with resumable signed 500 KB parts

The release workflow passes --pages-url to group generation. Before signing, every non-OpenWrt archive,
including reused packages, is verified and copied to packages/<sha256>/<filename> on Pages and split into
500,000-byte parts (only the final part may be shorter). Full-file URLs remain available to older updaters.
The additive optional chunks array binds ordered URLs, exact sizes and SHA-256 values inside the signed group
manifest. Group format remains version 1; older clients ignore chunks. OpenWrt ipk/apk assets stay on Releases
with no chunk metadata, and OCI images retain registry publication. No external mirror configuration is used.

The store updater downloads/retries each part with its existing bounded retry policy. Only verified regular
.part files enter a persistent downloads/<artifact-sha256>/ cache outside disposable staging. Restarts and
failed transactions reuse correct parts, redownload corrupt/missing parts and verify the assembled archive's
signed size and hash before installation. RAM use stays bounded to streaming/one part. Verified assembled
bytes remain visible during retries. Native backends and legacy groups continue complete-file downloads.

Successful installations discard their part cache. New verified groups retain only relevant uninstalled
platform artifacts, pruning obsolete owned part/temp files; unknown files and symlinked directories are not
traversed. Pending handoffs remain protected. All download/cache work shares the updater operation lock.

Pages history merging preserves immutable signed metadata and all referenced download files, deduplicates
content paths, verifies new files locally and restores historical chunks from their verified full archives.
Catalogs expose part URLs without GitHub API requests. The builder refuses a site above 1,000,000,000 bytes
before upload rather than silently dropping old assets. GitHub's published-site limit is documented at
https://docs.github.com/en/pages/getting-started-with-github-pages/github-pages-limits.

## D71 — Restore v1 subscription DNS inheritance and core bootstrap defaults

Compared the actual v1 generators/gen_clash.py and clash/base.yaml: v1 downloads the chosen subscription,
keeps its DNS object, and lets missing resolver fields use Mihomo defaults. V2 instead rejected empty node
nameservers before downloading and replaced subscription DNS with an empty controlled object. It also emitted
default-nameserver: [] even for literal custom upstreams, suppressing core defaults and causing another startup
failure. The live subscription already contains DoH upstreams, fallback and proxy-server resolvers.

Empty node upstreams now select subscription/core DNS resolution. Nonempty upstreams keep the existing fully
node-owned custom policy. Nonempty optional settings/positive flags may override inherited defaults. Enable,
listen and IPv6 remain project-owned; strip a subscription's listen-routing-mark. Missing nameserver and
bootstrap keys remain absent so core defaults work; explicit empty lists in a subscription are rejected.
No public resolvers are silently inserted by the adapter. This changes normalization, not node DNS policy.

Carry front-end listeners/local endpoints in the private worker spec and validate effective DNS before
accepting a new subscription or rebuilding from cache. Read local interface addresses without spawning native
commands so wildcard front ends also protect the node's own LAN IP. Refuse self/front-end loops, malformed
resolver/policy lists, invalid Fake-IP ranges and missing respect-rules proxy resolvers. Old cache/final config
survive a bad refresh. Local node override validation remains early; Manager compilation defers inherited-only
resolver requirements to the node after download.

The worker publishes initial/downloading/result states before and after network I/O. Probe rows carry specific
download/configuration errors (including existing lastError columns), and invalid local configuration reports
Down rather than indefinite Pending. Updated plugin/schema/descriptor and Manager are required; no production
binary or revision is replaced by this source change. Core-default behavior was verified with the installed
Mihomo v1.19.33 in -t mode, consistent with its pinned upstream DefaultRawConfig/parseDNS implementation:
https://raw.githubusercontent.com/MetaCubeX/mihomo/v1.19.33/config/config.go.

## D72 — Retry and reconcile GitHub Release mutations

The v0.2.4 publish job failed when release creation returned HTTP 500. Replace one-shot view/create/upload/edit
commands with tools/release-publish.py, using the authenticated gh CLI. Only a confirmed missing release permits
creation; temporary query errors do not. Each operation has six bounded attempts, a 180-second command deadline
and exponential backoff/jitter (longer for rate limiting). Permission/validation errors stop immediately.
Require an existing tag, and create a draft until the existing manifest/image validation reaches finalization.

After an ambiguous creation/upload/edit result, inspect the remote state before repeating the mutation. Paginate
asset inventory, verify existing uploaded assets by size and SHA-256 (download legacy assets lacking a digest),
and skip accepted uploads after lost responses or duplicate-name conflicts. Never overwrite/delete a remote
asset; incomplete or mismatched existing files fail explicitly. A published release needs no repeated edit.
Retry exhaustion remains a failure, preserving the real diagnostic. Tests use a fake gh transport and never
modify a remote release. Existing Actions re-runs keep their original workflow/ref; committing this helper does
not retrofit an already-created run.

## D73 — Preserve v1 MosDNS IP/CIDR and geosite regexp rule files

Compared the v1 generator/downloader and scripts/etcd.redacted.json with v2's importer, file rewriting and
download validator. The importer preserves the custom plugins pipeline and named rule files; v2 incorrectly
validated every downloaded entry as a simple domain. All four public URLs still download successfully:
geoip-cn contains 5,568 CIDRs; category-ads, geolocation-cn and private contain respectively 1, 3 and 1 regexp
entries rejected by the old character filter. This is a plugin regression, not an export/source outage.

Accept bounded regexp expressions using regex-syntax (syntax parsing, no matching engine), including octal
escapes. Custom pluginsYaml/advancedYaml runner specs permit homogeneous IP/CIDR files with checked IPv4/IPv6
prefix lengths. Structured generated domain sets remain domain-only. Keep the existing configuration shape;
already imported custom pipelines do not need to be imported again. Strip comments as the pinned MosDNS v5.3.4
text readers do; preserve original downloaded bytes and atomic replacement/unchanged-content behavior.
Malformed regex, mixed domain/IP lists, bad prefixes, empty data and HTML remain failures and retain old rules.

Verified all four complete public files (5,568 / 845 / 4,796 / 131 entries), plugin/schema/manifest checks, strict
Clippy and real local HTTP downloads covering valid replacement, invalid refresh preservation and unchanged
bytes. No production package/configuration was replaced; release the updated meduza-np-mosdns plugin.
Upstream parsing references: pkg/matcher/domain/{matcher,load_helper}.go and pkg/matcher/netlist/load_helper.go
at https://github.com/IrineSistiana/mosdns/tree/v5.3.4/pkg/matcher.

## D74 — Query wildcard DNS listeners through loopback

After the MosDNS rule-format fix, all 19 rule downloads report OK but mosdns/main still reports DNS timeout
with listen 0.0.0.0:1153. The shared plugin DNS probe sent to the wildcard address and compared recv_from's
source against that same wildcard address. Healthy replies from 127.0.0.1 were discarded. IPv6 :: has the same
problem. Reproduced both failures with local UDP responders before changing the implementation.

Normalize an unspecified probe target to its own address family's loopback before sending and comparing replies.
Keep configured listener/port and strict source/transaction-ID/response-code checks unchanged. Apply this in the
shared SDK so MosDNS, Mihomo, OpenClash and other DNS checks agree; dnsmasq already normalizes its own targets.
Tests cover IPv4 and IPv6 wildcard replies, literal listeners and real silence, plus a MosDNS plugin probe that
must report Up after a valid answer while still displaying the original wildcard listener.

The source fix does not itself establish the deployed resolver's real upstream health. The initial live SSH
verification was refused by the 1Password agent; no deployed binary or configuration was changed. Updated
consumer packages must be released to include the SDK fix. Do not narrow the actual listener to work around a
monitoring defect.

## D75 — Reconcile active configuration failures with successful ACKs and reports

The node reports rev-000011 desired/applied/lastSuccessful and state in-sync, but Manager still labels that
revision as an apply failure. node_sync kept failed_revision after an applied ACK cleared last_error and
advanced applied_revision; the UI independently favored the old failure pointer. The report's lastError is a
/nodes/self/jobs HTTP 429 transport failure, not evidence that configuration application failed.

An applied ACK clears active failures at the same/older revision, preserving newer failures and immutable ACK
history. Accepted, ordered in-sync reports whose desired/applied revision matches a published node snapshot
also reconcile success, healing existing databases and lost/deduplicated success ACKs without republishing.
Do not require an empty report lastError: keep transport diagnostics/backoff intact in the raw sync report.
Recovery errors or an active/degraded apply stage prevent this success reconciliation. Unchanged healthy
reports do not rewrite apply timestamps.

Late intermediate ACKs do not erase errors or regress terminal publish results. Older-revision failures and
known retired-boot ACKs cannot replace newer active outcomes. A genuine failed reapply at the same revision
remains a failure; the overview also recognizes an explicit current failed report when its ACK is missing.
Revision equality alone is insufficient for inSync. Source-only; update Manager and let the next node report
repair persisted stale status. No deployed database/configuration or Git index is modified.

## D76 — Show reported status as a named service/instance tree

Flat service rows shortened every WireGuard endpoint worker to endpoints and repeated wireguard without its
owning interface. Both Manager and the node Dashboard now share an expandable status tree. Service folders
contain interfaces/instances, then peers, processes and other probes. Use stable report ids and explicit process
ownership; frr/bgpd belongs to frr/main, and a peer named endpoints remains distinct from the endpoint worker.
Missing parent probes produce virtual folders, not invented healthy states. Include connection/routing probes
in Manager's status tree while preserving their detailed tabs. Retain each independent health result and reason.

WireGuard reports interface/peer name, peer owner and interfaceName. Manager fills missing names in older
reports from published configuration, never from unsaved drafts. Display names do not alter restart targets,
and WireGuard's connection rows also show peer/interface names. Per-row restart confirmation and operator/viewer
permissions remain intact. Expand/collapse choices survive report refreshes; collapsed branches retain issue
counts so a running interface or helper cannot conceal a failed peer.

Validated the shared hierarchy against repeated helpers, colliding peer/process ids, missing parents, explicit
ownership and reordered reports. Actual Manager/Dashboard browser checks cover names, collapse/expand, hidden
failure counts, refresh persistence, original process restart payload and viewer permissions. WireGuard's 14
tests, strict Clippy, shared/frontend type checks and both production builds pass. Source-only; release Manager,
Dashboard and WireGuard to deploy the complete change. Git submission remains with the user.

## D77 — Keep topology labels stable and fit the available viewport

Topology labels were greedily repositioned or hidden every frame in depth/hover order, so tiny depth crossings
and inspecting a router made IP labels jump or flicker. Keep a stable candidate slot per vertex id, fixed text
metrics and deterministic placement order. Hover/selection changes paint emphasis only. Offer additional nearby
slots with leader lines for crowded clusters instead of silently dropping visible routers' labels. Remove slots
for deleted vertices and recompute on explicit fit/focus or viewport changes.

Compare topology links by identity/endpoints/physical parameters rather than array index. Canonicalize iteration
order; color/name/report-order updates keep settled coordinates and do not reheat the force simulation. Actual
link additions/removals still update layout, and report refresh still preserves a dragged vertex's identity.

The previous 520px graph minimum plus viewport-based height and page padding overflowed small windows. Give the
topology route a bounded dynamic-viewport flex/grid layout, an independently sized canvas and a bounded details
pane. Short windows keep the toolbar accessible in its own scrolling area. Ordinary routes retain their existing
layout. A real resize updates the bitmap and redraws immediately; unchanged ResizeObserver callbacks do not
clear it. Refit the full graph/current focused selection, or scale the existing manual camera proportionally
without losing target/orientation or accumulating resize drift.

Three regressions reproduced failures before the fix; all nine graph tests and three i18n tests now pass.
Actual Manager Chrome checks exercise six viewport transitions (1500x900 down to 390x680 and back), canvas/DPR
dimensions, every framed node staying visible, a bounded selected-router pane, label stability and reordered
heartbeats. Shared UI/Manager/Dashboard type checks and the Manager production build pass. Source-only; updating
Manager deploys this change, with no node-side package/configuration changes required.

## D78 — Render labels inside the depth-sorted topology scene

Stable labels from D77 were still painted after all geometry with nearly opaque panels. They could obscure
routers/links and behave like an overlay. Put both node/IP and AS labels on their associated object's projected
depth plane as camera-facing text. Render the scene back-to-front; links and nodes win equal-depth ties over
labels. Split depth-spanning links into at most 24 sections so their foreground and background portions can
appear on the correct side of text. Preserve dashed-line phase and animated traffic particles across sections.

Node panels use at most 18% background opacity; AS panels use 16%, with separate readable text opacity and
depth fog. Label placement now penalizes projected link intersections, prioritizes node and label clearance,
and offers diagonal as well as axial candidates at three distances. Keep the chosen candidate through hover,
selection and ordinary reports, retain leader lines for offset text, and clip at the canvas boundary instead
of pinning labels to the viewport edge. This keeps the position-stability and responsive-layout fixes intact.

Reproduced opaque/foreground occlusion and crossing-link placement failures before the fix. All 12 graph tests
plus 3 i18n tests pass, including split-link depth ordering and labels following their object at the viewport
edge. Actual Manager Chrome checks cover translucent panels, stable hover/report labels, six viewport changes
and bounded details; front and rotated-scene previews were inspected. Manager type checks and production
build pass. Source-only; update Manager to deploy. No Git submission or production node changes.

## D79 — Simulate topology clusters and members in three dimensions

The camera was three-dimensional, but AS anchors and collision distances were planar and a continuous Z
restoring force flattened members. Keep the existing lightweight ForceGraph3D canvas implementation and
compute member repulsion/collision, link springs, body attraction and body separation in X/Y/Z. Seed new
members deterministically on a Fibonacci sphere; keep existing positions through status refreshes. Place
AS anchors on size-aware spherical shells around the primary AS, use actual three-dimensional collision
distances and apply collision/weak charge to whole bodies. Cross-AS springs honor configured strength and
translate groups without dismantling their local arrangement. Remove the separate flattening force.

Spacing now means physical three-dimensional separation; some spheres can overlap in projection and separate
when orbiting. Preserve depth-sorted translucent labels, camera interaction and responsive framing from D77/D78.
Do not enforce pairwise screen-space separation that would collapse the layout back into a plane.

New regressions reproduce the old flattening and Z-only collision failures. Check normalized covariance
determinants after settlement to reject any plane regardless of camera orientation, group/member volume,
world-space sphere clearance, deterministic coincident-body separation and no reheating on reordered reports.
All 14 graph tests plus 3 i18n tests pass. Actual Manager Chrome checks verify cluster/member depth, orbiting,
stable labels and six responsive viewport transitions. Type checks and production build pass. Source-only;
update Manager to deploy. Git submission remains with the user; no production node changes.

## D80 — Prepare OpenWrt FRR pathspaces for the binary's compiled identity

roaming-cat's FRR 10.4.1 exits immediately: `/var/lib/frr/meduza` has no parent, and PID/socket creation
in `/var/run/frr/meduza` fails. The supplied directory listing is traversable and owned by frr, but the
binary reports `--enable-user=root --enable-group=root --enable-vty-group=root`. A root UID with FRR's
reduced capabilities cannot rely on bypassing another account's directory permissions. Hardcoding frr
ownership therefore breaks this build; the earlier missing-account report was a separate packaging issue.

Inspect the resolved zebra binary with a bounded, read-only `--version` command and parse its configured
daemon user/group. Generate all runtime, saved-state and generation-config chown commands for that identity;
keep upstream frr defaults and respect explicitly disabled identity transitions. Failed commands, missing
configure metadata and invalid identities fail planning instead of silently assuming root. Disabled FRR
does not inspect or require a provider. No runtime privilege override is added to the daemon arguments.

Prepare missing shared parents with explicit 0755 creation modes, leaving existing parents untouched.
Prepare both dedicated `meduza` runtime/state directories, explicitly repair their modes and recursively
set their owner including old PID/state files. Ownership checks are required before launch. Do not chown
shared FRR parents or sibling instances; generation secret files retain 0600. This also avoids assuming
that the distribution's FRR init script has run, especially with OpenWrt's volatile /var.

The supervisor previously discarded stdout although FRR's generated config uses `log stdout`. Send both
streams through one append-only file and the existing bounded forwarder, retaining adoption/detach behavior.

New regressions reproduced missing-directory preparation and lost stdout before the fix. All 18 FRR tests
and 10 supervisor tests pass, plus strict Clippy for FRR/planner. Checks cover the reported root build,
dedicated/custom identities, failed/missing version commands, restrictive umask, repeated preparation,
existing shared/sibling permissions, secret file modes and both output streams. Source-only; release/update
meduza-np-frr for startup compatibility and planner for complete logs, then reapply. No live OpenWrt
verification, Git submission or production modifications performed.

The OpenWrt official packages feed (master and openwrt-24.10) explicitly sets `--enable-user=network`
and `--enable-group=network`: https://github.com/openwrt/packages/blob/openwrt-24.10/net/frr/Makefile .
The reported root build is therefore not the official account convention, and frr:frr must not be
assumed for OpenWrt. Add the official network:network configure flags to the same identity/plan regression;
the runtime detection already supports them without a platform-specific override. All 18 FRR tests and
strict FRR/planner Clippy still pass.

## D81 — Merge topology peers by learned Router ID, including managed nodes

FRR already reports `remoteRouterId` from `show bgp neighbors json` as each BGP instance's `routerId`.
The topology previously keyed unmanaged peers by that ID, but recognized managed counterparts only by
tunnel address. A session to a managed router's other address therefore drew an unmanaged duplicate.
An observer lacking the ID could also draw an address-only copy beside another observer's known peer.

Build a managed identity index from published ASN and FRR/node router ID (the FRR override wins). Resolve
learned peer identities to a unique managed node before the existing tunnel-address fallback. Never collapse
distinct managed registrations on duplicate IDs, and scope all identity matches to the AS because Router IDs
are not globally unique. Validate IPv4/nonzero IDs. A retained ID continues identifying the router through
session reconnects; unknown/invalid IDs do not justify a merge. Infer an unidentified observer's peer from a
known AS/address only when the observations agree. Ambiguous overlapping addresses stay independent.

Collect sessions before resolving peers so report/node ordering cannot leave an address-only duplicate.
Preserve every session, state, learned route and AS path when aggregating links; keep all peer addresses on
the vertex as well as individual session rows. Existing `address` remains for API compatibility. Preserve
managed node identity/name/navigation and expose its Router ID. Show all merged addresses in peer tooltips
and details. Clear obsolete selection/hover targets after a merge instead of retaining empty detail cards.

Five new regressions failed before the change; all seven topology identity cases now pass. The complete
focused run passes 42 tests (topology, graph, sync/API and i18n), plus Manager type checks and production
build. Actual Manager Chrome checks feed real server-generated before/after results through normal polling:
unknown peers merge, the managed duplicate disappears, both session rows/routes/addresses remain, stale
selection clears and mobile/desktop details stay bounded. Source-only; update Manager to deploy. The existing
FRR report already supplies the fields, so no new node-side package or configuration revision is required.
No Git submission or production changes performed.

## D82 — Global AS names and peer ASN choices (2026-11-10)

Store the optional `asAddresses: [{name, asn}]` catalog in the Manager cluster document. Its own Global
settings tab participates in the existing working copy, per-account draft, validation and Save/Apply flow.
Allow nonempty names up to 128 characters, unique numeric ASNs in 1..4294967294, and report invalid entries
at their field paths. Viewers can read the list; operators and administrators edit through normal permissions.

The shared descriptor renderer accepts optional console-provided numeric choices through context, so
WireGuard nested peer dialogs and OpenVPN instance dialogs both offer catalog selection beside manual
numeric input without changing their package schemas. Only the numeric remoteAsn is written. Names are
resolved globally by ASN, including manually entered matching numbers; no redundant per-peer name or
selection-provenance flag is stored. Deleting a mapping does not modify existing peer configuration.

Expose published names once in topology `asNames`; display them after AS numbers in 3D clusters,
collapsed AS vertices, remote path vertices, tooltips and session/path details. Keep topology identities
and routing snapshots independent of display names: catalog-only edits publish a Manager revision but
leave node desired revisions and content digests unchanged. Never expose unpublished names in topology.

API tests cover staging/publication, rename/removal, unchanged node snapshots and invalid catalog shapes,
duplicate ASNs and boundaries. Actual Chrome checks exercise Settings permissions and Save/Apply/reload,
both real package editors including nested WireGuard dialogs, manual numeric entry and named 3D labels.
Source-only; update Manager to deploy. No Git submission or production modifications.

## D83 — Report console capabilities independently of service configuration (2026-11-11)

OpenWrt overview displayed CLI and LuCI as `not enabled` because registry capabilities were derived only
from applied services, although these management entry points have no services.* configuration. For
registered CLI/UI-client roles, expose their `ui.*` capabilities independently of service configuration.
Entry-point running/healthy means the capability is available, without requiring a supervised daemon.
Keep availability and registration checks authoritative: absent dependencies, conflicts and incompatible
platforms remain errors. Non-UI capabilities and optional service switches keep their original semantics.

Add `in-sync` to the LuCI successful badge states. Do not promote unsynced, applying or unknown states to
green. Regression tests execute the shipped LuCI modules and cover console rows, disabled services and
failure badges; run these tests in web CI. Registry tests use the actual CLI and both dashboard manifests,
including the OpenWrt platform constraint, and check dependency/registration failure handling. Deploy the
updated planner plus dashboard-openwrt packages. Source-only; no Git submission or production changes.


## D84 - Serve Mihomo's native web panel from inherited or explicit settings (2026-11-11)

The downloaded subscription already requested a UI, but normalization removed external-ui-url and never
supplied external-ui, leaving /ui/ at 404. Add webUi.mode: auto (default, inherit a subscription's panel
request), enabled (fall back to the documented MetaCubeXD archive) and disabled, plus an optional HTTP(S)
webUi.url override. The shared package descriptor exposes these settings in Manager and Dashboard.
Keep the API address and per-node secret controlled; UI inheritance does not open a network listener.

Use Mihomo's native downloader/unpacker and /ui/ server. Replace subscription filesystem paths and
external-ui-name with a relative ui/<URL SHA-256> directory under the persistent Mihomo work directory.
This confines panel files and lets a changed source download without first destroying the previous cache.
After index.html exists, remove only obsolete owned hash directories, excluding symlinks and custom files.
The native controller's static file path is established at startup, so the worker restarts its core when
that path changes rather than accepting a successful configuration reload with an obsolete UI route.
Same-source restarts reuse the cache; same-source content updates remain available through Mihomo's
native UI updater. A panel download error must not stop the proxy core or remove the previous cache.

Regression tests cover inheritance, explicit source/default/disable, URL validation, listener/credential
ownership and safe cache cleanup. Real v1.19.33 plus the actual worker, a local subscription server and
small ZIP fixtures verify /ui/ download/serving, API authentication, offline cache, subscription refresh
switching, obsolete-cache cleanup and continued API service on download failure. Schema/manifests,
strict Clippy, descriptor translations and a real browser form check pass. No production access or
deployment performed; update Manager metadata and the node's mihomo adapter to enable the feature.


## D85 - Native Mihomo panel launch from both node dashboards (2026-11-12)

Add an explicit launch entry to the ordinary dashboard and LuCI, gated by both the enabled Mihomo service
and the active worker's native-panel status. Show it on the ordinary dashboard's anonymous login page as
well. Per the user's explicit clarification, the ordinary Dashboard permits direct anonymous launch with
the controller credential, without a login redirect. Periodic status calls never return the secret.
Launch uses a dedicated POST/custom header without opening cross-origin CORS access; other Dashboard
control APIs retain session/role/CSRF checks. LuCI retains its write-only ubus method. The CLI and privileged
local protocol share the same credential operation.

The running worker publishes a public enabled/ready/address file and a separate 0600 credential file only
when its controller and panel are ready. Revoke them at startup/shutdown, reject expired or malformed
credentials, bound all reads, and keep both files out of normal reports. An anonymous status route exposes
only a field whitelist. The private response remains no-store and is never included in periodic polling.
Atomic files set restrictive permissions before secret bytes are written, including temporary files.

Use the browser hostname for wildcard binds, retain explicit binds and IPv6 brackets, and report a remote
loopback-only endpoint as unreachable. Open a blank tab synchronously, detach its opener, then populate
MetaCubeXD's /ui/#/setup fragment with HTTP scheme, host, port and encoded secret. This keeps the secret
out of HTTP request URLs. Close the tab on errors and keep the enabled entry available for retry.

Tests cover IPv4/IPv6/default ports, fragment escaping, loopback handling, LuCI visibility/ACLs, private file
permissions/expiry, anonymous direct launch and real HTTP/UDS authentication boundaries for other APIs. Actual browser
checks exercise both shipped views, credential retrieval only on click, anonymous visibility and disabled
feature hiding. Real Mihomo checks use native Linux temporary storage to verify 0600 credentials and revoke
on shutdown. No production changes or Git submission.

## D86 - AS label layout, sortable catalog and prepaint theme restoration (2026-11-12)

Draw AS numbers and registered names on separate lines for clusters, collapsed ASes and path ASes.
Reserve the complete two-line rectangle before placing Router ID/IP/hostname labels. Retain cached
placements through hover, but invalidate them when an AS block moves or grows into their space. Extend
candidates rather than accept AS overlap; preserve depth ordering and translucent label backgrounds.

Add native drag handles to the AS address editor, plus up/down buttons for keyboard and touch use.
Move whole name/ASN records in the working copy and retain the existing Save, draft, Apply and validation
flow. Viewer accounts have disabled fields and no reorder controls. Published array order also determines
the ASN choices; sorting remains Manager metadata and does not reapply node networking.

Both web apps already restored the saved theme in their JavaScript module, after a light initial paint.
Emit one shared, small synchronous same-origin theme initializer and inject it before app modules/styles.
Keep the existing mnp.theme key, CSS system fallback and strict CSP without inline-script exceptions.
Storage errors must not prevent startup. The ordinary runtime still handles changes made in the selector.

Graph and theme regressions, typechecks, production builds and translation checks pass. Actual Chrome
checks reproduce the previous light flash with delayed app JavaScript, verify prepaint restoration and
full reloads for both built apps under production CSP, and exercise native mouse drag, arrow controls,
draft reload, publishing, order/value preservation and viewer permissions. Actual topology and peer forms
also pass. Evidence: target/check-settings-ui-browser.log, target/check-as-split-browser.log and
target/check-as-labels-after.log. No Git submission or production modifications.

## D87 - Use Node 24 inside all JavaScript Actions (2026-11-12)

The workflows already requested Node 24 for build commands, but their JavaScript Actions still declared
Node 20 internally. Upgrade checkout/setup-node/cache and pnpm setup to v5, upload-artifact to v6,
download-artifact to v7, configure-pages to v6, upload-pages-artifact to v5 and deploy-pages to v5.
Include release-tool/release-cache composite actions and the separate cache promotion workflow.
The Pages uploader's pinned transitive upload action also declares node24. Do not rely on forcing a
deprecated action to use another runtime or silencing its warning.

Keep explicit node-version: 24 and existing pnpm caching. Disable setup-node v5's automatic package-manager
cache in release jobs using the custom cache flow and in the manager-image job, which does not prepare
pnpm on the runner. Artifact names, inputs, permissions, cache keys and publication order remain intact.

Read-only verification parses all six local YAML documents, fetches all twelve upstream action manifests
including transitive references, confirms node24 for every JavaScript action, and validates all 86 action
calls' input names and Node setup options. The same actionlint 1.7.7 gate used in CI passes with its existing
cache-mode exception. Evidence: target/check-node24-actions.log. No remote workflow
execution, Git submission or production changes.

## D88 - Stable wildcard artifact download paths after the Node 24 migration (2026-11-12)

D87 missed download-artifact v5+'s single-match extraction change: a wildcard matching just one artifact
extracts directly into path even when merge-multiple is false. A manager-only rebuild therefore put its OS
database under in/meduza-np-manager.image-os while the SBOM command looked in in/oci-meduza-np-manager/.
The same count-dependent behavior also affected archive collection and this run's updater lookup.

Set merge-multiple: true and dedicated roots for every wildcard download: in/dist-rust for the image
builder, in/dist-packages for published archives and in/oci-images for runtime image records/OS databases.
Update updater discovery and SBOM arguments accordingly. Existing archive and OCI record collectors
continue to match their fixed prefix. Preserve per-package/per-platform OS data and digest verification;
missing inputs remain failures. Keep the Node 24 Actions versions.

Six regression tests use ZIP fixtures and upstream single/multiple extraction semantics to execute the
actual workflow's find/cp collectors, updater lookup and image OS argument assignment. Cover manager-only,
base-only and both-image rebuilds, one/multiple archive bundles and the old missing-path reproduction.
All 53 release pipeline tests (including the six new cases) and actionlint pass. Evidence: target/check-release-artifact-layout.log
and target/check-release-tests.log. No Git submission or remote workflow/production changes.

## D89 - Reconcile theme state, translate Dashboard and move transfer details into one page (2026-11-12)

The user reports Manager localStorage saved=dark while the document has data-theme=light and light tokens,
even with a dark OS preference. Read-only inspection confirms v0.3.3 serves the earlier theme bootstrap;
adding that script alone did not address post-startup divergence. Keep persisted preference authoritative,
reconcile the document and subscribed selectors on pageshow, storage events and theme-attribute changes,
and avoid redundant attribute writes. Retain a temporary per-document choice when storage is blocked.
The source of the reported attribute overwrite is not established; the fix repairs that observable state.

Replace Dashboard's hardcoded Chinese UI with English message keys and complete Simplified/Traditional
dictionaries for navigation, login, maintenance, package/instance status, updates, diagnostics and errors.
Add a browser language picker on authenticated and anonymous views, preserve it across reloads and use
the same locale in shared controls and dates. Keep the anonymous Mihomo panel entry. Extend the translation
coverage gate to Dashboard and prohibit hardcoded Chinese text in its TypeScript UI sources.

Manager node details gain an Updates and downloads tab; Dashboard's existing update page takes both
package and binary transfer details. Move full progress/history/error cards and update controls there.
Elsewhere show only a compact ongoing-work notice with a details link, removing it after work terminates.
Keep queued Manager requests distinct from completion and retain role checks. Dashboard reuses its node
report across overview/activity/update pages; binary details remain visible if updater status is unavailable.

Unit/API, translation and production-build checks pass. Actual Chrome tests use both built apps, strict
CSP and compressed delivery, recreate saved-dark/stale-light state, exercise pageshow, real cross-tab
storage events, explicit/system themes and full reloads. Poll-driven update/binary notices appear and
disappear while failures remain in details. English/Simplified/Traditional navigation, persistence,
anonymous panel availability, Manager viewer restrictions and remote update requests also pass.
Evidence: target/check-node-ui-browser.log and target/check-update-progress-regressions.log.
No Git submission or production modifications; deploy updated Manager and Dashboard assets together.

## D90 — Restore owned routes when the whole policy table is absent (2026-11-12)

After replacing the previous gateway with the macvlan planner, an authorized isolated Incus test client
using the planner as its real default gateway times out connecting to Google, Bing, Baidu, gstatic and
Cloudflare. SOCKS through the same Mihomo succeeds. Ordinary external UDP DNS and some TCP DNS still
work through that gateway. Production read-only inspection finds the TPROXY nft table and mark rules,
but both owned policy tables (4626 for local delivery and 4627 for main-router replies) are missing.
The persisted ownership journal still lists their routes. A marked route lookup falls through to main.

iproute2 returns nonzero with `FIB table does not exist` for a missing table. LinuxBackend previously
discarded stderr and returned unknown, causing apply's existing lost-resource reconciliation to skip
the routes. Capture the diagnostic and treat known absence as an empty result; permission errors,
invalid table arguments, spawn failures and timeouts remain unknown. Keep the existing ownership
boundary and targeted reconciliation; never flush another owner's routes or modify host networking.

Regression tests reproduce the old failure and cover IPv4/IPv6 missing-table diagnostics and unknown
errors. An explicitly guarded Linux example runs only in a separate network namespace, exercises the
real backend, removes/recreates both tables, checks proxy/reply marked lookups and preserves an
unrelated route. It passes on the user-authorized test container, independently of production.

The v1 comparison also checks PREROUTING-only interception, routed/private/VPN exclusions, port direction
rules, reply policy and DNS ownership. TCP-only proxying leaves UDP/QUIC outside the proxy; DNS port
exclusions do not force arbitrary client-selected resolvers through the front end. The live custom
MosDNS pipeline explicitly allows domestic plaintext fallback for some remote queries and includes an
unproxied remote DoH resolver. Do not silently rewrite that operator-supplied pipeline or confuse
encrypted DNS with a guarantee that queries are sent only to the intended resolver. Front-end fallback
availability and strict DNS privacy are separate policy choices. See operations section 8.

Evidence: target/check-route-query-before.log (red), target/check-route-query-after.log,
target/check-gateway-regression.log (44 planner, 17 Mihomo, 11 dnsmasq tests pass), and the isolated
route_recovery_probe PASS. Live restoration requires the user's production-operation confirmation;
investigation and isolated verification do not constitute production deployment.

## D91 — Do not use TPROXY's local-delivery mark for source validation (2026-11-12)

The user's post-upgrade retest confirms D90 restored both policy tables and marked lookups. Nevertheless,
two rounds of actual default-gateway HTTPS from the authorized Incus client still time out on Google,
Bing, Baidu, gstatic and Cloudflare; SOCKS through the same node succeeds for every target. Production
read-only inspection finds `all.src_valid_mark=1`, ingress `rp_filter=2`, ingress `src_valid_mark=0`
and `accept_local=0`. The old Compose creation setting still requests all.src_valid_mark=1; /proc/sys
is read-only, so package upgrades cannot repair it.

A real-kernel test with a transparent socket, nft TPROXY and separate client/router namespaces reproduces
the failure. Changing effective src_valid_mark to zero restores the TCP handshake and delivery from the
original destination; disabling rp_filter alone does not. Linux takes max(all, interface), so an explicit
ingress override of one still blocks after all=0. The mark is a one-way local-delivery mark; reverse
source validation must not use its local-default table. The kernel sysctl reference describes this
transparent-proxy use case: https://docs.kernel.org/networking/ip-sysctl.html.

Set all/default.src_valid_mark=0 in run.sh's bridge/macvlan planner creation and both Quadlets. Leave host
policy administrator-controlled, and do not weaken reverse-path checks globally as a workaround. Add a
deployment regression for both modes, host isolation and templates, plus an optional real-network
handshake regression in tools/container-network-test.py --real-tproxy. Ordinary gateway and resolver
configuration remain independent. Existing deployments need a reviewed Compose/Quadlet change and
planner recreation; document the exact procedure, brief service interruption and ingress-value checks.

20 bootstrap/deployment tests pass. The real-kernel test passes all five expected blocked/connected
cases on the authorized Incus client. During verification the user changes Compose to all.src_valid_mark=0
and recreates planner. Read-only observation confirms all/eth0=0 and logs automatic restoration of both
owned tables/rules. The client retains the planner as its actual gateway and DNS; two rounds of gateway
HTTPS to all five targets succeed (10/10), as do 10/10 SOCKS controls. Google takes 1.88–2.46 seconds,
Bing 1.52–2.01, Baidu 0.076–0.084, gstatic 1.21–1.34 and Cloudflare 1.10–1.81. Front-end UDP/TCP DNS,
MosDNS and ordinary external UDP/TCP DNS also answer successfully. Evidence:
target/meduza-gateway-retest.json, target/meduza-tproxy-regression.log, target/check-tproxy-deployment.log.
The agent makes no production mutations or workstation gateway changes. The custom DNS fallback policy
remains unchanged; successful resolution does not establish strict DNS privacy. No Git submission.

## D92 — Outward topology, shared detail labels and separate direct/all framing (2026-11-13)

The user finds the Manager topology cluttered and asks for node labels rendered like AS headings without
leader lines, detail disclosure by zoom/hover/selection, distant hops continuing outward, and distinct
default/reset versus fit-all views. Keep the lightweight three-dimensional force renderer and current
selection/session details; this change requires no topology API or node-network changes.

Render AS headings and node panels with one centered translucent, depth-sorted drawing routine. AS headings
stay visible as landmarks; ordinary node details appear above the readable projected-size threshold or when
hovered/selected (including relevant link/group endpoints). Use 8px/6px radius hysteresis to avoid zoom-boundary
flicker. Remove all label leader strokes, preserve cached placement, and avoid complete AS title/name panels
including neighboring AS headings. Include visible panels in hit testing. Retain detail through pointer
press/release, select the captured owner on a click, and preserve the initial screen offset when dragging by
a label. Reheat only after real dragging begins, so selecting a settled node cannot move the layout.

Mark managed vertices (including collapsed managed ASes) as roots and configured iBGP/eBGP edges as direct;
learned path edges are distinct. Assign AS force-body anchors by breadth-first hop distance from managed
roots, spread each layer through XYZ and continue later branches in their parent's outward direction.
Use the anchor separation in cross-AS springs to keep remote layers from folding back towards the centre.
Default spacing becomes 2×; existing browser values and the optional external-AS collapse remain available.
Unchanged heartbeat data retain settled coordinates and do not restart the simulation.

Default/reset frames all visible managed vertices and configured direct-session endpoints, regardless of
session health. Fit graph includes every visible learned remote AS; keyboard 0 resets orientation/direct
scope. Resize preserves each scope, manual camera control still disables fitting, and a vanished focus
falls back to direct scope. Framed views follow genuine topology changes without overriding manual views.
Add a short localized help line explaining both buttons and label disclosure.

33 graph/controller, topology API and translation regressions pass along with Manager typechecks and production
build. Real Chrome exercises the actual Manager with configured-down peers and four-hop remote branches:
default/reset versus fit-all, real pointer hover/click on labels, selection persistence, shared typography,
translucency, zoom detail, unchanged report order, collapsed managed roots, vanished-focus recovery and four desktop/mobile viewport sizes without page
scrollbars. Visual inspection verifies outward separation and avoids the observed AS-heading collisions.
Evidence: target/check-topology-outward-tests.log, target/check-topology-outward-browser.log and the
topology-outward-default/all/selected.png previews. Source-only; no Git submission or production deployment.

## D93 — Attach Node labels directly to their projected glyphs (2026-11-13)

The user reports labels jumping again after D92 and explicitly requests direct marking beside each Node.
The cached avoidance algorithm still invalidates a router's slot whenever an AS panel crosses it; that can
change sides or displace text far from its owner even without reheating the physical layout. Remove that
placement policy for ordinary nodes and external router/IP labels. Their panel starts six screen pixels
after the projected glyph's right edge and stays vertically centered. Respect each glyph shape's actual
horizontal extent. Geometry depends only on the owner/projection and panel size, never neighboring panels,
crossing links, metadata order, selection or a viewport edge. Keep clipping rather than edge-pinning.

Keep D92's zoom hysteresis, hover/selection disclosure, shared typography/translucency, depth ordering,
label hit testing and pointer-offset dragging. Precompute the visible fixed panels before placing AS
headings; AS clusters and AS-path/collapsed-AS vertices yield to their reserved bounds. Only AS labels
retain avoidance slots. This intentionally supersedes moving Node labels to avoid other scene content.

24 controller regressions pass, including anchored placement through AS movement, rename/depth crossing,
link overlap, report reordering, orbit, zoom and resize; AS headings still avoid fixed router panels.
Real Chrome additionally exercises nine AS/camera cases on the actual Manager and verifies label hover,
click, default/all framing, responsive sizing and stable polling. Typechecks and production build pass.
Evidence: target/check-node-label-anchor-tests.log, target/check-node-label-anchor-browser.log,
target/check-node-label-anchor-build.log and target/node-label-anchor-selected.png. Source-only;
no Git submission or production deployment.

## D94 — Ignore disabled objects when planner collects required secrets (2026-11-13)

Read-only SSH diagnosis of the user's OpenWrt node finds no OpenVPN processes, management sockets,
generated directories or applied OpenVPN instances. The revision is in-sync, but persistent last-good
state records heldBack=[openvpn]. Both the SSH shell and planner have /usr/sbin in PATH; the installed
openvpn-openssl binary is /usr/sbin/openvpn (2.7.4), and doctor resolves that system provider correctly.

There are eight enabled OpenVPN instances with all required secrets delivered, plus three disabled
instances retaining references whose secrets Manager deliberately does not deliver. Manager's
collectSecretRefs already skips explicit enabled=false objects (D58). Planner's collect_refs does not,
so missing keys for disabled entries hold back the entire feature before the plugin is planned. Calling
the existing installed plugin's pure plan operation with the delivered configuration produces all eight
enabled plans without writing generated files or launching any daemon, confirming the gating mismatch.

Skip explicit enabled=false objects recursively in planner's shared secret collector, aligning both
held-back checks and local-keystore loading with Manager delivery. Keep default/explicitly enabled
references and plural refs; genuine missing active keys must still hold back the feature and become
plannable once a local key is supplied. Do not deliver disabled secrets or weaken active validation.

Two unit tests reproduce the eight-active/three-disabled case and nested service/peer refs. A real-plugin
engine regression fails before the fix and passes after it, verifies disabled WireGuard instances/peers
do not block the active plan, and retains genuine-active-missing/local-key recovery and unrelated services.
All nine OpenVPN library tests pass. Broader engine checks pass 7/8: the existing WireGuard DNS-observation
test does not observe resolution-failed within its nine-second window and also fails in isolation in this
local environment; it is not changed by this fix. Do not claim the full engine suite passes.

Evidence: target/openwrt-openvpn-reference-audit.json and openwrt-openvpn-plan-readonly.json contain only
reference identifiers and plan names; target/check-disabled-secret-refs-before/after.log record red/green;
target/check-openwrt-openvpn-regression.log and check-openwrt-openvpn-final.log record broader results.
No device configuration changes, restarts, key generation, package installation or production deployment;
only source is fixed. Native startup and peer-handshake acceptance remain for a subsequent deployment.

## D95 — isolate WireGuard and OpenVPN application failures by instance (2026-11-13)

A missing active key, malformed instance, native check, resource operation, required reload or failed
exec previously could hold back an entire tunnel service or abort the revision. Prepare each enabled
tunnel separately, then validate the surviving group for shared device/port collisions and check the
combined generation plans. Preserve the complete configuration context for other adapters, including
FRR's lookup of OpenVPN Access pools. Disabled references retain D94's delivery semantics.

Commit records a failed tunnel as an instance outcome and continues subsequent instances. Prepare or
native-check failures retain previously applied files and processes; failures during runtime delivery
stop and clean only that instance's owned resources/processes. Incomplete cleanup retains ownership
intent. Shared storage/journal failures and errors outside these two tunnel adapters retain normal
revision recovery. This replaces whole-service holdback for genuine missing active tunnel keys.

Persist redacted instanceErrors in applied.json and overlay them on the existing instance status rows
for both node reports and the local instances API. Previously applied instances are degraded; newly
failed instances are down. Package health includes these errors. Probes call the adapters separately
per instance so one malformed entry cannot erase other instances' observations. No report schema or
UI-specific error store is introduced.

Retry failed instances independently with exponential backoff (five-second base, sixty-second cap,
jitter), respecting maintenance, pending revisions and the apply mutex. Healthy native checks,
reloads and processes are skipped during scoped retries. Instance start/restart jobs and local API
calls can retry failed delivery even when no process was ever started. Drain revisions queued during
retry. Successful retry clears its error; other failures retain their retry deadlines and attempts.
Same-revision retries stage a new generation instead of deleting the live directory; prune to bounded
history after success.

CI builds the real OpenVPN adapter alongside WireGuard for engine tests. Regressions cover both
adapters' malformed/invalid/missing-key and shared device/port cases, syntax/resource/reload/exec
failure isolation, healthy PID stability, retained old files, scoped recovery, persisted status,
local/report visibility, automatic and manual key recovery, and secret redaction. The existing DNS
integration wait is increased from nine to twenty seconds: local WSL NSS cancellation/reaping was
observed to publish its timeout state after twelve seconds; the resolver deadline remains unchanged.
No production configuration, package or service is changed as part of this source fix.

## D96 — disabled tunnel entries take precedence over runtime recovery (2026-11-13)

Disabled services, instances and WireGuard peers may keep unfinished operational fields and dormant
secret references. They must neither block active entries nor reserve their device/port/key values.
WireGuard/OpenVPN now use SDK active_service to filter disabled entries before typed parsing, while
their pure validators also skip explicitly disabled typed entries. The original request is preserved.
Planner counts only active instance identities when preparing; synthetic error identities avoid real IDs.

Manager validates an active projection of tunnel configuration and restores original array indices in
schema error paths. Connection policy and OpenVPN compiler checks skip dormant entries. Stored drafts
and disabled snapshot fields remain available for editing; secret delivery still skips disabled refs.
Re-enabling restores all operational checks. Other adapters keep their existing validation behavior.

Planner's raw configuration supplies disabled status rows even without a plugin/provider. These rows
override cached failures, clear obsolete error/retry/traffic details, propagate parent disablement and
remove stale rows for deleted entries. Disabled peers do not degrade the package's other healthy
instances. Applying a disable removes the instance's owned processes/resources; a disabled peer is
omitted from wg syncconf. Re-enabling renders/applies the entry again.

Automatic retries and local/Manager start/restart requests check applied activation state so even a
stale persisted failure cannot revive a disabled entry. Probes skip disabled instances. Manager status
rows and shared Manager/Dashboard collection forms hide restart actions for disabled entries, retaining
configuration editing and enable controls. No production changes or Git commits are made here.
