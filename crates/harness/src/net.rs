//! The guest's network connections as real sockets (the page relays them over WebSockets).
//! Off unless GMR_NET is set; GMR_NET_HOST=<address> sends every connection there instead
//! of the host the game names (a local test server).

use runtime::hle::winsock::NetState;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::time::Duration;

enum Conn {
    Tcp { s: TcpStream, buf: Vec<u8>, closed: bool },
    Udp { s: UdpSocket, queue: std::collections::VecDeque<Vec<u8>> },
    Failed,
}

#[derive(Default)]
pub struct Net {
    conns: HashMap<u32, Conn>,
}

impl Net {
    pub fn open(&mut self, id: u32, udp: bool, host: &str, port: u16) -> bool {
        if std::env::var_os("GMR_NET").is_none() {
            return false;
        }
        let host = std::env::var("GMR_NET_HOST").unwrap_or_else(|_| host.to_owned());
        let addr = (host.as_str(), port).to_socket_addrs().ok().and_then(|mut a| a.next());
        let conn = match addr {
            None => Conn::Failed,
            Some(a) if udp => match UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect(a).map(|_| s)) {
                Ok(s) => {
                    let _ = s.set_nonblocking(true);
                    Conn::Udp { s, queue: Default::default() }
                }
                Err(_) => Conn::Failed,
            },
            Some(a) => match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
                Ok(s) => {
                    let _ = s.set_nonblocking(true);
                    let _ = s.set_nodelay(true);
                    Conn::Tcp { s, buf: Vec::new(), closed: false }
                }
                Err(_) => Conn::Failed,
            },
        };
        eprintln!("net: {} {host}:{port} -> {}", if udp { "udp" } else { "tcp" }, if matches!(conn, Conn::Failed) { "failed" } else { "open" });
        self.conns.insert(id, conn);
        true
    }

    /// What arrived since the last look, into the connection's buffer.
    fn pump(&mut self, id: u32) {
        match self.conns.get_mut(&id) {
            Some(Conn::Tcp { s, buf, closed }) => {
                let mut chunk = [0u8; 65536];
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) => {
                            *closed = true;
                            break;
                        }
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            *closed = true;
                            break;
                        }
                    }
                }
            }
            Some(Conn::Udp { s, queue }) => {
                let mut chunk = [0u8; 65536];
                while let Ok(n) = s.recv(&mut chunk) {
                    queue.push_back(chunk[..n].to_vec());
                }
            }
            _ => {}
        }
    }

    pub fn send(&mut self, id: u32, data: &[u8]) -> bool {
        match self.conns.get_mut(&id) {
            Some(Conn::Tcp { s, closed: false, .. }) => {
                let _ = s.set_nonblocking(false);
                let r = s.write_all(data).is_ok();
                let _ = s.set_nonblocking(true);
                r
            }
            Some(Conn::Udp { s, .. }) => s.send(data).is_ok(),
            _ => false,
        }
    }

    pub fn recv(&mut self, id: u32, out: &mut [u8]) -> Option<usize> {
        self.pump(id);
        match self.conns.get_mut(&id) {
            Some(Conn::Tcp { buf, closed, .. }) => {
                if buf.is_empty() {
                    return closed.then_some(0);
                }
                let n = buf.len().min(out.len());
                out[..n].copy_from_slice(&buf[..n]);
                buf.drain(..n);
                Some(n)
            }
            Some(Conn::Udp { queue, .. }) => {
                let d = queue.pop_front()?;
                let n = d.len().min(out.len());
                out[..n].copy_from_slice(&d[..n]);
                Some(n)
            }
            _ => Some(0),
        }
    }

    pub fn available(&mut self, id: u32) -> u32 {
        self.pump(id);
        match self.conns.get(&id) {
            Some(Conn::Tcp { buf, .. }) => buf.len() as u32,
            Some(Conn::Udp { queue, .. }) => queue.front().map_or(0, |d| d.len().max(1) as u32),
            _ => 0,
        }
    }

    pub fn state(&mut self, id: u32) -> NetState {
        self.pump(id);
        match self.conns.get(&id) {
            Some(Conn::Tcp { closed: false, .. }) | Some(Conn::Udp { .. }) => NetState::Open,
            Some(Conn::Tcp { closed: true, .. }) => NetState::Closed,
            _ => NetState::Failed,
        }
    }

    pub fn close(&mut self, id: u32) {
        self.conns.remove(&id);
    }
}
