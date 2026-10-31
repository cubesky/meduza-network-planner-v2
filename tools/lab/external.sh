#!/bin/sh
# External (unmanaged) router for the lab: lab-ext 10.50.0.30 on mnpwan, its own AS 65100 and network 10.70.0.0/24.
# It is not a Meduza node: plain Debian OpenVPN + FRR, the way a partner or provider router would be. It dials the
# OpenVPN Site2Site TLS server `ext` on gateway lab-a (10.50.0.11:1194, tunnel 10.99.0.1 <-> 10.99.0.2) and runs eBGP
# over the tunnel. Certificates: lab-a's server certificate is issued by the manager's lab CA (key delivered to
# lab-a); lab-ext keeps its own key and only sends a CSR for the manager to sign.
#
#   tools/lab/external.sh        create/refresh lab-ext (idempotent); writes /root/lab/lab-a-ext-cert.json
set -eu
L="$(dirname "$0")/lab.sh"
LAB=/root/lab
X=lab-ext
lx() { incus exec "$X" -- "$@"; }

if ! incus info "$X" >/dev/null 2>&1; then
	incus init deb13 "$X" -p default -p mnplab >/dev/null
	incus config device add "$X" eth0 nic network=mnpwan ipv4.address=10.50.0.30 >/dev/null
	incus start "$X"
	sleep 8
fi
lx sh -c 'command -v openvpn >/dev/null && command -v vtysh >/dev/null || {
	export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y -qq openvpn frr openssl iproute2 iputils-ping >/dev/null; }'

# PKI: the lab CA (once), lab-a's server certificate (once), lab-ext's client certificate from its own CSR.
$L api call GET /api/v1/pki/cas | grep -q '"id": "lab"' || $L api call POST /api/v1/pki/cas '{"id":"lab","name":"Lab CA"}' >/dev/null
[ -s $LAB/lab-a-ext-cert.json ] ||
	$L api call POST /api/v1/pki/certs '{"caId":"lab","commonName":"lab-a-ext","usage":"server","node":"lab-a"}' >$LAB/lab-a-ext-cert.json
lx sh -c 'mkdir -p /etc/openvpn/client/ext && cd /etc/openvpn/client/ext && [ -s client.key ] ||
	openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout client.key -out client.csr -subj /CN=lab-ext 2>/dev/null; chmod 600 client.key'
if ! lx test -s /etc/openvpn/client/ext/client.crt; then
	incus file pull "$X/etc/openvpn/client/ext/client.csr" $LAB/lab-ext.csr
	jq -n --rawfile csr $LAB/lab-ext.csr '{caId: "lab", csrPem: $csr, usage: "client"}' >$LAB/lab-ext-csr.json
	$L api call POST /api/v1/pki/csr @$LAB/lab-ext-csr.json | jq -r .certPem >$LAB/lab-ext.crt
	incus file push -q $LAB/lab-ext.crt "$X/etc/openvpn/client/ext/client.crt"
fi
$L api pubkey pki-ca-lab-cert >$LAB/lab-ca.crt
incus file push -q $LAB/lab-ca.crt "$X/etc/openvpn/client/ext/ca.crt"

# OpenVPN client (p2p, TLS) towards lab-a; FRR eBGP 65100 <-> 65010 over the tunnel; the external network.
lx sh -c 'cat >/etc/openvpn/client/ext.conf' <<'EOF'
dev tun-ext
proto udp
remote 10.50.0.11 1194
nobind
ifconfig 10.99.0.2 10.99.0.1
tls-client
ca /etc/openvpn/client/ext/ca.crt
cert /etc/openvpn/client/ext/client.crt
key /etc/openvpn/client/ext/client.key
remote-cert-tls server
verify-x509-name lab-a-ext name
keepalive 10 60
persist-key
persist-tun
verb 3
EOF
lx sh -c 'sed -i "s/^bgpd=no/bgpd=yes/" /etc/frr/daemons; cat >/etc/frr/frr.conf' <<'EOF'
frr defaults traditional
hostname lab-ext
log syslog informational
!
router bgp 65100
 bgp router-id 10.70.0.1
 no bgp ebgp-requires-policy
 neighbor 10.99.0.1 remote-as 65010
 neighbor 10.99.0.1 description lab-a
 neighbor 10.99.0.1 timers 10 30
 address-family ipv4 unicast
  network 10.70.0.0/24
 exit-address-family
!
EOF
lx sh -c 'cat >/etc/systemd/system/lab-ext-lan.service' <<'EOF'
[Unit]
Description=lab-ext external network 10.70.0.0/24
Before=frr.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'ip link add ext0 type dummy 2>/dev/null || true; ip addr replace 10.70.0.1/24 dev ext0; ip link set ext0 up'
[Install]
WantedBy=multi-user.target
EOF
lx systemctl daemon-reload
lx systemctl enable --now lab-ext-lan.service openvpn-client@ext.service >/dev/null 2>&1
lx systemctl restart openvpn-client@ext.service frr.service
echo "lab-ext ready: OpenVPN client to lab-a, AS 65100, network 10.70.0.0/24"
