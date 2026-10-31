# Manager fleet load test

`fleet.mjs` starts a real manager process (fresh data directory, `probe.mjs` preloaded to report RSS/heap/CPU over
IPC), bootstraps an admin with a software passkey, creates and enrolls N simulated planners and drives them like
the real sync client: one WebSocket per static node, config pull + received/validated/applied ACKs after each
notification, periodic reports, ETag polling for roaming nodes, Retry-After honoured, jittered exponential backoff
(base 2 s, cap 300 s, level rises at most once per window, ends early when the channel is healthy again) and the
startup stagger after a full channel loss.

```bash
pnpm --filter @meduza-np/manager build
node tests/load/fleet.mjs --nodes 500 --restart --out results.json
```

Options: `--nodes` (200), `--ramp` s (30), `--duration` steady s (60), `--report-interval` s (30), `--roaming`
ratio (0.1), `--poll-interval` s (60), `--startup-spread` s (30), `--restart`, `--env KEY=VALUE` (manager limits,
repeatable), `--out`, `--keep` (keep data dir + manager.log).

Container mode (Linux, Podman, cgroup v2): `--container IMAGE [--memory 512m] [--container-cpus 2]` runs the
production image instead of a bare process, hardened like `deploy/podman/meduza-np-manager.container` (read-only
root, tmpfs /tmp, no capabilities, no-new-privileges, Podman secrets for uid 10001, named data volume, loopback
port), with `--memory` as a hard limit (no swap). Memory then comes from the container's cgroup
(`memory.current` incl. page cache, `anon`) and the node process' VmRSS, CPU from the cgroup's `cpu.stat`; there is
no heap figure. The restart is `podman stop -t 25` + `podman start`, and the result records each stop's exit code,
`OOMKilled` and the cgroup's `oom_kill` count. Container, volume and secrets are removed unless `--keep`.

```bash
podman build -f packages/meduza-np-manager/Containerfile -t localhost/meduza-np-manager:load .
node tests/load/fleet.mjs --container localhost/meduza-np-manager:load --memory 512m --nodes 1000 --restart
```

Phases: enroll → connect → publish to all → steady state → publish to one → (restart → reconnect → publish to all).
The JSON result has connect/enroll times, fan-out latency (push vs poll), per-route latency percentiles, overload
answers (429/503, rejected handshakes), manager memory/CPU per phase and the data directory size.

On Windows the restart is a hard kill (no graceful drain); run on Linux for the SIGTERM path.

## Recorded results

| Date | Host | Nodes | Steady RSS | Peak RSS | Steady CPU | Fan-out p95 (push) | Restart: all back | DB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 2026-10-23 | Windows 10 VM, Xeon 8269CY 8 vCPU, Node 24.22 | 300 (10 % roaming), report 30 s | 92 MiB | 137 MiB | 0.8 % | 1.4 s | 27 s (0 × 429/503) | 6.1 MiB |
| 2026-10-24 | Debian 13 VPS, Broadwell 2 vCPU, 4 GiB, Node 24.22 | 1000 (10 % roaming), report 30 s | 163 MiB | 227 MiB | 4.3 % | 3.8 s | 29 s (0 × 429/503) | 6.4 MiB |
| 2026-10-24 | same VPS | 2000 (10 % roaming), report 30 s | 239 MiB | 363 MiB | 8.2 % | 11.3 s | 57 s (684 handshakes answered 503, retried) | 12.4 MiB |
| 2026-10-24 | same VPS, **container** `--memory 512m` (Podman 5.4 rootful, cgroup v2) | 1000 (10 % roaming), report 30 s | 117 MiB RSS / 77 MiB cgroup | 126 MiB RSS / 89 MiB cgroup | 7.0 % | 5.5 s | 28 s (0 × 429/503); stops 0.7 s / 0.5 s, exit 0, 0 OOM kills | 6.5 MiB |
| 2026-10-24 | same VPS, **container** `--memory 768m` | 2000 (10 % roaming), report 30 s | 142 MiB RSS / 111 MiB cgroup | 202 MiB RSS / 183 MiB cgroup | 10.9 % | 12.5 s | 57 s (584 handshakes answered 503, retried); stops 1.1 s / 1.0 s, exit 0, 0 OOM kills | 12.4 MiB |

The Linux runs are bare `node` processes started by `fleet.mjs` (same Node and code as the image, no cgroup limit),
with the simulated fleet on the same 2 vCPUs, so fan-out latencies there are pessimistic; the manager's own CPU share
is the "Steady CPU" column (percent of one core, averaged over the steady phase). Steady RSS is the value at the end
of the steady phase, peak RSS the maximum over the whole run (reached while publishing to all nodes). During the
2000-node run a 15 s `cargo test` ran on the host in the steady phase.

The container runs use the production image hardened like the Quadlet unit, with the recommended limit from
`deploy/podman/README.md` as a hard cgroup limit (`memory.max`, no swap). Peak cgroup memory (anon + page cache
charged to the container) stayed at 17 % (1000) and 24 % (2000) of the limit, and both graceful stops (SIGTERM
with every stream open, then `podman start`) exited 0 in about a second. In the container the process RSS was
lower than in the bare runs at the same load; the bare and container figures are not directly comparable (different
sampling: IPC probe vs `/proc` + cgroup files, and the bare RSS includes the probe), so size from the larger one.

Two load-test pitfalls found on the way (fixed in `fleet.mjs`): the Podman calls must not block the event loop
(a synchronous `podman stop` froze every simulated planner, so nobody answered the WebSocket close frame and the
manager ran into its 20 s shutdown timeout, exit 1), and the simulated planners need the planner's timeouts
(10 s WebSocket handshake, 20 s per request): while the container is stopped, conmon keeps the published host
port listening without accepting, so a connection made in that window never gets an answer.

At 2000 nodes the reconnect after a restart is bounded by the WebSocket handshake limit (50/s by default): the
excess handshakes are answered 503 with Retry-After and the planners come back through their backoff, without 429s
on the HTTP routes. Publishing to every node costs 1.9–2.4 s of manager time at 2000 nodes (one snapshot per node).
