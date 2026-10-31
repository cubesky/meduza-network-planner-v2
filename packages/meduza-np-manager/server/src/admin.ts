// Host-side administration (run inside the container with `podman exec`):
//   node /app/packages/meduza-np-manager/dist/server/src/admin.js status
//   node /app/packages/meduza-np-manager/dist/server/src/admin.js backup /var/lib/meduza-np-manager/backups/manual.sqlite
//   node /app/packages/meduza-np-manager/dist/server/src/admin.js recovery-invite <admin-username>
// Recovery issues a one-hour invite for an existing admin account (audited); there is no password path.

import { join } from "node:path";
import { mkdirSync } from "node:fs";
import { loadConfig } from "./config.js";
import { Db, SCHEMA_VERSION } from "./db.js";
import { Audit } from "./audit.js";
import { AuthService } from "./auth.js";

const [cmd, arg] = process.argv.slice(2);
const cfg = loadConfig();
const db = Db.open(cfg.dataDir);

switch (cmd) {
  case "status": {
    const v = db.currentVersion();
    const admins = db.get<{ n: number }>("SELECT COUNT(*) AS n FROM accounts WHERE role = 'admin' AND disabled = 0")!.n;
    const nodes = db.get<{ n: number }>("SELECT COUNT(*) AS n FROM nodes WHERE revoked_at IS NULL")!.n;
    console.log(JSON.stringify({ schemaVersion: v, buildSchemaVersion: SCHEMA_VERSION, admins, nodes, bootstrap: db.setting("bootstrap_completed") === "1" ? "completed" : "open" }, null, 2));
    break;
  }
  case "backup": {
    const dest = arg ?? join(cfg.dataDir, "backups", `manager-${new Date().toISOString().replace(/[:.]/g, "-")}.sqlite`);
    mkdirSync(join(dest, ".."), { recursive: true, mode: 0o700 });
    db.backupTo(dest);
    new Audit(db).record("host-admin", "backup", dest);
    console.log(`consistent backup written to ${dest}`);
    console.log("Also back up SECRET_ENCRYPTION_KEY_FILE and SESSION_SECRET_FILE separately: without the encryption key the stored secrets cannot be decrypted.");
    break;
  }
  case "recovery-invite": {
    if (!arg) throw new Error("usage: recovery-invite <admin-username>");
    const auth = new AuthService(db, cfg, new Audit(db));
    const r = auth.offlineRecoveryInvite(arg);
    console.log(`recovery invite for ${arg} (expires ${r.expiresAt}):\n${r.inviteUrl}`);
    break;
  }
  default:
    console.error("usage: admin.js status | backup [dest] | recovery-invite <admin-username>");
    process.exitCode = 2;
}
db.close();
