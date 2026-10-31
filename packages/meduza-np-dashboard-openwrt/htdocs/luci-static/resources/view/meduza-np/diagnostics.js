'use strict';
'require view';
'require meduza-np.common as c';

return view.extend({
	load: function() { return Promise.all([c.call.doctor(), c.call.logs(300), c.call.config()]); },

	render: function(data) {
		var checks = c.result(data[0], _('Diagnostics')) || [], logs = c.result(data[1], _('Logs')) || [], cfg = c.result(data[2], _('Configuration'));
		var pre = { 'style': 'max-height:50vh;overflow:auto;font-size:11px' };
		return E('div', [
			E('h2', _('Diagnostics')),
			c.table([_('Check'), _('Result'), _('Detail')], checks.map(function(x) { return [x.check, c.badge(x.ok ? 'ok' : 'failed'), x.detail || '']; })),
			E('h3', _('planner log')),
			E('pre', pre, logs.join('\n')),
			E('h3', _('Last applied configuration (secrets redacted)')),
			cfg ? E('pre', pre, JSON.stringify(cfg.config, null, 2)) : E('p', _('No configuration applied yet.'))
		]);
	},

	handleSaveApply: null, handleSave: null, handleReset: null
});
