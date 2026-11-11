'use strict';
'require view';
'require poll';
'require ui';
'require meduza-np.common as c';

return view.extend({
	load: function() { return Promise.all([c.call.status(), c.call.features(), c.call.panel_access('ubus', 'meduza-np', 'mihomo_panel'), c.call.mihomo_panel_status()]); },

	renderStatus: function(st, features) {
		var s = st.sync || {}, b = s.backoff || {}, m = st.maintenance, r = st.resources || {};
		var kib = function(v) { return v == null ? null : '%1024.2mB'.format(v * 1024); };
		var revs = [s.desiredRevision, s.receivedRevision, s.appliedRevision].map(function(x) { return x || '—'; }).join(' / ');
		return E('div', { 'id': 'mnp-overview' }, [
			E('h3', _('Sync')),
			c.kv([
				[_('Node'), st.nodeId || _('not enrolled')],
				[_('State'), c.badge(s.state)],
				[_('Channel'), [s.transport, s.mode, s.behavior].join(' · ')],
				[_('Desired / received / applied'), revs],
				[_('Last successful'), s.lastSuccessfulRevision],
				[_('Degraded'), s.degradedReason],
				[_('Last error'), s.lastError],
				[_('Backoff'), b.failureLevel > 0
					? _('level %d, window %ds, next attempt in %ds — %s (the last good configuration keeps running)').format(b.failureLevel, b.windowSeconds, Math.ceil((b.nextAttemptInMs || 0) / 1000), b.lastFailure || '')
					: _('none')]
			]),
			E('h3', _('Maintenance')),
			m && m.active
				? E('div', [
					E('p', _('%s by %s until %s').format(m.reason, m.by, m.until) + (m.applyPaused ? ' ' + _('(apply paused)') : '')),
					E('button', { 'class': 'btn cbi-button', 'click': ui.createHandlerFn(this, function() { return c.act(c.call.maintenance_stop(), _('Maintenance ended')); }) }, _('End maintenance'))
				])
				: E('div', [
					E('input', { 'id': 'mnp-reason', 'class': 'cbi-input-text', 'placeholder': _('Reason') }), ' ',
					E('input', { 'id': 'mnp-minutes', 'class': 'cbi-input-text', 'value': '30', 'size': 4 }), ' ', _('minutes'), ' ',
					E('label', [E('input', { 'id': 'mnp-pause', 'type': 'checkbox', 'checked': true }), ' ', _('pause configuration apply')]), ' ',
					E('button', { 'class': 'btn cbi-button-action', 'click': ui.createHandlerFn(this, function() {
						return c.act(c.call.maintenance_start(document.getElementById('mnp-reason').value, +document.getElementById('mnp-minutes').value || 30, document.getElementById('mnp-pause').checked), _('Maintenance started'));
					}) }, _('Start maintenance'))
				]),
			E('h3', _('Features')),
			this.panelAllowed && this.panelEnabled && (features || []).some(function(f) { return f.id === 'mihomo.proxy' && f.enabled; })
				? E('p', [E('button', { 'class': 'btn cbi-button', 'click': ui.createHandlerFn(this, function() { return c.openMihomoPanel(); }) }, _('Open Mihomo panel'))]) : null,
			c.table([_('Feature'), _('Package'), _('State'), _('Provider'), _('Reason')], (features || []).map(function(f) {
				var label = !f.registered ? _('registration failed') : !f.available ? _('missing dependency') : !f.enabled ? _('not enabled')
					: f.running === false ? _('not running') : f.healthy === false ? _('degraded') : _('ok');
				return [f.id, f.package, c.badge(label), f.provider || '—', f.reason || ''];
			})),
			E('h3', _('Resources')),
			c.kv([
				[_('planner RSS'), kib(r.plannerRssKib)],
				[_('Memory available / total'), (kib(r.memAvailableKib) || '—') + ' / ' + (kib(r.memTotalKib) || '—')],
				[_('State storage free'), kib(r.stateDiskFreeKib)],
				[_('Supervised processes'), r.supervisedProcesses]
			]),
			E('p', { 'class': 'cbi-section-descr' }, _('Configuration changes are made in the manager (drafts + publish); this page does not keep a second configuration.')),
			E('p', [E('button', { 'class': 'btn cbi-button', 'click': ui.createHandlerFn(this, function() { return c.act(c.call.sync(), _('Sync requested')); }) }, _('Sync now'))])
		]);
	},

	render: function(data) {
		this.panelAllowed = data[2] === true;
		this.panelEnabled = !!(data[3] && data[3].ok && data[3].result && data[3].result.enabled);
		var st = c.result(data[0], _('Status')) || {}, features = c.result(data[1], _('Features')) || [];
		var self = this;
		poll.add(function() {
			return Promise.all([c.call.status(), c.call.features(), c.call.mihomo_panel_status()]).then(function(d) {
				self.panelEnabled = !!(d[2] && d[2].ok && d[2].result && d[2].result.enabled);
				var el = document.getElementById('mnp-overview');
				var typing = document.activeElement && String(document.activeElement.id || '').indexOf('mnp-') === 0;
				if (el && d[0] && d[0].ok && !typing) el.replaceWith(self.renderStatus(d[0].result, (d[1] && d[1].result) || []));
			});
		}, 10);
		return E('div', [E('h2', _('Meduza NP')), this.renderStatus(st, features)]);
	},

	handleSaveApply: null, handleSave: null, handleReset: null
});
