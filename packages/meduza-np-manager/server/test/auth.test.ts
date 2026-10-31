import { test } from "node:test";
import assert from "node:assert/strict";
import { startManager, SoftAuthenticator, BOOTSTRAP_TOKEN, ORIGIN, cookieFrom } from "./helpers.js";

const headers = { origin: ORIGIN, "content-type": "application/json" };

async function bootstrapAdmin(app: Awaited<ReturnType<typeof startManager>>["app"], authn: SoftAuthenticator) {
  const opt = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers, payload: { bootstrapToken: BOOTSTRAP_TOKEN, username: "alice" } });
  assert.equal(opt.statusCode, 200, opt.body);
  const { challengeId, options } = opt.json();
  assert.equal(options.authenticatorSelection.userVerification, "required");
  assert.equal(options.authenticatorSelection.residentKey, "required");
  const ver = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/verify", headers, payload: { challengeId, response: authn.register(options), name: "laptop" } });
  assert.equal(ver.statusCode, 200, ver.body);
  return cookieFrom(ver);
}

test("bootstrap registers the first admin once, then closes", async () => {
  const { app } = await startManager();
  const bad = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers, payload: { bootstrapToken: "wrong-token-wrong-token-wrong", username: "mallory" } });
  assert.equal(bad.statusCode, 403);

  const authn = new SoftAuthenticator();
  const cookie = await bootstrapAdmin(app, authn);
  const session = await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie } });
  assert.equal(session.json().user.role, "admin");
  assert.equal(session.json().bootstrap, "completed");

  const again = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers, payload: { bootstrapToken: BOOTSTRAP_TOKEN, username: "eve" } });
  assert.equal(again.statusCode, 410, "bootstrap entry is closed after the first admin");
  await app.close();
});

test("passkey login, challenge single use, origin and UV enforcement", async () => {
  const { app } = await startManager();
  const authn = new SoftAuthenticator();
  await bootstrapAdmin(app, authn);

  const opt = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/options", headers });
  const { challengeId, options } = opt.json();
  assert.equal(options.userVerification, "required");
  const response = authn.authenticate(options);
  const ok = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/verify", headers, payload: { challengeId, response } });
  assert.equal(ok.statusCode, 200, ok.body);
  assert.match(cookieFrom(ok), /^__Host-mnp_session=/);

  const replay = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/verify", headers, payload: { challengeId, response } });
  assert.equal(replay.statusCode, 400, "challenge cannot be reused");
  assert.equal(replay.json().error, "challenge-used");

  const o2 = (await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/options", headers })).json();
  const wrongOrigin = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/verify", headers, payload: { challengeId: o2.challengeId, response: authn.authenticate(o2.options, { origin: "https://evil.example" }) } });
  assert.equal(wrongOrigin.statusCode, 401);

  const o3 = (await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/options", headers })).json();
  const noUv = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/verify", headers, payload: { challengeId: o3.challengeId, response: authn.authenticate(o3.options, { uv: false }) } });
  assert.equal(noUv.statusCode, 401, "user verification is required");

  const cross = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/authenticate/options", headers: { ...headers, origin: "https://evil.example" } });
  assert.equal(cross.statusCode, 403, "mutating requests from foreign origins are rejected");
  await app.close();
});

test("CSRF, invites, roles and last-admin protection", async () => {
  const { app } = await startManager();
  const admin = new SoftAuthenticator();
  const cookie = await bootstrapAdmin(app, admin);
  const csrf = (await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie } })).json().csrfToken as string;

  const noCsrf = await app.inject({ method: "POST", url: "/api/v1/accounts", headers: { ...headers, cookie }, payload: { username: "bob", role: "viewer" } });
  assert.equal(noCsrf.statusCode, 403);

  const invite = await app.inject({ method: "POST", url: "/api/v1/accounts", headers: { ...headers, cookie, "x-csrf-token": csrf }, payload: { username: "bob", role: "viewer" } });
  assert.equal(invite.statusCode, 200, invite.body);
  const { inviteToken, accountId } = invite.json();

  const bobKey = new SoftAuthenticator();
  const ro = (await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers, payload: { inviteToken } })).json();
  const rv = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/verify", headers, payload: { challengeId: ro.challengeId, response: bobKey.register(ro.options) } });
  assert.equal(rv.statusCode, 200, rv.body);
  const bobCookie = cookieFrom(rv);
  const reuse = await app.inject({ method: "POST", url: "/api/v1/auth/passkeys/register/options", headers, payload: { inviteToken } });
  assert.equal(reuse.statusCode, 403, "invites are single use");

  const bobCsrf = (await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie: bobCookie } })).json().csrfToken as string;
  const forbidden = await app.inject({ method: "POST", url: "/api/v1/nodes", headers: { ...headers, cookie: bobCookie, "x-csrf-token": bobCsrf }, payload: { id: "gw-x" } });
  assert.equal(forbidden.statusCode, 403, "viewer cannot create nodes");

  const passkeys = (await app.inject({ method: "GET", url: "/api/v1/auth/passkeys", headers: { cookie } })).json() as Array<{ id: string }>;
  const last = await app.inject({ method: "DELETE", url: `/api/v1/auth/passkeys/${passkeys[0]!.id}`, headers: { ...headers, cookie, "x-csrf-token": csrf } });
  assert.equal(last.statusCode, 409, "cannot revoke the last admin credential");

  const demote = await app.inject({ method: "PATCH", url: `/api/v1/accounts/${accountId}`, headers: { ...headers, cookie, "x-csrf-token": csrf }, payload: { disabled: true } });
  assert.equal(demote.statusCode, 200);
  const bobSession = await app.inject({ method: "GET", url: "/api/v1/auth/session", headers: { cookie: bobCookie } });
  assert.equal(bobSession.json().authenticated, false, "disabling revokes sessions");
  await app.close();
});
