// Save states kept in this browser (player.mjs `saveStates`), a worker of their own so
// compressing and storing them stays off the game's thread. Per game up to SLOTS slots in
// IndexedDB: database gmrecomp-states, store "states", key "<game>\0<slot>", value {game,
// slot, time, bytes, stored, thumb: Blob, data: Blob (gzipped: magic, meta length, page
// count, meta, page numbers, pages)}. The game's worker keeps the state it last saved or
// loaded itself, to load it again at once.
//
// The page lists and deletes: messages {id, op: 'list' | 'delete', game, slot}, answered
// {id, ok, result | error}. The game's worker (worker.mjs) hands states in and takes them
// out through the port the page gives with {op: 'port', port}: {id, op: 'put', game, slot,
// time, thumb, state} and {id, op: 'get', game, slot}, answered the same way, a put once
// the state is stored. A put's pages are in shared memory, compressed from there a piece
// at a time (the game's worker keeps them, and writes the next state into them only once
// this put is answered); a state read back is handed over, not copied.

export const SLOTS = 10;

let opened = null;
const db = () =>
  (opened ??= new Promise((resolve, reject) => {
    const r = indexedDB.open('gmrecomp-states', 1);
    r.onupgradeneeded = () => r.result.createObjectStore('states');
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  }));

const done = (r) =>
  new Promise((resolve, reject) => {
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });

const committed = (tx) =>
  new Promise((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });

const key = (game, slot) => `${game}\0${String(slot).padStart(2, '0')}`;
const range = (game) => IDBKeyRange.bound(`${game}\0`, `${game}\0￿`);

/** Per slot, the latest put: an older one still compressing is not stored over it. */
const latest = new Map();

const MAGIC = 0x31534d47; // "GMS1"
const PIECE = 4 << 20;

/** A state gzipped (see the value's `data` above) and its size before. */
async function compress(state) {
  const gz = new CompressionStream('gzip');
  const out = new Response(gz.readable).blob();
  const w = gz.writable.getWriter();
  const parts = [new Uint8Array(new Uint32Array([MAGIC, state.meta.length, state.pages.length]).buffer), state.meta, new Uint8Array(state.pages.buffer, state.pages.byteOffset, state.pages.byteLength)];
  let bytes = 0;
  for (const p of parts) {
    await w.write(p.slice());
    bytes += p.length;
  }
  // (streams take no shared memory: a copy of each piece goes in)
  for (const c of state.chunks) {
    for (let at = 0; at < c.length; at += PIECE) {
      await w.write(c.slice(at, at + PIECE));
    }
    bytes += c.length;
  }
  await w.close();
  return { data: await out, bytes };
}

/** A state from its gzipped bytes, its memory in shared memory (the game's worker may save its next state into it). */
async function unpack(blob) {
  const reader = blob.stream().pipeThrough(new DecompressionStream('gzip')).getReader();
  let left = new Uint8Array(0);
  const fill = async (out) => {
    for (let at = 0; at < out.length; ) {
      if (!left.length) {
        const { done, value } = await reader.read();
        if (done) throw new Error('not a save state');
        left = value;
      }
      const n = Math.min(out.length - at, left.length);
      out.set(left.subarray(0, n), at);
      left = left.subarray(n);
      at += n;
    }
  };
  const head = new Uint32Array(3);
  await fill(new Uint8Array(head.buffer));
  if (head[0] !== MAGIC) throw new Error('not a save state');
  const [, metaLen, n] = head;
  const meta = new Uint8Array(metaLen);
  await fill(meta);
  const pages = new Uint32Array(n);
  await fill(new Uint8Array(pages.buffer));
  const data = new Uint8Array(new SharedArrayBuffer(Math.max(1, 4096 * n)), 0, 4096 * n);
  await fill(data);
  reader.cancel().catch(() => {});
  return { meta, pages, chunks: [data] };
}

async function list(game) {
  const all = await done((await db()).transaction('states').objectStore('states').getAll(range(game)));
  return all.map(({ slot, time, bytes, stored, thumb }) => ({ slot, time, bytes, stored, thumb }));
}

/** A state's buffers, to hand it over (shared memory is shared). */
const buffers = (state) => [...new Set([state.meta.buffer, state.pages.buffer, ...state.chunks.map((c) => c.buffer)])].filter((b) => !(b instanceof SharedArrayBuffer));

async function put({ game, slot, time, thumb, state }) {
  latest.set(key(game, slot), time);
  const { data, bytes } = await compress(state);
  if (latest.get(key(game, slot)) !== time) return;
  const tx = (await db()).transaction('states', 'readwrite');
  tx.objectStore('states').put({ game, slot, time, bytes, stored: data.size, thumb, data }, key(game, slot));
  await committed(tx);
}

async function get({ game, slot }) {
  const v = await done((await db()).transaction('states').objectStore('states').get(key(game, slot)));
  if (!v) throw new Error('empty slot');
  return unpack(v.data);
}

async function remove({ game, slot }) {
  latest.delete(key(game, slot));
  const tx = (await db()).transaction('states', 'readwrite');
  tx.objectStore('states').delete(key(game, slot));
  await committed(tx);
}

const ops = { list: (m) => list(m.game), delete: remove, put, get };

function serve(target) {
  return async ({ data }) => {
    if (data.op === 'port') {
      data.port.onmessage = serve(data.port);
      return;
    }
    const f = ops[data.op];
    try {
      if (!f) throw new Error(`unknown op ${data.op}`);
      const result = await f(data);
      target.postMessage({ id: data.id, ok: true, result }, data.op === 'get' ? buffers(result) : []);
    } catch (e) {
      target.postMessage({ id: data.id, ok: false, error: String(e?.message ?? e) });
    }
  };
}

if (typeof WorkerGlobalScope !== 'undefined' && self instanceof WorkerGlobalScope) self.onmessage = serve(self);
