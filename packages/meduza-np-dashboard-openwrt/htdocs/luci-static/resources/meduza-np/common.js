'use strict';
'require baseclass';
'require rpc';
'require ui';

// Shared helpers for the Meduza NP LuCI views: ubus calls to the `meduza-np` rpcd plugin, which wraps the CLI.
var calls = {};
['status', 'features', 'packages', 'instances', 'doctor', 'config', 'update_status', 'sync', 'maintenance_stop'].forEach(function(m) {
	calls[m] = rpc.declare({ object: 'meduza-np', method: m });
});
calls.logs = rpc.declare({ object: 'meduza-np', method: 'logs', params: ['lines'] });
calls.instance = rpc.declare({ object: 'meduza-np', method: 'instance', params: ['instance', 'action'] });
calls.maintenance_start = rpc.declare({ object: 'meduza-np', method: 'maintenance_start', params: ['reason', 'minutes', 'pause_apply'] });
calls.update = rpc.declare({ object: 'meduza-np', method: 'update', params: ['action'] });
calls.mihomo_panel = rpc.declare({ object: 'meduza-np', method: 'mihomo_panel' });
calls.mihomo_panel_status = rpc.declare({ object: 'meduza-np', method: 'mihomo_panel_status' });
calls.panel_access = rpc.declare({ object: 'session', method: 'access', params: ['scope', 'object', 'function'], expect: { access: false } });

return baseclass.extend({
	call: calls,

	panelUrl: function(info, hostname) {
		var url = new URL('http://' + info.address);
		var loopback = function(h) { return h === 'localhost' || h === '[::1]' || h === '::1' || /^127\./.test(h); };
		if (loopback(url.hostname) && !loopback(hostname)) throw new Error(_('Mihomo controller listens only on loopback; set api.address to the node LAN address or 0.0.0.0 in Manager.'));
		if (url.hostname === '0.0.0.0' || url.hostname === '[::]') url.hostname = hostname;
		url.pathname = '/ui/';
		url.hash = '/setup?' + new URLSearchParams({ http: 'true', hostname: url.hostname, port: url.port || '80', secret: info.secret });
		return url.toString();
	},

	openMihomoPanel: function() {
		var tab = window.open('about:blank', '_blank'), self = this;
		if (!tab) { ui.addNotification(null, E('p', _('Allow popups to open the Mihomo panel.')), 'warning'); return Promise.resolve(); }
		tab.opener = null;
		return calls.mihomo_panel().then(function(r) {
			if (!r || !r.ok) throw new Error((r && r.error) || _('Mihomo panel is unavailable.'));
			tab.location.replace(self.panelUrl(r.result, window.location.hostname));
		}).catch(function(error) {
			tab.close();
			ui.addNotification(null, E('p', error.message || String(error)), 'warning');
		});
	},

	/** Unwraps {ok, result, error}; an error becomes a visible notice instead of a silently empty view. */
	result: function(r, what) {
		if (r && r.ok) return r.result;
		ui.addNotification(null, E('p', _('%s: %s').format(what, (r && r.error) || _('planner not reachable'))), 'warning');
		return null;
	},

	act: function(promise, done) {
		return promise.then(function(r) {
			if (r && r.ok) ui.addNotification(null, E('p', done), 'info');
			else ui.addNotification(null, E('p', (r && r.error) || _('failed')), 'danger');
		});
	},

	badge: function(state) {
		var s = String(state || '');
		var good = /^(ok|up|running|healthy|registered|synced|in-sync|established|done)$/.test(s);
		var bad = /(fail|down|error|invalid|incompatible|conflict|missing)/.test(s);
		return E('span', { 'class': 'label', 'style': 'background:' + (good ? '#2e7d32' : bad ? '#c62828' : '#757575') + ';color:#fff;padding:1px 6px;border-radius:3px' }, s || '—');
	},

	table: function(headers, rows) {
		var t = E('table', { 'class': 'table' }, [E('tr', { 'class': 'tr table-titles' }, headers.map(function(h) { return E('th', { 'class': 'th' }, h); }))]);
		rows.forEach(function(r) { t.appendChild(E('tr', { 'class': 'tr' }, r.map(function(c) { return E('td', { 'class': 'td' }, c); }))); });
		if (!rows.length) t.appendChild(E('tr', { 'class': 'tr placeholder' }, [E('td', { 'class': 'td', 'colspan': headers.length }, _('Nothing here yet'))]));
		return t;
	},

	kv: function(pairs) {
		return E('table', { 'class': 'table' }, pairs.map(function(p) {
			return E('tr', { 'class': 'tr' }, [E('td', { 'class': 'td left', 'width': '33%' }, p[0]), E('td', { 'class': 'td left' }, p[1] == null || p[1] === '' ? '—' : p[1])]);
		}));
	}
});
