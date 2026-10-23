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
 * Test and diagnosis options, as worker.mjs takes them: frames, pace, pacing, ahead, script,
 * capture, monitor, replay, slowMs, useProfile.
 * Callbacks: onLog(line), onSize(w, h) (the game's resolution), onStats(stats),
 * onCapture(frame, png), onMonitor(line), onSoundPending(waiting) (sound waits for a click
 * or key press), onSaved() (saves stored), onDone(status) (the game ended: 0, or why not).
 * @returns {{worker: Worker, setKeymap(map): void, soundStats(): Promise<string | null>, stop(): void}}
 */
export function playGame(o) {
  const { game, local = false, canvas, modChoices = {}, pageKeys = {} } = o;
  let keymap = o.keymap ?? {};
  const listeners = [];
  const listen = (target, type, f) => {
    target.addEventListener(type, f);
    listeners.push(() => target.removeEventListener(type, f));
  };

  // The game's resolution, for mouse coordinates.
  let gameW = 640, gameH = 480;

  // Sound starts with the first user gesture (browser autoplay rules); o.audio starts it
  // without one (tests, with autoplay allowed).
  let audio = null;
  let audioStarted = false;
  let audioNode = null;
  const startAudio = async () => {
    if (audioStarted || !audio) return;
    audioStarted = true;
    try {
      const ctx = new AudioContext({ sampleRate: audio.rate, latencyHint: 0.005 }); // as low as the system allows
      await ctx.audioWorklet.addModule(new URL('./audio-worklet.js', import.meta.url));
      // the queue starts at what the system's audio period needs (see the worklet)
      const latency = Math.max(0.015, (ctx.baseLatency || 0) + 0.01);
      const node = new AudioWorkletNode(ctx, 'ring-player', { outputChannelCount: [2], processorOptions: { sab: audio.sab, frames: audio.frames, latency } });
      node.connect(ctx.destination);
      audioNode = node;
      // Skip what piled up before the user enabled sound.
      const idx = new Int32Array(audio.sab, 0, 2);
      Atomics.store(idx, 1, Atomics.load(idx, 0));
      o.onSoundPending?.(false);
    } catch (e) {
      audioStarted = false;
      o.onLog?.(`audio failed: ${e}`);
    }
  };
  for (const t of ['pointerdown', 'keydown']) listen(window, t, startAudio);
  const soundStats = () => !audioNode ? Promise.resolve(audioStarted ? 'audio: not running' : null) : new Promise((res) => {
    audioNode.port.onmessage = ({ data: a }) => {
      const rms = Math.sqrt(a.sumsq / Math.max(1, 2 * a.played));
      const s = (f) => `${(f / audio.rate).toFixed(2)}s`;
      res(`audio: played ${s(a.played)}, ${a.underruns} underruns (${s(a.starved)} silent), skipped ${s(a.skipped)}, rms ${rms.toFixed(4)}, context ${audioNode.context.state} at ${audioNode.context.sampleRate} Hz, output latency ${((audioNode.context.outputLatency ?? 0) * 1000).toFixed(1)} ms, base latency ${((audioNode.context.baseLatency ?? 0) * 1000).toFixed(1)} ms, target ${(a.target * 1000).toFixed(1)} ms; first sound at ${a.start.toFixed(2)}s, now ${audioNode.context.currentTime.toFixed(2)}s, underruns at ${a.events.join(' ')}`);
    };
    audioNode.port.postMessage(0);
  });

  const w = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
  w.onmessage = ({ data }) => {
    if (data.type === 'log') o.onLog?.(data.line);
    else if (data.type === 'audio') {
      audio = data;
      if (o.audio) startAudio();
      else if (!audioStarted) o.onSoundPending?.(true);
    } else if (data.type === 'size') {
      [gameW, gameH] = [data.w, data.h];
      o.onSize?.(data.w, data.h);
    } else if (data.type === 'monitor') o.onMonitor?.(data.line);
    else if (data.type === 'stats') o.onStats?.(data);
    else if (data.type === 'capture') o.onCapture?.(data.frame, data.png);
    else if (data.type === 'saved') o.onSaved?.();
    else if (data.type === 'done') o.onDone?.(data.status);
  };
  w.onerror = (e) => {
    o.onLog?.(`worker error: ${e.message}`);
    o.onDone?.(`worker error: ${e.message}`);
  };

  // Keys go to the worker through a shared ring (written here as they happen, read at
  // the guest's message polls): [write, read], then 256 × [vk, scan, down] and 256
  // event times.
  const inputRing = new SharedArrayBuffer(8 + 256 * 12 + 256 * 8);
  const ringIdx = new Int32Array(inputRing, 0, 2);
  const ringEv = new Int32Array(inputRing, 8, 256 * 3);
  const ringWhen = new Float64Array(inputRing, 8 + 256 * 12, 256);
  const sendKey = (code, down, numlock, when) => {
    const k = KEYS[code];
    if (!k) return;
    const wr = Atomics.load(ringIdx, 0);
    if (((wr - Atomics.load(ringIdx, 1)) | 0) >= 256) return; // the guest is not reading
    const i = wr & 255;
    // the NumLock state decides what the keypad sends (see the runtime's numpad_key)
    ringEv.set([k[0], k[1] | (numlock ? 0x200 : 0), down ? 1 : 0], i * 3);
    ringWhen[i] = when ?? performance.timeOrigin + performance.now();
    Atomics.store(ringIdx, 0, (wr + 1) | 0);
  };

  const offscreen = canvas.transferControlToOffscreen();
  w.postMessage(
    {
      game,
      local,
      frames: o.frames ?? 0,
      epoch: o.epoch ?? Math.floor(Date.now() / 1000),
      canvas: offscreen,
      capture: o.capture ?? [],
      pace: o.pace ?? true,
      pacing: o.pacing ?? 'sync',
      ahead: o.ahead ?? true,
      script: o.script ?? [],
      persist: o.persist ?? true,
      monitor: o.monitor ?? false,
      replay: o.replay ?? null,
      slowMs: o.slowMs,
      inputRing,
      useProfile: o.useProfile ?? true,
      modChoices,
    },
    [offscreen],
  );

  // Input goes to the worker as it happens; the runtime applies it at the guest's next
  // message poll. A key the page remaps reaches the game as its target, and is released
  // as the key it went down as (the map may change while it is held).
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
  const mouse = (e) => {
    const r = canvas.getBoundingClientRect();
    const buttons = (e.buttons & 1) | ((e.buttons & 2) ? 2 : 0) | ((e.buttons & 4) ? 4 : 0);
    const t = e.type === 'mousemove' ? undefined : performance.timeOrigin + e.timeStamp;
    w.postMessage({ type: 'mouse', x: (e.clientX - r.left) * (gameW / r.width), y: (e.clientY - r.top) * (gameH / r.height), buttons, t });
  };
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
        w.postMessage({ type: 'pad', index: i, ...state });
      }
    }
    requestAnimationFrame(pollPads);
  };
  requestAnimationFrame(pollPads);

  return {
    worker: w,
    /** A new key map, from the next key press on (held keys are released as they went down). */
    setKeymap(map) {
      keymap = map ?? {};
    },
    soundStats,
    /** Stops the game and the page's listeners. */
    stop() {
      stopped = true;
      for (const off of listeners) off();
      w.terminate();
      audioNode?.context.close().catch(() => {});
    },
  };
}
