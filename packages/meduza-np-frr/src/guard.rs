//! Anycast health guard (§9.5 item 4): announces a VIP only while its local service is healthy.
//!
//! The VIP lives on a dedicated dummy device that OSPF advertises as a passive stub. The guard adds the service
//! addresses and raises the device after `rise` consecutive good checks, and lowers it and removes the addresses
//! (a down device keeps its local routes) after `fall` bad ones, on maintenance/stop
//! (SIGTERM) and at startup — so a VIP is withdrawn before an upgrade drains the service and re-announced
//! only after it recovers.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::HealthCheck;

pub const RISE: u32 = 3;
pub const FALL: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(1);

/// Hysteresis state machine; returns `Some(new_state)` on transitions.
#[derive(Debug, Default)]
pub struct Hysteresis {
    pub healthy: bool,
    good: u32,
    bad: u32,
}

impl Hysteresis {
    pub fn observe(&mut self, ok: bool) -> Option<bool> {
        if ok {
            self.good += 1;
            self.bad = 0;
            if !self.healthy && self.good >= RISE {
                self.healthy = true;
                return Some(true);
            }
        } else {
            self.bad += 1;
            self.good = 0;
            if self.healthy && self.bad >= FALL {
                self.healthy = false;
                return Some(false);
            }
        }
        None
    }
}

pub fn check(h: &HealthCheck) -> Result<(), String> {
    match h {
        HealthCheck::Tcp { host, port } => {
            TcpStream::connect_timeout(&SocketAddr::new(*host, *port), TIMEOUT).map(|_| ()).map_err(|e| e.to_string())
        }
        HealthCheck::Dns { server, port, name } => meduza_np_plugin_sdk::dns::probe(SocketAddr::new(*server, *port), name, TIMEOUT),
        HealthCheck::Http { host, port, path } => {
            let mut s = TcpStream::connect_timeout(&SocketAddr::new(*host, *port), TIMEOUT).map_err(|e| e.to_string())?;
            s.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
            write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
            let mut head = [0u8; 64];
            let n = s.read(&mut head).map_err(|e| e.to_string())?;
            let line = String::from_utf8_lossy(&head[..n]);
            let code: u16 = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
            if (200..400).contains(&code) {
                Ok(())
            } else {
                Err(format!("HTTP status {code}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis() {
        let mut h = Hysteresis::default();
        assert_eq!(h.observe(true), None);
        assert_eq!(h.observe(true), None);
        assert_eq!(h.observe(true), Some(true));
        assert_eq!(h.observe(false), None);
        assert_eq!(h.observe(true), None, "a single success resets the failure streak");
        for _ in 0..2 {
            assert_eq!(h.observe(false), None);
        }
        assert_eq!(h.observe(false), Some(false));
    }

    #[test]
    fn tcp_check_against_local_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(check(&HealthCheck::Tcp { host: "127.0.0.1".parse().unwrap(), port }).is_ok());
        drop(l);
        assert!(check(&HealthCheck::Tcp { host: "127.0.0.1".parse().unwrap(), port }).is_err());
    }
}
