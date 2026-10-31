# Meduza NP manager on Podman

The manager runs as one rootless container: built WebUI + TypeScript API + SQLite in a persistent volume. It needs
no host Node.js, no repository checkout and no build on the server. Nodes never connect to it directly on its
container port; a host reverse proxy terminates HTTPS (WebUI, node API and the WebSocket push channel).

## Requirements

| Item | Minimum |
| --- | --- |
| Podman | 4.8 (Quadlet `HealthOnFailure`, secrets with `uid/gid`), rootless recommended |
| systemd | user session with lingering (`loginctl enable-linger <user>`) for rootless autostart; the system `network-online.target` must be wanted by some system unit (else, once as root: `systemctl add-wants multi-user.target network-online.target`) — rootless Quadlet units wait up to 90 s for it on every start (D57) |
| Reverse proxy | Caddy 2.7+ or nginx 1.25+ with WebSocket upgrade (examples in `proxy/`) |
| DNS / TLS | a public name for `PUBLIC_ORIGIN`; Passkeys require HTTPS and a stable RP ID |

### Sizing

Measured with `tests/load/fleet.mjs` (real manager, simulated planners with push channel, ACKs, 30 s reports, 10 %
roaming nodes polling every 60 s; details and raw numbers in `tests/load/README.md`) on a 2 vCPU / 4 GiB Debian 13
VPS:

| Nodes | Manager memory (steady / peak) | CPU (steady, one core) | Publish to all nodes, p95 until applied | Full reconnect after restart | Database |
| --- | --- | --- | --- | --- | --- |
| 1000 | 163 MiB / 227 MiB | 4 % | 3.8 s | 29 s | 6.4 MiB |
| 2000 | 239 MiB / 363 MiB | 8 % | 11.3 s | 57 s (handshake limit) | 12.4 MiB |

Recommendation:

| Fleet | vCPU | Memory for the container | Disk for the data volume |
| --- | --- | --- | --- |
| up to 500 nodes | 1 | 384 MiB | 1 GiB |
| up to 1000 nodes | 1–2 | 512 MiB | 2 GiB |
| up to 2000 nodes | 2 | 768 MiB | 4 GiB |

The memory column leaves room above the measured peak for Node's heap growth and backups (the `admin.js backup` command below
writes a consistent copy into the volume, so keep at least three database sizes free). To enforce it, add
`PodmanArgs=--memory=<limit>` to the `[Container]` section; a limit below the peak makes the kernel kill the
manager during a fleet-wide publish. The 1000 / 512 MiB and 2000 / 768 MiB rows were checked with the manager
inside this image under that hard limit (`fleet.mjs --container … --memory …`): peak cgroup memory 89 MiB and
183 MiB, no OOM kill, graceful stop with every node connected in about a second. Database growth is bounded (report history, events, access and audit logs are
trimmed), so disk use grows with node count, not with time. Above 2000 nodes, raise `MAX_STREAM_HANDSHAKES_PER_SECOND`
only together with CPU, and measure with the load test first.

## Files

| File | Install to |
| --- | --- |
| `meduza-np-manager.container` | `~/.config/containers/systemd/` |
| `meduza-np-manager-data.volume`, `meduza-np-manager.network` | `~/.config/containers/systemd/` |
| `manager.env.example` → `manager.env` | `~/.config/meduza-np-manager/` |
| `config.json.example` → `config.json` | `~/.config/meduza-np-manager/` (read-only mount) |
| `proxy/Caddyfile` or `proxy/nginx.conf` | your proxy configuration |
| `compose.yml`, `.env.compose.example` → `.env` | an alternative Compose deployment directory (see below) |

## First installation

```sh
mkdir -p ~/.config/containers/systemd ~/.config/meduza-np-manager
cp meduza-np-manager.container meduza-np-manager-data.volume meduza-np-manager.network ~/.config/containers/systemd/
cp manager.env.example ~/.config/meduza-np-manager/manager.env     # edit PUBLIC_ORIGIN, RP_ID, TRUSTED_PROXIES
cp config.json.example ~/.config/meduza-np-manager/config.json
# Pin the image: replace OWNER and IMAGE_DIGEST in the .container file with the values from the release
# manifest (releases/<group>/manifest.json, package meduza-np-manager, the "oci" field of its artifacts: the
# multi-platform index ghcr.io/<owner>/meduza-np-manager@sha256:..., D30). The tag <version> shows the same image.

# Secrets (never stored in the image, the env file or the database):
openssl rand -hex 32 | podman secret create meduza-np-session-key -
openssl rand -hex 32 | podman secret create meduza-np-secret-key -
openssl rand -base64 33 | tr -d '\n' | podman secret create meduza-np-bootstrap -
```

Back up the two keys now (`podman secret inspect --showsecret`, Podman ≥ 4.9, or keep the generated values in your
password manager). **Without `meduza-np-secret-key` the encrypted secrets in the database are unrecoverable.**

### Register the first administrator

1. Uncomment the `Secret=meduza-np-bootstrap…` line in the `.container` file and add
   `BOOTSTRAP_TOKEN_FILE=/run/secrets/bootstrap` to `manager.env`.
2. `systemctl --user daemon-reload && systemctl --user start meduza-np-manager`
3. Read the token: `podman secret inspect --showsecret meduza-np-bootstrap` and open `PUBLIC_ORIGIN`.
4. In "First administrator" enter the token and a username, then create the passkey.
5. The bootstrap entry closes permanently once an administrator credential exists (and expires 24 h after the
   token is first seen). Remove the secret line, the env entry and the secret:
   `podman secret rm meduza-np-bootstrap && systemctl --user daemon-reload && systemctl --user restart meduza-np-manager`

Further accounts are invited from 账号与 Passkey; each invite link is single use.

### Without systemd

```sh
podman volume create meduza-np-manager-data
podman network create meduza-np-manager
podman run -d --name meduza-np-manager --network meduza-np-manager \
  --env-file ~/.config/meduza-np-manager/manager.env \
  -v meduza-np-manager-data:/var/lib/meduza-np-manager:Z \
  -v ~/.config/meduza-np-manager/config.json:/etc/meduza-np-manager/config.json:ro,Z \
  --secret meduza-np-session-key,type=mount,target=/run/secrets/session.key,mode=0400,uid=10001,gid=10001 \
  --secret meduza-np-secret-key,type=mount,target=/run/secrets/secret.key,mode=0400,uid=10001,gid=10001 \
  -p 127.0.0.1:8787:8787 --read-only --tmpfs /tmp:rw,size=64m --security-opt no-new-privileges --cap-drop ALL \
  --health-cmd "node /app/packages/meduza-np-manager/dist/server/src/healthcheck.js" --health-interval 30s \
  --health-on-failure kill --restart on-failure --stop-timeout 30 \
  ghcr.io/OWNER/meduza-np-manager@sha256:IMAGE_DIGEST
```

## Podman Compose

`compose.yml` is an alternative to the Quadlet unit. Use the native **podman-compose >= 1.6** provider: this
configuration mounts external Podman secrets with target names and UID/GID 10001; Docker Compose's local mode
does not support these external secrets. Do not run the Quadlet unit and Compose deployment at the same time.

Copy `compose.yml` and `.env.compose.example` to a directory on the server, then:

```sh
cp .env.compose.example .env
# Edit .env: MANAGER_IMAGE = the manager artifact's OCI digest from the release manifest;
# PUBLIC_ORIGIN = your public HTTPS origin, e.g. https://manager.example.com.
# Reused manager packages may have a version older than the group tag.
export PODMAN_COMPOSE_PROVIDER=podman-compose

# Once only; keep backups of the two long-term keys as described above.
openssl rand -hex 32 | podman secret create meduza-np-session-key -
openssl rand -hex 32 | podman secret create meduza-np-secret-key -
openssl rand -hex 24 | podman secret create meduza-np-bootstrap -

podman compose -f compose.yml config
podman compose -f compose.yml up -d
curl -fsS http://127.0.0.1:8787/readyz
podman compose -f compose.yml logs -f manager
```

The process runs as 10001, with a read-only root filesystem and a writable named volume
`meduza-np-manager-data`. The database, passkeys and enrollment data survive container re-creation. Set the host
reverse proxy's HTTPS upstream to `127.0.0.1:8787` using `proxy/Caddyfile` or `proxy/nginx.conf`; use the hostname
from `PUBLIC_ORIGIN`. Set `TRUSTED_PROXIES` in `.env` to the proxy address visible inside the container (inspect
`podman network inspect meduza-np-manager` for the gateway), then run `up -d` again. The Compose healthcheck is
explicit because OCI images may omit the Containerfile healthcheck.

Read the first-admin token with `podman secret inspect --showsecret meduza-np-bootstrap` (Podman >= 4.9), open
`PUBLIC_ORIGIN` in a browser and register a passkey. Then remove `BOOTSTRAP_TOKEN_FILE`, the `meduza-np-bootstrap`
service secret entry, and its top-level declaration from `compose.yml`; run
`podman compose -f compose.yml up -d --force-recreate` and `podman secret rm meduza-np-bootstrap`.

Use `podman compose -f compose.yml down` to remove containers; retain the data volume and the two long-term
secrets. Upgrade by backing up the database/keys, changing `MANAGER_IMAGE` in `.env`, then running
`podman compose -f compose.yml pull` and `podman compose -f compose.yml up -d`.

Compose starts detached on a headless server. For startup after a host reboot, use the Quadlet/systemd setup
above; Compose's restart policy on its own does not establish rootless boot startup.

## Operations

| Task | Command |
| --- | --- |
| Status / logs | `systemctl --user status meduza-np-manager`, `journalctl --user -u meduza-np-manager -f` |
| Health | `curl -fsS http://127.0.0.1:8787/readyz` (`/healthz` = process alive) |
| Stop / start | `systemctl --user stop meduza-np-manager` (SIGTERM: streams close, requests drain, DB closes) |
| Rebuild container | `systemctl --user restart meduza-np-manager` — data, passkeys and node identities live in the volume |
| Admin status | `podman exec meduza-np-manager node /app/packages/meduza-np-manager/dist/server/src/admin.js status` |

Nodes reconnect after a restart with jittered exponential backoff; the manager answers early with 429/503 and
`Retry-After` while it is warming up, so a restart never triggers a reconnection storm.

### Backup

Online, consistent (SQLite `VACUUM INTO`):

```sh
podman exec meduza-np-manager node /app/packages/meduza-np-manager/dist/server/src/admin.js backup /var/lib/meduza-np-manager/backups/manual.sqlite
podman volume export meduza-np-manager-data > manager-volume.tar     # or copy the backups/ directory out
```

A complete backup is: the SQLite backup file **plus** `meduza-np-secret-key` and `meduza-np-session-key`, plus
the values of `PUBLIC_ORIGIN` and `RP_ID` (passkeys are bound to the RP ID). Do not copy the live
`manager.sqlite` file while the server runs.

### Restore

1. Stop the service. 2. Recreate the two secrets with their original values. 3. Restore the volume
(`podman volume import meduza-np-manager-data manager-volume.tar`) or copy the backup to
`/var/lib/meduza-np-manager/manager.sqlite` (remove `-wal`/`-shm` files). 4. Start with the same `PUBLIC_ORIGIN`
and `RP_ID`. Existing passkeys and node identities keep working; nodes resync automatically.

### Upgrade

1. Read the release notes (migration notes are in the release manifest).
2. Take a backup (above). The new version also writes `backups/pre-migration-v<N>-*.sqlite` before migrating.
3. Change `Image=…@sha256:<new digest>`, `systemctl --user daemon-reload && systemctl --user restart meduza-np-manager`.
4. Check `/readyz` and the overview page; nodes keep running their last configuration meanwhile.

### Rollback

If the new version fails before migrating, set the previous digest again. If it already migrated the
database, an older image refuses to start on the newer schema (by design); restore the
`pre-migration-v<N>-*.sqlite` backup into the volume, then start the previous digest.

### Lost passkeys

An operator with host access issues a one-hour, audited recovery invite for an existing administrator:

```sh
podman exec meduza-np-manager node /app/packages/meduza-np-manager/dist/server/src/admin.js recovery-invite <admin-username>
```

### Rootless UID/GID and SELinux

The process runs as UID/GID 10001 inside the container; in rootless mode this maps to a subordinate UID of your
user, which owns the named volume. `:Z` relabels mounted content for SELinux. If you use a host directory
instead of the named volume, prepare it with `podman unshare chown 10001:10001 <dir>`.

### LDAP outages

OpenLDAP being unreachable only affects new OpenVPN Access logins (denied with `ldap-unavailable`); Passkey
login, health checks and node sync are unaffected, and the container is not restarted.
