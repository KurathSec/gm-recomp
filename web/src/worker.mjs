// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
import { runGame } from './host.mjs';
import { KEYS } from './keys.mjs';

let input = null;
const early = [];

function handleInput(data) {
  if (!input) return early.push(data);
  if (data.type === 'key') {
    const k = KEYS[data.code];
    if (k) input.key(k[0], k[1], data.down);
  } else if (data.type === 'mouse') {
    input.mouse(data.x, data.y, data.buttons);
  }
}

self.onmessage = async ({ data }) => {
  if (data.type === 'key' || data.type === 'mouse') return handleInput(data);
  const { game, frames = 0, epoch, canvas, capture = [], pace = true, script = [] } = data;
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
    const want = new Set(capture);
    // Scripted input for tests: [frame, code, down] applied after that frame.
    const onFrame = async (frame, gpu) => {
      for (const [f, code, a, b, c] of script) {
        if (f !== frame) continue;
        if (code === 'mouse') handleInput({ type: 'mouse', x: a, y: b, buttons: c });
        else handleInput({ type: 'key', code, down: a });
      }
      if (!want.has(frame)) return;
      const png = await gpu.snapshot();
      self.postMessage({ type: 'capture', frame, png }, [png]);
    };
    const onReady = (api) => {
      input = api;
      early.splice(0).forEach(handleInput);
    };
    const onResize = (w, h) => self.postMessage({ type: 'size', w, h });
    const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, epoch, print: post, canvas, pace, onFrame, onReady, onResize });
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
