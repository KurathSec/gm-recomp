// Save states kept in this browser (player.mjs `saveStates`), a worker of their own so
// compressing and storing them stays off the game's thread. Per game up to SLOTS slots in
// IndexedDB: database gmrecomp-states, store "states", key "<game>\0<slot>", value {game,
// slot, time, bytes, stored, thumb: Blob, data: Blob (`pack`ed, gzipped)}. The state last
// saved or loaded is also kept as it is, to load it again at once.
//
// The page lists and deletes: messages {id, op: 'list' | 'delete', game, slot}, answered
// {id, ok, result | error}. The game's worker (worker.mjs) hands states in and takes them
// out through the port the page gives with {op: 'port', port}: {id, op: 'put', game, slot,
// time, thumb, state} and {id, op: 'get', game, slot}, answered the same way, a put once
// the state is stored.

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

/** {game, slot, time, state}: the state last saved or loaded. */
let kept = null;
/** Per slot, the latest put: an older one still compressing is not stored over it. */
const latest = new Map();

const MAGIC = 0x31534d47; // "GMS1"

/** A state as bytes: magic, meta length, page count, meta, page numbers, pages. */
function pack(state) {
  const head = new Uint32Array([MAGIC, state.meta.length, state.pages.length]);
  return new Blob([head, state.meta, state.pages, ...state.chunks]);
}

function unpack(buf) {
  const head = new Uint32Array(buf, 0, 3);
  if (head[0] !== MAGIC) throw new Error('not a save state');
  const [, metaLen, n] = head;
  const meta = new Uint8Array(buf, 12, metaLen);
  // (the page numbers are copied: their offset need not be aligned)
  const pages = new Uint32Array(buf.slice(12 + metaLen, 12 + metaLen + 4 * n));
  const data = new Uint8Array(buf, 12 + metaLen + 4 * n, 4096 * n);
  return { meta, pages, chunks: [data] };
}

const gzip = (blob) => new Response(blob.stream().pipeThrough(new CompressionStream('gzip'))).blob();
const gunzip = (blob) => new Response(blob.stream().pipeThrough(new DecompressionStream('gzip'))).arrayBuffer();

async function list(game) {
  const all = await done((await db()).transaction('states').objectStore('states').getAll(range(game)));
  return all.map(({ slot, time, bytes, stored, thumb }) => ({ slot, time, bytes, stored, thumb }));
}

async function put({ game, slot, time, thumb, state }) {
  kept = { game, slot, time, state };
  latest.set(key(game, slot), time);
  const raw = pack(state);
  const data = await gzip(raw);
  if (latest.get(key(game, slot)) !== time) return;
  const tx = (await db()).transaction('states', 'readwrite');
  tx.objectStore('states').put({ game, slot, time, bytes: raw.size, stored: data.size, thumb, data }, key(game, slot));
  await committed(tx);
}

async function get({ game, slot }) {
  if (kept && kept.game === game && kept.slot === slot) return kept.state;
  const v = await done((await db()).transaction('states').objectStore('states').get(key(game, slot)));
  if (!v) throw new Error('empty slot');
  const state = unpack(await gunzip(v.data));
  kept = { game, slot, time: v.time, state };
  return state;
}

async function remove({ game, slot }) {
  if (kept && kept.game === game && kept.slot === slot) kept = null;
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
      target.postMessage({ id: data.id, ok: true, result: await f(data) });
    } catch (e) {
      target.postMessage({ id: data.id, ok: false, error: String(e?.message ?? e) });
    }
  };
}

if (typeof WorkerGlobalScope !== 'undefined' && self instanceof WorkerGlobalScope) self.onmessage = serve(self);
