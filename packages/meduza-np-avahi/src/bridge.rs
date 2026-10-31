//! `.local` DNS → Avahi bridge (AGENT.md §9.11): a UDP DNS listener on loopback that answers only A/AAAA
//! queries under `.local` by asking Avahi over D-Bus. Everything else is REFUSED, so the front end can never
//! turn it into a general mDNS forwarder. Bounded in-flight lookups, per-lookup timeout, small TTL cache.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TYPE_A: u16 = 1;
pub const TYPE_AAAA: u16 = 28;
pub const RCODE_OK: u8 = 0;
pub const RCODE_FORMERR: u8 = 1;
pub const RCODE_SERVFAIL: u8 = 2;
pub const RCODE_NXDOMAIN: u8 = 3;
pub const RCODE_REFUSED: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub id: u16,
    pub rd: bool,
    /// Lowercase, without the trailing dot.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Raw question section (copied into the answer).
    pub question: Vec<u8>,
}

/// Parses a standard query with exactly one question (no compression in the question, as RFC 1035 senders do).
pub fn parse_query(b: &[u8]) -> Result<Query, u8> {
    if b.len() < 12 {
        return Err(RCODE_FORMERR);
    }
    let id = u16::from_be_bytes([b[0], b[1]]);
    if b[2] & 0x80 != 0 || (b[2] >> 3) & 0x0f != 0 || u16::from_be_bytes([b[4], b[5]]) != 1 {
        return Err(RCODE_FORMERR);
    }
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let l = *b.get(pos).ok_or(RCODE_FORMERR)? as usize;
        pos += 1;
        if l == 0 {
            break;
        }
        if l > 63 || pos + l > b.len() {
            return Err(RCODE_FORMERR);
        }
        let label = &b[pos..pos + l];
        if !label.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_') {
            return Err(RCODE_REFUSED);
        }
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        pos += l;
    }
    if pos + 4 > b.len() || labels.iter().map(|l| l.len() + 1).sum::<usize>() > 255 {
        return Err(RCODE_FORMERR);
    }
    let qtype = u16::from_be_bytes([b[pos], b[pos + 1]]);
    let qclass = u16::from_be_bytes([b[pos + 2], b[pos + 3]]);
    Ok(Query { id, rd: b[2] & 0x01 != 0, name: labels.join("."), qtype, qclass, question: b[12..pos + 4].to_vec() })
}

/// A response to `q` with the addresses that match its type.
pub fn response(q: &Query, rcode: u8, addrs: &[IpAddr], ttl: u32) -> Vec<u8> {
    let answers: Vec<&IpAddr> =
        addrs.iter().filter(|a| (q.qtype == TYPE_A && a.is_ipv4()) || (q.qtype == TYPE_AAAA && a.is_ipv6())).collect();
    let mut r = Vec::with_capacity(12 + q.question.len() + answers.len() * 28);
    r.extend_from_slice(&q.id.to_be_bytes());
    r.push(0x80 | u8::from(q.rd));
    r.push(0x80 | (rcode & 0x0f));
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q.question);
    for a in answers {
        r.extend_from_slice(&[0xc0, 0x0c]);
        r.extend_from_slice(&q.qtype.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&ttl.to_be_bytes());
        match a {
            IpAddr::V4(v4) => {
                r.extend_from_slice(&4u16.to_be_bytes());
                r.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                r.extend_from_slice(&16u16.to_be_bytes());
                r.extend_from_slice(&v6.octets());
            }
        }
    }
    r
}

/// Minimal answer for a header we could not parse as a query (keeps the client from waiting).
fn error_for(b: &[u8], rcode: u8) -> Option<Vec<u8>> {
    if b.len() < 12 || b[2] & 0x80 != 0 {
        return None;
    }
    let mut r = b[..12].to_vec();
    r[2] = 0x80 | (b[2] & 0x01);
    r[3] = 0x80 | rcode;
    r[4..12].fill(0);
    Some(r)
}

#[derive(Debug, Clone)]
pub struct Options {
    pub listen: SocketAddr,
    pub max_inflight: usize,
    pub timeout: Duration,
    pub cache_seconds: u32,
    /// Interface indexes to ask Avahi on; empty = any interface.
    pub interfaces: Vec<i32>,
}

/// Resolver used by the bridge: `(interface index or -1, name, v6) → address`.
pub type Resolver = dyn Fn(i32, &str, bool, Duration) -> Result<Option<IpAddr>, String> + Send + Sync;

/// Answers the node's own name without asking Avahi: `(name, v6) → Some(addresses)` when `name` is this host
/// (D50: with the reflector on, avahi-daemon's ResolveHostName never answers its own name).
pub type SelfLookup = dyn Fn(&str, bool) -> Option<Vec<IpAddr>> + Send + Sync;

type Cache = Mutex<HashMap<(String, u16), (u8, Vec<IpAddr>, Instant)>>;

const CACHE_MAX: usize = 512;
const NEGATIVE_SECONDS: u64 = 5;

/// Asks Avahi on every allowed interface at once: the first address wins, and a miss costs one timeout instead of
/// one per interface (a reflector needs at least two, which pushed misses past the probe and client deadlines).
fn lookup(resolver: &Arc<Resolver>, opts: &Options, name: &str, v6: bool) -> (u8, Vec<IpAddr>) {
    let ifaces: Vec<i32> = if opts.interfaces.is_empty() { vec![crate::dbus::IF_UNSPEC] } else { opts.interfaces.clone() };
    let (tx, rx) = std::sync::mpsc::channel();
    for i in ifaces.iter().copied() {
        let (tx, resolver, name, timeout) = (tx.clone(), resolver.clone(), name.to_string(), opts.timeout);
        std::thread::spawn(move || {
            let _ = tx.send(resolver(i, &name, v6, timeout));
        });
    }
    drop(tx);
    let mut failed = false;
    for r in rx {
        match r {
            Ok(Some(a)) => return (RCODE_OK, vec![a]),
            Ok(None) => {}
            Err(e) => {
                eprintln!("resolve {name}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        (RCODE_SERVFAIL, Vec::new())
    } else {
        (RCODE_NXDOMAIN, Vec::new())
    }
}

/// Serves until `stop` is set. Only loopback clients are answered.
pub fn serve(
    sock: UdpSocket,
    opts: Options,
    resolver: Arc<Resolver>,
    self_lookup: Option<Arc<SelfLookup>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_millis(500)))?;
    let sock = Arc::new(sock);
    let cache: Arc<Cache> = Arc::new(Mutex::new(HashMap::new()));
    let inflight = Arc::new(AtomicUsize::new(0));
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::SeqCst) {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            // SIGTERM interrupts recvfrom (a socket with SO_RCVTIMEO is never restarted): re-check `stop`, exit 0
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => continue,
            Err(e) => return Err(e),
        };
        if !from.ip().is_loopback() {
            continue;
        }
        let pkt = &buf[..n];
        let q = match parse_query(pkt) {
            Ok(q) => q,
            Err(rcode) => {
                if let Some(r) = error_for(pkt, rcode) {
                    let _ = sock.send_to(&r, from);
                }
                continue;
            }
        };
        if q.qclass != 1 || !q.name.ends_with(".local") {
            let _ = sock.send_to(&response(&q, RCODE_REFUSED, &[], 0), from);
            continue;
        }
        if q.qtype != TYPE_A && q.qtype != TYPE_AAAA {
            // The name may exist; we just have no data of that type (NODATA).
            let _ = sock.send_to(&response(&q, RCODE_OK, &[], 0), from);
            continue;
        }
        let key = (q.name.clone(), q.qtype);
        if let Some((rcode, addrs, until)) = cache.lock().unwrap().get(&key).cloned() {
            if until > Instant::now() {
                let ttl = until.saturating_duration_since(Instant::now()).as_secs() as u32;
                let _ = sock.send_to(&response(&q, rcode, &addrs, ttl.max(1)), from);
                continue;
            }
        }
        if inflight.fetch_add(1, Ordering::SeqCst) >= opts.max_inflight {
            inflight.fetch_sub(1, Ordering::SeqCst);
            let _ = sock.send_to(&response(&q, RCODE_SERVFAIL, &[], 0), from);
            continue;
        }
        let (sock, cache, inflight, resolver, opts) = (sock.clone(), cache.clone(), inflight.clone(), resolver.clone(), opts.clone());
        let self_lookup = self_lookup.clone();
        std::thread::spawn(move || {
            let v6 = q.qtype == TYPE_AAAA;
            // Own name with no address of the asked family: NODATA, like Avahi for a host without one.
            let (rcode, addrs) = match self_lookup.as_ref().and_then(|f| f(&q.name, v6)) {
                Some(addrs) => (RCODE_OK, addrs),
                None => lookup(&resolver, &opts, &q.name, v6),
            };
            if rcode != RCODE_SERVFAIL {
                let secs = if rcode == RCODE_OK { u64::from(opts.cache_seconds) } else { NEGATIVE_SECONDS };
                let mut c = cache.lock().unwrap();
                if c.len() >= CACHE_MAX {
                    let now = Instant::now();
                    c.retain(|_, v| v.2 > now);
                    if c.len() >= CACHE_MAX {
                        c.clear();
                    }
                }
                c.insert(key, (rcode, addrs.clone(), Instant::now() + Duration::from_secs(secs)));
            }
            let _ = sock.send_to(&response(&q, rcode, &addrs, opts.cache_seconds), from);
            inflight.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(())
}

/// The node's own addresses on `names` (any non-loopback interface when empty) for the self answer: global
/// addresses first, then link-local (Avahi announces both).
#[cfg(unix)]
fn local_addresses(names: &[String], v6: bool) -> Vec<IpAddr> {
    let mut found: Vec<(String, IpAddr)> = Vec::new();
    let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list that is only read here and released with freeifaddrs; every sockaddr is
    // read according to its sa_family.
    unsafe {
        if libc::getifaddrs(&mut ifap) != 0 {
            return Vec::new();
        }
        let mut p = ifap;
        while !p.is_null() {
            let ifa = &*p;
            p = ifa.ifa_next;
            if ifa.ifa_addr.is_null() || ifa.ifa_name.is_null() || ifa.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
            let addr = match i32::from((*ifa.ifa_addr).sa_family) {
                libc::AF_INET if !v6 => {
                    let sa = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(u32::from_be(sa.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 if v6 => IpAddr::from((*(ifa.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr),
                _ => continue,
            };
            found.push((name, addr));
        }
        libc::freeifaddrs(ifap);
    }
    order_addresses(names, &found)
}

#[cfg(not(unix))]
fn local_addresses(_names: &[String], _v6: bool) -> Vec<IpAddr> {
    Vec::new()
}

/// Addresses in the order of `names` (as found when empty), link-local ones last.
#[cfg_attr(not(unix), allow(dead_code))]
fn order_addresses(names: &[String], found: &[(String, IpAddr)]) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = if names.is_empty() {
        found.iter().map(|x| x.1).collect()
    } else {
        names.iter().flat_map(|n| found.iter().filter(move |(i, _)| i == n).map(|x| x.1)).collect()
    };
    out.sort_by_key(|a| matches!(a, IpAddr::V6(v) if v.segments()[0] & 0xffc0 == 0xfe80));
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(*a));
    out
}

fn own_host_name(timeout: Duration) -> Option<String> {
    #[cfg(unix)]
    {
        crate::dbus::Conn::system(timeout).ok()?.host_name_fqdn().ok()
    }
    #[cfg(not(unix))]
    {
        let _ = timeout;
        None
    }
}

/// `bridge` mode entry: `bridge --listen 127.0.0.1:5354 --max-inflight 16 --timeout-ms 3000 --cache-seconds 30
/// [--interfaces a,b] [--self-answer v4|v6|v4,v6]`. `--self-answer` (reflector on, addresses published) answers
/// the daemon's own host name from the local addresses of those families.
pub fn run(args: &[String]) -> ! {
    let get = |k: &str| args.windows(2).find(|w| w[0] == k).map(|w| w[1].clone());
    let listen: SocketAddr = get("--listen").and_then(|s| s.parse().ok()).unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 5354)));
    if !listen.ip().is_loopback() {
        eprintln!("the bridge only listens on loopback");
        std::process::exit(2);
    }
    let names: Vec<String> =
        get("--interfaces").map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
    let interfaces: Vec<i32> = names
        .iter()
        .filter_map(|n| std::fs::read_to_string(format!("/sys/class/net/{n}/ifindex")).ok().and_then(|t| t.trim().parse().ok()))
        .collect();
    if !names.is_empty() && interfaces.is_empty() {
        eprintln!("none of the allowed interfaces exist yet; answering SERVFAIL until restarted");
    }
    let opts = Options {
        listen,
        max_inflight: get("--max-inflight").and_then(|s| s.parse().ok()).unwrap_or(16),
        timeout: Duration::from_millis(get("--timeout-ms").and_then(|s| s.parse().ok()).unwrap_or(3000)),
        cache_seconds: get("--cache-seconds").and_then(|s| s.parse().ok()).unwrap_or(30),
        interfaces: if names.is_empty() {
            Vec::new()
        } else if interfaces.is_empty() {
            vec![i32::MIN]
        } else {
            interfaces
        },
    };
    let sock = UdpSocket::bind(listen).unwrap_or_else(|e| {
        eprintln!("cannot listen on {listen}: {e}");
        std::process::exit(1)
    });
    let stop = Arc::new(AtomicBool::new(false));
    crate::install_term(stop.clone());
    let resolver: Arc<Resolver> = Arc::new(|i, name: &str, v6, timeout| {
        if i == i32::MIN {
            return Err("allowed interfaces not present".into());
        }
        crate::dbus::resolve_once(i, name, v6, timeout)
    });
    let families = get("--self-answer").unwrap_or_default();
    let (self_v4, self_v6) = (families.split(',').any(|f| f == "v4"), families.split(',').any(|f| f == "v6"));
    let self_lookup: Option<Arc<SelfLookup>> = (self_v4 || self_v6).then(|| {
        let timeout = opts.timeout;
        let fqdn: Mutex<Option<(String, Instant)>> = Mutex::new(None);
        let f: Arc<SelfLookup> = Arc::new(move |name: &str, v6| {
            let own = {
                let mut c = fqdn.lock().unwrap();
                match c.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(10)) {
                    Some((n, _)) => n.clone(),
                    None => {
                        let n = own_host_name(timeout)?.to_ascii_lowercase();
                        *c = Some((n.clone(), Instant::now()));
                        n
                    }
                }
            };
            if own.trim_end_matches('.') != name {
                return None;
            }
            let wanted = if v6 { self_v6 } else { self_v4 };
            Some(if wanted { local_addresses(&names, v6) } else { Vec::new() })
        });
        f
    });
    match serve(sock, opts, resolver, self_lookup, stop) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("bridge: {e}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(name: &str, qtype: u16) -> Vec<u8> {
        let mut p = meduza_np_plugin_sdk::dns::query(0x4242, name);
        let n = p.len();
        p[n - 4..n - 2].copy_from_slice(&qtype.to_be_bytes());
        p
    }

    #[test]
    fn parse_and_answer() {
        let query = parse_query(&q("Printer.LOCAL", TYPE_A)).unwrap();
        assert_eq!((query.name.as_str(), query.qtype, query.rd), ("printer.local", TYPE_A, true));
        let r = response(&query, RCODE_OK, &["10.0.0.5".parse().unwrap(), "fe80::5".parse().unwrap()], 30);
        assert_eq!(&r[6..8], &[0, 1], "only the A record answers an A query");
        assert_eq!(&r[r.len() - 4..], &[10, 0, 0, 5]);
        assert!(meduza_np_plugin_sdk::dns::answer_ok(0x4242, &r));
        let mut resp = q("x.local", TYPE_A);
        resp[2] |= 0x80;
        assert_eq!(parse_query(&resp), Err(RCODE_FORMERR), "responses are not queries");
        assert_eq!(parse_query(&q("bad name.local", TYPE_A)), Err(RCODE_REFUSED));
    }

    #[test]
    fn serves_only_local_names_with_cache() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let c2 = calls.clone();
        let resolver: Arc<Resolver> = Arc::new(move |_i, name: &str, v6, _t| {
            c2.fetch_add(1, Ordering::SeqCst);
            match (name, v6) {
                ("printer.local", false) => Ok(Some("192.168.1.20".parse().unwrap())),
                ("broken.local", _) => Err("dbus down".into()),
                _ => Ok(None),
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let opts = Options { listen: addr, max_inflight: 4, timeout: Duration::from_secs(1), cache_seconds: 30, interfaces: Vec::new() };
        let s2 = stop.clone();
        let t = std::thread::spawn(move || serve(sock, opts, resolver, None, s2).unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let ask = |name: &str, qtype: u16| {
            client.send_to(&q(name, qtype), addr).unwrap();
            let mut buf = [0u8; 512];
            let (n, _) = client.recv_from(&mut buf).unwrap();
            buf[..n].to_vec()
        };
        let r = ask("printer.local", TYPE_A);
        assert_eq!((r[3] & 0x0f, &r[r.len() - 4..]), (RCODE_OK, &[192, 168, 1, 20][..]));
        let r = ask("printer.local", TYPE_A);
        assert_eq!(r[3] & 0x0f, RCODE_OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "second answer from cache");
        assert_eq!(ask("ghost.local", TYPE_A)[3] & 0x0f, RCODE_NXDOMAIN);
        assert_eq!(ask("broken.local", TYPE_A)[3] & 0x0f, RCODE_SERVFAIL);
        assert_eq!(ask("example.com", TYPE_A)[3] & 0x0f, RCODE_REFUSED, "never a general forwarder");
        let before = calls.load(Ordering::SeqCst);
        assert_eq!(ask("printer.local", 16)[3] & 0x0f, RCODE_OK, "TXT → NODATA without a lookup");
        assert_eq!(calls.load(Ordering::SeqCst), before);
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    }

    /// D50: interfaces are asked at once (a miss costs one timeout, a hit does not wait for a silent interface),
    /// and the own name is answered by the self lookup without asking Avahi.
    #[test]
    fn concurrent_interfaces_and_own_name() {
        let resolver: Arc<Resolver> = Arc::new(|i, name: &str, _v6, t| {
            std::thread::sleep(t.min(Duration::from_millis(if i == 2 && name == "printer.local" { 10 } else { 400 })));
            Ok((i == 2 && name == "printer.local").then(|| "10.62.0.50".parse().unwrap()))
        });
        let opts = Options {
            listen: "127.0.0.1:0".parse().unwrap(),
            max_inflight: 4,
            timeout: Duration::from_millis(400),
            cache_seconds: 30,
            interfaces: vec![1, 2, 3],
        };
        let t0 = Instant::now();
        assert_eq!(lookup(&resolver, &opts, "printer.local", false).0, RCODE_OK);
        assert!(t0.elapsed() < Duration::from_millis(300), "hit waits for the slow interfaces: {:?}", t0.elapsed());
        let t0 = Instant::now();
        assert_eq!(lookup(&resolver, &opts, "ghost.local", false).0, RCODE_NXDOMAIN);
        assert!(t0.elapsed() < Duration::from_millis(1000), "misses are asked one after the other: {:?}", t0.elapsed());

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let self_lookup: Arc<SelfLookup> =
            Arc::new(|name: &str, v6| (name == "lab-b.local").then(|| if v6 { Vec::new() } else { vec!["10.62.0.12".parse().unwrap()] }));
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || serve(sock, opts, resolver, Some(self_lookup), s2).unwrap());
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let ask = |name: &str, qtype: u16| {
            client.send_to(&q(name, qtype), addr).unwrap();
            let mut buf = [0u8; 512];
            let (n, _) = client.recv_from(&mut buf).unwrap();
            buf[..n].to_vec()
        };
        let t0 = Instant::now();
        let r = ask("lab-b.local", TYPE_A);
        assert_eq!((r[3] & 0x0f, &r[r.len() - 4..]), (RCODE_OK, &[10, 62, 0, 12][..]));
        assert!(t0.elapsed() < Duration::from_millis(300), "own name asked Avahi");
        let r = ask("lab-b.local", TYPE_AAAA);
        assert_eq!((r[3] & 0x0f, &r[6..8]), (RCODE_OK, &[0, 0][..]), "own name without that family: NODATA");
        assert_eq!(ask("printer.local", TYPE_A)[3] & 0x0f, RCODE_OK);
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    }

    #[test]
    fn own_addresses_follow_interface_order_link_local_last() {
        let found: Vec<(String, IpAddr)> = vec![
            ("eth0".into(), "fe80::2".parse().unwrap()),
            ("eth0".into(), "2001:db8::2".parse().unwrap()),
            ("eth1".into(), "fe80::1".parse().unwrap()),
            ("eth1".into(), "2001:db8::1".parse().unwrap()),
            ("tnc-mesh".into(), "2001:db8::9".parse().unwrap()),
        ];
        let names = vec!["eth1".to_string(), "eth0".to_string()];
        let want: Vec<IpAddr> = ["2001:db8::1", "2001:db8::2", "fe80::1", "fe80::2"].iter().map(|a| a.parse().unwrap()).collect();
        assert_eq!(order_addresses(&names, &found), want);
        assert_eq!(order_addresses(&[], &found).len(), 5);
    }
}
