#!/bin/sh
# Prints the lab cluster document (D17/D18/D24/D25/D27 roles):
#   - internal mesh: Tinc between lab-a, lab-b, lab-c on the WAN, no NAT between them (each has publicAddresses);
#     iBGP (internal ASN 65010) with BFD over the Tinc addresses syncs the internal LAN routes 10.61-63.0.0/24.
#     Nothing FRR-specific is written: Tinc membership plus the internal ASN imply it (BFD timers: the default 300/3)
#   - roaming member: lab-roam (behind NAT, network 10.64.0.0/24) is in the same mesh (D27): it only dials out to
#     the static members, and its iBGP sessions run without BFD on short timers (3/9); what it only transits is the
#     last resort
#   - Anycast: pool 10.255.0.0/24, service "web" (10.255.0.0/30, TCP 8080) on lab-a and lab-c; every static mesh
#     member runs OSPF over its Tinc device and routes the pool to the nearest healthy instance
#   - external router: lab-ext (unmanaged, tools/lab/external.sh; AS 65100, network 10.70.0.0/24) dials the OpenVPN
#     Site2Site TLS server `ext` on lab-a (p2p 10.99.0.1 <-> 10.99.0.2); the instance's bgp block is the eBGP session
#     (accepts 10.70.0.0/24, advertises the internal network). Included once /root/lab/lab-a-ext-cert.json exists
# Keys come from the manager: tinc-<node> (node-held, generated on the node).
set -eu

# OpenVPN Site2Site server for lab-ext (certificate refs from tools/lab/external.sh).
CERT=/root/lab/lab-a-ext-cert.json
openvpn=""
if [ -s "$CERT" ]; then
	openvpn=$(jq -c '{ instances: [ {
		id: "ext", kind: "site2site", role: "server", topology: "p2p", proto: "udp", port: 1194,
		ifconfig: { local: "10.99.0.1", remote: "10.99.0.2" }, publicEndpoint: "10.50.0.11:1194",
		tls: { caRef: .caRef, certRef: .certRef, keyRef: .keyRef, crlRef: .crlRef },
		bgp: { remoteAsn: 65100, keepalive: 10, hold: 30, accept: ["10.70.0.0/24"], exportInternal: true } } ] }' "$CERT")
	openvpn=", \"openvpn\": $openvpn"
fi

# One internal mesh (D22): services.tinc is this node's member settings; a roaming member has no public address.
tinc() { # node ip [public]
	pub=""
	[ -n "${3:-}" ] && pub="\"publicAddresses\": [\"$3\"], "
	cat <<EOF
"tinc": { "privateKeyRef": "tinc-$1", "address": "10.42.0.$2/24", $pub"port": 655 }
EOF
}

cat <<EOF
{
  "schemaVersion": 1,
  "routing": {
    "internalAsn": 65010, "maximumPaths": 4,
    "anycast": { "pool": "10.255.0.0/24", "services": [
      { "id": "web", "description": "lab web", "prefix": "10.255.0.0/30", "health": { "kind": "tcp", "port": 8080 } } ] }
  },
  "nodes": {
    "lab-a": {
      "behavior": "static", "routerId": "10.42.0.1",
      "networks": { "lan": ["10.61.0.0/24"] },
      "routing": { "originate": { "lan": true }, "anycast": ["web"] },
      "services": { $(tinc lab-a 1 10.50.0.11)$openvpn }
    },
    "lab-b": {
      "behavior": "static", "routerId": "10.42.0.2",
      "networks": { "lan": ["10.62.0.0/24"] }, "routing": { "originate": { "lan": true } },
      "services": { $(tinc lab-b 2 10.50.0.12) }
    },
    "lab-c": {
      "behavior": "static", "routerId": "10.42.0.4",
      "networks": { "lan": ["10.63.0.0/24"] }, "routing": { "originate": { "lan": true }, "anycast": ["web"] },
      "services": { $(tinc lab-c 4 10.50.0.14) }
    },
    "lab-roam": {
      "behavior": "roaming", "routerId": "10.42.0.5",
      "networks": { "lan": ["10.64.0.0/24"] }, "routing": { "originate": { "lan": true } },
      "services": { $(tinc lab-roam 5) }
    }
  }
}
EOF
