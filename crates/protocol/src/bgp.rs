//! External BGP session bound to an external connection (D24).
//!
//! eBGP runs over WireGuard and OpenVPN only (D17), so operators set it where the connection is defined: on a
//! WireGuard peer or an OpenVPN Site2Site instance. The VPN plugins never act on it; the manager compiles every
//! session into the node's FRR configuration (`services.frr.externalNeighbors`, transport filled in) and strips it
//! from the VPN plugin's own config.

use std::net::IpAddr;

use ipnet::Ipv4Net;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Validator;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BgpSession {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// AS number of the external side.
    #[schemars(range(min = 1, max = 4294967294u32))]
    pub remote_asn: u32,
    /// The neighbor's tunnel address. Defaults to the peer's host route in `allowedIps` (WireGuard) or the remote
    /// `ifconfig` address (OpenVPN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<IpAddr>,
    /// Prefixes accepted from the peer, `10.64.0.0/16` or `10.64.0.0/16 le 24`. Empty: everything the peer
    /// originates (see `noTransit`).
    #[serde(default)]
    #[schemars(regex(pattern = r"^\d{1,3}(\.\d{1,3}){3}/\d{1,2}( le \d{1,2})?$"))]
    pub accept: Vec<String>,
    /// Also accept a default route from the peer.
    #[serde(default)]
    pub accept_default: bool,
    /// Advertise this node's LAN prefixes.
    #[serde(default = "default_true")]
    pub export_lan: bool,
    /// Advertise the public prefixes of the whole internal network (the other mesh members' LANs).
    #[serde(default)]
    pub export_internal: bool,
    #[serde(default)]
    pub export_mappings: bool,
    #[serde(default)]
    pub export_access_pools: bool,
    #[serde(default)]
    pub export_default_route: bool,
    /// Further prefixes to advertise.
    #[serde(default)]
    pub export_extra: Vec<Ipv4Net>,
    /// Do not use the peer as a transit path: accept only routes it originated itself.
    #[serde(default = "default_true")]
    pub no_transit: bool,
    /// Advertise only routes of our own AS to the peer, never routes learned from other external peers.
    #[serde(default = "default_true")]
    pub no_forward: bool,
    /// BFD with the cluster's BFD timers. Never used with roaming nodes (it costs bandwidth).
    #[serde(default)]
    pub bfd: bool,
    /// BGP keepalive seconds (FRR default 60). Sessions without BFD notice a vanished peer only by the hold timer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 3600))]
    pub keepalive: Option<u16>,
    /// BGP hold seconds (FRR default 180), at least three keepalives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 3, max = 65535))]
    pub hold: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_preference: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// A prefix filter as written in `accept`: the network and the optional `le` bound.
pub fn parse_prefix_filter(s: &str) -> Option<(Ipv4Net, Option<u8>)> {
    let mut parts = s.split_whitespace();
    let net: Ipv4Net = parts.next()?.parse().ok()?;
    let le = match (parts.next(), parts.next()) {
        (None, _) => None,
        (Some("le"), Some(n)) => Some(n.parse::<u8>().ok().filter(|&n| n >= net.prefix_len() && n <= 32)?),
        _ => return None,
    };
    if parts.next().is_some() || net.network() != net.addr() {
        return None;
    }
    Some((net, le))
}

impl BgpSession {
    pub fn validate(&self, v: &mut Validator, path: &str) {
        v.check((1..=4_294_967_294).contains(&self.remote_asn), format!("{path}/remoteAsn"), "ASN must be 1..4294967294");
        for (i, a) in self.accept.iter().enumerate() {
            v.check(
                parse_prefix_filter(a).is_some(),
                format!("{path}/accept/{i}"),
                "expected an IPv4 network, optionally followed by `le <length>`",
            );
        }
        if let (Some(k), Some(h)) = (self.keepalive, self.hold) {
            v.check(h >= 3 * k, format!("{path}/hold"), "hold must be at least three keepalives");
        }
        v.check(self.keepalive.is_some() == self.hold.is_some(), format!("{path}/hold"), "set keepalive and hold together");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_filters() {
        assert_eq!(parse_prefix_filter("10.64.0.0/16"), Some(("10.64.0.0/16".parse().unwrap(), None)));
        assert_eq!(parse_prefix_filter("10.64.0.0/16 le 24"), Some(("10.64.0.0/16".parse().unwrap(), Some(24))));
        for bad in ["10.64.0.1/16", "10.64.0.0/16 le 8", "10.64.0.0/16 ge 24", "10.64.0.0/16 le", "x"] {
            assert_eq!(parse_prefix_filter(bad), None, "{bad}");
        }
    }

    #[test]
    fn defaults_and_checks() {
        let s: BgpSession = serde_json::from_value(serde_json::json!({ "remoteAsn": 65020 })).unwrap();
        assert!(s.enabled && s.export_lan && s.no_transit && s.no_forward && !s.bfd && !s.export_internal);
        let mut v = Validator::new();
        BgpSession { keepalive: Some(5), hold: Some(9), accept: vec!["10.0.0.0/8 le 40".into()], ..s }.validate(&mut v, "/bgp");
        let paths: Vec<_> = v.errors().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["/bgp/accept/0", "/bgp/hold"]);
    }
}
