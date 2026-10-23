//! http_dll_2_3.dll as I wanna play online 1.2.3 rebuilt it (its
//! modder/native/http_dll_2_3_x64/Exports.cs, MIT): buffers, a TCP socket that sends and
//! receives length-prefixed messages, UDP sockets, text conversions, a folder hash and
//! two dialogs, for GameMaker 8 games that converter made online. Ported rather than
//! recompiled (the original is a .NET NativeAOT DLL), with its sockets on the host's
//! connections (the page's WebSocket relay) and its files in the game's file system.
//!
//! A converted game carries a marker file of that name (`MARKER`), which loads this
//! module (kernel32 `load_library`); a game's own http_dll_2_3.dll (Maarten Baert's
//! original) is still loaded and recompiled. The functions are cdecl; numbers are
//! doubles and come back in ST(0), strings are char* in the system code page (or UTF-8
//! after `set_utf8_mode(1)`) and come back in a buffer the next string call reuses.

use super::kernel32::{decode_cp, encode_cp};
use super::winsock::NetState;
use super::{ApiResult, Ctx, Registry, Ret};
use crate::mem::Guest;
use std::collections::HashMap;

/// The contents of a converted game's http_dll_2_3.dll.
pub const MARKER: &[u8] = b"GMR-HLE http_dll_2_3\0";

/// The system code page (kernel32 GetACP).
const ACP: u32 = 1252;

#[derive(Default)]
pub struct State {
    next_buffer: i32,
    next_socket: i32,
    next_udp: i32,
    buffers: HashMap<i32, Buffer>,
    sockets: HashMap<i32, Tcp>,
    udp: HashMap<i32, Udp>,
    /// set_utf8_mode(1): strings at the boundary are UTF-8 (else the system code page)
    utf8: bool,
    /// the returned strings' buffer and its size
    ret: u32,
    ret_cap: u32,
}

// ---- NativeCast ---------------------------------------------------------------------------

fn to_i32(v: f64) -> i32 {
    if v <= i32::MIN as f64 { i32::MIN } else if v >= i32::MAX as f64 { i32::MAX } else { v as i32 }
}
fn to_u16(v: f64) -> u16 {
    if v <= 0.0 { 0 } else if v >= u16::MAX as f64 { u16::MAX } else { v as u16 }
}
fn to_float(v: f64) -> f32 {
    if v.is_nan() {
        0.0
    } else if v <= -(f32::MAX as f64) {
        -f32::MAX
    } else if v >= f32::MAX as f64 {
        f32::MAX
    } else {
        v as f32
    }
}

// ---- NativeBuffer -------------------------------------------------------------------------

#[derive(Default)]
struct Buffer {
    data: Vec<u8>,
    pos: usize,
    error: bool,
}

impl Buffer {
    fn clear(&mut self) {
        *self = Buffer::default();
    }
    fn set_pos(&mut self, p: usize) {
        self.pos = p.min(self.data.len());
    }
    fn load(&mut self, bytes: &[u8]) {
        self.clear();
        self.data.extend_from_slice(bytes);
    }
    fn compact(&mut self) {
        if self.pos == 0 {
            return;
        }
        if self.pos >= self.data.len() {
            self.clear();
            return;
        }
        if self.pos > self.data.len() / 4 {
            self.data.drain(..self.pos);
            self.pos = 0;
        }
    }
    fn readable(&mut self, n: usize) -> bool {
        if self.error {
            return false;
        }
        if self.pos + n > self.data.len() {
            self.error = true;
            return false;
        }
        true
    }
    fn read<const N: usize>(&mut self) -> Option<[u8; N]> {
        if !self.readable(N) {
            return None;
        }
        let b: [u8; N] = self.data[self.pos..self.pos + N].try_into().unwrap();
        self.pos += N;
        Some(b)
    }
    fn read_uintv(&mut self) -> u32 {
        if !self.readable(1) {
            return 0;
        }
        let v = self.data[self.pos] as u32;
        if v & 1 != 0 {
            self.pos += 1;
            return v >> 1;
        }
        if v & 2 != 0 {
            return self.read::<2>().map_or(0, |b| (u16::from_le_bytes(b) as u32 >> 2) + 0x80);
        }
        if v & 4 != 0 {
            return self.read::<3>().map_or(0, |b| ((b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16) >> 3) + 0x4080);
        }
        self.read::<4>().map_or(0, |b| (u32::from_le_bytes(b) >> 3) + 0x20_4080)
    }
    fn read_string(&mut self) -> String {
        if self.error {
            return String::new();
        }
        match self.data[self.pos..].iter().position(|&b| b == 0) {
            Some(n) => {
                let s = String::from_utf8_lossy(&self.data[self.pos..self.pos + n]).into_owned();
                self.pos += n + 1;
                s
            }
            None => {
                self.error = true;
                String::new()
            }
        }
    }
    fn write(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }
    fn write_uintv(&mut self, v: u32) {
        if v < 0x80 {
            self.write(&[(v << 1 | 1) as u8]);
        } else if v < 0x4080 {
            self.write(&(((v - 0x80) << 2 | 2) as u16).to_le_bytes());
        } else if v < 0x20_4080 {
            let e = (v - 0x4080) << 3 | 4;
            self.write(&e.to_le_bytes()[..3]);
        } else {
            self.write(&((v - 0x20_4080) << 3).to_le_bytes());
        }
    }
}

// ---- sockets ------------------------------------------------------------------------------

const TCP_NOT_CONNECTED: i32 = 0;
const TCP_CONNECTING: i32 = 1;
const TCP_CONNECTED: i32 = 2;
const TCP_CLOSED: i32 = 4;
const TCP_ERROR: i32 = 5;

#[derive(Default)]
struct Tcp {
    state: i32,
    /// the host connection
    net: Option<u32>,
    read: Buffer,
    write: Buffer,
    shutdown: bool,
}

const UDP_NOT_STARTED: i32 = 0;
const UDP_STARTED: i32 = 1;
const UDP_ERROR: i32 = 2;

#[derive(Default)]
struct Udp {
    state: i32,
    /// the host connection to the destination
    net: Option<u32>,
}

fn close_net(c: &mut Ctx, net: Option<u32>) {
    if let Some(n) = net {
        c.host.net_close(n);
    }
}

fn tcp_reset(c: &mut Ctx, s: &mut Tcp) {
    close_net(c, s.net.take());
    *s = Tcp::default();
}

fn tcp_connect(c: &mut Ctx, s: &mut Tcp, host: &str, port: u16) {
    if s.state != TCP_NOT_CONNECTED {
        tcp_reset(c, s);
    }
    let id = c.p.sockets.net_id();
    if host.is_empty() || !c.host.net_open(id, false, host, port) {
        s.state = TCP_ERROR;
        return;
    }
    s.net = Some(id);
    s.state = TCP_CONNECTING;
}

fn tcp_error(c: &mut Ctx, s: &mut Tcp) {
    close_net(c, s.net.take());
    s.state = TCP_ERROR;
}

fn tcp_update_read(c: &mut Ctx, s: &mut Tcp) {
    let Some(net) = s.net else {
        if s.state == TCP_CONNECTING {
            s.state = TCP_ERROR;
        }
        return;
    };
    if s.state == TCP_CONNECTING {
        match c.host.net_state(net) {
            NetState::Connecting => return,
            NetState::Open => s.state = TCP_CONNECTED,
            NetState::Closed | NetState::Failed => return tcp_error(c, s),
        }
    }
    if s.state != TCP_CONNECTED {
        return;
    }
    let mut buf = vec![0u8; 10240];
    loop {
        match c.host.net_recv(net, &mut buf) {
            Some(0) => {
                if c.host.net_state(net) == NetState::Failed {
                    return tcp_error(c, s);
                }
                s.state = TCP_CLOSED;
                return;
            }
            Some(n) => {
                s.read.write(&buf[..n]);
                if n < buf.len() {
                    return;
                }
            }
            None => return,
        }
    }
}

fn tcp_update_write(c: &mut Ctx, s: &mut Tcp) {
    let Some(net) = s.net else { return };
    if (s.state != TCP_CONNECTED && s.state != TCP_CLOSED) || s.write.pos >= s.write.data.len() {
        return;
    }
    if !c.host.net_send(net, &s.write.data[s.write.pos..]) {
        return tcp_error(c, s);
    }
    s.write.set_pos(s.write.data.len());
    s.write.compact();
    // (a WebSocket cannot close one direction: socket_shut_down only stops the writes)
}

fn tcp_read_message(s: &mut Tcp, dest: &mut Buffer) -> bool {
    s.read.error = false;
    let start = s.read.pos;
    let len = s.read.read_uintv() as usize;
    let content = s.read.pos;
    s.read.set_pos(start);
    if s.read.error || len > s.read.data.len() - content {
        return false;
    }
    dest.load(&s.read.data[content..content + len]);
    s.read.set_pos(content + len);
    s.read.compact();
    true
}

fn tcp_write_message(s: &mut Tcp, src: &Buffer) {
    if s.shutdown || s.state == TCP_ERROR {
        return;
    }
    s.write.write_uintv(src.data.len() as u32);
    s.write.write(&src.data);
}

// ---- strings at the boundary --------------------------------------------------------------

fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => decode_cp(ACP, bytes),
    }
}

/// A string argument (NativeStrings.Read).
fn str_in(c: &Ctx, slot: u32) -> String {
    let a = c.arg(slot);
    let bytes = if a == 0 { Vec::new() } else { c.host.read_cstr(a).unwrap_or_default() };
    if !c.p.httpdll.utf8 { decode_cp(ACP, &bytes) } else { decode(&bytes) }
}

/// The raw bytes of a string argument.
fn bytes_in(c: &Ctx, slot: u32) -> Vec<u8> {
    let a = c.arg(slot);
    if a == 0 { Vec::new() } else { c.host.read_cstr(a).unwrap_or_default() }
}

/// Returns `bytes` as a C string in the shared return buffer (NativeStrings.WriteRaw).
fn ret_raw(c: &mut Ctx, bytes: &[u8]) -> ApiResult {
    let need = bytes.len() as u32 + 1;
    if need > c.p.httpdll.ret_cap {
        if c.p.httpdll.ret != 0 {
            c.p.heap_free(c.p.httpdll.ret);
        }
        let cap = need.max(256);
        let heap = c.p.process_heap;
        let g: &mut dyn Guest = &mut *c.host;
        c.p.httpdll.ret = c.p.heap_alloc(g, heap, cap, false);
        c.p.httpdll.ret_cap = cap;
    }
    let at = c.p.httpdll.ret;
    let _ = c.host.write_cstr(at, bytes);
    Ok(Ret::cdecl(at))
}

/// Returns `s` in the boundary encoding (NativeStrings.Write).
fn ret_str(c: &mut Ctx, s: &str) -> ApiResult {
    let bytes = if c.p.httpdll.utf8 { s.as_bytes().to_vec() } else { encode_cp(ACP, s) };
    ret_raw(c, &bytes)
}

fn ret_f64(c: &mut Ctx, v: f64) -> ApiResult {
    c.host.fpu_push(v);
    Ok(Ret::cdecl(0))
}

/// A double argument starting at stack slot `slot` (doubles take two).
fn num(c: &Ctx, slot: u32) -> f64 {
    f64::from_bits(c.arg(slot) as u64 | (c.arg(slot + 1) as u64) << 32)
}

fn id(c: &Ctx, slot: u32) -> i32 {
    to_i32(num(c, slot))
}

// ---- files --------------------------------------------------------------------------------

fn read_file(c: &mut Ctx, path: &str) -> Option<Vec<u8>> {
    let key = c.p.vpath(path);
    if !c.p.fs.is_file(&key) {
        return None;
    }
    c.p.fs.read_all(&key)
}

fn write_file(c: &mut Ctx, path: &str, bytes: &[u8]) -> bool {
    let key = c.p.vpath(path);
    c.p.fs.create(&key) && c.p.fs.set_len(&key, 0) && c.p.fs.write_at(&key, 0, bytes)
}

// ---- the exports --------------------------------------------------------------------------

pub fn register(r: &mut Registry) {
    let d = "http_dll_2_3.dll";
    // (on demand: the stubs of every other module keep their numbers)
    macro_rules! add {
        ($name:expr, $f:expr) => {
            r.add_on_demand(d, $name, $f)
        };
    }

    add!("buffer_create", |c| {
        let st = &mut c.p.httpdll;
        st.next_buffer += 1;
        let id = st.next_buffer;
        st.buffers.insert(id, Buffer::default());
        ret_f64(c, id as f64)
    });
    add!("buffer_destroy", |c| {
        let removed = c.p.httpdll.buffers.remove(&id(c, 0)).is_some();
        ret_f64(c, removed as u32 as f64)
    });
    add!("buffer_clear", |c| {
        let r = match c.p.httpdll.buffers.get_mut(&id(c, 0)) {
            Some(b) => {
                b.clear();
                1.0
            }
            None => 0.0,
        };
        ret_f64(c, r)
    });
    add!("buffer_read_from_file", |c| {
        let (b, path) = (id(c, 0), str_in(c, 2));
        if !c.p.httpdll.buffers.contains_key(&b) {
            return ret_f64(c, 0.0);
        }
        let data = read_file(c, &path);
        let buf = c.p.httpdll.buffers.get_mut(&b).unwrap();
        let r = match data {
            Some(d) => {
                buf.load(&d);
                1.0
            }
            None => {
                buf.clear();
                0.0
            }
        };
        ret_f64(c, r)
    });
    add!("buffer_write_to_file", |c| {
        let (b, path) = (id(c, 0), str_in(c, 2));
        let Some(data) = c.p.httpdll.buffers.get(&b).map(|b| b.data.clone()) else { return ret_f64(c, 0.0) };
        let ok = write_file(c, &path, &data);
        ret_f64(c, ok as u32 as f64)
    });
    add!("buffer_get_length", |c| {
        let n = c.p.httpdll.buffers.get(&id(c, 0)).map_or(0, |b| b.data.len());
        ret_f64(c, n as f64)
    });
    add!("buffer_to_string", |c| {
        let s = match c.p.httpdll.buffers.get(&id(c, 0)) {
            // the string ends at the first NUL, as the original's char* does
            Some(b) => decode(&b.data[..b.data.iter().position(|&x| x == 0).unwrap_or(b.data.len())]),
            None => String::new(),
        };
        ret_str(c, &s)
    });

    add!("buffer_read_uint8", |c| read_with(c, |b| b.read::<1>().map_or(0.0, |x| x[0] as f64)));
    add!("buffer_read_int8", |c| read_with(c, |b| b.read::<1>().map_or(0.0, |x| x[0] as i8 as f64)));
    add!("buffer_read_uint16", |c| read_with(c, |b| b.read::<2>().map_or(0.0, |x| u16::from_le_bytes(x) as f64)));
    add!("buffer_read_int16", |c| read_with(c, |b| b.read::<2>().map_or(0.0, |x| i16::from_le_bytes(x) as f64)));
    add!("buffer_read_int32", |c| read_with(c, |b| b.read::<4>().map_or(0.0, |x| i32::from_le_bytes(x) as f64)));
    add!("buffer_read_uint32", |c| read_with(c, |b| b.read::<4>().map_or(0.0, |x| u32::from_le_bytes(x) as f64)));
    add!("buffer_read_int64", |c| read_with(c, |b| b.read::<8>().map_or(0.0, |x| i64::from_le_bytes(x) as f64)));
    add!("buffer_read_uint64", |c| read_with(c, |b| b.read::<8>().map_or(0.0, |x| u64::from_le_bytes(x) as f64)));
    add!("buffer_read_float32", |c| read_with(c, |b| f32::from_bits(b.read::<4>().map_or(0, u32::from_le_bytes)) as f64));
    add!("buffer_read_float64", |c| read_with(c, |b| b.read::<8>().map_or(0.0, f64::from_le_bytes)));
    add!("buffer_read_string", |c| {
        let s = c.p.httpdll.buffers.get_mut(&id(c, 0)).map(|b| b.read_string()).unwrap_or_default();
        ret_str(c, &s)
    });

    add!("buffer_write_uint8", |c| write_with(c, |b, v| b.write(&[if v <= 0.0 { 0 } else if v >= 255.0 { 255 } else { v as u8 }])));
    add!("buffer_write_int8", |c| write_with(c, |b, v| b.write(&[(if v <= -128.0 { -128 } else if v >= 127.0 { 127 } else { v as i8 }) as u8])));
    add!("buffer_write_uint16", |c| write_with(c, |b, v| b.write(&to_u16(v).to_le_bytes())));
    add!("buffer_write_int16", |c| write_with(c, |b, v| b.write(&(if v <= -32768.0 { i16::MIN } else if v >= 32767.0 { i16::MAX } else { v as i16 }).to_le_bytes())));
    add!("buffer_write_int32", |c| write_with(c, |b, v| b.write(&to_i32(v).to_le_bytes())));
    add!("buffer_write_uint32", |c| write_with(c, |b, v| b.write(&(if v <= 0.0 { 0 } else if v >= u32::MAX as f64 { u32::MAX } else { v as u32 }).to_le_bytes())));
    add!("buffer_write_int64", |c| write_with(c, |b, v| b.write(&(if v <= i64::MIN as f64 { i64::MIN } else if v >= i64::MAX as f64 { i64::MAX } else { v as i64 }).to_le_bytes())));
    add!("buffer_write_uint64", |c| write_with(c, |b, v| b.write(&(if v <= 0.0 { 0 } else if v >= u64::MAX as f64 { u64::MAX } else { v as u64 }).to_le_bytes())));
    add!("buffer_write_float32", |c| write_with(c, |b, v| b.write(&to_float(v).to_le_bytes())));
    add!("buffer_write_float64", |c| write_with(c, |b, v| b.write(&v.to_le_bytes())));
    add!("buffer_write_string", |c| {
        let s = str_in(c, 2);
        let r = match c.p.httpdll.buffers.get_mut(&id(c, 0)) {
            Some(b) => {
                b.write(s.as_bytes());
                b.write(&[0]);
                1.0
            }
            None => 0.0,
        };
        ret_f64(c, r)
    });

    add!("set_utf8_mode", |c| {
        c.p.httpdll.utf8 = num(c, 0) >= 0.5;
        ret_f64(c, 1.0)
    });
    add!("strip_non_bmp", |c| {
        let s: String = str_in(c, 0).chars().filter(|&ch| (ch as u32) < 0x10000).collect();
        ret_str(c, &s)
    });
    add!("ansi_to_utf8", |c| {
        let bytes = bytes_in(c, 0);
        let s = decode_cp(ACP, &bytes);
        ret_raw(c, s.as_bytes())
    });
    add!("utf8_to_ansi", |c| {
        let bytes = bytes_in(c, 0);
        let s = decode(&bytes);
        let out = encode_cp(ACP, &s);
        ret_raw(c, &out)
    });
    add!("file_read_text", |c| {
        let path = str_in(c, 0);
        let s = if path.is_empty() { String::new() } else { read_file(c, &path).map(|d| decode(&d)).unwrap_or_default() };
        ret_str(c, &s)
    });
    add!("file_write_text", |c| {
        let (path, text, ansi) = (str_in(c, 0), str_in(c, 1), num(c, 2));
        if path.is_empty() {
            return ret_f64(c, 0.0);
        }
        let dir = crate::vfs::parent(&c.p.vpath(&path)).to_string();
        if !dir.is_empty() && !c.p.fs.is_dir(&dir) {
            let mut acc = String::new();
            for part in dir.split('\\') {
                if !acc.is_empty() {
                    acc.push('\\');
                }
                acc.push_str(part);
                if !c.p.fs.is_dir(&acc) {
                    c.p.fs.mkdir(&acc);
                }
            }
        }
        let bytes = if ansi != 0.0 { encode_cp(ACP, &text) } else { text.into_bytes() };
        let ok = write_file(c, &path, &bytes);
        ret_f64(c, ok as u32 as f64)
    });
    add!("md5_dir", |c| {
        let dir = str_in(c, 0);
        let hash = md5_dir(c, &dir).unwrap_or_default();
        ret_str(c, &hash)
    });
    add!("message_box", |c| {
        let (text, title) = (str_in(c, 0), str_in(c, 1));
        let title = if title.is_empty() { "IWPO".to_owned() } else { title };
        c.p.log(format!("message_box [{title}]: {text}"));
        c.host.prompt(super::Ask::Message, &title, &text);
        ret_f64(c, 0.0)
    });
    add!("input_box", |c| {
        let (title, prompt, default) = (str_in(c, 0), str_in(c, 1), str_in(c, 2));
        // (cancelling gives "", as the original's dialog does)
        let answer = c.host.prompt(super::Ask::Text(&default), &title, &prompt).unwrap_or_default();
        ret_str(c, &answer)
    });

    add!("socket_create", |c| {
        let st = &mut c.p.httpdll;
        st.next_socket += 1;
        let id = st.next_socket;
        st.sockets.insert(id, Tcp::default());
        ret_f64(c, id as f64)
    });
    add!("socket_destroy", |c| {
        let r = match c.p.httpdll.sockets.remove(&id(c, 0)) {
            Some(mut s) => {
                tcp_reset(c, &mut s);
                1.0
            }
            None => 0.0,
        };
        ret_f64(c, r)
    });
    add!("socket_get_state", |c| {
        let s = c.p.httpdll.sockets.get(&id(c, 0)).map_or(0, |s| s.state);
        ret_f64(c, s as f64)
    });
    add!("socket_reset", |c| socket_with(c, 0, tcp_reset));
    add!("socket_connect", |c| {
        let (host, port) = (str_in(c, 2), to_u16(num(c, 3)));
        socket_with(c, 0, move |c, s| tcp_connect(c, s, &host, port))
    });
    add!("socket_update_read", |c| socket_with(c, 0, tcp_update_read));
    add!("socket_update_write", |c| socket_with(c, 0, tcp_update_write));
    add!("socket_shut_down", |c| socket_with(c, 0, |_, s| s.shutdown = true));
    add!("socket_read_message", |c| {
        let (sid, bid) = (id(c, 0), id(c, 2));
        let st = &mut c.p.httpdll;
        let (Some(s), Some(b)) = (st.sockets.get_mut(&sid), st.buffers.get_mut(&bid)) else { return ret_f64(c, 0.0) };
        let r = tcp_read_message(s, b) as u32 as f64;
        ret_f64(c, r)
    });
    add!("socket_write_message", |c| {
        let (sid, bid) = (id(c, 0), id(c, 2));
        let st = &mut c.p.httpdll;
        let (Some(s), Some(b)) = (st.sockets.get_mut(&sid), st.buffers.get(&bid)) else { return ret_f64(c, 0.0) };
        tcp_write_message(s, b);
        ret_f64(c, 1.0)
    });

    add!("udpsocket_create", |c| {
        let st = &mut c.p.httpdll;
        st.next_udp += 1;
        let id = st.next_udp;
        st.udp.insert(id, Udp::default());
        ret_f64(c, id as f64)
    });
    add!("udpsocket_destroy", |c| {
        let r = match c.p.httpdll.udp.remove(&id(c, 0)) {
            Some(u) => {
                close_net(c, u.net);
                1.0
            }
            None => 0.0,
        };
        ret_f64(c, r)
    });
    add!("udpsocket_exists", |c| {
        let r = c.p.httpdll.udp.contains_key(&id(c, 0)) as u32 as f64;
        ret_f64(c, r)
    });
    add!("udpsocket_get_state", |c| {
        let s = c.p.httpdll.udp.get(&id(c, 0)).map_or(0, |u| u.state);
        ret_f64(c, s as f64)
    });
    add!("udpsocket_start", |c| {
        // (the local port is the relay's business)
        let uid = id(c, 0);
        let Some(u) = c.p.httpdll.udp.get_mut(&uid) else { return ret_f64(c, 0.0) };
        let old = if u.state != UDP_NOT_STARTED { u.net.take() } else { None };
        u.state = UDP_STARTED;
        close_net(c, old);
        ret_f64(c, 1.0)
    });
    add!("udpsocket_set_destination", |c| {
        let (uid, host, port) = (id(c, 0), str_in(c, 2), to_u16(num(c, 3)));
        let Some(u) = c.p.httpdll.udp.get_mut(&uid) else { return ret_f64(c, 0.0) };
        if u.state != UDP_STARTED {
            u.state = UDP_ERROR;
            return ret_f64(c, 1.0);
        }
        let old = u.net.take();
        close_net(c, old);
        let net = c.p.sockets.net_id();
        let opened = !host.is_empty() && c.host.net_open(net, true, &host, port);
        let u = c.p.httpdll.udp.get_mut(&uid).unwrap();
        if opened {
            u.net = Some(net);
        } else {
            u.state = UDP_ERROR;
        }
        ret_f64(c, 1.0)
    });
    add!("udpsocket_receive", |c| {
        let (uid, bid) = (id(c, 0), id(c, 2));
        let st = &c.p.httpdll;
        let (Some(u), true) = (st.udp.get(&uid), st.buffers.contains_key(&bid)) else { return ret_f64(c, 0.0) };
        let (state, net) = (u.state, u.net);
        if state != UDP_STARTED {
            return ret_f64(c, 0.0);
        }
        let mut data = vec![0u8; 65507];
        let got = net.and_then(|n| c.host.net_recv(n, &mut data));
        let b = c.p.httpdll.buffers.get_mut(&bid).unwrap();
        let r = match got {
            Some(n) if net.is_some() && c.host.net_state(net.unwrap()) != NetState::Failed => {
                b.load(&data[..n]);
                1.0
            }
            _ => {
                b.clear();
                0.0
            }
        };
        ret_f64(c, r)
    });
    add!("udpsocket_send", |c| {
        let (uid, bid) = (id(c, 0), id(c, 2));
        let st = &c.p.httpdll;
        let (Some(u), Some(b)) = (st.udp.get(&uid), st.buffers.get(&bid)) else { return ret_f64(c, 0.0) };
        if let (UDP_STARTED, Some(net)) = (u.state, u.net) {
            let data = b.data.clone();
            if !c.host.net_send(net, &data) {
                if let Some(u) = c.p.httpdll.udp.get_mut(&uid) {
                    u.state = UDP_ERROR;
                    u.net = None;
                }
                close_net(c, Some(net));
            }
        }
        ret_f64(c, 1.0)
    });
}

/// A buffer read export: the buffer's value, or 0 without the buffer.
fn read_with(c: &mut Ctx, f: impl FnOnce(&mut Buffer) -> f64) -> ApiResult {
    let v = c.p.httpdll.buffers.get_mut(&id(c, 0)).map_or(0.0, f);
    ret_f64(c, v)
}

/// A buffer write export: 1, or 0 without the buffer.
fn write_with(c: &mut Ctx, f: impl FnOnce(&mut Buffer, f64)) -> ApiResult {
    let v = num(c, 2);
    let r = match c.p.httpdll.buffers.get_mut(&id(c, 0)) {
        Some(b) => {
            f(b, v);
            1.0
        }
        None => 0.0,
    };
    ret_f64(c, r)
}

/// A socket export: 1 after `f`, or 0 without the socket.
fn socket_with(c: &mut Ctx, slot: u32, f: impl FnOnce(&mut Ctx, &mut Tcp)) -> ApiResult {
    let sid = id(c, slot);
    let Some(mut s) = c.p.httpdll.sockets.remove(&sid) else { return ret_f64(c, 0.0) };
    f(c, &mut s);
    c.p.httpdll.sockets.insert(sid, s);
    ret_f64(c, 1.0)
}

/// md5 over (name, NUL, contents) of each file directly in `dir`, names in byte order;
/// None when a name is not ASCII or a file cannot be read (the GML walk then hashes).
fn md5_dir(c: &mut Ctx, dir: &str) -> Option<String> {
    if dir.is_empty() {
        return None;
    }
    let key = c.p.vpath(dir);
    if !c.p.fs.is_dir(&key) {
        return None;
    }
    let mut names: Vec<String> = c.p.fs.list(&key).into_iter().filter(|(_, m)| !m.is_dir).map(|(n, _)| n).collect();
    if names.iter().any(|n| !n.is_ascii()) {
        return None;
    }
    names.sort();
    let mut ctx = md5::Context::new();
    for n in names {
        ctx.consume(n.as_bytes());
        ctx.consume([0u8]);
        ctx.consume(c.p.fs.read_all(&format!("{key}\\{n}"))?);
    }
    Some(format!("{:x}", ctx.compute()))
}
