// Serves memtest/ with cross-origin isolation headers (required for SharedArrayBuffer)
// and prints the page's POSTed result as one JSON line, then exits.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] ?? 8765);
const timeoutMs = Number(process.argv[3] ?? 60000);

const server = createServer(async (req, res) => {
  if (req.method === 'POST' && req.url === '/report') {
    let body = '';
    for await (const chunk of req) body += chunk;
    res.writeHead(204).end();
    console.log(body);
    setTimeout(() => process.exit(0), 100);
    return;
  }
  try {
    const html = await readFile(join(root, 'index.html'));
    res.writeHead(200, {
      'content-type': 'text/html; charset=utf-8',
      'cross-origin-opener-policy': 'same-origin',
      'cross-origin-embedder-policy': 'require-corp',
      'cache-control': 'no-store',
    });
    res.end(html);
  } catch {
    res.writeHead(404).end();
  }
});
server.listen(port, '127.0.0.1', () => console.error(`memtest on http://127.0.0.1:${port}/`));
setTimeout(() => { console.log(JSON.stringify({ error: 'timeout: no report received' })); process.exit(1); }, timeoutMs);
