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

return baseclass.extend({
	call: calls,

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
