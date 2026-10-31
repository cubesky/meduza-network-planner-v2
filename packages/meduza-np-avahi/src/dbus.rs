//! Minimal D-Bus client for the few Avahi calls the bridge and probes need (AGENT.md §9.11): system-bus UDS,
//! SASL EXTERNAL, `Hello`, one method call at a time. Marshaling covers the basic types used by the
//! `org.freedesktop.Avahi.Server` methods (i, u, s, o, g); messages from big-endian peers are decoded too
//! (OpenWrt runs on big-endian MIPS). No libdbus, no async runtime: the bridge stays a few hundred KiB.

use std::net::IpAddr;
use std::time::Duration;

pub const AVAHI_DEST: &str = "org.freedesktop.Avahi";
pub const AVAHI_SERVER: &str = "org.freedesktop.Avahi.Server";
pub const PROTO_INET: i32 = 0;
pub const PROTO_INET6: i32 = 1;
pub const PROTO_UNSPEC: i32 = -1;
pub const IF_UNSPEC: i32 = -1;
/// `AVAHI_SERVER_RUNNING` from `GetState`.
pub const SERVER_RUNNING: i32 = 2;

const MAX_MESSAGE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg<'a> {
    I32(i32),
    U32(u32),
    Str(&'a str),
}

impl Arg<'_> {
    fn code(&self) -> char {
        match self {
            Arg::I32(_) => 'i',
            Arg::U32(_) => 'u',
            Arg::Str(_) => 's',
        }
    }
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn pad(&mut self, a: usize) {
        while !self.buf.len().is_multiple_of(a) {
            self.buf.push(0);
        }
    }
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn string(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    fn signature(&mut self, s: &str) {
        self.u8(s.len() as u8);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }
    fn arg(&mut self, a: &Arg<'_>) {
        match a {
            Arg::I32(v) => self.u32(*v as u32),
            Arg::U32(v) => self.u32(*v),
            Arg::Str(s) => self.string(s),
        }
    }
}

/// Header flag: the bus must not start the destination service when nobody owns its name (D51). Debian's
/// avahi-daemon is bus-activatable (`SystemdService=`), so without it every probe or bridge lookup restarted a
/// system daemon the operator had stopped, and in managed mode could start one next to planner's own.
pub const NO_AUTO_START: u8 = 0x2;

/// A METHOD_CALL message (little-endian, never auto-starts the destination).
pub fn method_call(serial: u32, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg<'_>]) -> Vec<u8> {
    let mut body = Writer::default();
    for a in args {
        body.arg(a);
    }
    let sig: String = args.iter().map(Arg::code).collect();
    let mut w = Writer::default();
    w.buf.extend_from_slice(&[b'l', 1, NO_AUTO_START, 1]);
    w.u32(body.buf.len() as u32);
    w.u32(serial);
    w.u32(0); // header field array length, patched below
    let start = w.buf.len();
    let field = |w: &mut Writer, code: u8, ty: &str, value: &str| {
        w.pad(8);
        w.u8(code);
        w.signature(ty);
        if ty == "g" {
            w.signature(value);
        } else {
            w.string(value);
        }
    };
    field(&mut w, 1, "o", path);
    field(&mut w, 6, "s", dest);
    field(&mut w, 2, "s", iface);
    field(&mut w, 3, "s", member);
    if !sig.is_empty() {
        field(&mut w, 8, "g", &sig);
    }
    let len = (w.buf.len() - start) as u32;
    w.buf[12..16].copy_from_slice(&len.to_le_bytes());
    w.pad(8);
    w.buf.extend_from_slice(&body.buf);
    w.buf
}

pub struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    big: bool,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8], big: bool) -> Self {
        Self { b, pos: 0, big }
    }
    fn align(&mut self, a: usize) {
        self.pos = self.pos.div_ceil(a) * a;
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.b.len()).ok_or("truncated D-Bus message")?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32, String> {
        self.align(4);
        let b: [u8; 4] = self.take(4)?.try_into().expect("4 bytes");
        Ok(if self.big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }
    pub fn i32(&mut self) -> Result<i32, String> {
        self.u32().map(|v| v as i32)
    }
    pub fn string(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        self.take(1)?;
        String::from_utf8(s.to_vec()).map_err(|_| "non-UTF-8 D-Bus string".to_string())
    }
    pub fn signature(&mut self) -> Result<String, String> {
        let n = self.u8()? as usize;
        let s = self.take(n)?;
        self.take(1)?;
        String::from_utf8(s.to_vec()).map_err(|_| "bad signature".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub msg_type: u8,
    pub serial: u32,
    pub reply_serial: Option<u32>,
    pub error_name: Option<String>,
    pub signature: String,
    pub body: Vec<u8>,
    pub big: bool,
}

impl Message {
    pub fn body(&self) -> Reader<'_> {
        Reader::new(&self.body, self.big)
    }
}

/// Total length of a message from its first 16 bytes.
pub fn message_len(head: &[u8; 16]) -> Result<usize, String> {
    let big = match head[0] {
        b'l' => false,
        b'B' => true,
        _ => return Err("bad D-Bus endianness byte".into()),
    };
    let rd = |o: usize| {
        let b: [u8; 4] = head[o..o + 4].try_into().expect("4");
        (if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) }) as usize
    };
    let total = (16 + rd(12)).div_ceil(8) * 8 + rd(4);
    if total > MAX_MESSAGE {
        return Err("D-Bus message too large".into());
    }
    Ok(total)
}

pub fn parse_message(b: &[u8]) -> Result<Message, String> {
    if b.len() < 16 {
        return Err("truncated D-Bus header".into());
    }
    let big = b[0] == b'B';
    let mut r = Reader::new(b, big);
    r.pos = 1;
    let msg_type = r.u8()?;
    r.pos = 4;
    let body_len = r.u32()? as usize;
    let serial = r.u32()?;
    let fields_len = r.u32()? as usize;
    let fields_end = 16 + fields_len;
    let (mut reply_serial, mut error_name, mut signature) = (None, None, String::new());
    while r.pos < fields_end {
        r.align(8);
        if r.pos >= fields_end {
            break;
        }
        let code = r.u8()?;
        let ty = r.signature()?;
        match ty.as_str() {
            "s" | "o" => {
                let v = r.string()?;
                if code == 4 {
                    error_name = Some(v);
                }
            }
            "g" => {
                let v = r.signature()?;
                if code == 8 {
                    signature = v;
                }
            }
            "u" => {
                let v = r.u32()?;
                if code == 5 {
                    reply_serial = Some(v);
                }
            }
            other => return Err(format!("unexpected header field type {other}")),
        }
    }
    let body_start = fields_end.div_ceil(8) * 8;
    let body = b.get(body_start..body_start + body_len).ok_or("truncated D-Bus body")?.to_vec();
    Ok(Message { msg_type, serial, reply_serial, error_name, signature, body, big })
}

/// System bus socket path (`DBUS_SYSTEM_BUS_ADDRESS=unix:path=...` or the usual locations).
pub fn system_bus_path() -> Option<String> {
    if let Ok(addr) = std::env::var("DBUS_SYSTEM_BUS_ADDRESS") {
        if let Some(p) = addr.split(';').find_map(|a| a.strip_prefix("unix:path=")) {
            return Some(p.split(',').next().unwrap_or(p).to_string());
        }
    }
    ["/run/dbus/system_bus_socket", "/var/run/dbus/system_bus_socket"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
}

/// Error prefix of a method call whose reply did not arrive within the connection's timeout.
pub const REPLY_TIMEOUT: &str = "D-Bus reply timeout";

/// Result of `ResolveHostName`: `Ok(None)` when Avahi found nobody answering for the name.
pub fn parse_resolve_reply(m: &Message) -> Result<Option<IpAddr>, String> {
    if m.msg_type == 3 {
        let name = m.error_name.clone().unwrap_or_default();
        if name.ends_with(".TimeoutError") || name.ends_with(".NotFoundError") {
            return Ok(None);
        }
        let detail = if m.signature.starts_with('s') { m.body().string().unwrap_or_default() } else { String::new() };
        return Err(format!("{name}: {detail}"));
    }
    if m.signature != "iisisu" {
        return Err(format!("unexpected ResolveHostName signature {}", m.signature));
    }
    let mut r = m.body();
    r.i32()?;
    r.i32()?;
    r.string()?;
    r.i32()?;
    let addr = r.string()?;
    // Link-local IPv6 answers carry a scope (`fe80::1%eth0`) that a DNS answer cannot express.
    addr.split('%').next().unwrap_or_default().parse().map(Some).map_err(|_| format!("bad address {addr:?} from Avahi"))
}

#[cfg(unix)]
pub struct Conn {
    stream: std::os::unix::net::UnixStream,
    serial: u32,
}

#[cfg(unix)]
impl Conn {
    pub fn system(timeout: Duration) -> Result<Conn, String> {
        use std::io::{Read, Write};
        let path = system_bus_path().ok_or("system D-Bus socket not found")?;
        let stream = std::os::unix::net::UnixStream::connect(&path).map_err(|e| format!("D-Bus {path}: {e}"))?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let mut conn = Conn { stream, serial: 0 };
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        conn.stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes()).map_err(|e| e.to_string())?;
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while !line.ends_with(b"\r\n") {
            if line.len() > 512 || conn.stream.read(&mut byte).map_err(|e| e.to_string())? == 0 {
                return Err("D-Bus authentication failed".into());
            }
            line.push(byte[0]);
        }
        if !line.starts_with(b"OK ") {
            return Err(format!("D-Bus authentication rejected: {}", String::from_utf8_lossy(&line).trim()));
        }
        conn.stream.write_all(b"BEGIN\r\n").map_err(|e| e.to_string())?;
        conn.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", &[])?;
        Ok(conn)
    }

    pub fn call(&mut self, dest: &str, path: &str, iface: &str, member: &str, args: &[Arg<'_>]) -> Result<Message, String> {
        use std::io::{Read, Write};
        self.serial += 1;
        let serial = self.serial;
        self.stream.write_all(&method_call(serial, dest, path, iface, member, args)).map_err(|e| e.to_string())?;
        loop {
            let mut head = [0u8; 16];
            self.stream.read_exact(&mut head).map_err(|e| match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => format!("{REPLY_TIMEOUT}: {e}"),
                _ => format!("D-Bus read: {e}"),
            })?;
            let total = message_len(&head)?;
            let mut buf = head.to_vec();
            buf.resize(total, 0);
            self.stream.read_exact(&mut buf[16..]).map_err(|e| format!("D-Bus read: {e}"))?;
            let m = parse_message(&buf)?;
            // Signals (NameAcquired, ...) and unrelated replies are skipped.
            if m.reply_serial == Some(serial) && (m.msg_type == 2 || m.msg_type == 3) {
                return Ok(m);
            }
        }
    }

    pub fn resolve_host(&mut self, iface: i32, name: &str, v6: bool) -> Result<Option<IpAddr>, String> {
        let aproto = if v6 { PROTO_INET6 } else { PROTO_INET };
        let m = self.call(
            AVAHI_DEST,
            "/",
            AVAHI_SERVER,
            "ResolveHostName",
            &[Arg::I32(iface), Arg::I32(PROTO_UNSPEC), Arg::Str(name), Arg::I32(aproto), Arg::U32(0)],
        );
        match m {
            Ok(m) => parse_resolve_reply(&m),
            // Avahi itself gives up on a silent name only after ~5 s, later than the bridge's budget: no answer
            // within the budget is "not found" like Avahi's TimeoutError (the connection itself already worked).
            Err(e) if e.starts_with(REPLY_TIMEOUT) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The daemon's current host name (`<host>.local`, or the renamed one after a conflict).
    pub fn host_name_fqdn(&mut self) -> Result<String, String> {
        let m = self.call(AVAHI_DEST, "/", AVAHI_SERVER, "GetHostNameFqdn", &[])?;
        if m.msg_type == 3 {
            return Err(m.error_name.unwrap_or_default());
        }
        m.body().string()
    }

    /// `(version string, server state, host name fqdn)` of the running Avahi daemon.
    pub fn avahi_info(&mut self) -> Result<(String, i32, String), String> {
        let ok = |m: Message| if m.msg_type == 3 { Err(m.error_name.unwrap_or_default()) } else { Ok(m) };
        let version = ok(self.call(AVAHI_DEST, "/", AVAHI_SERVER, "GetVersionString", &[])?)?.body().string()?;
        let state = ok(self.call(AVAHI_DEST, "/", AVAHI_SERVER, "GetState", &[])?)?.body().i32()?;
        let fqdn = ok(self.call(AVAHI_DEST, "/", AVAHI_SERVER, "GetHostNameFqdn", &[])?)?.body().string()?;
        Ok((version, state, fqdn))
    }
}

/// Resolves one name with a fresh connection (the bridge bounds concurrency, a stuck call cannot block others).
pub fn resolve_once(iface: i32, name: &str, v6: bool, timeout: Duration) -> Result<Option<IpAddr>, String> {
    #[cfg(unix)]
    {
        Conn::system(timeout)?.resolve_host(iface, name, v6)
    }
    #[cfg(not(unix))]
    {
        let _ = (iface, name, v6, timeout);
        Err("D-Bus is only available on unix".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(big: bool, reply_to: u32, msg_type: u8, error: Option<&str>, sig: &str, body: &[u8]) -> Vec<u8> {
        // Build a reply the way a peer of the given endianness would.
        let u = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut h: Vec<u8> = vec![if big { b'B' } else { b'l' }, msg_type, 1, 1];
        h.extend_from_slice(&u(body.len() as u32));
        h.extend_from_slice(&u(7));
        h.extend_from_slice(&[0; 4]);
        let start = h.len();
        let pad = |h: &mut Vec<u8>, a: usize| {
            while !h.len().is_multiple_of(a) {
                h.push(0)
            }
        };
        pad(&mut h, 8);
        h.extend_from_slice(&[5, 1, b'u', 0]);
        h.extend_from_slice(&u(reply_to));
        if let Some(e) = error {
            pad(&mut h, 8);
            h.extend_from_slice(&[4, 1, b's', 0]);
            h.extend_from_slice(&u(e.len() as u32));
            h.extend_from_slice(e.as_bytes());
            h.push(0);
        }
        pad(&mut h, 8);
        h.extend_from_slice(&[8, 1, b'g', 0, sig.len() as u8]);
        h.extend_from_slice(sig.as_bytes());
        h.push(0);
        let len = (h.len() - start) as u32;
        h[12..16].copy_from_slice(&u(len));
        pad(&mut h, 8);
        h.extend_from_slice(body);
        h
    }

    fn resolve_body(big: bool, addr: &str) -> Vec<u8> {
        let u = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut b = Vec::new();
        b.extend_from_slice(&u(2));
        b.extend_from_slice(&u(0));
        let name = "printer.local";
        b.extend_from_slice(&u(name.len() as u32));
        b.extend_from_slice(name.as_bytes());
        b.push(0);
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b.extend_from_slice(&u(0));
        b.extend_from_slice(&u(addr.len() as u32));
        b.extend_from_slice(addr.as_bytes());
        b.push(0);
        while !b.len().is_multiple_of(4) {
            b.push(0);
        }
        b.extend_from_slice(&u(0));
        b
    }

    #[test]
    fn method_call_layout() {
        let m = method_call(
            3,
            AVAHI_DEST,
            "/",
            AVAHI_SERVER,
            "ResolveHostName",
            &[Arg::I32(-1), Arg::I32(-1), Arg::Str("a.local"), Arg::I32(0), Arg::U32(0)],
        );
        assert_eq!(&m[..4], &[b'l', 1, NO_AUTO_START, 1]);
        let head: [u8; 16] = m[..16].try_into().unwrap();
        assert_eq!(message_len(&head).unwrap(), m.len());
        let parsed = parse_message(&m).unwrap();
        assert_eq!((parsed.msg_type, parsed.serial, parsed.signature.as_str()), (1, 3, "iisiu"));
        let mut r = parsed.body();
        assert_eq!(r.i32().unwrap(), -1);
        assert_eq!(r.i32().unwrap(), -1);
        assert_eq!(r.string().unwrap(), "a.local");
        assert_eq!(r.i32().unwrap(), 0);
        assert_eq!(r.u32().unwrap(), 0);
    }

    #[test]
    fn replies_in_both_byte_orders() {
        for big in [false, true] {
            let m = parse_message(&reply(big, 3, 2, None, "iisisu", &resolve_body(big, "192.168.1.20"))).unwrap();
            assert_eq!(m.reply_serial, Some(3));
            assert_eq!(parse_resolve_reply(&m).unwrap(), Some("192.168.1.20".parse().unwrap()), "big={big}");
            let m = parse_message(&reply(big, 3, 2, None, "iisisu", &resolve_body(big, "fe80::1%br-lan"))).unwrap();
            assert_eq!(parse_resolve_reply(&m).unwrap(), Some("fe80::1".parse().unwrap()));
        }
        let m = parse_message(&reply(
            false,
            3,
            3,
            Some("org.freedesktop.Avahi.TimeoutError"),
            "s",
            &[7, 0, 0, 0, b'T', b'i', b'm', b'e', b'o', b'u', b't', 0],
        ))
        .unwrap();
        assert_eq!(parse_resolve_reply(&m).unwrap(), None, "not found is not an error");
        let m = parse_message(&reply(false, 3, 3, Some("org.freedesktop.DBus.Error.ServiceUnknown"), "s", &[1, 0, 0, 0, b'x', 0])).unwrap();
        assert!(parse_resolve_reply(&m).unwrap_err().contains("ServiceUnknown"));
        assert!(parse_message(&[b'l', 2, 0, 1, 255, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]).is_err(), "truncated body");
    }

    /// A bus that accepts the call but stays silent past the budget means "not found" (Avahi answers TimeoutError
    /// only after ~5 s); a bus that went away is an error (SERVFAIL in the bridge).
    #[cfg(unix)]
    #[test]
    fn silent_resolve_is_not_found_but_a_closed_bus_is_an_error() {
        let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        client.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut conn = Conn { stream: client, serial: 1 };
        assert_eq!(conn.resolve_host(IF_UNSPEC, "ghost.local", false), Ok(None));
        drop(server);
        assert!(conn.resolve_host(IF_UNSPEC, "ghost.local", false).is_err());
    }
}
