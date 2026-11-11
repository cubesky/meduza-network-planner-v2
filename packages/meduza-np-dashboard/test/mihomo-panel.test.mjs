import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { mihomoPanelUrl } from '../web/src/mihomo-panel.ts';

const common = new Function('baseclass', 'rpc', 'ui', 'E', '_', readFileSync(new URL('../../meduza-np-dashboard-openwrt/htdocs/luci-static/resources/meduza-np/common.js', import.meta.url), 'utf8'))(
  { extend: x => x }, { declare: () => () => {} }, {}, () => {}, s => s,
);
for (const [name, build] of [['Dashboard', mihomoPanelUrl], ['LuCI', common.panelUrl]]) {
  test(`${name}: native panel uses node IP, port and encoded fragment credentials`, () => {
    for (const [address, browser, expected] of [
      ['0.0.0.0:9090', '10.42.10.12', '10.42.10.12'],
      ['[::]:9090', '[2001:db8::1]', '[2001:db8::1]'],
      ['10.42.10.2:8080', 'router.example', '10.42.10.2'],
      ['127.0.0.1:9090', 'localhost', '127.0.0.1'],
      ['0.0.0.0:80', '10.42.10.12', '10.42.10.12'],
    ]) {
      const secret = 'fixture &?#=/密钥';
      const url = new URL(build({ address, secret }, browser));
      assert.equal(url.hostname, expected);
      assert.equal(url.pathname, '/ui/');
      assert.equal(url.search, '', 'credential must not be in the server request URL');
      const params = new URLSearchParams(url.hash.split('?')[1]);
      assert.equal(params.get('hostname'), expected);
      assert.equal(params.get('port'), url.port || '80');
      assert.equal(params.get('http'), 'true');
      assert.equal(params.get('secret'), secret);
    }
  });
  test(`${name}: LAN browsers cannot be sent to a node's loopback controller`, () => {
    for (const address of ['127.0.0.1:9090', '[::1]:9090']) assert.throws(() => build({ address, secret: 'fixture' }, '10.42.10.12'));
  });
}
