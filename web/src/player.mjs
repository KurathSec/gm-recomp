// A game playing in a page: the worker that runs it (worker.mjs), the page's input fed to
// it (keyboard, mouse, game controllers), and its sound. The page around it (gmrecomp's
// own, or a site that embeds the player) does the rest: layout, buttons, saves.

import { KEYS } from './keys.mjs';

/** Keys the page keeps from the browser while a game plays (the game needs them). */
const GAME_KEYS = /^(Arrow|Space|Tab|Backspace|F\d|Enter|Shift|Control|Alt|Key|Digit|Escape|Numpad|Page|Home|End|Insert|Delete)/;

/**
 * Starts `game` in `canvas` (not yet transferred to a worker).
 * @param {object} o
 * @param {string} o.game  the game's slug
 * @param {boolean} [o.local]  imported in this browser (import.mjs), rather than served at games/<slug>/
 * @param {HTMLCanvasElement} o.canvas
 * @param {Record<string, {on: boolean, config?: object}>} [o.modChoices]  the mods to apply (mods.mjs)
 * @param {Record<string, string>} [o.keymap]  a key you press (KeyboardEvent.code) -> the key the game gets
 * @param {Record<string, () => void>} [o.pageKeys]  keys the page handles itself, never sent to the game
 * @param {number} [o.epoch]  the date the game sees (Unix seconds; default now)
 * @param {boolean} [o.persist]  keep the game's saves in this browser (default true)
 * @param {boolean} [o.saveStates]  save states (state-store.mjs): the returned `states`
 *   saves and loads them (statepanel.mjs is a page's list of them)
 * @param {boolean} [o.instantLoad]  with save states: a second copy of the game waits,
 *   loaded and stopped, at the state last saved or loaded, and takes over at a load of it
 *   (at once; about twice the memory). Not with a game online (PlayOnline).
 * Test and diagnosis options, as worker.mjs takes them: frames, pace, pacing, ahead, script,
 * capture, monitor, replay, slowMs, useProfile.
 * Callbacks: onLog(line), onSize(w, h) (the game's resolution), onStats(stats),
 * onCapture(frame, png), onMonitor(line), onSoundPending(waiting) (sound waits for a click
 * or key press), onSaved() (saves stored), onDone(status) (the game ended: 0, or why not),
 * onPrompt({title, text, def, kind}) (a dialog the game opened, kind 'message', 'text',
 * 'question' or 'error' (GameMaker 8's runtime error: Ignore or Abort): resolve to the text,
 * '' for OK, Yes or Ignore, null to cancel, for No or Abort; the browser's
 * alert/prompt/confirm without it).
 * onStateStored(slot, error) (a save state is in the browser's storage, or could not be).
 * @returns {{worker: Worker, setKeymap(map): void, soundStats(): Promise<string | null>, stop(): void, states: object | null}}
 */
export function playGame(o) {
  const { game, local = false, canvas, modChoices = {}, pageKeys = {} } = o;
  let keymap = o.keymap ?? {};
  const listeners = [];
  const listen = (target, type, f) => {
    target.addEventListener(type, f);
    listeners.push(() => target.removeEventListener(type, f));
  };
  // (the standby stops at a state in a suspending import: JSPI)
  const instant = !!(o.saveStates && o.instantLoad && typeof WebAssembly.Suspending === 'function' && !modChoices.PlayOnline?.on);

  // The copies of the game: the one playing (`cur`) and, for instant loads, the one
  // waiting at a state (`standby`). Each has its worker, canvas, key ring and sound ring.
  const copies = [];
  let cur = null;
  let standby = null;

  // The game's resolution, for mouse coordinates.
  let gameW = 640, gameH = 480;

  // Sound starts with the first user gesture (browser autoplay rules); o.audio starts it
  // without one (tests, with autoplay allowed). Each copy has its ring; the one playing is
  // heard.
  let ctx = null;
  let audioStarted = false;
  const latency = () => Math.max(0.015, (ctx.baseLatency || 0) + 0.01); // as low as the system allows
  const attachAudio = (c) => {
    if (!ctx || c.node || !c.audio) return;
    // the queue starts at what the system's audio period needs (see the worklet)
    c.node = new AudioWorkletNode(ctx, 'ring-player', { outputChannelCount: [2], processorOptions: { sab: c.audio.sab, frames: c.audio.frames, latency: latency() } });
    if (c === cur) c.node.connect(ctx.destination);
    // Skip what piled up before the user enabled sound.
    const idx = new Int32Array(c.audio.sab, 0, 2);
    Atomics.store(idx, 1, Atomics.load(idx, 0));
  };
  const startAudio = async () => {
    if (audioStarted || !cur?.audio) return;
    audioStarted = true;
    try {
      ctx = new AudioContext({ sampleRate: cur.audio.rate, latencyHint: 0.005 });
      await ctx.audioWorklet.addModule(new URL('./audio-worklet.js', import.meta.url));
      for (const c of copies) attachAudio(c);
      o.onSoundPending?.(false);
    } catch (e) {
      audioStarted = false;
      o.onLog?.(`audio failed: ${e}`);
    }
  };
  for (const t of ['pointerdown', 'keydown']) listen(window, t, startAudio);
  const soundStats = () => {
    const audioNode = cur?.node;
    const audio = cur?.audio;
    return !audioNode ? Promise.resolve(audioStarted ? 'audio: not running' : null) : new Promise((res) => {
      audioNode.port.onmessage = ({ data: a }) => {
        const rms = Math.sqrt(a.sumsq / Math.max(1, 2 * a.played));
        const s = (f) => `${(f / audio.rate).toFixed(2)}s`;
        res(`audio: played ${s(a.played)}, ${a.underruns} underruns (${s(a.starved)} silent), skipped ${s(a.skipped)}, rms ${rms.toFixed(4)}, context ${audioNode.context.state} at ${audioNode.context.sampleRate} Hz, output latency ${((audioNode.context.outputLatency ?? 0) * 1000).toFixed(1)} ms, base latency ${((audioNode.context.baseLatency ?? 0) * 1000).toFixed(1)} ms, target ${(a.target * 1000).toFixed(1)} ms; first sound at ${a.start.toFixed(2)}s, now ${audioNode.context.currentTime.toFixed(2)}s, underruns at ${a.events.join(' ')}`);
      };
      audioNode.port.postMessage(0);
    });
  };

  // A dialog the game opened (worker.mjs `prompt`): o.onPrompt answers (a string, or null
  // to cancel or for No), else the browser's own.
  const answerPrompt = async ({ title, text, def, kind: k, sab }) => {
    // ('error': GameMaker 8's error form; an answer ignores the error, null aborts the game)
    const kind = k === 3 ? 'error' : k === 2 ? 'question' : k === 1 ? 'text' : 'message';
    const shown = title ? `${title}\n\n${text}` : text;
    let answer = null;
    try {
      if (o.onPrompt) answer = await o.onPrompt({ title, text, def: kind === 'text' ? def ?? '' : null, kind });
      else if (kind === 'question' || kind === 'error') answer = window.confirm(shown) ? '' : null;
      else if (kind === 'text') answer = window.prompt(shown, def ?? '');
      else {
        window.alert(shown);
        answer = '';
      }
    } catch {}
    const st = new Int32Array(sab, 0, 2);
    if (answer == null) Atomics.store(st, 0, 2);
    else {
      const bytes = new TextEncoder().encode(answer).subarray(0, 4096);
      new Uint8Array(sab, 8).set(bytes);
      Atomics.store(st, 1, bytes.length);
      Atomics.store(st, 0, 1);
    }
    Atomics.notify(st, 0);
  };

  // Save states: the store worker keeps them; the game's workers make and load them, and
  // reach the store through ports of their own.
  let store = null;
  let seq = 0;
  const calls = new Map();
  const answer = (r) => {
    const c = calls.get(r.id);
    calls.delete(r.id);
    if (r.ok) c?.resolve(r.result);
    else c?.reject(new Error(r.error));
  };
  const call = (target, msg) =>
    new Promise((resolve, reject) => {
      const id = ++seq;
      calls.set(id, { resolve, reject });
      target.postMessage({ ...msg, id });
    });
  if (o.saveStates) {
    store = new Worker(new URL('./state-store.mjs', import.meta.url), { type: 'module' });
    store.onmessage = ({ data }) => answer(data);
  }

  // Keys go to a worker through a shared ring (written here as they happen, read at the
  // guest's message polls): [write, read], then 256 × [vk, scan, down] and 256 event times.
  const newRing = () => {
    const sab = new SharedArrayBuffer(8 + 256 * 12 + 256 * 8);
    return { sab, idx: new Int32Array(sab, 0, 2), ev: new Int32Array(sab, 8, 256 * 3), when: new Float64Array(sab, 8 + 256 * 12, 256) };
  };
  const sendKeyTo = (c, code, down, numlock, when) => {
    const k = KEYS[code];
    if (!k || !c) return;
    const { idx, ev, when: times } = c.ring;
    const wr = Atomics.load(idx, 0);
    if (((wr - Atomics.load(idx, 1)) | 0) >= 256) return; // the guest is not reading
    const i = wr & 255;
    // the NumLock state decides what the keypad sends (see the runtime's numpad_key)
    ev.set([k[0], k[1] | (numlock ? 0x200 : 0), down ? 1 : 0], i * 3);
    times[i] = when ?? performance.timeOrigin + performance.now();
    Atomics.store(idx, 0, (wr + 1) | 0);
  };
  const sendKey = (code, down, numlock, when) => sendKeyTo(cur, code, down, numlock, when);

  const init = {
    game,
    local,
    frames: o.frames ?? 0,
    epoch: o.epoch ?? Math.floor(Date.now() / 1000),
    pace: o.pace ?? true,
    pacing: o.pacing ?? 'sync',
    ahead: o.ahead ?? true,
    persist: o.persist ?? true,
    slowMs: o.slowMs,
    useProfile: o.useProfile ?? true,
    modChoices,
    instantLoad: instant,
  };
  /** Starts a copy of the game in `el` ('main': the one playing; 'standby'). */
  const spawn = (role, el) => {
    const c = { role, canvas: el, ring: newRing(), audio: null, node: null, size: null, w: null, want: null, ready: false };
    const w = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
    c.w = w;
    w.onmessage = ({ data }) => fromCopy(c, data);
    w.onerror = (e) => {
      if (c === cur) {
        o.onLog?.(`worker error: ${e.message}`);
        o.onDone?.(`worker error: ${e.message}`);
      } else {
        o.onLog?.(`standby copy: worker error: ${e.message}`);
        drop(c);
      }
    };
    let port = null;
    if (store) {
      const ch = new MessageChannel();
      store.postMessage({ op: 'port', port: ch.port1 }, [ch.port1]);
      port = ch.port2;
    }
    const offscreen = el.transferControlToOffscreen();
    const main = role === 'main';
    w.postMessage(
      { ...init, role, canvas: offscreen, inputRing: c.ring.sab, statePort: port, capture: main ? o.capture ?? [] : [], script: main ? o.script ?? [] : [], monitor: o.monitor ?? false, replay: main ? o.replay ?? null : null },
      port ? [offscreen, port] : [offscreen],
    );
    copies.push(c);
    return c;
  };
  const drop = (c) => {
    c.w.terminate();
    c.node?.disconnect();
    copies.splice(copies.indexOf(c), 1);
    if (standby === c) standby = null;
  };

  // Instant loads. Each save of a slot here is a new version of it; the standby waits at
  // one (`want`, `ready` once loaded), and a load of exactly that takes it over.
  const versions = new Map(); // slot -> saves of it so far
  let twin = null;
  const makeTwin = () => {
    // a second canvas over the first, as large (the page sizes the first)
    twin = document.createElement('canvas');
    twin.width = canvas.width;
    twin.height = canvas.height;
    twin.style.cssText = 'position: absolute; visibility: hidden; display: block; image-rendering: pixelated; background: #000;';
    canvas.after(twin);
    const place = () => {
      twin.style.left = `${canvas.offsetLeft}px`;
      twin.style.top = `${canvas.offsetTop}px`;
      twin.style.width = `${canvas.offsetWidth}px`;
      twin.style.height = `${canvas.offsetHeight}px`;
    };
    place();
    const ro = new ResizeObserver(place);
    ro.observe(canvas);
    if (canvas.parentElement) ro.observe(canvas.parentElement);
    listeners.push(() => ro.disconnect());
    for (const t of ['mousemove', 'mousedown', 'mouseup']) listen(twin, t, mouse);
    listen(twin, 'contextmenu', (e) => e.preventDefault());
    return twin;
  };
  /** The standby waits at `slot` as saved `version` times (`state`: shared, or the store's). */
  const prepare = (slot, version, state) => {
    if (!standby) standby = spawn('standby', twin ?? makeTwin());
    standby.want = { slot, version };
    standby.ready = false;
    standby.w.postMessage({ type: 'state', op: 'prepare', slot, version, state });
  };
  /** Shows `c` and sends it the input; the other copies are hidden and silent. */
  const show = (c) => {
    for (const k of copies) {
      k.canvas.style.visibility = k === c ? '' : 'hidden';
      if (k.node) {
        k.node.disconnect();
        if (k === c) {
          const idx = new Int32Array(k.audio.sab, 0, 2);
          Atomics.store(idx, 1, Atomics.load(idx, 0));
          k.node.connect(ctx.destination);
        }
      }
    }
    if (c.size) [gameW, gameH] = c.size;
  };
  /** The standby takes over at its state; the copy that played becomes the standby. */
  const swap = async () => {
    const next = standby;
    const was = cur;
    const { slot, version } = next.want;
    standby = null;
    cur = next;
    next.ready = false;
    show(next);
    // the keys and controllers held now are the new copy's (it had none)
    for (const [code, numlock] of held.values()) sendKeyTo(next, code, true, numlock);
    padLast.forEach((p, i) => p && next.w.postMessage({ type: 'pad', index: i, ...JSON.parse(p) }));
    await call(next.w, { type: 'state', op: 'activate' });
    standby = was;
    was.want = { slot, version };
    was.ready = false;
    was.w.postMessage({ type: 'state', op: 'standby', slot, version });
  };

  const fromCopy = (c, data) => {
    const playing = c === cur;
    switch (data.type) {
      case 'log':
        o.onLog?.(playing ? data.line : `[standby] ${data.line}`);
        break;
      case 'audio':
        c.audio = data;
        if (o.audio) startAudio();
        else if (!audioStarted && playing) o.onSoundPending?.(true);
        attachAudio(c);
        break;
      case 'size':
        c.size = [data.w, data.h];
        if (playing) {
          [gameW, gameH] = c.size;
          o.onSize?.(data.w, data.h);
        }
        break;
      case 'monitor':
        if (playing) o.onMonitor?.(data.line);
        break;
      case 'stats':
        if (playing) o.onStats?.(data);
        break;
      case 'capture':
        if (playing) o.onCapture?.(data.frame, data.png);
        break;
      case 'saved':
        if (playing) o.onSaved?.();
        break;
      case 'prompt':
        answerPrompt(data);
        break;
      case 'state':
        answer(data);
        break;
      case 'state-stored':
        o.onStateStored?.(data.slot, data.ok ? null : data.error);
        break;
      case 'files':
        // (the files the game wrote: the standby's stay the same)
        if (playing && standby) standby.w.postMessage({ type: 'state', op: 'files', changes: data.changes });
        break;
      case 'state-made': {
        if (!playing) break;
        const version = (versions.get(data.slot) ?? 0) + 1;
        versions.set(data.slot, version);
        prepare(data.slot, version, data.state);
        break;
      }
      case 'standby-ready':
        if (c === standby && c.want?.slot === data.slot && c.want?.version === data.version) c.ready = true;
        break;
      case 'standby-failed':
        o.onLog?.(`standby copy: ${data.error}`);
        break;
      case 'done':
        if (playing) o.onDone?.(data.status);
        else drop(c);
        break;
    }
  };

  cur = spawn('main', canvas);

  const states = store && {
    /** The game's states: [{slot, time, bytes, stored, thumb (Blob | null)}]. */
    list: () => call(store, { op: 'list', game }),
    /** Saves the game as it is at the end of its next frame into `slot` (resolves once made). */
    save: (slot) => call(cur.w, { type: 'state', op: 'save', slot }),
    /** Goes back to the state in `slot` (at once with instant loads, once the standby waits there). */
    load: async (slot) => {
      const version = versions.get(slot) ?? 0;
      if (standby?.ready && standby.want?.slot === slot && standby.want.version === version) return swap();
      await call(cur.w, { type: 'state', op: 'load', slot });
      if (instant && !(standby?.want?.slot === slot && standby.want.version === version)) prepare(slot, version, null);
    },
    remove: (slot) => {
      for (const c of copies) c.w.postMessage({ type: 'state', op: 'drop', slot });
      if (standby?.want?.slot === slot) standby.ready = false;
      versions.set(slot, (versions.get(slot) ?? 0) + 1);
      return call(store, { op: 'delete', game, slot });
    },
  };

  // Input goes to the playing copy as it happens; the runtime applies it at the guest's
  // next message poll. A key the page remaps reaches the game as its target, and is
  // released as the key it went down as (the map may change while it is held).
  const held = new Map(); // your key -> [the game's key, NumLock state]
  for (const type of ['keydown', 'keyup']) {
    listen(window, type, (e) => {
      if (e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement) return;
      if (e.target instanceof Element && e.target.closest('dialog[open]')) return;
      const page = pageKeys[e.code];
      if (page) {
        e.preventDefault();
        if (type === 'keydown' && !e.repeat) page();
        return;
      }
      const target = keymap[e.code] ?? e.code;
      if ((GAME_KEYS.test(e.code) || target !== e.code) && !e.metaKey && e.code !== 'F11') e.preventDefault();
      if (e.repeat) return;
      const when = performance.timeOrigin + e.timeStamp;
      if (type === 'keydown') {
        const numlock = e.getModifierState ? e.getModifierState('NumLock') : true;
        held.set(e.code, [target, numlock]);
        sendKey(target, true, numlock, when);
      } else {
        const [code, numlock] = held.get(e.code) ?? [target, true];
        held.delete(e.code);
        sendKey(code, false, numlock, when);
      }
    });
  }
  // Keys held while the page loses focus never see their keyup: release them then.
  const releaseHeld = () => {
    for (const [code, numlock] of held.values()) sendKey(code, false, numlock);
    held.clear();
  };
  listen(window, 'blur', releaseHeld);
  listen(document, 'visibilitychange', () => document.hidden && releaseHeld());
  function mouse(e) {
    const r = canvas.getBoundingClientRect();
    const buttons = (e.buttons & 1) | ((e.buttons & 2) ? 2 : 0) | ((e.buttons & 4) ? 4 : 0);
    const t = e.type === 'mousemove' ? undefined : performance.timeOrigin + e.timeStamp;
    cur.w.postMessage({ type: 'mouse', x: (e.clientX - r.left) * (gameW / r.width), y: (e.clientY - r.top) * (gameH / r.height), buttons, t });
  }
  for (const t of ['mousemove', 'mousedown', 'mouseup']) listen(canvas, t, mouse);
  listen(canvas, 'contextmenu', (e) => e.preventDefault());

  // Game controllers (the standard layout matches an Xbox controller): XInput state,
  // sent when it changes.
  const PAD_BUTTONS = { 0: 0x1000, 1: 0x2000, 2: 0x4000, 3: 0x8000, 4: 0x0100, 5: 0x0200, 8: 0x0020, 9: 0x0010, 10: 0x0040, 11: 0x0080, 12: 0x1, 13: 0x2, 14: 0x4, 15: 0x8 };
  const padLast = ['', '', '', ''];
  const axis = (v) => Math.max(-32768, Math.min(32767, Math.round((v ?? 0) * 32767)));
  let stopped = false;
  const pollPads = () => {
    if (stopped) return;
    const pads = navigator.getGamepads ? navigator.getGamepads() : [];
    for (let i = 0; i < 4; i++) {
      const g = pads[i];
      let state;
      if (g && g.connected) {
        let buttons = 0;
        for (const [k, bit] of Object.entries(PAD_BUTTONS)) if (g.buttons[k]?.pressed) buttons |= bit;
        const trig = (k) => Math.round(255 * (g.buttons[k]?.value ?? 0));
        state = { connected: true, buttons, lt: trig(6), rt: trig(7), lx: axis(g.axes[0]), ly: axis(-(g.axes[1] ?? 0)), rx: axis(g.axes[2]), ry: axis(-(g.axes[3] ?? 0)) };
      } else state = { connected: false, buttons: 0, lt: 0, rt: 0, lx: 0, ly: 0, rx: 0, ry: 0 };
      const key = JSON.stringify(state);
      if (key !== padLast[i]) {
        padLast[i] = key;
        cur.w.postMessage({ type: 'pad', index: i, ...state });
      }
    }
    requestAnimationFrame(pollPads);
  };
  requestAnimationFrame(pollPads);

  return {
    worker: copies[0].w,
    /** A new key map, from the next key press on (held keys are released as they went down). */
    setKeymap(map) {
      keymap = map ?? {};
    },
    soundStats,
    states,
    /** Stops the game and the page's listeners. */
    stop() {
      stopped = true;
      for (const off of listeners) off();
      for (const c of [...copies]) c.w.terminate();
      store?.terminate();
      twin?.remove();
      ctx?.close().catch(() => {});
    },
  };
}
