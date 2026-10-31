// Manager login with Passkeys only (AGENT.md §6.4). No password or LDAP fallback.
//
// - First admin: one-time bootstrap token from BOOTSTRAP_TOKEN_FILE, valid for 24 h after first seen, closed
//   for good once an admin credential exists.
// - Later accounts: created by an admin together with a single-use, expiring invite; or an authenticated user
//   adding another passkey to their own account.
// - Discoverable credentials, userVerification=required for registration and login.
// - Challenges: random, 5 min TTL, single use, bound to purpose (+ account / session).
// - Sessions: random cookie token, stored as HMAC(SESSION_SECRET, token); CSRF token + Origin check on writes.

import { createHmac, timingSafeEqual, randomBytes } from "node:crypto";
import {
  generateAuthenticationOptions,
  generateRegistrationOptions,
  verifyAuthenticationResponse,
  verifyRegistrationResponse,
  type AuthenticationResponseJSON,
  type AuthenticatorTransportFuture,
  type RegistrationResponseJSON,
} from "@simplewebauthn/server";
import type { Db } from "./db.js";
import { isoIn, nowIso } from "./db.js";
import type { Audit } from "./audit.js";
import { randomId, randomToken } from "./audit.js";
import type { ManagerConfig } from "./config.js";

export type Role = "admin" | "operator" | "viewer";
const ROLE_RANK: Record<Role, number> = { viewer: 0, operator: 1, admin: 2 };
export function roleAtLeast(have: Role, need: Role): boolean {
  return ROLE_RANK[have] >= ROLE_RANK[need];
}

export interface SessionInfo {
  sessionId: string;
  accountId: string;
  username: string;
  displayName: string;
  role: Role;
  csrfToken: string;
  lastUvAt: string;
  settings: AccountSettings;
}

/** Interface languages of the manager web UI (BCP 47). */
export const LOCALES = ["en", "zh-Hans", "zh-Hant"] as const;
export type Locale = (typeof LOCALES)[number];

/** Personal preferences of an account. `locale` absent: the UI follows the browser language. */
export interface AccountSettings {
  locale?: Locale;
}

function parseSettings(text: string | null | undefined): AccountSettings {
  try {
    const v = JSON.parse(text ?? "{}") as Record<string, unknown>;
    return LOCALES.includes(v.locale as Locale) ? { locale: v.locale as Locale } : {};
  } catch {
    return {};
  }
}

export class AuthError extends Error {
  constructor(readonly status: number, readonly code: string, message: string) {
    super(message);
  }
}

const CHALLENGE_TTL = 300;
const BOOTSTRAP_WINDOW = 24 * 3600;
const USERNAME_RE = /^[a-z0-9][a-z0-9._-]{1,63}$/;

export class AuthService {
  constructor(private readonly db: Db, private readonly cfg: ManagerConfig, private readonly audit: Audit) {}

  private hashToken(token: string): string {
    return createHmac("sha256", this.cfg.sessionSecret).update(token).digest("base64url");
  }

  // ---- bootstrap ---------------------------------------------------------------------------------------

  bootstrapState(): "completed" | "open" | "unavailable" | "expired" {
    if (this.db.setting("bootstrap_completed") === "1") return "completed";
    if (!this.cfg.bootstrapToken) return "unavailable";
    const key = `bootstrap_first_seen:${this.hashToken(this.cfg.bootstrapToken)}`;
    let first = this.db.setting(key);
    if (!first) {
      first = nowIso();
      this.db.setSetting(key, first);
    }
    return Date.now() - Date.parse(first) > BOOTSTRAP_WINDOW * 1000 ? "expired" : "open";
  }

  private checkBootstrapToken(token: string): void {
    const state = this.bootstrapState();
    if (state !== "open") throw new AuthError(410, "bootstrap-closed", `bootstrap is ${state}`);
    const a = Buffer.from(this.hashToken(token));
    const b = Buffer.from(this.hashToken(this.cfg.bootstrapToken!));
    if (a.length !== b.length || !timingSafeEqual(a, b)) throw new AuthError(403, "bad-bootstrap-token", "invalid bootstrap token");
  }

  // ---- registration ------------------------------------------------------------------------------------

  /**
   * Starts a registration ceremony. Exactly one authorization source is used:
   * bootstrap token (first admin), invite token (pre-created account) or an authenticated session (own account).
   */
  async registrationOptions(input: {
    bootstrapToken?: string;
    username?: string;
    displayName?: string;
    inviteToken?: string;
    session?: SessionInfo;
  }) {
    let accountId: string;
    let purpose: "bootstrap" | "register";
    let data: Record<string, unknown> = {};
    if (input.bootstrapToken) {
      this.checkBootstrapToken(input.bootstrapToken);
      const username = (input.username ?? "").trim().toLowerCase();
      if (!USERNAME_RE.test(username)) throw new AuthError(400, "invalid-username", "username: 2-64 chars a-z 0-9 . _ -");
      const existing = this.db.get<{ id: string; role: string }>("SELECT id, role FROM accounts WHERE username = ?", username);
      if (existing && existing.role !== "admin") throw new AuthError(409, "username-taken", "username belongs to a non-admin account");
      accountId = existing?.id ?? this.createAccount(username, input.displayName?.trim() || username, "admin", "bootstrap");
      purpose = "bootstrap";
    } else if (input.inviteToken) {
      const invite = this.db.get<{ id: string; account_id: string; expires_at: string; used_at: string | null }>(
        "SELECT * FROM invites WHERE id = ?", this.hashToken(input.inviteToken));
      if (!invite || invite.used_at || Date.parse(invite.expires_at) < Date.now()) throw new AuthError(403, "bad-invite", "invite is invalid or expired");
      accountId = invite.account_id;
      purpose = "register";
      data = { inviteId: invite.id };
    } else if (input.session) {
      this.requireRecentUv(input.session);
      accountId = input.session.accountId;
      purpose = "register";
      data = { sessionId: input.session.sessionId };
    } else {
      throw new AuthError(401, "unauthorized", "registration needs a bootstrap token, an invite or a session");
    }
    const account = this.account(accountId)!;
    if (account.disabled) throw new AuthError(403, "account-disabled", "account is disabled");
    const existing = this.db.all<{ id: string; transports: string }>("SELECT id, transports FROM passkeys WHERE account_id = ? AND revoked_at IS NULL", accountId);
    const options = await generateRegistrationOptions({
      rpName: this.cfg.rpName,
      rpID: this.cfg.rpId,
      userName: account.username,
      userDisplayName: account.displayName,
      userID: Buffer.from(account.webauthnUserId, "base64url"),
      attestationType: "none",
      excludeCredentials: existing.map((c) => ({ id: c.id, transports: JSON.parse(c.transports) as AuthenticatorTransportFuture[] })),
      authenticatorSelection: { residentKey: "required", requireResidentKey: true, userVerification: "required" },
      timeout: CHALLENGE_TTL * 1000,
    });
    const challengeId = this.storeChallenge(purpose, options.challenge, accountId, input.session?.sessionId ?? null, data);
    return { challengeId, options };
  }

  async registrationVerify(input: { challengeId: string; response: RegistrationResponseJSON; name?: string; ip?: string; userAgent?: string }) {
    const ch = this.consumeChallenge(input.challengeId, ["bootstrap", "register"]);
    if (ch.purpose === "bootstrap") this.checkBootstrapOpenOnly();
    let verification;
    try {
      verification = await verifyRegistrationResponse({
        response: input.response,
        expectedChallenge: ch.challenge,
        expectedOrigin: this.cfg.publicOrigin,
        expectedRPID: this.cfg.rpId,
        requireUserVerification: true,
      });
    } catch (e) {
      throw new AuthError(400, "verification-failed", (e as Error).message);
    }
    if (!verification.verified || !verification.registrationInfo) throw new AuthError(400, "verification-failed", "registration not verified");
    const info = verification.registrationInfo;
    const cred = info.credential;
    if (this.db.get("SELECT 1 AS x FROM passkeys WHERE id = ?", cred.id)) throw new AuthError(409, "credential-exists", "credential already registered");
    const name = (input.name ?? "").trim().slice(0, 64) || `Passkey ${new Date().toISOString().slice(0, 10)}`;
    this.db.tx(() => {
      this.db.run(
        "INSERT INTO passkeys (id, account_id, public_key, counter, transports, device_type, backed_up, name, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        cred.id, ch.accountId!, Buffer.from(cred.publicKey), cred.counter, JSON.stringify(cred.transports ?? []),
        info.credentialDeviceType, info.credentialBackedUp ? 1 : 0, name, nowIso(),
      );
      const inviteId = ch.data.inviteId as string | undefined;
      if (inviteId) this.db.run("UPDATE invites SET used_at = ? WHERE id = ?", nowIso(), inviteId);
      if (ch.purpose === "bootstrap") this.db.setSetting("bootstrap_completed", "1");
    });
    this.audit.record(this.account(ch.accountId!)!.username, ch.purpose === "bootstrap" ? "auth.bootstrap" : "auth.passkey.register", cred.id, { name });
    // Bootstrap and invites end with a signed-in session; adding a key to an existing session does not.
    if (ch.purpose === "bootstrap" || ch.data.inviteId) {
      return { session: this.createSession(ch.accountId!, cred.id, input.ip, input.userAgent) };
    }
    return { session: null };
  }

  private checkBootstrapOpenOnly(): void {
    const state = this.bootstrapState();
    if (state !== "open") throw new AuthError(410, "bootstrap-closed", `bootstrap is ${state}`);
  }

  // ---- authentication ----------------------------------------------------------------------------------

  async authenticationOptions(session?: SessionInfo) {
    const options = await generateAuthenticationOptions({ rpID: this.cfg.rpId, userVerification: "required", timeout: CHALLENGE_TTL * 1000 });
    const purpose = session ? "reauth" : "authenticate";
    const challengeId = this.storeChallenge(purpose, options.challenge, session?.accountId ?? null, session?.sessionId ?? null, {});
    return { challengeId, options };
  }

  async authenticationVerify(input: { challengeId: string; response: AuthenticationResponseJSON; ip?: string; userAgent?: string; session?: SessionInfo }) {
    const ch = this.consumeChallenge(input.challengeId, input.session ? ["reauth"] : ["authenticate"]);
    if (ch.purpose === "reauth" && ch.sessionId !== input.session?.sessionId) throw new AuthError(403, "challenge-mismatch", "challenge belongs to another session");
    const cred = this.db.get<{ id: string; account_id: string; public_key: Uint8Array; counter: number; transports: string; revoked_at: string | null }>(
      "SELECT id, account_id, public_key, counter, transports, revoked_at FROM passkeys WHERE id = ?", input.response.id);
    if (!cred || cred.revoked_at) throw new AuthError(401, "unknown-credential", "credential is not registered");
    const account = this.account(cred.account_id)!;
    if (account.disabled) throw new AuthError(403, "account-disabled", "account is disabled");
    if (ch.purpose === "reauth" && cred.account_id !== ch.accountId) throw new AuthError(403, "wrong-account", "credential belongs to another account");
    let verification;
    try {
      verification = await verifyAuthenticationResponse({
        response: input.response,
        expectedChallenge: ch.challenge,
        expectedOrigin: this.cfg.publicOrigin,
        expectedRPID: this.cfg.rpId,
        requireUserVerification: true,
        credential: {
          id: cred.id,
          publicKey: new Uint8Array(cred.public_key),
          counter: cred.counter,
          transports: JSON.parse(cred.transports) as AuthenticatorTransportFuture[],
        },
      });
    } catch (e) {
      this.audit.record(account.username, "auth.login.failed", cred.id, { reason: (e as Error).message });
      throw new AuthError(401, "verification-failed", (e as Error).message);
    }
    if (!verification.verified) throw new AuthError(401, "verification-failed", "authentication not verified");
    const { newCounter, credentialBackedUp } = verification.authenticationInfo;
    this.db.run("UPDATE passkeys SET counter = ?, backed_up = ?, last_used_at = ? WHERE id = ?", newCounter, credentialBackedUp ? 1 : 0, nowIso(), cred.id);
    if (ch.purpose === "reauth") {
      this.db.run("UPDATE sessions SET last_uv_at = ? WHERE id = ?", nowIso(), input.session!.sessionId);
      this.audit.record(account.username, "auth.reauth", cred.id);
      return { session: null };
    }
    this.audit.record(account.username, "auth.login", cred.id);
    return { session: this.createSession(account.id, cred.id, input.ip, input.userAgent) };
  }

  // ---- challenges --------------------------------------------------------------------------------------

  private storeChallenge(purpose: string, challenge: string, accountId: string | null, sessionId: string | null, data: Record<string, unknown>): string {
    const id = randomId("ch");
    this.db.run("DELETE FROM webauthn_challenges WHERE expires_at < ?", nowIso());
    this.db.run(
      "INSERT INTO webauthn_challenges (id, purpose, challenge, account_id, session_id, data, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
      id, purpose, challenge, accountId, sessionId, JSON.stringify(data), isoIn(CHALLENGE_TTL),
    );
    return id;
  }

  private consumeChallenge(id: string, purposes: string[]) {
    const ch = this.db.get<{ id: string; purpose: string; challenge: string; account_id: string | null; session_id: string | null; data: string; expires_at: string; consumed_at: string | null }>(
      "SELECT * FROM webauthn_challenges WHERE id = ?", id);
    if (!ch || !purposes.includes(ch.purpose)) throw new AuthError(400, "unknown-challenge", "unknown challenge");
    // Single use: mark consumed atomically before verifying, so a replay loses even if verification is slow.
    const r = this.db.run("UPDATE webauthn_challenges SET consumed_at = ? WHERE id = ? AND consumed_at IS NULL", nowIso(), id);
    if (r.changes === 0) throw new AuthError(400, "challenge-used", "challenge already used");
    if (Date.parse(ch.expires_at) < Date.now()) throw new AuthError(400, "challenge-expired", "challenge expired");
    return { purpose: ch.purpose, challenge: ch.challenge, accountId: ch.account_id, sessionId: ch.session_id, data: JSON.parse(ch.data) as Record<string, unknown> };
  }

  // ---- sessions ----------------------------------------------------------------------------------------

  createSession(accountId: string, credentialId: string | null, ip?: string, userAgent?: string): { token: string; info: SessionInfo } {
    const token = randomToken(32);
    const id = this.hashToken(token);
    const csrf = randomToken(24);
    const now = nowIso();
    this.db.run(
      "INSERT INTO sessions (id, account_id, csrf_token, created_at, expires_at, last_seen_at, last_uv_at, credential_id, user_agent, ip) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
      id, accountId, csrf, now, isoIn(this.cfg.sessionTtlSeconds), now, now, credentialId, (userAgent ?? "").slice(0, 200), ip ?? null,
    );
    return { token, info: this.sessionById(id)! };
  }

  private sessionById(id: string): SessionInfo | undefined {
    const r = this.db.get<{ id: string; account_id: string; csrf_token: string; last_uv_at: string; username: string; display_name: string; role: Role; settings: string }>(
      `SELECT s.id, s.account_id, s.csrf_token, s.last_uv_at, a.username, a.display_name, a.role, a.settings
       FROM sessions s JOIN accounts a ON a.id = s.account_id WHERE s.id = ?`, id);
    return r
      ? { sessionId: r.id, accountId: r.account_id, username: r.username, displayName: r.display_name, role: r.role, csrfToken: r.csrf_token, lastUvAt: r.last_uv_at, settings: parseSettings(r.settings) }
      : undefined;
  }

  /** Resolves a session cookie; enforces expiry, idle timeout, revocation and disabled accounts. */
  resolveSession(token: string | undefined): SessionInfo | null {
    if (!token) return null;
    const id = this.hashToken(token);
    const r = this.db.get<{ expires_at: string; last_seen_at: string; revoked_at: string | null; disabled: number }>(
      "SELECT s.expires_at, s.last_seen_at, s.revoked_at, a.disabled FROM sessions s JOIN accounts a ON a.id = s.account_id WHERE s.id = ?", id);
    if (!r || r.revoked_at || r.disabled) return null;
    const now = Date.now();
    if (Date.parse(r.expires_at) < now || now - Date.parse(r.last_seen_at) > this.cfg.sessionIdleSeconds * 1000) return null;
    // Touch at most once a minute to avoid a write per request.
    if (now - Date.parse(r.last_seen_at) > 60_000) this.db.run("UPDATE sessions SET last_seen_at = ? WHERE id = ?", nowIso(), id);
    return this.sessionById(id) ?? null;
  }

  requireRecentUv(session: SessionInfo): void {
    if (Date.now() - Date.parse(session.lastUvAt) > this.cfg.reauthWindowSeconds * 1000) {
      throw new AuthError(403, "reauth-required", "confirm with your passkey to continue");
    }
  }

  /** Updates the signed-in account's own settings; `locale: null` returns to following the browser. */
  updateSettings(session: SessionInfo, patch: { locale?: string | null }): AccountSettings {
    const next: AccountSettings = { ...session.settings };
    if (patch.locale === null) delete next.locale;
    else if (patch.locale !== undefined) {
      if (!LOCALES.includes(patch.locale as Locale)) throw new AuthError(400, "invalid-locale", `locale must be one of ${LOCALES.join(", ")}`);
      next.locale = patch.locale as Locale;
    }
    this.db.run("UPDATE accounts SET settings = ? WHERE id = ?", JSON.stringify(next), session.accountId);
    return next;
  }

  logout(session: SessionInfo): void {
    this.db.run("UPDATE sessions SET revoked_at = ? WHERE id = ?", nowIso(), session.sessionId);
    this.audit.record(session.username, "auth.logout", null);
  }

  revokeSessions(accountId: string, actor: string): number {
    const n = this.db.run("UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL", nowIso(), accountId).changes;
    this.audit.record(actor, "auth.sessions.revoke", accountId, { count: n });
    return n;
  }

  // ---- accounts & credentials --------------------------------------------------------------------------

  account(id: string) {
    const r = this.db.get<{ id: string; username: string; display_name: string; role: Role; webauthn_user_id: string; disabled: number; created_at: string }>(
      "SELECT * FROM accounts WHERE id = ?", id);
    return r ? { id: r.id, username: r.username, displayName: r.display_name, role: r.role, webauthnUserId: r.webauthn_user_id, disabled: !!r.disabled, createdAt: r.created_at } : undefined;
  }

  listAccounts() {
    return this.db.all<Record<string, unknown>>(
      `SELECT a.id, a.username, a.display_name, a.role, a.disabled, a.created_at,
              (SELECT COUNT(*) FROM passkeys p WHERE p.account_id = a.id AND p.revoked_at IS NULL) AS passkeys
       FROM accounts a ORDER BY a.username`);
  }

  private createAccount(username: string, displayName: string, role: Role, actor: string): string {
    const id = randomId("acct");
    this.db.run("INSERT INTO accounts (id, username, display_name, role, webauthn_user_id, created_at) VALUES (?, ?, ?, ?, ?, ?)",
      id, username, displayName, role, randomBytes(32).toString("base64url"), nowIso());
    this.audit.record(actor, "account.create", id, { username, role });
    return id;
  }

  /** Admin creates an account plus a single-use invite for its first passkey. */
  inviteAccount(input: { username: string; displayName?: string; role: Role }, actor: SessionInfo, ttlSeconds = 72 * 3600) {
    const username = input.username.trim().toLowerCase();
    if (!USERNAME_RE.test(username)) throw new AuthError(400, "invalid-username", "username: 2-64 chars a-z 0-9 . _ -");
    if (!ROLE_RANK.hasOwnProperty(input.role)) throw new AuthError(400, "invalid-role", "role must be admin, operator or viewer");
    if (this.db.get("SELECT 1 AS x FROM accounts WHERE username = ?", username)) throw new AuthError(409, "username-taken", "username exists");
    const accountId = this.createAccount(username, input.displayName?.trim() || username, input.role, actor.username);
    return { accountId, ...this.newInvite(accountId, actor, ttlSeconds) };
  }

  newInvite(accountId: string, actor: SessionInfo, ttlSeconds = 72 * 3600) {
    if (!this.account(accountId)) throw new AuthError(404, "not-found", "account not found");
    const token = randomToken(32);
    const expiresAt = isoIn(ttlSeconds);
    this.db.run("INSERT INTO invites (id, account_id, created_by, expires_at) VALUES (?, ?, ?, ?)", this.hashToken(token), accountId, actor.username, expiresAt);
    this.audit.record(actor.username, "account.invite", accountId, { expiresAt });
    return { inviteToken: token, inviteUrl: `${this.cfg.publicOrigin}/#/invite/${token}`, expiresAt };
  }

  private activeAdminsWithCredentials(excludingCredential?: string, excludingAccount?: string): number {
    return this.db.get<{ n: number }>(
      `SELECT COUNT(DISTINCT a.id) AS n FROM accounts a JOIN passkeys p ON p.account_id = a.id
       WHERE a.role = 'admin' AND a.disabled = 0 AND p.revoked_at IS NULL AND p.id != ? AND a.id != ?`,
      excludingCredential ?? "", excludingAccount ?? "")!.n;
  }

  updateAccount(accountId: string, patch: { role?: Role; disabled?: boolean; displayName?: string }, actor: SessionInfo): void {
    const acct = this.account(accountId);
    if (!acct) throw new AuthError(404, "not-found", "account not found");
    const losesAdmin = acct.role === "admin" && ((patch.role && patch.role !== "admin") || patch.disabled === true);
    if (losesAdmin && this.activeAdminsWithCredentials(undefined, accountId) === 0) {
      throw new AuthError(409, "last-admin", "cannot remove the last administrator able to sign in");
    }
    if (patch.role && !ROLE_RANK.hasOwnProperty(patch.role)) throw new AuthError(400, "invalid-role", "invalid role");
    this.db.run("UPDATE accounts SET role = COALESCE(?, role), disabled = COALESCE(?, disabled), display_name = COALESCE(?, display_name) WHERE id = ?",
      patch.role ?? null, patch.disabled === undefined ? null : patch.disabled ? 1 : 0, patch.displayName ?? null, accountId);
    if (patch.disabled) this.revokeSessions(accountId, actor.username);
    this.audit.record(actor.username, "account.update", accountId, patch);
  }

  listPasskeys(accountId: string) {
    return this.db.all<Record<string, unknown>>(
      "SELECT id, name, device_type, backed_up, created_at, last_used_at, revoked_at FROM passkeys WHERE account_id = ? ORDER BY created_at", accountId);
  }

  renamePasskey(credentialId: string, name: string, actor: SessionInfo): void {
    const owner = this.db.get<{ account_id: string }>("SELECT account_id FROM passkeys WHERE id = ?", credentialId);
    if (!owner || (owner.account_id !== actor.accountId && actor.role !== "admin")) throw new AuthError(404, "not-found", "passkey not found");
    this.db.run("UPDATE passkeys SET name = ? WHERE id = ?", name.trim().slice(0, 64), credentialId);
    this.audit.record(actor.username, "auth.passkey.rename", credentialId, { name });
  }

  revokePasskey(credentialId: string, actor: SessionInfo): void {
    const owner = this.db.get<{ account_id: string; revoked_at: string | null }>("SELECT account_id, revoked_at FROM passkeys WHERE id = ?", credentialId);
    if (!owner || owner.revoked_at || (owner.account_id !== actor.accountId && actor.role !== "admin")) throw new AuthError(404, "not-found", "passkey not found");
    const acct = this.account(owner.account_id)!;
    if (acct.role === "admin" && !acct.disabled && this.activeAdminsWithCredentials(credentialId) === 0) {
      throw new AuthError(409, "last-admin", "this is the last passkey of the last administrator");
    }
    this.db.run("UPDATE passkeys SET revoked_at = ? WHERE id = ?", nowIso(), credentialId);
    this.db.run("UPDATE sessions SET revoked_at = ? WHERE credential_id = ? AND revoked_at IS NULL", nowIso(), credentialId);
    this.audit.record(actor.username, "auth.passkey.revoke", credentialId);
  }

  /**
   * Offline recovery run on the server host (`node dist/server/src/admin.js reset-admin <username>`): issues an
   * invite for an existing admin account so a new passkey can be enrolled. Audited; no password path exists.
   */
  offlineRecoveryInvite(username: string): { inviteUrl: string; expiresAt: string } {
    const acct = this.db.get<{ id: string; role: string }>("SELECT id, role FROM accounts WHERE username = ?", username);
    if (!acct || acct.role !== "admin") throw new Error(`no admin account named ${username}`);
    this.db.run("UPDATE accounts SET disabled = 0 WHERE id = ?", acct.id);
    const system: SessionInfo = { sessionId: "", accountId: "", username: "host-recovery", displayName: "", role: "admin", csrfToken: "", lastUvAt: "", settings: {} };
    const { inviteUrl, expiresAt } = this.newInvite(acct.id, system, 3600);
    this.audit.record("host-recovery", "auth.recovery", acct.id, { username });
    return { inviteUrl, expiresAt };
  }
}
