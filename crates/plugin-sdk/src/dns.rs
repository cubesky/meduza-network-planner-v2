//! Minimal DNS probing used by health checks (Anycast guard, DNS front end): one A query over UDP with a
//! timeout. A NOERROR or NXDOMAIN answer to our id proves the resolver answers queries.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::Duration;

/// A query for `name`, type A, class IN, recursion desired.
pub fn query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        let l = &label.as_bytes()[..label.len().min(63)];
        q.push(l.len() as u8);
        q.extend_from_slice(l);
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

pub fn answer_ok(id: u16, resp: &[u8]) -> bool {
    resp.len() >= 12 && resp[0..2] == id.to_be_bytes() && resp[2] & 0x80 != 0 && matches!(resp[3] & 0x0f, 0 | 3)
}

fn query_id() -> u16 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0x5a5a);
    (nanos as u16) ^ (std::process::id() as u16)
}

/// Sends one query and waits up to `timeout` for a matching answer. Wildcard listener addresses describe a
/// local bind, not a remote endpoint: query their matching loopback address and validate replies against it.
pub fn probe(mut server: SocketAddr, name: &str, timeout: Duration) -> Result<(), String> {
    match server.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => server.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => server.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    let bind = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let id = query_id();
    sock.send_to(&query(id, name), server).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 512];
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == server && answer_ok(id, &buf[..n]) => return Ok(()),
            Ok((_n, from)) if from == server => return Err("unexpected DNS answer".into()),
            Ok(_) => continue,
            // the read timeout surfaces as EAGAIN (Unix) or TimedOut (Windows)
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("timeout".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets() {
        let q = query(0x1234, "health.local");
        assert_eq!(&q[..2], &[0x12, 0x34]);
        assert_eq!(&q[12..19], b"\x06health");
        let mut resp = q.clone();
        resp[2] = 0x81;
        resp[3] = 0x83;
        assert!(answer_ok(0x1234, &resp), "NXDOMAIN still proves a working resolver");
        resp[3] = 0x82;
        assert!(!answer_ok(0x1234, &resp), "SERVFAIL does not");
        assert!(!answer_ok(0x9999, &q));
    }

    #[test]
    fn probe_against_fake_server() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let mut resp = buf[..n].to_vec();
            resp[2] = 0x81;
            resp[3] = 0x80;
            server.send_to(&resp, from).unwrap();
        });
        probe(addr, "example.test", Duration::from_secs(2)).unwrap();
        t.join().unwrap();
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(probe(silent.local_addr().unwrap(), "example.test", Duration::from_millis(200)), Err("timeout".into()));
    }

    fn probe_wildcard_listener(ip: &str) {
        let server = UdpSocket::bind(ip).unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let addr = server.local_addr().unwrap();
        let t = std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).unwrap();
            let mut resp = buf[..n].to_vec();
            resp[2] = 0x81;
            resp[3] = 0x80;
            server.send_to(&resp, from).unwrap();
        });
        let result = probe(addr, "example.test", Duration::from_millis(300));
        t.join().unwrap();
        assert_eq!(result, Ok(()), "a wildcard listener must be queried through loopback");
    }

    #[test]
    fn probe_ipv4_wildcard_listener() {
        probe_wildcard_listener("0.0.0.0:0");
    }

    #[test]
    fn probe_ipv6_wildcard_listener() {
        probe_wildcard_listener("[::]:0");
    }
}
