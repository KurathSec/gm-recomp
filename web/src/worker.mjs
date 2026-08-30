// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
import { runGame } from './host.mjs';

self.onmessage = async ({ data }) => {
  const { game, frames = 0, epoch } = data;
  const post = (line) => self.postMessage({ type: 'log', line });
  try {
    const base = `/games/${encodeURIComponent(game)}/`;
    const manifest = await (await fetch(`${base}manifest.json`)).json();
    const t0 = performance.now();
    const files = await Promise.all(
      manifest.files.map(async (f) => {
        const r = await fetch(`${base}files/${f.path.split('/').map(encodeURIComponent).join('/')}`);
        if (!r.ok) throw new Error(`${f.path}: HTTP ${r.status}`);
        const mtime = Date.parse(r.headers.get('last-modified') ?? '') || 0;
        return [f.path, new Uint8Array(await r.arrayBuffer()), mtime];
      }),
    );
    post(`downloaded in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    const runtimeBytes = await (await fetch('/runtime/web.wasm')).arrayBuffer();
    const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, epoch, print: post });
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
