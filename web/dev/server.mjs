// Development server: serves the web runtime with cross-origin isolation headers
// (SharedArrayBuffer, high-resolution timers). With --report, it also prints each
// line the page POSTs to /log and exits when the page POSTs /done. Pages opened with
// ?monitor record their session in target/monitor/ (see index.html).
//   node web/dev/server.mjs [--port 8766] [--report] [--timeout SECONDS]
import { createServer } from 'node:http';
import { appendFileSync, createReadStream, statSync, mkdirSync, writeFileSync } from 'node:fs';
import { extname, join, normalize, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, '..');
const repoRoot = join(webRoot, '..');
const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const port = Number(opt('--port', 8766));
const report = args.includes('--report');
const timeout = Number(opt('--timeout', 0));
const captureDir = opt('--capture-dir', null);

const types = {
  '.html': 'text/html; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.css': 'text/css',
};

// URL prefix -> directory
const mounts = [
  ['/runtime/', join(repoRoot, 'target/wasm/wasm32-unknown-unknown/release')],
  ['/monitor/', join(repoRoot, 'target/monitor')],
  ['/games/', join(webRoot, 'public/games')],
  // the browser importer (crates/webimport, built without debug info) and the system
  // DLLs it may add (tools/system32.sh)
  ['/import/', join(repoRoot, 'target/wasm-import/wasm32-unknown-unknown/release')],
  ['/system32/', join(webRoot, 'public/system32')],
  // what each runner's code needs first (tools/module-profiles.mjs)
  ['/profiles/', join(webRoot, 'public/profiles')],
  ['/', webRoot],
];

function resolve(urlPath) {
  for (const [prefix, dir] of mounts) {
    if (urlPath.startsWith(prefix)) {
      const rel = normalize(decodeURIComponent(urlPath.slice(prefix.length))).replace(/^(\.\.(\/|\\|$))+/, '');
      return join(dir, rel);
    }
  }
  return null;
}

const server = createServer(async (req, res) => {
  const headers = {
    'cross-origin-opener-policy': 'same-origin',
    'cross-origin-embedder-policy': 'require-corp',
    'cross-origin-resource-policy': 'same-origin',
    'cache-control': 'no-store',
  };
  const url = new URL(req.url, 'http://x');
  if (req.method === 'POST' && url.pathname.startsWith('/capture/')) {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    res.writeHead(204, headers).end();
    if (captureDir) {
      mkdirSync(captureDir, { recursive: true });
      const n = url.pathname.slice('/capture/'.length).replace(/[^0-9]/g, '');
      writeFileSync(join(captureDir, `frame-${n.padStart(5, '0')}.png`), Buffer.concat(chunks));
      if (report) console.log(`captured frame ${n}`);
    }
    return;
  }
  if (req.method === 'POST' && url.pathname === '/monitor') {
    // a monitored session (page ?monitor): appended to target/monitor/<session>.log
    let body = '';
    for await (const chunk of req) body += chunk;
    res.writeHead(204, headers).end();
    const name = (url.searchParams.get('s') ?? 'session').replace(/[^A-Za-z0-9._-]/g, '_');
    const dir = join(repoRoot, 'target', 'monitor');
    mkdirSync(dir, { recursive: true });
    appendFileSync(join(dir, `${name}.log`), body);
    return;
  }
  if (req.method === 'POST' && (url.pathname === '/log' || url.pathname === '/done')) {
    let body = '';
    for await (const chunk of req) body += chunk;
    res.writeHead(204, headers).end();
    if (report) {
      process.stdout.write(body.endsWith('\n') ? body : body + '\n');
      if (url.pathname === '/done') setTimeout(() => process.exit(0), 100);
    }
    return;
  }
  let path = resolve(url.pathname === '/' ? '/index.html' : url.pathname);
  try {
    if (!path || !statSync(path).isFile()) throw new Error('not found');
  } catch {
    res.writeHead(404, headers).end('not found');
    return;
  }
  res.writeHead(200, { ...headers, 'content-type': types[extname(path)] ?? 'application/octet-stream' });
  createReadStream(path).pipe(res);
});
server.listen(port, '127.0.0.1', () => console.error(`dev server on http://127.0.0.1:${port}/`));
if (timeout) setTimeout(() => (console.log('timeout: no /done received'), process.exit(1)), timeout * 1000);
