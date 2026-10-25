// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
import { asset } from './asset.mjs';
import { GL_OPTIONS } from './gpu.mjs';
import { runGame } from './host.mjs';
import { applyMods, pageMod, selectedMods } from './mods.mjs';
import { KEYS } from './keys.mjs';
import { applySaves, loadGame, loadProfile, loadSaves, openSaves, storeProfile, storeSaves } from './saves.mjs';

let input = null;
const early = [];

// Save states (state-store.mjs keeps them; the page asks with {type: 'state', id, op:
// 'save' | 'load', slot} and hears {type: 'state', id, ok, error?}, and for a save once more
// {type: 'state-stored', slot, ok, error?}).
let game = null;
let store = null;
let storeSeq = 0;
const storeCalls = new Map();
// (states handed to the store, by call: what takes them back)
const lent = new Map();
const storeCall = (msg, transfer = [], onBack = null) =>
  new Promise((resolve, reject) => {
    const id = ++storeSeq;
    storeCalls.set(id, { resolve, reject });
    if (onBack) lent.set(id, onBack);
    store.postMessage({ ...msg, id }, transfer);
  });

/** A small picture of the frame a state was made at (WebP where the browser makes it). */
async function thumbnail(frame) {
  if (!frame || typeof OffscreenCanvas !== 'function') return null;
  const img = new ImageData(new Uint8ClampedArray(frame.px.buffer), frame.w, frame.h);
  for (let i = 3; i < img.data.length; i += 4) img.data[i] = 255;
  const full = new OffscreenCanvas(frame.w, frame.h);
  full.getContext('2d').putImageData(img, 0, 0);
  const s = Math.min(1, 240 / frame.w);
  const small = new OffscreenCanvas(Math.max(1, Math.round(frame.w * s)), Math.max(1, Math.round(frame.h * s)));
  const g = small.getContext('2d');
  g.imageSmoothingEnabled = s < 1;
  g.drawImage(full, 0, 0, small.width, small.height);
  return small.convertToBlob({ type: 'image/webp', quality: 0.85 });
}

/** `p`, or a rejection once the game shows no frame for a while (a dialog is open). */
function soon(p) {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => {
      input.state.cancel();
      reject(new Error('the game shows no frames now'));
    }, 5000);
  });
  return Promise.race([p, late]).finally(() => clearTimeout(timer));
}

/** The state last saved or loaded here ({slot, state}): loaded again without the store. */
let held = null;

async function handleState({ id, op, slot }) {
  // (the page deleted the slot)
  if (op === 'drop') {
    if (held?.slot === slot) held = null;
    return;
  }
  const reply = (r) => self.postMessage({ type: 'state', id, ...r });
  if (!input?.state || !store) return reply({ ok: false, error: input ? 'save states are off' : 'the game has not started yet' });
  try {
    if (op === 'save') {
      // (the state held here gives its buffers to the new one, which replaces it)
      const old = held && (await held.state);
      held = null;
      const st = await soon(input.state.save(old ? [...new Set(old.chunks.map((c) => c.buffer))] : []));
      reply({ ok: true });
      const time = Date.now();
      const state = { meta: st.meta, pages: st.pages, chunks: st.chunks };
      // (handed to the store, which gives it back as soon as it has its own copy)
      let back;
      held = { slot, state: new Promise((res) => (back = res)) };
      const thumb = await thumbnail(st.frame).catch(() => null);
      const transfer = [...new Set([state.meta.buffer, state.pages.buffer, ...state.chunks.map((c) => c.buffer)])];
      storeCall({ op: 'put', game, slot, time, thumb, state }, transfer, back).then(
        () => self.postMessage({ type: 'state-stored', slot, ok: true }),
        (e) => self.postMessage({ type: 'state-stored', slot, ok: false, error: String(e?.message ?? e) }),
      );
    } else if (op === 'load') {
      if (held?.slot !== slot) {
        held = null;
        held = { slot, state: await storeCall({ op: 'get', game, slot }) };
      }
      await soon(input.state.load(await held.state));
      reply({ ok: true });
    } else reply({ ok: false, error: `unknown op ${op}` });
  } catch (e) {
    reply({ ok: false, error: String(e?.message ?? e) });
  }
}

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
  if (data.type === 'state') return handleState(data);
  if (data.statePort) {
    store = data.statePort;
    store.onmessage = ({ data: r }) => {
      if (r.back) {
        lent.get(r.back)?.(r.state);
        lent.delete(r.back);
        return;
      }
      const c = storeCalls.get(r.id);
      storeCalls.delete(r.id);
      if (r.ok) c?.resolve(r.result);
      else c?.reject(new Error(r.error));
    };
  }
  game = data.game;
  const { frames = 0, epoch, canvas, capture = [], pace = true, pacing = 'sync', ahead = true, script = [], persist = true, monitor = false, replay = null, slowMs, inputRing, useProfile = true, local = false, modChoices = {} } = data;
  if (slowMs !== undefined) self.slowMs = slowMs;
  const post = (line) => self.postMessage({ type: 'log', line });
  try {
    // The runtime compiles while the game downloads.
    const runtime = fetch(asset('runtime/web.wasm')).then((r) => {
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
      const base = asset(`games/${encodeURIComponent(game)}/`);
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
    // the folders the game comes with empty (a save folder), as paths ending in '/'
    for (const d of manifest.dirs ?? []) downloaded.push([`${d}/`, new Uint8Array(0), 0]);
    // The mods chosen for the game change its files before the saves go on top.
    const chosen = selectedMods(modChoices, manifest.engine);
    for (const id of Object.keys(modChoices)) {
      if (modChoices[id]?.on && !chosen.some((m) => m.id === id)) post(`mod ${id}: does not work with this game's engine, left out`);
    }
    const mods = chosen.filter((m) => !pageMod(m.id));
    if (mods.length) {
      const t = performance.now();
      downloaded = await applyMods(downloaded, manifest, mods, post);
      post(`mods ${mods.map((m) => m.id).join(', ')} applied in ${((performance.now() - t) / 1000).toFixed(1)}s`);
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
      fetch(asset(`profiles/module-${m.id.toString(16)}-${m.base.toString(16)}.bin`))
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
      // (the page hears once they are stored: a site may copy them elsewhere)
      if (db) saving = saving.then(() => storeSaves(db, game, changes)).then(() => self.postMessage({ type: 'saved' })).catch((e) => post(`saving failed: ${e}`));
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
    // A dialog the game opens (GameMaker 8's dialogs, http_dll's input_box): the page
    // answers through a shared buffer ([state: 0 waiting, 1 answered, 2 cancelled;
    // length], then the UTF-8 text) while the game's thread waits, as on Windows.
    const promptSab = new SharedArrayBuffer(8 + 4096);
    const prompt = (title, text, def, kind) => {
      const st = new Int32Array(promptSab, 0, 2);
      Atomics.store(st, 0, 0);
      self.postMessage({ type: 'prompt', title, text, def, kind, sab: promptSab });
      while (Atomics.load(st, 0) === 0) Atomics.wait(st, 0, 0);
      if (Atomics.load(st, 0) === 2) return null;
      return new TextDecoder().decode(new Uint8Array(promptSab, 8, Math.min(Atomics.load(st, 1), 4096)).slice());
    };
    // the guest's network connections go through PlayOnline's relay, when it is on
    const relayUrl = mods.find((m) => m.id === 'PlayOnline')?.config.RelayUrl || undefined;
    const status = await runGame({ relayUrl, prompt, runtimeBytes, files, exe: manifest.entry, frames, epoch, print, canvas, pace, pacing, ahead, onFrame, onReady, onResize, audioRing, onSave, onStats, monitor, onSlow, replay: replay?.input, inputRing, profile, onProfile, sharedProfile });
    await saving;
    await profileSaving;
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
