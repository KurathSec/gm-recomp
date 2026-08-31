// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
import { runGame } from './host.mjs';
import { KEYS } from './keys.mjs';

let input = null;
const early = [];

// Saves: every file a game writes outside the temp directory, per game, in IndexedDB
// (key "<game>\0<lowercase path>", value {path, data | null for removed, mtime}).
function openSaves() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open('gmrecomp', 1);
    r.onupgradeneeded = () => r.result.createObjectStore('saves');
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function loadSaves(db, game) {
  return new Promise((resolve, reject) => {
    const range = IDBKeyRange.bound(`${game}\0`, `${game}\0\uffff`);
    const r = db.transaction('saves').objectStore('saves').getAll(range);
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

function storeSaves(db, game, changes) {
  // strict: on disk before the transaction completes (saves are small and rare)
  const tx = db.transaction('saves', 'readwrite', { durability: 'strict' });
  const store = tx.objectStore('saves');
  for (const c of changes) store.put(c, `${game}\0${c.path.toLowerCase()}`);
  return new Promise((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
}

/** The game's files with its saves applied: written files replace or add, removed ones go. */
function applySaves(files, saves) {
  const key = (p) => p.replaceAll('/', '\\').toLowerCase();
  const byPath = new Map(files.map((f) => [key(f[0]), f]));
  for (const s of saves) {
    if (s.data) byPath.set(key(s.path), [s.path, s.data, s.mtime]);
    else byPath.delete(key(s.path));
  }
  return [...byPath.values()];
}

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
  const { game, frames = 0, epoch, canvas, capture = [], pace = true, script = [], persist = true } = data;
  const post = (line) => self.postMessage({ type: 'log', line });
  try {
    const base = `/games/${encodeURIComponent(game)}/`;
    const manifest = await (await fetch(`${base}manifest.json`)).json();
    const t0 = performance.now();
    const downloaded = await Promise.all(
      manifest.files.map(async (f) => {
        const r = await fetch(`${base}files/${f.path.split('/').map(encodeURIComponent).join('/')}`);
        if (!r.ok) throw new Error(`${f.path}: HTTP ${r.status}`);
        const mtime = Date.parse(r.headers.get('last-modified') ?? '') || 0;
        return [f.path, new Uint8Array(await r.arrayBuffer()), mtime];
      }),
    );
    post(`downloaded in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    let db = null;
    let files = downloaded;
    if (persist) {
      try {
        db = await openSaves();
        const saves = await loadSaves(db, game);
        if (saves.length) post(`restored ${saves.length} saved file${saves.length === 1 ? '' : 's'}`);
        files = applySaves(downloaded, saves);
      } catch (e) {
        post(`saves unavailable: ${e}`);
        db = null;
      }
    }
    let saving = Promise.resolve();
    const onSave = (changes) => {
      if (db) saving = saving.then(() => storeSaves(db, game, changes)).catch((e) => post(`saving failed: ${e}`));
    };
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
    // One second of 48 kHz stereo; the page plays it once the user allows audio.
    const ringFrames = 48000;
    const audioRing = { sab: new SharedArrayBuffer(8 + ringFrames * 8), frames: ringFrames };
    self.postMessage({ type: 'audio', sab: audioRing.sab, frames: ringFrames, rate: 48000 });
    const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, epoch, print: post, canvas, pace, onFrame, onReady, onResize, audioRing, onSave });
    await saving;
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
