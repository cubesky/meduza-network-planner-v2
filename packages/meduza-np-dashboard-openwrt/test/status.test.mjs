import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import assert from 'node:assert/strict';

// Execute the shipped LuCI modules with their native dependency/DOM functions substituted.
const E = (tag, attrs, children) => ({ tag, attrs, children });
const c = new Function('baseclass', 'rpc', 'ui', 'E', '_', readFileSync(new URL('../htdocs/luci-static/resources/meduza-np/common.js', import.meta.url), 'utf8'))(
  { extend: (obj) => obj }, { declare: () => () => {} }, {}, E, (s) => s,
);
const overview = new Function('view', 'poll', 'ui', 'c', 'E', '_', readFileSync(new URL('../htdocs/luci-static/resources/view/meduza-np/overview.js', import.meta.url), 'utf8'))(
  { extend: (obj) => obj }, {}, { createHandlerFn: (_owner, fn) => fn }, { ...c, kv: (rows) => rows, table: (headers, rows) => ({ headers, rows }) }, E, (s) => s,
);

test('LuCI sync badges distinguish in-sync, failures and unknown states', () => {
  assert.match(c.badge('in-sync').attrs.style, /background:#2e7d32/);
  for (const state of ['apply-failed', 'credential-error', 'missing dependency']) assert.match(c.badge(state).attrs.style, /background:#c62828/, state);
  for (const state of ['unsynced', 'applying', 'unknown', null]) assert.match(c.badge(state).attrs.style, /background:#757575/, String(state));
});

test('LuCI renders available consoles and preserves feature errors and disabled services', () => {
  const good = { registered: true, available: true, enabled: true, running: true, healthy: true };
  const result = overview.renderStatus({ sync: { state: 'in-sync' } }, [
    { ...good, id: 'ui.cli', package: 'meduza-np-cli' },
    { ...good, id: 'ui.luci', package: 'meduza-np-dashboard-openwrt' },
    { ...good, id: 'wireguard', enabled: false },
    { ...good, id: 'broken-console', registered: false },
    { ...good, id: 'missing-console', available: false },
  ]);
  const rows = result.children.find((child) => child?.headers?.[0] === 'Feature').rows;
  assert.deepEqual(rows.map((row) => row[2].children), ['ok', 'ok', 'not enabled', 'registration failed', 'missing dependency']);
  assert.match(rows[0][2].attrs.style, /background:#2e7d32/);
  assert.match(rows[1][2].attrs.style, /background:#2e7d32/);
});

test('LuCI panel launch requires write permission and does not disclose a credential in polling', () => {
  const acl = JSON.parse(readFileSync(new URL('../root/usr/share/rpcd/acl.d/luci-app-meduza-np.json', import.meta.url), 'utf8'))['luci-app-meduza-np'];
  assert.ok(!acl.read.ubus['meduza-np'].includes('mihomo_panel'));
  assert.ok(acl.write.ubus['meduza-np'].includes('mihomo_panel'));
  const feature = [{ id: 'mihomo.proxy', registered: true, available: true, enabled: true }];
  const hasButton = result => JSON.stringify(result).includes('Open Mihomo panel');
  assert.ok(!hasButton(overview.renderStatus({}, feature)));
  overview.panelAllowed = true;
  overview.panelEnabled = false;
  assert.ok(!hasButton(overview.renderStatus({}, feature)));
  overview.panelEnabled = true;
  assert.ok(hasButton(overview.renderStatus({}, feature)));
  assert.ok(!hasButton(overview.renderStatus({}, [{ ...feature[0], enabled: false }])));
  overview.panelAllowed = false;
  overview.panelEnabled = false;
});
