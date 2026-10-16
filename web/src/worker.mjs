// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
import { GL_OPTIONS } from './gpu.mjs';
import { runGame } from './host.mjs';
import { KEYS } from './keys.mjs';
import { applySaves, loadGame, loadProfile, loadSaves, openSaves, storeProfile, storeSaves } from './saves.mjs';

let input = null;
const early = [];

function handleInput(data) {
  if (!input) return early.push(data);
  if (data.type === 'key') {
    const k = KEYS[data.code];
    // the NumLock state decides what the keypad sends (see the runtime's numpad_key)
    if (k) input.key(k[0], k[1] | (data.numlock ? 0x200 : 0), data.down, data.t);
  } else if (data.type === 'mouse') {
    input.mouse(data.x, data.y, data.buttons, data.t);
  } else if (data.type === 'pad') {
    input.pad(data.index, data.connected, data.buttons, data.lt, data.rt, data.lx, data.ly, data.rx, data.ry);
  }
}

self.onmessage = async ({ data }) => {
  if (data.type === 'key' || data.type === 'mouse' || data.type === 'pad') return handleInput(data);
  const { game, frames = 0, epoch, canvas, capture = [], pace = true, pacing = 'sync', ahead = true, script = [], persist = true, monitor = false, replay = null, slowMs, inputRing, useProfile = true, local = false } = data;
  if (slowMs !== undefined) self.slowMs = slowMs;
  const post = (line) => self.postMessage({ type: 'log', line });
  try {
    // The runtime compiles while the game downloads.
    const runtime = fetch('/runtime/web.wasm').then((r) => {
      if (!r.ok) throw new Error(`runtime: HTTP ${r.status}`);
      return r.headers.get('content-type')?.startsWith('application/wasm') ? WebAssembly.compileStreaming(r) : r.arrayBuffer().then((b) => WebAssembly.compile(b));
    });
    runtime.catch(() => {}); // (awaited below)
    // Creating the WebGL context blocks this thread (seconds in a browser that has not
    // used WebGL yet): it is done while the downloads are under way.
    const warmGl = () => canvas?.getContext('webgl2', GL_OPTIONS);
    let manifest;
    let downloaded;
    const t0 = performance.now();
    if (local) {
      // imported in this browser (import.mjs)
      const pending = openSaves().then((db) => loadGame(db, game));
      warmGl();
      const g = await pending;
      if (!g) throw new Error(`no imported game ${game}`);
      manifest = g.manifest;
      downloaded = g.files.map(([path, data]) => [path, data, 0]);
      post(`loaded in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    } else {
      const base = `/games/${encodeURIComponent(game)}/`;
      manifest = await (await fetch(`${base}manifest.json`)).json();
      const pending = Promise.all(
        manifest.files.map(async (f) => {
          const r = await fetch(`${base}files/${f.path.split('/').map(encodeURIComponent).join('/')}`);
          if (!r.ok) throw new Error(`${f.path}: HTTP ${r.status}`);
          const mtime = Date.parse(r.headers.get('last-modified') ?? '') || 0;
          return [f.path, new Uint8Array(await r.arrayBuffer()), mtime];
        }),
      );
      warmGl();
      downloaded = await pending;
      post(`downloaded in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    }
    let db = null;
    let files = downloaded;
    if (persist) {
      try {
        db = await openSaves();
        const saves = await loadSaves(db, game);
        if (saves.length) post(`restored ${saves.length} saved file${saves.length === 1 ? '' : 's'}`);
        // monitor: the saves the session starts from, so it can be replayed
        if (monitor) {
          const hex = (u8) => Array.from(u8, (b) => b.toString(16).padStart(2, '0')).join('');
          for (const sv of saves) self.postMessage({ type: 'monitor', line: sv.data ? `save ${sv.path.replaceAll(' ', '%20')} ${hex(sv.data)}` : `deleted ${sv.path.replaceAll(' ', '%20')}` });
        }
        files = applySaves(downloaded, saves);
      } catch (e) {
        post(`saves unavailable: ${e}`);
        db = null;
      }
    }
    // a replayed session starts from its saves
    if (replay) files = applySaves(files, replay.saves);
    // the translation profile: what earlier sessions needed, and what this one adds
    let profile = null;
    let profileDb = null;
    if (useProfile) {
      try {
        profileDb = db ?? (await openSaves());
        profile = await loadProfile(profileDb, game);
        if (profile) post(`translation profile: ${profile.length / 3} functions`);
      } catch (e) {
        post(`translation profile unavailable: ${e}`);
        profileDb = null;
      }
    }
    let profileSaving = Promise.resolve();
    // Modules' own profiles (key `module:<code id>:<base>`): games built with the same
    // runner (and the D3DX DLL GMS games ship) run the same code, so a game's first
    // session starts from what other games needed of it; before any has, from the
    // site's (web/tools/module-profiles.mjs).
    const moduleKey = (m) => `module:${m.id.toString(16)}:${m.base.toString(16)}`;
    const moduleProfiles = new Map(); // key -> {data, seen: Set of addresses}
    const siteProfile = (m) =>
      fetch(`/profiles/module-${m.id.toString(16)}-${m.base.toString(16)}.bin`)
        // (a host may answer a missing file with a page)
        .then((r) => (r.ok && !r.headers.get('content-type')?.includes('html') ? r.arrayBuffer() : null))
        .then((b) => (b && b.byteLength % 12 === 0 ? new Uint32Array(b) : null))
        .catch(() => null);
    const moduleProfile = async (m) => {
      const key = moduleKey(m);
      if (!moduleProfiles.has(key)) {
        const data = (await loadProfile(profileDb, key).catch(() => null)) ?? (await siteProfile(m)) ?? new Uint32Array(0);
        const seen = new Set();
        for (let i = 0; i < data.length; i += 3) seen.add(data[i]);
        moduleProfiles.set(key, { data, seen });
      }
      return moduleProfiles.get(key);
    };
    const sharedProfile = async (modules) => {
      if (!profileDb) return null;
      const all = [];
      for (const m of modules) {
        const { data } = await moduleProfile(m);
        for (let i = 0; i < data.length; i += 3) all.push([data[i], data[i + 1], data[i + 2]]);
      }
      if (!all.length) return null;
      // (in order of need across the modules)
      all.sort((a, b) => a[2] - b[2]);
      post(`translation profile shared with other games: ${all.length} functions`);
      return Uint32Array.from(all.flat());
    };
    const onProfile = (entries, modules) => {
      if (!profileDb) return;
      const merged = new Uint32Array((profile?.length ?? 0) + entries.length);
      if (profile) merged.set(profile);
      merged.set(entries, profile?.length ?? 0);
      profile = merged;
      profileSaving = profileSaving
        .then(async () => {
          await storeProfile(profileDb, game, merged);
          for (const m of modules ?? []) {
            const mp = await moduleProfile(m);
            const add = [];
            for (let i = 0; i < entries.length; i += 3) {
              const a = entries[i];
              if (a >= m.base && a < m.base + m.size && !mp.seen.has(a)) {
                mp.seen.add(a);
                add.push(entries[i], entries[i + 1], entries[i + 2]);
              }
            }
            if (!add.length) continue;
            const data = new Uint32Array(mp.data.length + add.length);
            data.set(mp.data);
            data.set(add, mp.data.length);
            mp.data = data;
            await storeProfile(profileDb, moduleKey(m), data);
          }
        })
        .catch((e) => post(`storing the translation profile failed: ${e}`));
    };
    let saving = Promise.resolve();
    const onSave = (changes) => {
      if (db) saving = saving.then(() => storeSaves(db, game, changes)).catch((e) => post(`saving failed: ${e}`));
    };
    const runtimeBytes = await runtime;
    const want = new Set(capture);
    // Scripted input for tests: [frame, code, down] applied after that frame.
    const onFrame = async (frame, gpu) => {
      for (const [f, code, a, b, c] of script) {
        if (f !== frame) continue;
        if (code === 'mouse') handleInput({ type: 'mouse', x: a, y: b, buttons: c });
        else handleInput({ type: 'key', code, down: a, numlock: b });
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
    const onStats = (stats) => self.postMessage({ type: 'stats', ...stats });
    const onSlow = monitor
      ? (frame, busy, interval, clockMs, d) => {
          const f = (v) => v.toFixed(1);
          self.postMessage({ type: 'monitor', line: `slow ${frame} guest ${f(busy)} interval ${f(interval)} clock ${clockMs.toFixed(0)} waits ${d.waits} waited ${f(d.waited)} late ${f(d.late)} threads ${f(d.threads)} ahead ${f(d.ahead)} translate ${f(d.translate)} api ${d.api} dispatches ${d.dispatches}` });
        }
      : undefined;
    // monitor: input lines go to the page as monitor lines, everything else to the log
    const print = monitor ? (line) => (line.startsWith('input ') ? self.postMessage({ type: 'monitor', line }) : post(line)) : post;
    const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, epoch, print, canvas, pace, pacing, ahead, onFrame, onReady, onResize, audioRing, onSave, onStats, monitor, onSlow, replay: replay?.input, inputRing, profile, onProfile, sharedProfile });
    await saving;
    await profileSaving;
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
