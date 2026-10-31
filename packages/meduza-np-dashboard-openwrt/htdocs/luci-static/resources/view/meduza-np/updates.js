'use strict';
'require view';
'require ui';
'require meduza-np.common as c';

return view.extend({
	load: function() { return Promise.all([c.call.packages(), c.call.update_status()]); },

	render: function(data) {
		var p = c.result(data[0], _('Packages')) || {}, u = c.result(data[1], _('Updates')) || {}, self = this;
		var upd = function(action, done) { return ui.createHandlerFn(self, function() { return c.act(c.call.update(action), done); }); };
		var providers = p.providers || {}, perr = p.providerErrors || {};
		var nodes = [
			E('h2', _('Packages & updates')),
			c.table([_('Package'), _('Version'), _('Role'), _('Registration'), _('Reason')], (p.packages || []).map(function(x) {
				return [x.name, x.version, x.role, c.badge(x.registration), x.reason || ''];
			})),
			E('h3', _('Software providers')),
			c.table([_('Slot'), _('Source'), _('Path')], Object.keys(providers).map(function(k) { return [k, providers[k].source, providers[k].path]; })
				.concat(Object.keys(perr).map(function(k) { return [k, c.badge('missing'), perr[k]]; }))),
			E('h3', _('Automatic updates'))
		];
		if (u.available === false) {
			nodes.push(E('p', _('meduza-np-updater is not installed: this node has no automatic updates.')));
		} else {
			var st = u.state || {}, inst = u.installed || {}, ctl = u.control || {};
			nodes.push(c.kv([
				[_('Channel'), (u.config || {}).channel],
				[_('Installed group'), (inst.groupVersion || '—') + ' (' + _('generation %d').format(inst.generation || 0) + ')'],
				[_('Available group'), (st.available || {}).groupVersion],
				[_('Last check'), (st.lastCheck || '—') + (st.lastCheckError ? ' — ' + st.lastCheckError : '')],
				[_('Last result'), st.lastResult ? [st.lastResult.groupVersion, st.lastResult.outcome, st.lastResult.detail || ''].join(' ') : null],
				[_('In progress'), u.journal ? u.journal.groupVersion + ' · ' + u.journal.phase : null],
				[_('Paused'), ctl.paused ? _('yes') : _('no')]
			]));
			nodes.push(E('p', [
				E('button', { 'class': 'btn cbi-button', 'click': upd('check', _('Update check started')) }, _('Check')), ' ',
				E('button', { 'class': 'btn cbi-button-action', 'click': upd('apply', _('Update started')) }, _('Update now')), ' ',
				ctl.paused
					? E('button', { 'class': 'btn cbi-button', 'click': upd('resume', _('Updates resumed')) }, _('Resume'))
					: E('button', { 'class': 'btn cbi-button', 'click': upd('pause', _('Updates paused')) }, _('Pause'))
			]));
			nodes.push(E('p', { 'class': 'cbi-section-descr' }, _('OpenWrt packages are installed one by one with the native package manager; this is not an atomic group switch. Packages without a cached previous version cannot be rolled back automatically.')));
		}
		return E('div', nodes);
	},

	handleSaveApply: null, handleSave: null, handleReset: null
});
