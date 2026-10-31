#!/bin/sh
# Dashboard sign-in with the manager identity against the live lab manager (AGENT §12.3, D15). lab-av (bridge-mode
# base-container node, already enrolled) gets the dashboard component with MEDUZA_NP_DASHBOARD_PUBLIC_URL on loopback;
# the "browser" is curl inside its planner container, the manager user is the lab's software passkey
# (tools/lab/manager-api.mjs) — the same requests a browser sends to /auth/login, the approval page and /auth/callback.
#
#   tools/lab/dashboard-login.sh setup       add `dashboard` to lab-av's components + public URL, re-create its
#                                            containers, install the component, wait for the dashboard
#   tools/lab/dashboard-login.sh check       full round trip as admin, replay, wrong code burns the login, deny,
#                                            state mismatch, viewer account (read only), role change and disabled
#                                            account between approval and exchange, audit entries
#   tools/lab/dashboard-login.sh expiry      an approved login older than 5 minutes is refused (waits 305 s; the
#                                            dashboard's 300 s login cookie ends it first: 400)
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/dashboard-login
N=${NODE:-lab-av}
D=http://127.0.0.1:8443
IMAGE=localhost/meduza-np-base-container:e2e
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
pl() { lx $N podman exec meduza-np-planner "$@"; }
# curl inside the planner container ("the browser"), cookie jar /tmp/$1
br() { j=$1; shift; pl curl -s -b "/tmp/$j" -c "/tmp/$j" "$@"; }
api() { sh "$L" api "$@"; }
vapi() { MANAGER=http://10.50.0.10:8787 ORIGIN=${MANAGER_ORIGIN:-http://localhost:8787} STATE=$S/viewer.json node "$REPO/tools/lab/manager-api.mjs" "$@"; }
fail() { echo "FAIL: $*"; exit 1; }
ok() { echo "ok: $*"; }
mkdir -p $S

# start a sign-in in jar $1: prints the pending login id (from the 302 to the manager's approval page)
begin() {
	pl rm -f "/tmp/$1"
	loc=$(br "$1" -o /dev/null -w '%{redirect_url}' $D/auth/login)
	case "$loc" in */#/dashboard-login/*) echo "${loc##*/}" ;; *) fail "/auth/login did not redirect to the manager: '$loc' $(br "$1" $D/auth/login)" ;; esac
}
# approve with admin (api) or viewer (vapi): prints the callback URL
approve() { $1 call POST "/api/v1/dashboard-logins/$2/approve" '{}' | jq -r .redirect; }
code_of() { pl curl -s -o /dev/null -w '%{http_code}' -b "/tmp/$1" -c "/tmp/$1" "$2"; }
session() { br "$1" $D/api/session; }
account_id() { api call GET /api/v1/accounts | jq -r --arg u "$1" '.[] | select(.username == $u) | .id'; }

case "${1:-}" in
setup)
	env=/etc/meduza-np/node.env
	lx $N sh -c "grep -q '^MEDUZA_NP_COMPONENTS=.*dashboard' $env || sed -i 's/^MEDUZA_NP_COMPONENTS=\(.*\)/MEDUZA_NP_COMPONENTS=\1,dashboard/' $env"
	lx $N sh -c "grep -q '^MEDUZA_NP_DASHBOARD_PUBLIC_URL=' $env || echo MEDUZA_NP_DASHBOARD_PUBLIC_URL=$D >>$env"
	lx $N grep -E 'COMPONENTS|DASHBOARD' $env
	net=$(lx $N podman inspect meduza-np-planner --format '{{.HostConfig.NetworkMode}}')
	[ "$net" = host ] || net=bridge
	incus file push -q -r "$REPO/packages/meduza-np-base-container/deploy/podman" "$N/root/"
	lx $N sh -c 'sed -i "s/\r$//" /root/podman/run.sh && chmod +x /root/podman/run.sh'
	lx $N /root/podman/run.sh --env-file $env --image $IMAGE --network "$net" --import /srv/import
	i=0
	until pl meduza-np --json update status >/dev/null 2>&1 || [ $i -gt 30 ]; do sleep 2; i=$((i + 1)); done
	pl meduza-np update check >/dev/null 2>&1 || true
	pl meduza-np update apply >/dev/null 2>&1 || true
	i=0
	until pl meduza-np --json update status 2>/dev/null | jq -e '.installed.packages["meduza-np-dashboard"] and .busy == null' >/dev/null || [ $i -gt 60 ]; do
		sleep 5
		i=$((i + 1))
	done
	pl meduza-np --json update status | jq -c '{g: .installed.groupVersion, components: .config.components, dashboard: .installed.packages["meduza-np-dashboard"].version}'
	i=0
	until [ "$(pl curl -s -o /dev/null -w '%{http_code}' $D/api/session 2>/dev/null)" = 200 ] || [ $i -gt 30 ]; do sleep 2; i=$((i + 1)); done
	pl curl -s $D/api/session
	echo
	;;
check)
	s=$(pl curl -s $D/api/session)
	[ "$(echo "$s" | jq -r .managerLogin)" = true ] || fail "dashboard has no manager sign-in: $s"
	admin_user=$(api call GET /api/v1/auth/session | jq -r .user.username)

	echo "== 1. round trip as $admin_user (admin)"
	id=$(begin a)
	v=$(api call GET "/api/v1/dashboard-logins/$id")
	echo "$v" | jq -c '{nodeId, returnUrl, state}'
	[ "$(echo "$v" | jq -r .nodeId)" = "$N" ] || fail "approval page shows the wrong node"
	[ "$(echo "$v" | jq -r .returnUrl)" = "$D/auth/callback" ] || fail "approval page shows the wrong return URL"
	[ "$(echo "$v" | jq -r .state)" = pending ] || fail "new login not pending"
	cb=$(approve api "$id")
	case "$cb" in "$D/auth/callback?code="*"&state=$id") ok "approved, callback $D/auth/callback?code=…&state=$id" ;; *) fail "callback URL: $cb" ;; esac
	[ "$(api call GET "/api/v1/dashboard-logins/$id" | jq -r .state)" = approved ] || fail "state after approval"
	pl cp /tmp/a /tmp/a-before
	t0=$(date +%s%N)
	loc=$(br a -o /dev/null -w '%{http_code} %{redirect_url}' "$cb")
	ms=$((($(date +%s%N) - t0) / 1000000))
	[ "$loc" = "302 $D/" ] || fail "callback: $loc"
	s=$(session a)
	echo "$s" | jq -c '{authenticated, user, role, via}'
	[ "$(echo "$s" | jq -r '.authenticated, .role, .via, .user' | paste -sd' ')" = "true admin manager $admin_user" ] || fail "session after callback: $s"
	ok "callback → session in $ms ms"
	br a $D/api/status | jq -e .bootId >/dev/null || fail "status through the manager session"
	csrf=$(echo "$s" | jq -r .csrf)
	c=$(pl curl -s -o /dev/null -w '%{http_code}' -b /tmp/a -H "x-csrf-token: $csrf" -H 'content-type: application/json' -d '{}' -X POST $D/api/sync)
	[ "$c" = 200 ] || [ "$c" = 503 ] || fail "admin POST /api/sync: $c"
	ok "API reads + operator action with the manager session ($c)"
	[ "$(api call GET "/api/v1/dashboard-logins/$id" | jq -r .state)" = used ] || fail "state after exchange"

	echo "== 2. replay of the same callback (login cookie restored)"
	pl cp /tmp/a-before /tmp/r
	c=$(code_of r "$cb")
	[ "$c" = 403 ] || fail "replayed code: $c"
	[ "$(session r | jq -r .authenticated)" = false ] || fail "replay gave a session"
	ok "replay refused (403)"

	echo "== 3. wrong code burns the login"
	id=$(begin w)
	cb=$(approve api "$id")
	pl cp /tmp/w /tmp/w2
	c=$(code_of w "$D/auth/callback?code=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&state=$id")
	[ "$c" = 403 ] || fail "wrong code: $c"
	c=$(code_of w2 "$cb")
	[ "$c" = 403 ] || fail "right code after a failed attempt: $c"
	ok "wrong code 403, then the right code 403"

	echo "== 4. deny"
	id=$(begin d)
	api call POST "/api/v1/dashboard-logins/$id/deny" '{}' >/dev/null
	[ "$(api call GET "/api/v1/dashboard-logins/$id" | jq -r .state)" = used ] || fail "state after deny"
	c2=$(api call POST "/api/v1/dashboard-logins/$id/approve" '{}' 2>&1 | grep -o 'not-pending\|409' | head -1 || true)
	[ -n "$c2" ] || fail "denied login could still be approved"
	ok "denied login cannot be approved afterwards ($c2)"

	echo "== 5. callback that this browser did not start"
	id=$(begin x)
	cb=$(approve api "$id")
	c=$(pl curl -s -o /dev/null -w '%{http_code}' "$cb")
	[ "$c" = 400 ] || fail "callback without the login cookie: $c"
	pl curl -s -o /dev/null -c /tmp/y $D/auth/login
	c=$(code_of y "$cb")
	[ "$c" = 400 ] || fail "callback with another login's cookie: $c"
	c=$(code_of x "$cb")
	[ "$c" = 302 ] || fail "the browser that started it: $c"
	ok "no login cookie 400, other login's cookie 400 (state mismatch), own browser 302"

	echo "== 6. viewer account"
	if ! jq -e .jwk $S/viewer.json >/dev/null 2>&1; then
		rm -f $S/viewer.json
		inv=$(api invite-account dash-viewer viewer 2>/dev/null | awk '{print $1}')
		[ -n "$inv" ] || inv=$(api call POST "/api/v1/accounts/$(account_id dash-viewer)/invite" '{}' | jq -r .inviteUrl)
		vapi invite "$inv" >/dev/null
	fi
	vid=$(account_id dash-viewer)
	api call PATCH "/api/v1/accounts/$vid" '{"role":"viewer","disabled":false}' >/dev/null
	id=$(begin v)
	cb=$(approve vapi "$id")
	[ "$(code_of v "$cb")" = 302 ] || fail "viewer callback"
	s=$(session v)
	[ "$(echo "$s" | jq -r '.role, .user' | paste -sd' ')" = "viewer dash-viewer" ] || fail "viewer session: $s"
	br v $D/api/status | jq -e .bootId >/dev/null || fail "viewer read"
	csrf=$(echo "$s" | jq -r .csrf)
	c=$(pl curl -s -o /dev/null -w '%{http_code}' -b /tmp/v -H "x-csrf-token: $csrf" -H 'content-type: application/json' -d '{}' -X POST $D/api/sync)
	[ "$c" = 403 ] || fail "viewer POST /api/sync: $c"
	ok "viewer session reads, action refused (403)"

	echo "== 7. role raised between approval and exchange (dashboard gets the current role)"
	id=$(begin o)
	cb=$(approve vapi "$id")
	api call PATCH "/api/v1/accounts/$vid" '{"role":"operator"}' >/dev/null
	[ "$(code_of o "$cb")" = 302 ] || fail "callback after role change"
	[ "$(session o | jq -r .role)" = operator ] || fail "role after change: $(session o)"
	api call PATCH "/api/v1/accounts/$vid" '{"role":"viewer"}' >/dev/null
	ok "approved as viewer, exchanged as operator"

	echo "== 8. account disabled between approval and exchange"
	id=$(begin z)
	cb=$(approve vapi "$id")
	api call PATCH "/api/v1/accounts/$vid" '{"disabled":true}' >/dev/null
	c=$(code_of z "$cb")
	api call PATCH "/api/v1/accounts/$vid" '{"disabled":false}' >/dev/null
	[ "$c" = 403 ] || fail "disabled account: $c"
	[ "$(session z | jq -r .authenticated)" = false ] || fail "disabled account got a session"
	ok "disabled account refused (403)"

	echo "== 9. audit"
	api call GET /api/v1/audit | jq -r '.[] | select(.action | startswith("dashboard.")) | "\(.at) \(.actor) \(.action) \(.target // "-")"' | head -12
	echo "PASS: dashboard sign-in with the manager identity on $N"
	;;
expiry)
	id=$(begin e)
	cb=$(approve api "$id")
	echo "approved $id, waiting 305 s"
	sleep 305
	c=$(code_of e "$cb")
	[ "$c" = 403 ] || [ "$c" = 400 ] || fail "expired login: $c"
	[ "$(session e | jq -r .authenticated)" = false ] || fail "expired login gave a session"
	echo "PASS: expired login refused ($c)"
	;;
*)
	sed -n '2,14p' "$0"
	exit 2
	;;
esac
