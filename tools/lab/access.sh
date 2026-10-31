#!/bin/sh
# OpenVPN Access in the lab (AGENT §6.5, §9.3, D27): remote users log in with their LDAP account to the Access
# server `access` on gateway lab-a; the manager checks them against a directory the nodes never reach.
#
#   lab-mgr    OpenLDAP (osixia, test-only like D27) next to the manager, ldaps on the Podman gateway 10.88.0.1:636
#              (reachable from the manager container, not from mnpwan); users alice (group vpn-users), bob (no
#              group), carol (group vpn-users, removed from it by `group carol off`); read-only service bind
#   manager    LDAP settings (CA pinned, certificate verified), bind password secret, policy `lab-vpn`
#              (vpn-users → lab-a/access), server certificate lab-a-access from the lab CA
#   lab-a      Access instance `access` (udp 1195, pool 10.98.0.0/24, pushes the internal LANs, auth-gen-token,
#              renegotiation every 300 s, the shortest the plugin allows); the pool is originated into iBGP
#   lab-vpn    a remote user's machine on mnpwan (10.50.0.40, plain Debian + OpenVPN), the exported .ovpn profile
#
#   tools/lab/access.sh setup               create/refresh all of the above (idempotent); state in /root/lab/access/
#   tools/lab/access.sh publish on|off      add/remove the Access instance in the live cluster document and publish
#   tools/lab/access.sh profile             export the client profile (admin, re-verified) to lab-vpn
#   tools/lab/access.sh connect USER [PW]   (re)start the client as USER (password from the state unless given);
#                                           prints "connected <address>" or the failure seen in the client log
#   tools/lab/access.sh disconnect          stop the client
#   tools/lab/access.sh probe               lab-vpn → host-a/b/c over the tunnel; lab-b/lab-c route to the pool
#   tools/lab/access.sh ldap stop|start     stop/start the directory
#   tools/lab/access.sh policy on|off [S]   enable/disable the policy, session lifetime S seconds (default 3600)
#   tools/lab/access.sh manager stop|start  stop/start the manager container
#   tools/lab/access.sh group USER on|off   add/remove USER in vpn-users
#   tools/lab/access.sh log [N]             the manager's last N Access decisions
#   tools/lab/access.sh state               client service + address on lab-vpn, lab-a's view of the instance
#   tools/lab/access.sh watch SECS          lab-vpn pings host-b every 0.2 s for SECS; prints sent/lost
#   tools/lab/access.sh sessions            the manager's online-sessions list (from the nodes' reports, D39)
#   tools/lab/access.sh kick USER           disconnect USER from lab-a/access through the manager; prints the node
#                                           job's result and what the client on lab-vpn saw
set -eu
REPO=$(cd "$(dirname "$0")/../.." && pwd)
L="$REPO/tools/lab/lab.sh"
LAB=/root/lab
S=$LAB/access
V=lab-vpn
POOL=10.98.0.0/24
LDAP_IMAGE=docker.io/osixia/openldap:1.5.0
BASE=dc=lab,dc=test
lx() { c=$1; shift; incus exec "$c" -- "$@"; }
st() { jq -r ".$1" $S/state.json; }
ldapc() { lx lab-mgr podman exec -i ldap "$@"; }
# ldapmodify as the directory admin, LDIF on stdin
ldapmod() { ldapc ldapmodify -x -H ldap://localhost -D cn=admin,$BASE -w "$(st adminPassword)" "$@" >/dev/null; }

group_ldif() { # USER add|delete
	printf 'dn: cn=vpn-users,ou=groups,%s\nchangetype: modify\n%s: member\nmember: uid=%s,ou=people,%s\n' "$BASE" "$2" "$1" "$BASE"
}

case "${1:-}" in
setup)
	mkdir -p $S && chmod 700 $S
	[ -s $S/state.json ] || jq -n --arg a "$(openssl rand -hex 16)" --arg s "$(openssl rand -hex 16)" \
		--arg alice "$(openssl rand -hex 8)" --arg bob "$(openssl rand -hex 8)" --arg carol "$(openssl rand -hex 8)" \
		'{adminPassword: $a, servicePassword: $s, users: {alice: $alice, bob: $bob, carol: $carol}}' >$S/state.json
	chmod 600 $S/state.json
	# Directory certificates: own CA, server certificate for the Podman gateway address the manager connects to.
	if [ ! -s $S/ldap.crt ]; then
		openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 -subj "/CN=lab LDAP CA" \
			-keyout $S/ca.key -out $S/ca.crt 2>/dev/null
		openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=ldap.lab.test \
			-keyout $S/ldap.key -out $S/ldap.csr 2>/dev/null
		printf 'subjectAltName=DNS:ldap.lab.test,IP:10.88.0.1\nextendedKeyUsage=serverAuth\n' >$S/ext.cnf
		openssl x509 -req -in $S/ldap.csr -CA $S/ca.crt -CAkey $S/ca.key -CAcreateserial -days 30 -extfile $S/ext.cnf \
			-out $S/ldap.crt 2>/dev/null
		openssl genpkey -genparam -algorithm DH -pkeyopt group:ffdhe2048 -out $S/dhparam.pem 2>/dev/null
	fi
	lx lab-mgr mkdir -p /root/ldap/certs
	for f in ca.crt ldap.crt ldap.key dhparam.pem; do incus file push -q --mode 0644 $S/$f lab-mgr/root/ldap/certs/$f; done
	if ! lx lab-mgr podman container exists ldap; then
		lx lab-mgr podman image exists $LDAP_IMAGE || lx lab-mgr podman pull -q $LDAP_IMAGE >/dev/null
		lx lab-mgr podman volume exists ldap-data || lx lab-mgr podman volume create ldap-data >/dev/null
		lx lab-mgr podman volume exists ldap-conf || lx lab-mgr podman volume create ldap-conf >/dev/null
		# --ulimit: slapd sizes its tables by the file limit (D27); --restart always: survives a lab-mgr reboot
		lx lab-mgr podman run -d --name ldap --restart always --ulimit nofile=1024:1024 -p 10.88.0.1:636:636 \
			-v /root/ldap/certs:/container/service/slapd/assets/certs:ro \
			-v ldap-data:/var/lib/ldap -v ldap-conf:/etc/ldap/slapd.d \
			-e LDAP_ORGANISATION="Meduza lab" -e LDAP_DOMAIN=lab.test -e LDAP_ADMIN_PASSWORD="$(st adminPassword)" \
			-e LDAP_READONLY_USER=true -e LDAP_READONLY_USER_USERNAME=readonly \
			-e LDAP_READONLY_USER_PASSWORD="$(st servicePassword)" \
			-e LDAP_TLS=true -e LDAP_TLS_CRT_FILENAME=ldap.crt -e LDAP_TLS_KEY_FILENAME=ldap.key \
			-e LDAP_TLS_CA_CRT_FILENAME=ca.crt -e LDAP_TLS_DH_PARAM_FILENAME=dhparam.pem \
			-e LDAP_TLS_VERIFY_CLIENT=never -e LDAP_TLS_ENFORCE=false \
			$LDAP_IMAGE --copy-service >/dev/null
	fi
	i=0
	until ldapc ldapsearch -x -H ldap://localhost -b $BASE -D cn=readonly,$BASE -w "$(st servicePassword)" -s base dn >/dev/null 2>&1; do
		i=$((i + 1)); [ $i -gt 90 ] && { lx lab-mgr podman logs ldap | tail -30; exit 1; }; sleep 1
	done
	# Users and the group (once).
	if ! ldapc ldapsearch -x -H ldap://localhost -b ou=people,$BASE -D cn=admin,$BASE -w "$(st adminPassword)" -s base dn >/dev/null 2>&1; then
		{
			printf 'dn: ou=people,%s\nobjectClass: organizationalUnit\nou: people\n\n' $BASE
			printf 'dn: ou=groups,%s\nobjectClass: organizationalUnit\nou: groups\n\n' $BASE
			for u in alice bob carol; do
				printf 'dn: uid=%s,ou=people,%s\nobjectClass: inetOrgPerson\nuid: %s\ncn: %s\nsn: %s\nuserPassword: %s\n\n' \
					$u $BASE $u $u $u "$(jq -r ".users.$u" $S/state.json)"
			done
			printf 'dn: cn=vpn-users,ou=groups,%s\nobjectClass: groupOfNames\ncn: vpn-users\nmember: uid=alice,ou=people,%s\nmember: uid=carol,ou=people,%s\n' \
				$BASE $BASE $BASE
		} | ldapc ldapadd -x -H ldap://localhost -D cn=admin,$BASE -w "$(st adminPassword)" >/dev/null
	fi
	# Manager: bind password secret, LDAP settings, policy.
	if ! $L api call GET /api/v1/secrets | jq -e '.[] | select(.id == "ldap-bind-password")' >/dev/null; then
		jq -n --arg v "$(st servicePassword)" '{id: "ldap-bind-password", kind: "ldap-bind-password", value: $v, description: "lab LDAP read-only bind"}' >$S/secret.json
		$L api call POST /api/v1/secrets @$S/secret.json >/dev/null
		rm -f $S/secret.json
	fi
	jq -n --rawfile ca $S/ca.crt --arg base $BASE '{url: "ldaps://10.88.0.1:636", startTls: false, baseDn: $base,
		bindDn: ("cn=readonly," + $base), bindPasswordRef: "ldap-bind-password",
		userFilter: "(&(objectClass=inetOrgPerson)(uid={username}))", groupBaseDn: ("ou=groups," + $base),
		groupFilter: "(&(objectClass=groupOfNames)(member={dn}))", groupNameAttribute: "cn", useMemberOf: false,
		caPem: $ca, verifyCertificate: true, connectTimeoutMs: 3000, timeoutMs: 5000}' >$S/ldap.json
	$L api call PUT /api/v1/access/ldap @$S/ldap.json >/dev/null
	$L api call PUT /api/v1/access/policies/lab-vpn '{"name":"lab VPN users","enabled":true,"groups":["vpn-users"],"nodes":["lab-a"],"instances":["access"],"sessionSeconds":3600,"priority":10}' >/dev/null
	$L api call POST /api/v1/access/ldap/test '{"username":"alice"}' | jq -c .
	# Server certificate for the Access instance (key delivered to lab-a only).
	[ -s $S/server-cert.json ] ||
		$L api call POST /api/v1/pki/certs '{"caId":"lab","commonName":"lab-a-access","usage":"server","node":"lab-a"}' >$S/server-cert.json
	# The remote user's machine.
	if ! incus info $V >/dev/null 2>&1; then
		incus init deb13 $V -p default -p mnplab >/dev/null
		incus config device add $V eth0 nic network=mnpwan ipv4.address=10.50.0.40 >/dev/null
		incus start $V
		sleep 8
	fi
	lx $V sh -c 'command -v openvpn >/dev/null || { export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq openvpn iputils-ping iproute2 >/dev/null; }'
	echo "access lab ready: LDAP ldaps://10.88.0.1:636 on lab-mgr, policy lab-vpn, client machine $V 10.50.0.40"
	;;
publish)
	$L api call GET /api/v1/config | jq '.document // .' >$S/doc.json
	if [ "$2" = on ]; then
		jq --slurpfile c $S/server-cert.json --arg pool $POOL '
			($c[0]) as $cert
			| .nodes["lab-a"].services.openvpn.instances = ([.nodes["lab-a"].services.openvpn.instances[]? | select(.id != "access")] + [{
				id: "access", kind: "access", proto: "udp", port: 1195, publicEndpoint: "10.50.0.11:1195",
				tls: { caRef: $cert.caRef, certRef: $cert.certRef, keyRef: $cert.keyRef, crlRef: $cert.crlRef },
				access: { pool: $pool, renegSeconds: 300, pushRoutes: { lan: true, extra: ["10.62.0.0/24", "10.63.0.0/24"] } } }])
			| .nodes["lab-a"].routing.originate.accessPools = ["access"]' $S/doc.json >$S/doc2.json
	else
		jq '.nodes["lab-a"].services.openvpn.instances |= [.[] | select(.id != "access")]
			| del(.nodes["lab-a"].routing.originate.accessPools)' $S/doc.json >$S/doc2.json
	fi
	$L api publish $S/doc2.json "access $2"
	;;
profile)
	$L api call POST /api/v1/pki/access-profile '{"node":"lab-a","instance":"access"}' | jq -r .content >$S/client.ovpn
	sed -i 's|^auth-user-pass$|auth-user-pass /etc/openvpn/client/access.pass|' $S/client.ovpn
	incus file push -q --mode 0600 $S/client.ovpn $V/etc/openvpn/client/access.conf
	rm -f $S/client.ovpn
	echo "profile on $V: /etc/openvpn/client/access.conf"
	;;
connect)
	user=$2
	pw=${3:-$(jq -r ".users.$user // empty" $S/state.json)}
	lx $V sh -c 'systemctl stop openvpn-client@access 2>/dev/null; true'
	printf '%s\n%s\n' "$user" "$pw" | lx $V sh -c 'umask 077; cat >/etc/openvpn/client/access.pass'
	since=$(lx $V date '+%Y-%m-%d %H:%M:%S')
	t0=$(date +%s%N)
	lx $V systemctl start openvpn-client@access
	i=0
	while :; do
		log=$(lx $V journalctl -u openvpn-client@access --since "$since" --no-pager -o cat 2>/dev/null || true)
		if echo "$log" | grep -q 'Initialization Sequence Completed'; then
			echo "connected $(lx $V ip -4 -o addr show dev tun0 | awk '{print $4}') after $(( ($(date +%s%N) - t0) / 1000000 )) ms"
			break
		fi
		if echo "$log" | grep -q 'AUTH_FAILED'; then
			lx $V systemctl stop openvpn-client@access
			echo "refused after $(( ($(date +%s%N) - t0) / 1000000 )) ms: $(echo "$log" | grep -m1 'AUTH_FAILED' | sed 's/^.*AUTH_FAILED/AUTH_FAILED/')"
			break
		fi
		i=$((i + 1))
		[ $i -gt 60 ] && { echo "no result after 30 s"; echo "$log" | tail -8; break; }
		sleep 0.5
	done
	;;
disconnect)
	lx $V systemctl stop openvpn-client@access
	;;
probe)
	for h in 10.61.0.50 10.62.0.50 10.63.0.50; do
		printf '%s %s\n' $h "$(lx $V ping -c 3 -i 0.2 -W 1 -I tun0 $h 2>&1 | grep -oE '[0-9]+% packet loss' || echo 'no tun0')"
	done
	for n in lab-b lab-c; do
		printf '%s: %s\n' $n "$(lx $n podman exec meduza-np-planner ip route show $POOL 2>/dev/null | head -1)"
	done
	;;
ldap)
	lx lab-mgr podman "$2" ldap >/dev/null
	echo "ldap $2"
	;;
manager)
	lx lab-mgr podman "$2" mgr >/dev/null
	echo "manager $2"
	;;
state)
	printf 'client %s, tun0 %s
' "$(lx $V systemctl is-active openvpn-client@access || true)" 		"$(lx $V ip -4 -o addr show dev tun0 2>/dev/null | awk '{print $4}')"
	lx lab-a podman exec meduza-np-planner meduza-np --json status |
		jq -c '[.instances[]? | select((.id | startswith("openvpn/access")) and .kind != "process")
			| {id, state, clients, username, virtualAddress, connectedSince} | with_entries(select(.value != null))]'
	;;
policy)
	enabled=false; [ "$2" = on ] && enabled=true
	secs=${3:-3600}
	$L api call PUT /api/v1/access/policies/lab-vpn "{\"name\":\"lab VPN users\",\"enabled\":$enabled,\"groups\":[\"vpn-users\"],\"nodes\":[\"lab-a\"],\"instances\":[\"access\"],\"sessionSeconds\":$secs,\"priority\":10}" >/dev/null
	echo "policy lab-vpn enabled=$enabled sessionSeconds=$secs"
	;;
group)
	op=add; [ "$3" = off ] && op=delete
	group_ldif "$2" $op | ldapmod
	echo "$2 vpn-users: $3"
	;;
log)
	$L api call GET /api/v1/access/log | jq -c ".[:${2:-5}][] | {at, user: .username, node: .node_id, instance: .instance_id, decision, reason: .reason_code, ms: .duration_ms}"
	;;
sessions)
	$L api call GET /api/v1/access/sessions | jq -c '.[] | {node: .nodeId, instance: .instanceId, user: .username, vpn: .virtualAddress, from: .realAddress, since: .connectedSince, clientId, reportedAt}'
	;;
kick)
	since=$(lx $V date '+%Y-%m-%d %H:%M:%S')
	t0=$(date +%s%N)
	job=$($L api call POST /api/v1/access/sessions/disconnect "{\"node\":\"lab-a\",\"instance\":\"access\",\"username\":\"$2\"}" | jq -r .jobId)
	i=0
	while :; do
		r=$($L api call GET '/api/v1/jobs?node=lab-a' | jq -c --arg j "$job" '.[] | select(.id == $j) | {status, error_code, message, result: (.result // "null" | fromjson? // .)}')
		case "$r" in *'"status":"pending"'*|*'"status":"delivered"'*|'') ;; *) break ;; esac
		i=$((i + 1))
		[ $i -gt 120 ] && break
		sleep 0.5
	done
	echo "job $job after $(( ($(date +%s%N) - t0) / 1000000 )) ms: $r"
	sleep 2
	echo "client: $(lx $V systemctl is-active openvpn-client@access || true)"
	lx $V journalctl -u openvpn-client@access --since "$since" --no-pager -o cat 2>/dev/null | grep -iE 'halt|exit|restart|AUTH_FAILED|SIGTERM' | head -5 || true
	;;
watch)
	n=$(( $2 * 5 ))
	lx $V ping -c $n -i 0.2 -W 1 10.62.0.50 2>&1 | grep -E 'packets transmitted'
	;;
*)
	sed -n '2,34p' "$0"
	exit 2
	;;
esac
