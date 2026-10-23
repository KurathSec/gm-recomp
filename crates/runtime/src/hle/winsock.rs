//! ws2_32/wsock32 sockets over connections the host makes (the page relays each one
//! through a WebSocket to a relay the player chose; the harness opens real sockets).
//!
//! A TCP socket is one host connection; a UDP socket one per destination it sends to.
//! Calls do not block unless the guest made the socket blocking, and then they wait on
//! the virtual clock (the host's events come in while it waits). Host names resolve to
//! stand-in addresses (10.77.x.y) the host connects to by name. Without a network on the
//! host every call returns what the stubs before it did (WSASYSNOTREADY and failures), so
//! games that look at the network see no change.

use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use std::collections::HashMap;

fn ok(nargs: u32, v: u32) -> ApiResult {
    Ok(Ret::stdcall(nargs, v))
}

const SOCKET_ERROR: u32 = 0xFFFF_FFFF;
const WSAEWOULDBLOCK: u32 = 10035;
const WSAEINPROGRESS: u32 = 10036;
const WSAENOTSOCK: u32 = 10038;
const WSAEOPNOTSUPP: u32 = 10045;
const WSAEISCONN: u32 = 10056;
const WSAENOTCONN: u32 = 10057;
const WSAECONNREFUSED: u32 = 10061;
const WSAECONNRESET: u32 = 10054;
const WSAETIMEDOUT: u32 = 10060;
const WSAHOST_NOT_FOUND: u32 = 11001;
const WSAEAFNOSUPPORT: u32 = 10047;

/// A host connection's state (`Host::net_state`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetState {
    Connecting,
    Open,
    /// closed by the other end (what arrived before can still be read)
    Closed,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Conn {
    /// not connected (UDP: unbound or bound)
    Idle,
    Connecting,
    Connected,
    Closed,
}

pub struct Socket {
    udp: bool,
    nonblocking: bool,
    conn: Conn,
    /// TCP: the host connection; UDP: one per destination (address, port, connection)
    net: Option<u32>,
    dests: Vec<(u32, u16, u32)>,
    peer: (u32, u16),
    error: u32,
}

#[derive(Default)]
pub struct Sockets {
    pub list: HashMap<u32, Socket>,
    next: u32,
    next_net: u32,
    /// host names behind the stand-in addresses 10.77.x.y
    pub names: Vec<String>,
    started: bool,
    /// gethostbyname's result block, per name
    hostent: HashMap<String, u32>,
    ntoa: u32,
}

impl Sockets {
    pub(crate) fn net_id(&mut self) -> u32 {
        self.next_net += 1;
        self.next_net
    }
    /// The name to connect to for an IPv4 address (in network byte order as stored).
    pub fn name_of(&self, addr: u32) -> String {
        let b = addr.to_le_bytes();
        if b[0] == 10 && b[1] == 77 {
            let i = (b[2] as usize) << 8 | b[3] as usize;
            if let Some(n) = self.names.get(i) {
                return n.clone();
            }
        }
        format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
    /// The address (network byte order) a name resolves to: itself if dotted, else a stand-in.
    pub fn address_of(&mut self, name: &str) -> Option<u32> {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
            let b: Vec<u8> = parts.iter().map(|p| p.parse().unwrap()).collect();
            return Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
        if name.is_empty() {
            return None;
        }
        let i = match self.names.iter().position(|n| n.eq_ignore_ascii_case(name)) {
            Some(i) => i,
            None => {
                self.names.push(name.to_owned());
                self.names.len() - 1
            }
        };
        Some(u32::from_le_bytes([10, 77, (i >> 8) as u8, i as u8]))
    }
}

fn set_error(c: &mut Ctx, e: u32) {
    c.set_last_error(e);
}

fn fail(c: &mut Ctx, nargs: u32, e: u32) -> ApiResult {
    set_error(c, e);
    ok(nargs, SOCKET_ERROR)
}

fn galloc(c: &mut Ctx, size: u32) -> u32 {
    let heap = c.p.process_heap;
    let g: &mut dyn Guest = &mut *c.host;
    c.p.heap_alloc(g, heap, size, true)
}

/// sockaddr_in at `p`: (address as stored, port in host order).
fn read_sockaddr(c: &Ctx, p: u32) -> Option<(u32, u16)> {
    let family = c.host.read_u16(p).ok()?;
    if family != 2 {
        return None;
    }
    let port = c.host.read_u16(p + 2).ok()?.swap_bytes();
    Some((c.host.read_u32(p + 4).ok()?, port))
}

fn write_sockaddr(c: &mut Ctx, p: u32, len_p: u32, (addr, port): (u32, u16)) {
    if p == 0 {
        return;
    }
    let mut b = [0u8; 16];
    b[0..2].copy_from_slice(&2u16.to_le_bytes());
    b[2..4].copy_from_slice(&port.swap_bytes().to_le_bytes());
    b[4..8].copy_from_slice(&addr.to_le_bytes());
    let _ = c.host.write(p, &b);
    if len_p != 0 {
        let _ = c.host.write_u32(len_p, 16);
    }
}

/// The socket's connection state, following the host's.
fn refresh(c: &mut Ctx, s: u32) {
    let Some(sock) = c.p.sockets.list.get(&s) else { return };
    let (Some(net), Conn::Connecting | Conn::Connected) = (sock.net, sock.conn) else { return };
    let st = c.host.net_state(net);
    let sock = c.p.sockets.list.get_mut(&s).unwrap();
    match st {
        NetState::Connecting => {}
        NetState::Open => sock.conn = Conn::Connected,
        NetState::Closed => sock.conn = if sock.conn == Conn::Connecting { Conn::Closed } else { sock.conn },
        NetState::Failed => {
            if sock.conn == Conn::Connecting {
                sock.error = WSAECONNREFUSED;
            }
            sock.conn = Conn::Closed;
        }
    }
}

/// Readiness for select: (readable, writable, failed).
fn ready(c: &mut Ctx, s: u32) -> (bool, bool, bool) {
    refresh(c, s);
    let Some(sock) = c.p.sockets.list.get(&s) else { return (false, false, true) };
    if sock.udp {
        let nets: Vec<u32> = sock.dests.iter().map(|d| d.2).collect();
        let readable = nets.iter().any(|&n| c.host.net_available(n) > 0);
        return (readable, true, false);
    }
    let (conn, net, error) = (sock.conn, sock.net, sock.error);
    match conn {
        Conn::Idle => (false, false, false),
        Conn::Connecting => (false, false, false),
        Conn::Connected => {
            let st = net.map_or(NetState::Failed, |n| c.host.net_state(n));
            let avail = net.map_or(0, |n| c.host.net_available(n));
            (avail > 0 || matches!(st, NetState::Closed | NetState::Failed), true, false)
        }
        // a failed connect shows in the except set; a closed one reads its end
        Conn::Closed => (error == 0, false, error != 0),
    }
}

/// Waits on the virtual clock (1 ms steps) until `done` or `ms` pass; whether it was done.
fn wait_until(c: &mut Ctx, ms: u32, mut done: impl FnMut(&mut Ctx) -> bool) -> bool {
    for _ in 0..ms {
        if done(c) {
            return true;
        }
        c.advance_clock(1000);
    }
    done(c)
}

pub fn register(r: &mut Registry) {
    let w = "ws2_32.dll";
    r.add(w, "WSAStartup", |c| {
        let (version, data) = (c.arg(0), c.arg(1));
        if !c.host.net_enabled() {
            // WSASYSNOTREADY: no network here (as before sockets existed)
            return ok(2, 10091);
        }
        if data != 0 {
            // WSADATA: version, high version, description, system status
            let _ = c.host.fill(data, 400, 0);
            let _ = c.host.write_u16(data, (version as u16).min(0x0202));
            let _ = c.host.write_u16(data + 2, 0x0202);
            let _ = c.host.write(data + 4, b"WinSock 2.0\0");
            let _ = c.host.write(data + 4 + 257, b"Running\0");
            let _ = c.host.write_u16(data + 4 + 257 + 129, 0x7FFF);
        }
        c.p.sockets.started = true;
        ok(2, 0)
    });
    r.add(w, "WSACleanup", |c| {
        if !c.host.net_enabled() {
            return ok(0, 0);
        }
        let _ = &c;
        ok(0, 0)
    });
    r.add(w, "WSAGetLastError", |c| {
        if !c.host.net_enabled() {
            return ok(0, 10093);
        }
        let e = c.p.current_thread_mut().last_error;
        ok(0, e)
    });
    r.add(w, "WSASetLastError", |c| {
        if !c.host.net_enabled() {
            return ok(1, 0);
        }
        let e = c.arg(0);
        set_error(c, e);
        ok(1, 0)
    });
    r.add(w, "socket", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        let (af, kind) = (c.arg(0), c.arg(1));
        if af != 2 {
            return fail(c, 3, WSAEAFNOSUPPORT);
        }
        let udp = kind == 2;
        let ss = &mut c.p.sockets;
        ss.next += 4;
        let h = 0x1000 + ss.next;
        ss.list.insert(h, Socket { udp, nonblocking: false, conn: Conn::Idle, net: None, dests: Vec::new(), peer: (0, 0), error: 0 });
        ok(3, h)
    });
    r.add(w, "closesocket", |c| {
        if !c.host.net_enabled() {
            return ok(1, SOCKET_ERROR);
        }
        let s = c.arg(0);
        let Some(sock) = c.p.sockets.list.remove(&s) else { return fail(c, 1, WSAENOTSOCK) };
        for n in sock.net.into_iter().chain(sock.dests.iter().map(|d| d.2)) {
            c.host.net_close(n);
        }
        ok(1, 0)
    });
    r.add(w, "shutdown", |c| {
        if !c.host.net_enabled() {
            return ok(2, SOCKET_ERROR);
        }
        // a WebSocket cannot half-close: the connection ends when the socket is closed
        if !c.p.sockets.list.contains_key(&c.arg(0)) {
            return fail(c, 2, WSAENOTSOCK);
        }
        ok(2, 0)
    });
    r.add(w, "connect", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        let (s, sa) = (c.arg(0), c.arg(1));
        let Some((addr, port)) = read_sockaddr(c, sa) else { return fail(c, 3, WSAEAFNOSUPPORT) };
        let Some(sock) = c.p.sockets.list.get(&s) else { return fail(c, 3, WSAENOTSOCK) };
        if sock.udp {
            // a default destination
            c.p.sockets.list.get_mut(&s).unwrap().peer = (addr, port);
            return ok(3, 0);
        }
        match sock.conn {
            Conn::Connected => return fail(c, 3, WSAEISCONN),
            Conn::Connecting => return fail(c, 3, WSAEINPROGRESS),
            _ => {}
        }
        let name = c.p.sockets.name_of(addr);
        let net = c.p.sockets.net_id();
        if !c.host.net_open(net, false, &name, port) {
            return fail(c, 3, WSAECONNREFUSED);
        }
        let sock = c.p.sockets.list.get_mut(&s).unwrap();
        sock.net = Some(net);
        sock.conn = Conn::Connecting;
        sock.peer = (addr, port);
        sock.error = 0;
        if sock.nonblocking {
            return fail(c, 3, WSAEWOULDBLOCK);
        }
        let connected = wait_until(c, 15_000, |c| {
            refresh(c, s);
            c.p.sockets.list.get(&s).is_none_or(|k| k.conn != Conn::Connecting)
        });
        let sock = c.p.sockets.list.get(&s).unwrap();
        if !connected {
            return fail(c, 3, WSAETIMEDOUT);
        }
        if sock.conn != Conn::Connected {
            return fail(c, 3, WSAECONNREFUSED);
        }
        ok(3, 0)
    });
    r.add(w, "bind", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        if !c.p.sockets.list.contains_key(&c.arg(0)) {
            return fail(c, 3, WSAENOTSOCK);
        }
        ok(3, 0)
    });
    r.add(w, "listen", |c| {
        if !c.host.net_enabled() {
            return ok(2, SOCKET_ERROR);
        }
        fail(c, 2, WSAEOPNOTSUPP)
    });
    r.add(w, "accept", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        fail(c, 3, WSAEOPNOTSUPP)
    });
    r.add(w, "send", |c| {
        if !c.host.net_enabled() {
            return ok(4, SOCKET_ERROR);
        }
        let (s, buf, len) = (c.arg(0), c.arg(1), c.arg(2));
        refresh(c, s);
        let Some(sock) = c.p.sockets.list.get(&s) else { return fail(c, 4, WSAENOTSOCK) };
        if sock.udp {
            let to = sock.peer;
            return send_to(c, s, buf, len, to, 4);
        }
        match (sock.conn, sock.net) {
            (Conn::Connected, Some(net)) => {
                let mut data = vec![0u8; len as usize];
                let _ = c.host.read(buf, &mut data);
                if c.host.net_send(net, &data) {
                    ok(4, len)
                } else {
                    fail(c, 4, WSAECONNRESET)
                }
            }
            (Conn::Connecting, _) => fail(c, 4, WSAEWOULDBLOCK),
            _ => fail(c, 4, WSAENOTCONN),
        }
    });
    r.add(w, "recv", |c| {
        if !c.host.net_enabled() {
            return ok(4, SOCKET_ERROR);
        }
        let (s, buf, len) = (c.arg(0), c.arg(1), c.arg(2));
        refresh(c, s);
        let Some(sock) = c.p.sockets.list.get(&s) else { return fail(c, 4, WSAENOTSOCK) };
        if sock.udp {
            return recv_from(c, s, buf, len, 0, 0, 4);
        }
        let (Some(net), nonblocking, conn) = (sock.net, sock.nonblocking, sock.conn) else { return fail(c, 4, WSAENOTCONN) };
        if conn == Conn::Connecting {
            return fail(c, 4, WSAEWOULDBLOCK);
        }
        if !nonblocking {
            wait_until(c, 60_000, |c| c.host.net_available(net) > 0 || matches!(c.host.net_state(net), NetState::Closed | NetState::Failed));
        }
        let mut data = vec![0u8; len as usize];
        match c.host.net_recv(net, &mut data) {
            Some(n) => {
                let _ = c.host.write(buf, &data[..n]);
                ok(4, n as u32)
            }
            None => fail(c, 4, WSAEWOULDBLOCK),
        }
    });
    r.add(w, "sendto", |c| {
        if !c.host.net_enabled() {
            return ok(6, SOCKET_ERROR);
        }
        let (s, buf, len, to, tolen) = (c.arg(0), c.arg(1), c.arg(2), c.arg(4), c.arg(5));
        let dest = if to != 0 && tolen >= 8 { read_sockaddr(c, to) } else { c.p.sockets.list.get(&s).map(|k| k.peer) };
        let Some(dest) = dest else { return fail(c, 6, WSAEAFNOSUPPORT) };
        send_to(c, s, buf, len, dest, 6)
    });
    r.add(w, "recvfrom", |c| {
        if !c.host.net_enabled() {
            return ok(6, SOCKET_ERROR);
        }
        let (s, buf, len, from, fromlen) = (c.arg(0), c.arg(1), c.arg(2), c.arg(4), c.arg(5));
        recv_from(c, s, buf, len, from, fromlen, 6)
    });
    r.add(w, "select", |c| {
        if !c.host.net_enabled() {
            return ok(5, SOCKET_ERROR);
        }
        let (rd, wr, ex, timeout) = (c.arg(1), c.arg(2), c.arg(3), c.arg(4));
        let read_set = |c: &Ctx, p: u32| -> Vec<u32> {
            if p == 0 {
                return Vec::new();
            }
            let n = c.host.read_u32(p).unwrap_or(0).min(64);
            (0..n).map(|i| c.host.read_u32(p + 4 + 4 * i).unwrap_or(0)).collect()
        };
        let sets = [read_set(c, rd), read_set(c, wr), read_set(c, ex)];
        let wait_ms = if timeout == 0 { 60_000 } else { (c.host.read_u32(timeout).unwrap_or(0) * 1000 + c.host.read_u32(timeout + 4).unwrap_or(0) / 1000).min(60_000) };
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        wait_until(c, wait_ms, |c| {
            for (k, set) in sets.iter().enumerate() {
                out[k] = set.iter().copied().filter(|&s| {
                    let r = ready(c, s);
                    [r.0, r.1, r.2][k]
                }).collect();
            }
            out.iter().any(|o| !o.is_empty())
        });
        for (k, p) in [rd, wr, ex].into_iter().enumerate() {
            if p != 0 {
                let _ = c.host.write_u32(p, out[k].len() as u32);
                for (i, &s) in out[k].iter().enumerate() {
                    let _ = c.host.write_u32(p + 4 + 4 * i as u32, s);
                }
            }
        }
        ok(5, out.iter().map(|o| o.len() as u32).sum())
    });
    r.add(w, "__WSAFDIsSet", |c| {
        if !c.host.net_enabled() {
            return ok(2, 0);
        }
        let (s, set) = (c.arg(0), c.arg(1));
        let n = c.host.read_u32(set).unwrap_or(0).min(64);
        let found = (0..n).any(|i| c.host.read_u32(set + 4 + 4 * i).ok() == Some(s));
        ok(2, found as u32)
    });
    r.add(w, "ioctlsocket", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        let (s, cmd, argp) = (c.arg(0), c.arg(1), c.arg(2));
        if !c.p.sockets.list.contains_key(&s) {
            return fail(c, 3, WSAENOTSOCK);
        }
        match cmd {
            // FIONBIO
            0x8004_667E => {
                let on = c.host.read_u32(argp).unwrap_or(0) != 0;
                c.p.sockets.list.get_mut(&s).unwrap().nonblocking = on;
            }
            // FIONREAD
            0x4004_667F => {
                let sock = c.p.sockets.list.get(&s).unwrap();
                let nets: Vec<u32> = sock.net.into_iter().chain(sock.dests.iter().map(|d| d.2)).collect();
                let n: u32 = nets.iter().map(|&n| c.host.net_available(n)).sum();
                let _ = c.host.write_u32(argp, n);
            }
            _ => {}
        }
        ok(3, 0)
    });
    r.add(w, "setsockopt", |c| {
        if !c.host.net_enabled() {
            return ok(5, SOCKET_ERROR);
        }
        if !c.p.sockets.list.contains_key(&c.arg(0)) {
            return fail(c, 5, WSAENOTSOCK);
        }
        ok(5, 0)
    });
    r.add(w, "getsockopt", |c| {
        if !c.host.net_enabled() {
            return ok(5, SOCKET_ERROR);
        }
        let (s, level, name, val, len) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3), c.arg(4));
        refresh(c, s);
        let Some(sock) = c.p.sockets.list.get_mut(&s) else { return fail(c, 5, WSAENOTSOCK) };
        let v = if level == 0xFFFF && name == 0x1007 {
            // SO_ERROR, cleared by reading it
            std::mem::take(&mut sock.error)
        } else {
            0
        };
        let _ = c.host.write_u32(val, v);
        if len != 0 {
            let _ = c.host.write_u32(len, 4);
        }
        ok(5, 0)
    });
    r.add(w, "getpeername", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        let (s, sa, len) = (c.arg(0), c.arg(1), c.arg(2));
        let Some(peer) = c.p.sockets.list.get(&s).map(|k| k.peer) else { return fail(c, 3, WSAENOTSOCK) };
        write_sockaddr(c, sa, len, peer);
        ok(3, 0)
    });
    r.add(w, "getsockname", |c| {
        if !c.host.net_enabled() {
            return ok(3, SOCKET_ERROR);
        }
        let (s, sa, len) = (c.arg(0), c.arg(1), c.arg(2));
        if !c.p.sockets.list.contains_key(&s) {
            return fail(c, 3, WSAENOTSOCK);
        }
        write_sockaddr(c, sa, len, (u32::from_le_bytes([127, 0, 0, 1]), 50000 + (s as u16 & 0xFFF)));
        ok(3, 0)
    });
    r.add(w, "gethostname", |c| {
        if !c.host.net_enabled() {
            return ok(2, SOCKET_ERROR);
        }
        let (buf, len) = (c.arg(0), c.arg(1));
        if len < 10 {
            return fail(c, 2, 10014);
        }
        let _ = c.host.write(buf, b"localhost\0");
        ok(2, 0)
    });
    r.add(w, "inet_addr", |c| {
        if !c.host.net_enabled() {
            return ok(1, SOCKET_ERROR);
        }
        let s = c.host.read_cstr_lossy(c.arg(0));
        let parts: Vec<&str> = s.trim().split('.').collect();
        if parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
            let b: Vec<u8> = parts.iter().map(|p| p.parse().unwrap()).collect();
            return ok(1, u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        }
        ok(1, 0xFFFF_FFFF)
    });
    r.add(w, "inet_ntoa", |c| {
        if !c.host.net_enabled() {
            return ok(1, 0);
        }
        let addr = c.arg(0);
        let b = addr.to_le_bytes();
        if c.p.sockets.ntoa == 0 {
            c.p.sockets.ntoa = galloc(c, 16);
        }
        let p = c.p.sockets.ntoa;
        let _ = c.host.write(p, format!("{}.{}.{}.{}\0", b[0], b[1], b[2], b[3]).as_bytes());
        ok(1, p)
    });
    r.add(w, "gethostbyname", |c| {
        if !c.host.net_enabled() {
            return ok(1, 0);
        }
        let name = c.host.read_cstr_lossy(c.arg(0));
        let Some(addr) = c.p.sockets.address_of(&name) else {
            set_error(c, WSAHOST_NOT_FOUND);
            return ok(1, 0);
        };
        if let Some(&p) = c.p.sockets.hostent.get(&name) {
            return ok(1, p);
        }
        // hostent {name, aliases, type, length, addresses} + the lists + the address + the name
        let p = galloc(c, 64 + name.len() as u32 + 1);
        let (aliases, list, a, n) = (p + 16, p + 20, p + 28, p + 32);
        let _ = c.host.write_u32(p, n);
        let _ = c.host.write_u32(p + 4, aliases);
        let _ = c.host.write_u16(p + 8, 2);
        let _ = c.host.write_u16(p + 10, 4);
        let _ = c.host.write_u32(p + 12, list);
        let _ = c.host.write_u32(aliases, 0);
        let _ = c.host.write_u32(list, a);
        let _ = c.host.write_u32(list + 4, 0);
        let _ = c.host.write_u32(a, addr);
        let mut nb = name.clone().into_bytes();
        nb.push(0);
        let _ = c.host.write(n, &nb);
        c.p.sockets.hostent.insert(name, p);
        ok(1, p)
    });
    r.add(w, "getaddrinfo", |c| {
        if !c.host.net_enabled() {
            return ok(4, WSAHOST_NOT_FOUND);
        }
        let (node, service, hints, res) = (c.arg(0), c.arg(1), c.arg(2), c.arg(3));
        let name = if node == 0 { "127.0.0.1".to_owned() } else { c.host.read_cstr_lossy(node) };
        let port: u16 = if service == 0 { 0 } else { c.host.read_cstr_lossy(service).trim().parse().unwrap_or(0) };
        let Some(addr) = c.p.sockets.address_of(&name) else { return ok(4, WSAHOST_NOT_FOUND) };
        let (socktype, protocol) = if hints != 0 { (c.host.read_u32(hints + 8).unwrap_or(1), c.host.read_u32(hints + 12).unwrap_or(0)) } else { (1, 0) };
        // addrinfo {flags, family, socktype, protocol, addrlen, canonname, addr, next} + sockaddr_in
        let p = galloc(c, 32 + 16);
        for (i, v) in [0, 2, socktype, protocol, 16, 0, p + 32, 0].into_iter().enumerate() {
            let _ = c.host.write_u32(p + 4 * i as u32, v);
        }
        write_sockaddr(c, p + 32, 0, (addr, port));
        let _ = c.host.write_u32(res, p);
        ok(4, 0)
    });
    r.add(w, "freeaddrinfo", |c| {
        if !c.host.net_enabled() {
            return ok(1, 0);
        }
        let p = c.arg(0);
        c.p.heap_free(p);
        ok(1, 0)
    });
}

/// Sends a datagram from UDP socket `s` to `to` (one host connection per destination).
fn send_to(c: &mut Ctx, s: u32, buf: u32, len: u32, to: (u32, u16), nargs: u32) -> ApiResult {
    let Some(sock) = c.p.sockets.list.get(&s) else { return fail(c, nargs, WSAENOTSOCK) };
    if !sock.udp {
        return fail(c, nargs, WSAEOPNOTSUPP);
    }
    let net = match sock.dests.iter().find(|d| d.0 == to.0 && d.1 == to.1) {
        Some(d) => d.2,
        None => {
            let name = c.p.sockets.name_of(to.0);
            let net = c.p.sockets.net_id();
            if !c.host.net_open(net, true, &name, to.1) {
                return fail(c, nargs, WSAECONNREFUSED);
            }
            c.p.sockets.list.get_mut(&s).unwrap().dests.push((to.0, to.1, net));
            net
        }
    };
    let mut data = vec![0u8; len as usize];
    let _ = c.host.read(buf, &mut data);
    // (a datagram before the relay connection is open waits for it in the host)
    c.host.net_send(net, &data);
    ok(nargs, len)
}

/// Receives a datagram on UDP socket `s` from any destination it sent to.
fn recv_from(c: &mut Ctx, s: u32, buf: u32, len: u32, from: u32, fromlen: u32, nargs: u32) -> ApiResult {
    let Some(sock) = c.p.sockets.list.get(&s) else { return fail(c, nargs, WSAENOTSOCK) };
    if !sock.udp {
        // (recvfrom on a TCP socket reads its stream)
        let (Some(net), peer) = (sock.net, sock.peer) else { return fail(c, nargs, WSAENOTCONN) };
        let mut data = vec![0u8; len as usize];
        return match c.host.net_recv(net, &mut data) {
            Some(n) => {
                let _ = c.host.write(buf, &data[..n]);
                write_sockaddr(c, from, fromlen, peer);
                ok(nargs, n as u32)
            }
            None => fail(c, nargs, WSAEWOULDBLOCK),
        };
    }
    let (nonblocking, dests) = (sock.nonblocking, sock.dests.clone());
    if !nonblocking {
        wait_until(c, 60_000, |c| dests.iter().any(|d| c.host.net_available(d.2) > 0));
    }
    for (addr, port, net) in dests {
        if c.host.net_available(net) == 0 {
            continue;
        }
        let mut data = vec![0u8; len as usize];
        if let Some(n) = c.host.net_recv(net, &mut data) {
            let _ = c.host.write(buf, &data[..n]);
            write_sockaddr(c, from, fromlen, (addr, port));
            return ok(nargs, n as u32);
        }
    }
    fail(c, nargs, WSAEWOULDBLOCK)
}
