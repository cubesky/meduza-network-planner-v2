// SQLite persistence (AGENT.md §6.1) on the built-in node:sqlite driver. Migrations are ordered, run inside a
// transaction each, and recorded in schema_migrations; startup refuses a database newer than this build.

import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

export type Row = Record<string, SQLInputValue | null>;

const MIGRATIONS: string[] = [
  // 1 — identity, nodes, configuration, sync, jobs, secrets, PKI, Access, audit
  `
  CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

  CREATE TABLE accounts (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin','operator','viewer')),
    webauthn_user_id TEXT NOT NULL UNIQUE,
    disabled INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
  );

  CREATE TABLE passkeys (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    public_key BLOB NOT NULL,
    counter INTEGER NOT NULL DEFAULT 0,
    transports TEXT NOT NULL DEFAULT '[]',
    device_type TEXT NOT NULL,
    backed_up INTEGER NOT NULL DEFAULT 0,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_used_at TEXT,
    revoked_at TEXT
  );
  CREATE INDEX passkeys_account ON passkeys(account_id);

  CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    csrf_token TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    last_uv_at TEXT NOT NULL,
    credential_id TEXT,
    revoked_at TEXT,
    user_agent TEXT,
    ip TEXT
  );
  CREATE INDEX sessions_account ON sessions(account_id);

  CREATE TABLE webauthn_challenges (
    id TEXT PRIMARY KEY,
    purpose TEXT NOT NULL CHECK (purpose IN ('bootstrap','register','authenticate','reauth')),
    challenge TEXT NOT NULL,
    account_id TEXT,
    session_id TEXT,
    data TEXT NOT NULL DEFAULT '{}',
    expires_at TEXT NOT NULL,
    consumed_at TEXT
  );

  CREATE TABLE invites (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    created_by TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at TEXT
  );

  CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    behavior TEXT NOT NULL DEFAULT 'static' CHECK (behavior IN ('static','roaming')),
    tags TEXT NOT NULL DEFAULT '[]',
    public_key TEXT,
    enrolled_at TEXT,
    hostname TEXT,
    arch TEXT,
    planner_version TEXT,
    last_seen_at TEXT,
    revoked_at TEXT,
    created_at TEXT NOT NULL
  );

  CREATE TABLE enrollment_tokens (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    created_by TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at TEXT
  );

  CREATE TABLE node_challenges (
    nonce TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    expires_at TEXT NOT NULL
  );

  CREATE TABLE node_tokens (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    expires_at TEXT NOT NULL,
    revoked_at TEXT
  );

  CREATE TABLE drafts (
    id TEXT PRIMARY KEY,
    base_revision INTEGER,
    document TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open','published','discarded'))
  );

  CREATE TABLE cluster_revisions (
    number INTEGER PRIMARY KEY,
    revision TEXT NOT NULL UNIQUE,
    document TEXT NOT NULL,
    document_digest TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('publish','rollback')),
    rollback_of TEXT,
    message TEXT,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL
  );

  CREATE TABLE node_snapshots (
    node_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    config TEXT NOT NULL,
    digest TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    secrets_ciphertext BLOB,
    secrets_nonce BLOB,
    created_at TEXT NOT NULL,
    PRIMARY KEY (node_id, revision)
  );

  CREATE TABLE node_desired (
    node_id TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    digest TEXT NOT NULL,
    content_digest TEXT NOT NULL,
    published_in TEXT NOT NULL,
    updated_at TEXT NOT NULL
  );

  CREATE TABLE publish_results (
    publish_revision TEXT NOT NULL,
    node_id TEXT NOT NULL,
    node_revision TEXT NOT NULL,
    changed INTEGER NOT NULL,
    stage TEXT NOT NULL DEFAULT 'pending',
    error_code TEXT,
    message TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (publish_revision, node_id)
  );

  CREATE TABLE acks (
    node_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    stage TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    config_digest TEXT NOT NULL,
    error_code TEXT,
    message TEXT,
    instances TEXT NOT NULL DEFAULT '[]',
    received_at TEXT NOT NULL,
    PRIMARY KEY (node_id, revision, stage, boot_id)
  );

  CREATE TABLE node_sync (
    node_id TEXT PRIMARY KEY,
    received_revision TEXT,
    validated_revision TEXT,
    applied_revision TEXT,
    failed_revision TEXT,
    last_error TEXT,
    updated_at TEXT NOT NULL
  );

  CREATE TABLE node_events (
    node_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    type TEXT NOT NULL,
    revision TEXT,
    digest TEXT,
    job_id TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY (node_id, seq)
  );

  CREATE TABLE node_reports (
    node_id TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    observed_at TEXT NOT NULL,
    received_at TEXT NOT NULL,
    previous_boots TEXT NOT NULL DEFAULT '[]',
    report TEXT NOT NULL
  );

  CREATE TABLE report_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id TEXT NOT NULL,
    received_at TEXT NOT NULL,
    summary TEXT NOT NULL
  );
  CREATE INDEX report_history_node ON report_history(node_id, id);

  CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    package TEXT,
    instance TEXT,
    params TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','delivered','succeeded','failed','expired','cancelled')),
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    delivered_at TEXT,
    finished_at TEXT,
    result TEXT,
    error_code TEXT,
    message TEXT
  );
  CREATE INDEX jobs_node_status ON jobs(node_id, status);

  CREATE TABLE secrets (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    node_scope TEXT NOT NULL DEFAULT '[]',
    holder TEXT NOT NULL DEFAULT 'manager' CHECK (holder IN ('manager','node')),
    holder_node TEXT,
    ciphertext BLOB,
    nonce BLOB,
    public_value TEXT,
    created_at TEXT NOT NULL,
    rotated_at TEXT,
    version INTEGER NOT NULL DEFAULT 1
  );

  CREATE TABLE pki_cas (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cert_pem TEXT NOT NULL,
    key_secret_id TEXT NOT NULL,
    crl_number INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
  );

  CREATE TABLE pki_certs (
    serial TEXT PRIMARY KEY,
    ca_id TEXT NOT NULL REFERENCES pki_cas(id),
    subject TEXT NOT NULL,
    usage TEXT NOT NULL CHECK (usage IN ('server','client')),
    not_before TEXT NOT NULL,
    not_after TEXT NOT NULL,
    cert_pem TEXT NOT NULL,
    key_secret_id TEXT,
    revoked_at TEXT,
    revocation_reason TEXT,
    created_at TEXT NOT NULL
  );

  CREATE TABLE access_policies (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    groups TEXT NOT NULL DEFAULT '[]',
    nodes TEXT NOT NULL DEFAULT '[]',
    instances TEXT NOT NULL DEFAULT '[]',
    session_seconds INTEGER NOT NULL DEFAULT 3600,
    priority INTEGER NOT NULL DEFAULT 100
  );

  CREATE TABLE access_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    node_id TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    username TEXT NOT NULL,
    decision TEXT NOT NULL,
    reason_code TEXT NOT NULL,
    duration_ms INTEGER NOT NULL
  );

  CREATE TABLE audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    detail TEXT NOT NULL DEFAULT '{}'
  );
  `,
  // 2 — node dashboard sign-in through the manager identity (dashboard.ts)
  `
  CREATE TABLE dashboard_logins (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    nonce_hash TEXT NOT NULL,
    return_url TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    code_hash TEXT,
    account_id TEXT,
    username TEXT,
    role TEXT,
    used INTEGER NOT NULL DEFAULT 0
  );
  CREATE INDEX dashboard_logins_node ON dashboard_logins(node_id, used);
  `,
  // 3 — per-account UI settings (JSON object; today the interface language)
  `
  ALTER TABLE accounts ADD COLUMN settings TEXT NOT NULL DEFAULT '{}';
  `,
  // 4 — node-held keys requested before their package is on the node (nodekeys.ts, D28)
  `
  CREATE TABLE key_requests (
    secret_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    package TEXT NOT NULL,
    replace INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    job_id TEXT,
    requested_by TEXT NOT NULL,
    created_at TEXT NOT NULL
  );
  CREATE INDEX key_requests_node ON key_requests(node_id);
  `,
  // 5 — Access re-authorization finds the session's password login (access.ts, D38)
  `
  CREATE INDEX access_log_session ON access_log(node_id, instance_id, username, id);
  `,
];

export const SCHEMA_VERSION = MIGRATIONS.length;

export class Db {
  readonly sql: DatabaseSync;

  constructor(path: string) {
    this.sql = new DatabaseSync(path);
    this.sql.exec("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000; PRAGMA synchronous = NORMAL;");
  }

  static open(dataDir: string): Db {
    mkdirSync(dataDir, { recursive: true, mode: 0o700 });
    return new Db(join(dataDir, "manager.sqlite"));
  }

  static memory(): Db {
    return new Db(":memory:");
  }

  currentVersion(): number {
    this.sql.exec("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)");
    const row = this.sql.prepare("SELECT MAX(version) AS v FROM schema_migrations").get() as { v: number | null } | undefined;
    return row?.v ?? 0;
  }

  /** Applies pending migrations. Returns the versions applied. */
  migrate(): number[] {
    const current = this.currentVersion();
    if (current > MIGRATIONS.length) {
      throw new Error(`database schema ${current} is newer than this build (${MIGRATIONS.length}); refusing to start`);
    }
    const applied: number[] = [];
    for (let v = current + 1; v <= MIGRATIONS.length; v++) {
      this.tx(() => {
        this.sql.exec(MIGRATIONS[v - 1]!);
        this.sql.prepare("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)").run(v, new Date().toISOString());
      });
      applied.push(v);
    }
    return applied;
  }

  tx<T>(fn: () => T): T {
    this.sql.exec("BEGIN IMMEDIATE");
    try {
      const result = fn();
      this.sql.exec("COMMIT");
      return result;
    } catch (e) {
      this.sql.exec("ROLLBACK");
      throw e;
    }
  }

  get<T = Row>(query: string, ...params: SQLInputValue[]): T | undefined {
    return this.sql.prepare(query).get(...params) as T | undefined;
  }

  all<T = Row>(query: string, ...params: SQLInputValue[]): T[] {
    return this.sql.prepare(query).all(...params) as T[];
  }

  run(query: string, ...params: SQLInputValue[]): { changes: number; lastInsertRowid: number } {
    const r = this.sql.prepare(query).run(...params);
    return { changes: Number(r.changes), lastInsertRowid: Number(r.lastInsertRowid) };
  }

  setting(key: string): string | undefined {
    return this.get<{ value: string }>("SELECT value FROM settings WHERE key = ?", key)?.value;
  }

  setSetting(key: string, value: string): void {
    this.run("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value", key, value);
  }

  /** Consistent online backup (SQLite VACUUM INTO), safe while the server runs (§14.4.2). */
  backupTo(path: string): void {
    this.sql.prepare("VACUUM INTO ?").run(path);
  }

  close(): void {
    this.sql.close();
  }
}

export function nowIso(): string {
  return new Date().toISOString();
}

export function isoIn(seconds: number): string {
  return new Date(Date.now() + seconds * 1000).toISOString();
}
