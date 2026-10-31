# Meduza Network Planner (Meduza NP)

A plugin-based network planner and node controller: the **manager** (web console, configuration, sync, PKI, access)
and the **planner** on each node (Linux, Podman container, OpenWrt) that generates and runs WireGuard, Tinc, FRR,
OpenVPN, Mihomo, MosDNS and more from the published configuration.

- Specification: [AGENT.md](AGENT.md) (Chinese); design decisions: [docs/DECISIONS.md](docs/DECISIONS.md);
  installation and operation: [docs/operations.md](docs/operations.md); progress log: [PROGRESS.md](PROGRESS.md).
- Layout: `crates/` (protocol, plugin SDK, runtime), `packages/` (planner, updater, CLI, plugins, manager,
  dashboards, provider recipes), `web/` (shared UI kit and descriptor renderer), `schemas/`, `tools/` (CI, release
  and lab scripts), `tests/`.

## Build and test

Rust 1.99.1 (`rust-toolchain.toml`), Node 24 and pnpm 12 (`packageManager` in `package.json`).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
pnpm install --frozen-lockfile
pnpm -r run typecheck && pnpm -r run build && MEDUZA_NP_SKIP_E2E=1 pnpm -r run test
```

## Creating a new repository from a source pack

The CI (`.github/workflows/ci.yml`) runs on every push to `main` and on pull requests; the release workflow
(`release.yml`) only on `v*` tags or by hand, and needs the `MEDUZA_NP_RELEASE_KEY` secret (see its header).

1. Create an **empty** repository on GitHub (no README, licence or .gitignore, so the first push needs no merge).
2. Unpack the pack and, inside `meduza-network-planner-v2/`:

   ```sh
   git init -b main
   git add .
   sh tools/fix-exec-bits.sh      # needed when adding on Windows: git records new files there as non-executable
   sh tools/fix-exec-bits.sh --check
   git commit -m "Import Meduza NP sources"
   git remote add origin https://github.com/<owner>/<repo>.git
   git push -u origin main
   ```

   On Windows run the `sh` lines in Git Bash (installed with Git for Windows).
3. The push starts the `ci` workflow (Actions tab). Actions must be enabled for the repository; on a fork they are off
   until enabled once.
