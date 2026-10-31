// Process entry: config → database migration (with pre-migration backup) → HTTP server → graceful SIGTERM.

import { existsSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { loadConfig, ConfigError } from "./config.js";
import { Db, SCHEMA_VERSION } from "./db.js";
import { buildApp, createServices } from "./app.js";

const here = dirname(fileURLToPath(import.meta.url));

async function main(): Promise<void> {
  let cfg;
  try {
    cfg = loadConfig();
  } catch (e) {
    if (e instanceof ConfigError) {
      console.error(`configuration error: ${e.message}`);
      process.exit(2);
    }
    throw e;
  }

  const db = Db.open(cfg.dataDir);
  const current = db.currentVersion();
  if (current > 0 && current < SCHEMA_VERSION) {
    // Consistent backup before migrating so a failed upgrade can be rolled back with the previous image (§14.4.3).
    const dir = join(cfg.dataDir, "backups");
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const file = join(dir, `pre-migration-v${current}-${new Date().toISOString().replace(/[:.]/g, "-")}.sqlite`);
    db.backupTo(file);
    console.log(`backup before migration written to ${file}`);
  }
  const applied = db.migrate();
  if (applied.length) console.log(`applied database migrations ${applied.join(", ")}`);

  const schemasDir = process.env.SCHEMAS_DIR ?? [join(here, "../../../../../schemas"), "/usr/share/meduza-np-manager/schemas"].find((p) => existsSync(p)) ?? "";
  let ready = false;
  let stopping = false;
  const services = { ...createServices(cfg, db, schemasDir), ready: () => ready, shuttingDown: () => stopping };
  const app = await buildApp(services);
  await app.listen({ host: cfg.listenHost, port: cfg.port });
  ready = true;

  const shutdown = async (signal: string) => {
    if (stopping) return;
    stopping = true;
    app.log.info({ signal }, "shutting down: closing streams and draining requests");
    services.hub.closeAll("manager shutting down");
    const timer = setTimeout(() => {
      app.server.getConnections((_err, connections) => {
        app.log.error({ connections, streams: services.hub.size }, "shutdown timed out after 20 s, exiting");
        process.exit(1);
      });
    }, 20_000);
    timer.unref();
    try {
      await app.close();
    } finally {
      db.close();
      process.exit(0);
    }
  };
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
  process.on("SIGINT", () => void shutdown("SIGINT"));
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
