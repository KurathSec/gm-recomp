// Saves: every file a game writes outside its temp directory, per game, in IndexedDB
// (key "<game>\0<lowercase path>", value {path, data: Uint8Array | null if removed, mtime}).
// Used by the worker (restore at start, store as the game writes) and by the page
// (export, import, delete).

export function openSaves() {
  return new Promise((resolve, reject) => {
    const r = indexedDB.open('gmrecomp', 3);
    r.onupgradeneeded = () => {
      for (const name of ['saves', 'profiles', 'games']) if (!r.result.objectStoreNames.contains(name)) r.result.createObjectStore(name);
    };
    r.onsuccess = () => resolve(r.result);
    r.onerror = () => reject(r.error);
  });
}

const range = (game) => IDBKeyRange.bound(`${game}\0`, `${game}\0￿`);

function done(req) {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function committed(tx) {
  return new Promise((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
}

export function loadSaves(db, game) {
  return done(db.transaction('saves').objectStore('saves').getAll(range(game)));
}

export function storeSaves(db, game, changes) {
  // strict: on disk before the transaction completes (saves are small and rare)
  const tx = db.transaction('saves', 'readwrite', { durability: 'strict' });
  const store = tx.objectStore('saves');
  for (const c of changes) store.put(c, `${game}\0${c.path.toLowerCase()}`);
  return committed(tx);
}

export function deleteSaves(db, game) {
  const tx = db.transaction('saves', 'readwrite', { durability: 'strict' });
  tx.objectStore('saves').delete(range(game));
  return committed(tx);
}

/** The game's files with its saves applied: written files replace or add, removed ones go. */
export function applySaves(files, saves) {
  const key = (p) => p.replaceAll('/', '\\').toLowerCase();
  const byPath = new Map(files.map((f) => [key(f[0]), f]));
  for (const s of saves) {
    if (s.data) byPath.set(key(s.path), [s.path, s.data, s.mtime]);
    else byPath.delete(key(s.path));
  }
  return [...byPath.values()];
}

/** A save file to download: JSON with base64 contents. */
export function exportSaves(game, saves) {
  const b64 = (u8) => {
    let s = '';
    for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
    return btoa(s);
  };
  const files = saves.map((s) => ({ path: s.path, mtime: s.mtime, data: s.data ? b64(s.data) : null }));
  return JSON.stringify({ format: 'gmrecomp-saves', version: 1, game, files });
}

/** Parses an exported save file; returns the entries for `storeSaves`. */
export function parseSaves(text, game) {
  const j = JSON.parse(text);
  if (j.format !== 'gmrecomp-saves') throw new Error('not a gmrecomp save file');
  if (j.game !== game) throw new Error(`these saves are for ${j.game}`);
  const bytes = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
  return j.files.map((f) => ({ path: f.path, mtime: f.mtime, data: f.data === null ? null : bytes(f.data) }));
}

// Translation profiles: per game, [address, code hash] pairs (Uint32Array) of the
// functions sessions translated when they ran, in the order they first did. The next
// session translates them ahead while the game waits (see host.mjs `profile`).

export function loadProfile(db, game) {
  return done(db.transaction('profiles').objectStore('profiles').get(game)).then((v) => (v instanceof Uint32Array ? v : null));
}

export function storeProfile(db, game, pairs) {
  const tx = db.transaction('profiles', 'readwrite');
  tx.objectStore('profiles').put(pairs, game);
  return committed(tx);
}

// Games imported in the browser (import.mjs): per slug, {manifest, files: [path, bytes][]}.

export function storeGame(db, slug, game) {
  const tx = db.transaction('games', 'readwrite');
  tx.objectStore('games').put(game, slug);
  return committed(tx);
}

export function loadGame(db, slug) {
  return done(db.transaction('games').objectStore('games').get(slug));
}

/** The imported games' manifests. */
export function listGames(db) {
  return new Promise((resolve, reject) => {
    const out = [];
    const r = db.transaction('games').objectStore('games').openCursor();
    r.onsuccess = () => {
      const c = r.result;
      if (!c) return resolve(out);
      out.push(c.value.manifest);
      c.continue();
    };
    r.onerror = () => reject(r.error);
  });
}

export function deleteGame(db, slug) {
  const tx = db.transaction(['games', 'profiles'], 'readwrite');
  tx.objectStore('games').delete(slug);
  tx.objectStore('profiles').delete(slug);
  return committed(tx);
}
