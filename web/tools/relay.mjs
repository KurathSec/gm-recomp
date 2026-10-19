// A WebSocket relay for games' network connections in the browser (crates/runtime
// hle/winsock.rs, web/src/host.mjs): each WebSocket, opened as
//   ws://<relay>/?proto=tcp|udp&host=<name or address>&port=<port>
// is a TCP connection or a UDP "connection" (datagrams to and from one address) the relay
// makes on the browser's behalf. The handshake completes only once the TCP connection is
// made, so a refused connection fails the WebSocket.
//
//   node web/tools/relay.mjs [--port 8790] [--host 127.0.0.1] [--allow host[:port],...] [--to host]
//
// --to sends every connection to that host instead (a local test server).
//
// Without --allow it connects anywhere it is asked to: anyone who can reach it can use it
// as a proxy. Put it behind TLS (wss://) when the page is served over https.
import { createHash } from 'node:crypto';
import dgram from 'node:dgram';
import http from 'node:http';
import net from 'node:net';

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const port = Number(opt('--port', 8790));
const bindHost = opt('--host', '127.0.0.1');
const allow = opt('--allow', '')
  .split(',')
  .filter(Boolean)
  .map((a) => a.toLowerCase());
if (!allow.length) console.warn('relay: no --allow list; it will connect to any host and port it is asked to');

const redirect = opt('--to', null);
const allowed = (host, p) => !allow.length || allow.includes(host.toLowerCase()) || allow.includes(`${host.toLowerCase()}:${p}`);

/** A WebSocket frame (server to client: unmasked). */
function frame(opcode, payload) {
  const n = payload.length;
  const head = n < 126 ? Buffer.from([0x80 | opcode, n]) : n < 65536 ? Buffer.from([0x80 | opcode, 126, n >> 8, n & 255]) : Buffer.concat([Buffer.from([0x80 | opcode, 127]), (() => { const b = Buffer.alloc(8); b.writeBigUInt64BE(BigInt(n)); return b; })()]);
  return Buffer.concat([head, payload]);
}

/** Reads client frames from `socket`, calling `onMessage(Buffer)` per binary/text message. */
function readFrames(socket, onMessage, onClose) {
  let buf = Buffer.alloc(0);
  let parts = [];
  socket.on('data', (d) => {
    buf = Buffer.concat([buf, d]);
    for (;;) {
      if (buf.length < 2) return;
      const fin = buf[0] & 0x80;
      const opcode = buf[0] & 0x0f;
      const masked = buf[1] & 0x80;
      let len = buf[1] & 0x7f;
      let o = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        o = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        o = 10;
      }
      const maskAt = o;
      if (masked) o += 4;
      if (buf.length < o + len) return;
      const payload = Buffer.from(buf.subarray(o, o + len));
      if (masked) for (let i = 0; i < len; i++) payload[i] ^= buf[maskAt + (i & 3)];
      buf = buf.subarray(o + len);
      if (opcode === 0x8) {
        socket.end(frame(0x8, Buffer.alloc(0)));
        onClose();
        return;
      }
      if (opcode === 0x9) {
        socket.write(frame(0xa, payload));
        continue;
      }
      if (opcode === 0xa) continue;
      parts.push(payload);
      if (fin) {
        onMessage(Buffer.concat(parts));
        parts = [];
      }
    }
  });
}

const server = http.createServer((req, res) => {
  res.writeHead(426, { 'content-type': 'text/plain' });
  res.end('a WebSocket relay: ?proto=tcp|udp&host=...&port=...\n');
});

server.on('upgrade', (req, socket) => {
  const url = new URL(req.url, 'http://relay');
  const proto = url.searchParams.get('proto');
  const host = url.searchParams.get('host') ?? '';
  const p = Number(url.searchParams.get('port'));
  const key = req.headers['sec-websocket-key'];
  const refuse = (code, why) => {
    socket.end(`HTTP/1.1 ${code} ${why}\r\nconnection: close\r\n\r\n`);
    console.log(`relay: ${proto} ${host}:${p} refused (${why})`);
  };
  if (!key || !['tcp', 'udp'].includes(proto) || !host || !(p > 0 && p < 65536)) return refuse(400, 'Bad Request');
  if (!allowed(host, p)) return refuse(403, 'Forbidden');
  const target = redirect ?? host;
  let opened = false;
  const accept = () => {
    const hash = createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    socket.write(`HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: ${hash}\r\n\r\n`);
  };
  const send = (d) => !socket.destroyed && socket.write(frame(0x2, d));
  const close = () => !socket.destroyed && socket.end(frame(0x8, Buffer.alloc(0)));
  if (proto === 'tcp') {
    const tcp = net.connect(p, target);
    tcp.setNoDelay(true);
    tcp.once('connect', () => {
      opened = true;
      accept();
      console.log(`relay: tcp ${host}:${p} open`);
      readFrames(socket, (m) => tcp.write(m), () => tcp.end());
      tcp.on('data', send);
      tcp.on('close', close);
    });
    tcp.once('error', (e) => (opened ? close() : refuse(502, e.code ?? 'Bad Gateway')));
    socket.on('close', () => tcp.destroy());
    socket.on('error', () => tcp.destroy());
  } else {
    const udp = dgram.createSocket('udp4');
    udp.once('error', (e) => (opened ? close() : refuse(502, e.code ?? 'Bad Gateway')));
    udp.connect(p, target, () => {
      opened = true;
      accept();
      console.log(`relay: udp ${host}:${p} open`);
      readFrames(socket, (m) => udp.send(m), () => udp.close());
      udp.on('message', send);
    });
    socket.on('close', () => {
      try {
        udp.close();
      } catch {}
    });
    socket.on('error', () => {});
  }
});

server.listen(port, bindHost, () => console.log(`relay on ws://${bindHost}:${port}/`));
