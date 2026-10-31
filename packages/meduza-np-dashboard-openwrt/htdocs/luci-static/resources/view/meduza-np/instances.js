'use strict';
'require view';
'require ui';
'require meduza-np.common as c';

return view.extend({
	load: function() { return Promise.all([c.call.instances(), c.call.status()]); },

	action: function(id, action) {
		return c.act(c.call.instance(id, action), _('%s: %s requested').format(id, action)).then(function() {
			window.setTimeout(function() { location.reload(); }, 1500);
		});
	},

	render: function(data) {
		var inst = c.result(data[0], _('Instances')) || {}, st = c.result(data[1], _('Status')) || {};
		var maint = !!(st.maintenance && st.maintenance.active), self = this, applied = inst.applied || {};
		return E('div', [
			E('h2', _('Instances')),
			c.table([_('Instance'), _('Package'), _('Processes'), ''], Object.keys(applied).sort().map(function(id) {
				return [id, applied[id].package, (applied[id].processes || []).join(', '), E('span', [
					E('button', { 'class': 'btn cbi-button', 'click': ui.createHandlerFn(self, 'action', id, 'restart') }, _('Restart')), ' ',
					E('button', { 'class': 'btn cbi-button', 'disabled': maint ? null : true, 'title': maint ? '' : _('Start maintenance first, otherwise reconcile restores it'),
						'click': ui.createHandlerFn(self, 'action', id, 'stop') }, _('Stop')), ' ',
					E('button', { 'class': 'btn cbi-button', 'click': ui.createHandlerFn(self, 'action', id, 'start') }, _('Start'))
				])];
			})),
			E('h3', _('Processes')),
			c.table([_('Process'), _('State'), _('PID'), _('Restarts'), _('Last exit')], (inst.processes || []).map(function(p) {
				return [p.id, c.badge(p.state), p.pid || '—', p.restarts, p.lastExit || ''];
			})),
			E('h3', _('Status and neighbours')),
			c.table([_('Object'), _('Kind'), _('State'), _('Reason')], (st.instances || []).map(function(i) {
				return [i.id, i.kind || '', c.badge(i.state), i.reason || ''];
			}))
		]);
	},

	handleSaveApply: null, handleSave: null, handleReset: null
});
