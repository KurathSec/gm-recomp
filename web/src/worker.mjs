// The guest's main thread: fetches the runtime and the game files, then runs the game.
// Messages to the page: {type: 'log', line} and {type: 'done', status}.
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
    let manifest;
    let downloaded;
    const t0 = performance.now();
    if (local) {
      // imported in this browser (import.mjs)
      const g = await loadGame(await openSaves(), game);
      if (!g) throw new Error(`no imported game ${game}`);
      manifest = g.manifest;
      downloaded = g.files.map(([path, data]) => [path, data, 0]);
      post(`loaded in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    } else {
      const base = `/games/${encodeURIComponent(game)}/`;
      manifest = await (await fetch(`${base}manifest.json`)).json();
      downloaded = await Promise.all(
        manifest.files.map(async (f) => {
          const r = await fetch(`${base}files/${f.path.split('/').map(encodeURIComponent).join('/')}`);
          if (!r.ok) throw new Error(`${f.path}: HTTP ${r.status}`);
          const mtime = Date.parse(r.headers.get('last-modified') ?? '') || 0;
          return [f.path, new Uint8Array(await r.arrayBuffer()), mtime];
        }),
      );
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
    const onProfile = (entries) => {
      if (!profileDb) return;
      const merged = new Uint32Array((profile?.length ?? 0) + entries.length);
      if (profile) merged.set(profile);
      merged.set(entries, profile?.length ?? 0);
      profile = merged;
      profileSaving = profileSaving.then(() => storeProfile(profileDb, game, merged)).catch((e) => post(`storing the translation profile failed: ${e}`));
    };
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
    const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, epoch, print, canvas, pace, pacing, ahead, onFrame, onReady, onResize, audioRing, onSave, onStats, monitor, onSlow, replay: replay?.input, inputRing, profile, onProfile });
    await saving;
    await profileSaving;
    self.postMessage({ type: 'done', status });
  } catch (e) {
    post(`error: ${e && e.stack ? e.stack : e}`);
    self.postMessage({ type: 'done', status: -1 });
  }
};
