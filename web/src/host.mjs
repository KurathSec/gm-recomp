// Host side of the browser runtime (crates/web): instantiates the engine, its cpu glue
// module and translated modules, and hands it the game files. Runs unchanged in a
// browser worker and in Node.

import { asset } from './asset.mjs';
import { D3DGL } from './gpu.mjs';

// (crates/web/src/lib.rs GLUE_FUNCS; nop64 is 64-bit code's no-op function)
const GLUE_FUNCS = ['g2r', 'r2g', 'fill', 'get', 'set', 'size', 'grow', 'vget', 'vset', 'call', 'gcopy', 'rget', 'rset', 'get64', 'set64', 'call64', 'nop64'];
const jspi = typeof WebAssembly.Suspending === 'function' && typeof WebAssembly.promising === 'function';
// A save state's fresh buffer is mapped by helper threads in chunks of this size.
const MAP_CHUNK = 1 << 20;

/**
 * @param {object} o
 * @param {BufferSource|WebAssembly.Module} o.runtimeBytes  web.wasm, or compiled
 * @param {Array<[string, Uint8Array, number?]>} o.files  game files: [path relative to the game dir ('\\' or '/'), bytes, mtime in ms]
 * @param {string} o.exe  entry executable
 * @param {number} [o.frames]  stop after this many Presents (0 = no limit)
 * @param {boolean} [o.trace]
 * @param {number} [o.epoch]  wall-clock time at start (Unix seconds)
 * @param {(line: string) => void} [o.print]
 * @param {(state: {guestMemory: WebAssembly.Memory}) => void} [o.onExit]  inspection after the run
 * @param {OffscreenCanvas} [o.canvas]  render target; without it nothing is drawn
 * @param {boolean} [o.pace]  hold frames to the game's own timing (needs JSPI)
 * @param {'clock'|'jit'|'asap'} [o.pacing]  when the guest waits and frames are shown (see `due`)
 * @param {boolean} [o.ahead]  translate code the game has not reached yet while it waits
 * @param {(frame: number, gpu: D3DGL|null) => void|Promise<void>} [o.onFrame]  after each Present
 * @param {(input: {key: Function, mouse: Function, pad: Function}) => void} [o.onReady]  input entry points,
 *   usable while the guest is suspended in Present
 * @param {(w: number, h: number) => void} [o.onResize]  the shown frame changed size
 * @param {{sab: SharedArrayBuffer, frames: number}} [o.audioRing]  where the sound mix goes
 * @param {(changes: {path: string, data: Uint8Array|null, mtime: number}[]) => void} [o.onSave]
 *   files the game wrote or removed (its saves; not the temp directory)
 * @param {string} [o.soundfontUrl]  a General MIDI SoundFont, fetched when a game first plays MIDI
 * @param {(stats: object) => void} [o.onStats]  twice a second while rendering: frame rate, real frame
 *   interval, guest time per frame, late frames, and input latency (event to the Present of the frame
 *   that read it; key/mouse calls pass the event's time as performance.timeOrigin + timeStamp)
 * @param {Uint32Array} [o.profile]  [address, code hash, frame first needed] of functions earlier
 *   sessions needed, in the order they did: translated before the start, after the first frame or
 *   while the game waits
 * @param {(entries: Uint32Array, modules: {base: number, size: number, id: number}[]) => void} [o.onProfile]
 *   functions this session needed that the profile lacked (every few seconds and at the end), as
 *   the same triples, to add to it, with the modules loaded at the start (to file them by module)
 * @param {string} [o.relayUrl]  a WebSocket relay (tools/relay.mjs) for the guest's network
 *   connections; without one, sockets fail to connect
 * @param {(modules: {base: number, size: number, id: number}[]) => Promise<Uint32Array|null>} [o.sharedProfile]
 *   without `profile`: what other games with the same modules needed (a runner's code is the same
 *   in every game built with it)
 * @param {number} [o.stateThreads]  helper threads (state-map.mjs) that map save states' fresh
 *   buffers
 */
export async function runGame({ runtimeBytes, files, exe, frames = 0, trace = false, epoch = Math.floor(Date.now() / 1000), print = console.log, onExit, canvas, pace = true, pacing = 'clock', ahead = true, onFrame, onReady, onResize, audioRing, onSave, soundfontUrl = asset('soundfont.sf2'), onStats, monitor = false, onSlow, replay, inputRing, profile, onProfile, sharedProfile, relayUrl, prompt, ctl, stateThreads = 0 }) {
  const dec = new TextDecoder();
  const enc = new TextEncoder();
  let rt;
  let table;
  let cpuImports;
  let rtImports;
  let x87Imports;
  let modules = 0;
  const mem = () => new Uint8Array(rt.exports.memory.buffer);

  let gpu = null;
  let glue;
  let startReal;
  // Wall-clock cost of each frame: from the guest resuming after one Present to the
  // next (the wait for the frame's time, and the event loop, are not counted).
  const frameMs = [];
  let lastPresent = 0;
  let resumed = 0;
  let headlessFrames = 0;
  // Live statistics (onStats): this window's samples.
  const epochNow = () => performance.timeOrigin + performance.now();
  let pendingInputs = [];
  const slackStats = { n: 0, est: 0, left: 0 };
  let gpuMs = 0;
  let compileMs = 0;
  let instantiateMs = 0;
  let moduleBytes = 0;
  // this frame's waits (count, ms waited, latest wake) and the runtime's counters at its start
  let waits = { n: 0, ms: 0, late: 0 };
  let marks = [0, 0, 0, 0, 0];
  // ms of other threads, ahead translation and translation; api calls; dispatches
  const counters = () => [0, 1, 2, 3, 4].map((k) => rt.exports.stat_us(k) / (k < 3 ? 1000 : 1));
  const frameDetail = () => {
    const now = counters();
    const d = { waits: waits.n, waited: waits.ms, late: waits.late, threads: now[0] - marks[0], ahead: now[1] - marks[1], translate: now[2] - marks[2], api: now[3] - marks[3], dispatches: now[4] - marks[4] };
    return d;
  };
  const frameReset = () => {
    waits = { n: 0, ms: 0, late: 0 };
    if (onSlow) marks = counters();
  };
  let win = { start: performance.now(), frames: 0, intervals: [], busy: [], late: 0, inputs: [], wake: [] };
  let lastPresentReal = 0;
  let lastClockMs = 0;
  const collect = (t, clockMs, busy) => {
    if (!onStats) return;
    if (lastPresentReal) {
      const interval = t - lastPresentReal;
      win.intervals.push(interval);
      // later than half a frame beyond the guest's own frame time
      const target = clockMs - lastClockMs;
      if (target > 0 && interval > target * 1.5) win.late++;
      // monitor: frames that took long or reached the screen late (the frame number
      // is the one being presented), with where the time went
      if (onSlow && ((busy ?? 0) > 12 || (target > 0 && interval > target + (self.slowMs ?? 4)))) onSlow(gpu.frames + 1, busy ?? 0, interval, clockMs, frameDetail());
    }
    lastPresentReal = t;
    lastClockMs = clockMs;
    if (busy !== undefined) win.busy.push(busy);
    win.frames++;
    // Input that arrived while the guest was suspended was read by this frame.
    const now = epochNow();
    for (const when of pendingInputs) win.inputs.push(now - when);
    pendingInputs = [];
    const elapsed = t - win.start;
    if (elapsed < 500) return;
    const sorted = (a) => [...a].sort((x, y) => x - y);
    const med = (a) => (a.length ? sorted(a)[a.length >> 1] : 0);
    const max = (a) => (a.length ? Math.max(...a) : 0);
    const avg = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0);
    onStats({
      fps: (win.frames * 1000) / elapsed,
      interval: avg(win.intervals),
      intervalMax: max(win.intervals),
      busy: med(win.busy),
      busyMax: max(win.busy),
      late: win.late,
      input: avg(win.inputs),
      inputMax: max(win.inputs),
      inputs: win.inputs.length,
      wake: avg(win.wake),
      wakeMax: max(win.wake),
    });
    win = { start: t, frames: 0, intervals: [], busy: [], late: 0, inputs: [], wake: [] };
  };
  // Frame timing. The guest runs on virtual time, which real time follows from startReal.
  // 'clock' pacing holds the guest wherever its clock moves on (sleeps, waits, timer
  // polls) until real time gets there, and shows each frame as soon as it is presented:
  // threads that wake while the main thread sleeps run in that idle time, and input is
  // read right after the wait. 'jit' holds a finished frame until its time and resumes
  // the guest just early enough for the next one; 'asap' shows each frame when it is
  // done and resumes the guest at the frame's time.
  // A loaded save state moves the guest's clock to the state's time: pacing starts over
  // from there.
  let clockMoved = () => {};
  const due = (clockMs) => {
    const now = performance.now();
    // more than 250 ms behind (a long load, a hidden tab): start over from now
    if (startReal === undefined || now - (startReal + clockMs) > 250) startReal = now - clockMs;
    return startReal + clockMs;
  };
  // functions this session needed that the profile lacked, handed out from `sent` on
  let sent = 0;
  // the modules loaded at the start, by code (see sharedProfile)
  const startModules = [];
  const flushProfile = () => {
    if (!onProfile) return;
    const n = rt.exports.demand_len();
    if (n <= sent) return;
    const entries = new Uint32Array(3 * (n - sent));
    for (let i = sent; i < n; i++) for (let k = 0; k < 3; k++) entries[3 * (i - sent) + k] = rt.exports.demand_at(i, k);
    sent = n;
    onProfile(entries, startModules);
  };
  const started = performance.now();
  const show = (clockMs, x, y, w, h, dw, dh, busy) => {
    collect(performance.now(), clockMs, busy);
    frameReset();
    if (gpu.frames === 0) print(`first frame after ${((performance.now() - started) / 1000).toFixed(2)}s, ${(rt.exports.stat_us(2) / 1e6).toFixed(2)}s of it translating`);
    if (gpu.frames % 250 === 249) flushProfile();
    const resized = canvas.width !== dw || canvas.height !== dh;
    gpu.present(x, y, w, h, dw, dh);
    if (resized && onResize) onResize(dw, dh);
    return gpu.frames;
  };
  // Recent guest time per frame, for the next frame's start.
  const recent = [];
  // (asked at every wait: sorted once per frame)
  let estimated = null;
  const estimate = () => {
    if (!recent.length) return 4;
    if (estimated === null) {
      const s = [...recent].sort((a, b) => a - b);
      estimated = s[Math.floor(s.length * 0.9)];
    }
    return estimated;
  };
  const MARGIN_MS = 2;
  let prevClock;
  // Nested setTimeout chains are clamped to 4 ms; Atomics.waitAsync on a word nobody
  // notifies is not, so short waits end on time where it exists.
  const never = typeof Atomics.waitAsync === 'function' && typeof SharedArrayBuffer === 'function' ? new Int32Array(new SharedArrayBuffer(4)) : null;
  const sleepUntil = (t) => {
    const ms = Math.max(0, t - performance.now());
    if (never && ms > 0) {
      const w = Atomics.waitAsync(never, 0, 0, ms);
      if (w.async) return w.value;
    }
    return new Promise((r) => setTimeout(r, ms));
  };
  const gpuImports = {
    gpu_resize: (w, h) => gpu?.resize(w, h),
    gpu_texture_create: (id, w, h, levels, fmt, rt) => gpu?.textureCreate(id, w, h, levels, fmt, rt),
    // (guest addresses come as signed i32: a large-address-aware game's reach past 2 GB)
    gpu_texture_upload: (id, level, w, h, fmt, pitch, bits) => gpu?.textureUpload(id, level, w, h, fmt, pitch, bits >>> 0),
    gpu_texture_delete: (id) => gpu?.textureDelete(id),
    gpu_set_target: (id, w, h) => gpu?.setTarget(id, w, h),
    gpu_clear: (flags, color, z, stencil, x, y, w, h) => gpu?.clear(flags, color >>> 0, z, stencil, x, y, w, h),
    gpu_draw: (ptr) => {
      const t = performance.now();
      gpu?.draw(ptr);
      gpuMs += performance.now() - t;
    },
    gpu_draw9: (ptr) => {
      const t = performance.now();
      gpu?.draw9(ptr);
      gpuMs += performance.now() - t;
    },
    gpu_shader_create: (id, vertex, ptr, len) => gpu?.shaderCreate(id, !!vertex, dec.decode(mem().subarray(ptr, ptr + len))),
    gpu_shader_delete: (id) => gpu?.shaderDelete(id),
    gpu_blit: (src, sx, sy, sw, sh, dst, dx, dy, dw, dh, linear) => gpu?.blit(src, sx, sy, sw, sh, dst, dx, dy, dw, dh, !!linear),
    gpu_read_target: (id, w, h, dst, pitch) => gpu?.readTarget(id, w, h, dst >>> 0, pitch),
    gpu_pace: () => {},
    gpu_slack: () => 0,
    gpu_present: (clockMs, x, y, w, h, dw, dh) => {
      if (!gpu) {
        // Headless (Node): frames are only counted.
        headlessFrames++;
        if (onFrame) onFrame(headlessFrames, null);
        return;
      }
      // Without JSPI the guest cannot wait: frames show as they come.
      const t = performance.now();
      if (lastPresent) frameMs.push(t - lastPresent);
      lastPresent = t;
      const frame = show(clockMs, x, y, w, h, dw, dh);
      if (onFrame) onFrame(frame, gpu);
    },
  };
  if (canvas && jspi) {
    // While the guest is suspended in these imports the worker's event loop runs: the
    // canvas shows the last frame and input messages arrive.
    // How late a wait ended (timer precision), when there was one.
    const wake = async (t) => {
      const before = performance.now();
      await sleepUntil(t);
      const after = performance.now();
      if (onStats && t > before) win.wake.push(after - t);
      waits.n++;
      waits.ms += after - before;
      waits.late = Math.max(waits.late, after - t);
    };
    const guestTime = () => {
      const busy = resumed ? performance.now() - resumed : undefined;
      if (busy !== undefined) {
        frameMs.push(busy);
        recent.push(busy);
        if (recent.length > 60) recent.shift();
        estimated = null;
      }
      return busy;
    };
    if (pace && pacing === 'sync' && never) {
      // Waits block the worker (Atomics.wait ends on time; an event-loop timer can
      // be late by several ms); each frame then yields once so the canvas shows it
      // and messages arrive.
      // Translating ahead may use the rest of the frame, not only this wait (the game
      // sleeps a millisecond at a time): until the next frame is due, less what the
      // guest needs for it and a margin; the waits after it then end at once.
      let shownClock = 0;
      // The frame period: the shortest of the last 8 clock steps, so a one-off long
      // frame (GMS skips a Present when it changes rooms: a 40 ms step) does not make
      // the next frame look 40 ms away and ahead translation run into it.
      let frameStep = 20;
      const steps = [];
      clockMoved = (clockMs) => {
        shownClock = clockMs;
        steps.length = 0;
      };
      if (ahead)
        gpuImports.gpu_slack = (clockMs) => {
          const now = performance.now();
          const est = estimate();
          const frameLeft = due(shownClock + frameStep) - est - 2 - now;
          slackStats.n++;
          slackStats.est += est;
          slackStats.left += Math.max(0, frameLeft);
          return Math.max(due(clockMs) - now, frameLeft);
        };
      gpuImports.gpu_pace = (clockMs) => {
        const at = due(clockMs);
        const before = performance.now();
        if (at - before < 1) return;
        Atomics.wait(never, 0, 0, at - before);
        const after = performance.now();
        if (onStats) win.wake.push(after - at);
        waits.n++;
        waits.ms += after - before;
        waits.late = Math.max(waits.late, after - at);
        resumed = after;
      };
      const channel = new MessageChannel();
      let wakeUp = null;
      channel.port1.onmessage = () => wakeUp?.();
      // Chrome hands a worker canvas's frame to the compositor only while the worker's
      // event loop handles a BeginFrame; blocked in the waits, it handled them late and
      // Chrome dropped some (a steady 50 fps reached the screen with 40-60 ms gaps, ~3 a
      // second). There the guest resumes at the worker's next animation frame, when the
      // frame has gone out. Firefox commits at the end of the task (and gating cost it
      // late frames). A hidden page gets no animation frames: after a timeout the guest
      // resumes at once until one comes again.
      const chromium = typeof requestAnimationFrame === 'function' && !!self.navigator?.userAgentData;
      let gated = chromium;
      // A game faster than the display (a game speed of 75 or 120 on a 60 Hz screen) ran
      // at the display's rate, one frame per animation frame. Its frames due before the
      // next display frame run at once, without yielding: the one just presented would
      // not be shown, and committing it held the next one back. The display's period is
      // the shortest of the last 16 gaps between animation frames.
      let lastFrameAt = 0;
      let displayStep = 0;
      const gaps = [];
      const noteFrame = (t) => {
        if (lastFrameAt && t - lastFrameAt > 2) {
          gaps.push(t - lastFrameAt);
          if (gaps.length > 16) gaps.shift();
          if (gaps.length >= 4) displayStep = Math.min(...gaps);
        }
        lastFrameAt = t;
      };
      const runNow = () => {
        if (!displayStep || frameStep > displayStep - 1) return false;
        const now = performance.now();
        const nextDisplay = lastFrameAt + Math.ceil((now - lastFrameAt) / displayStep) * displayStep;
        return due(shownClock + frameStep) < nextDisplay - 2;
      };
      // (frames in a row run without yielding, at most 4: input still arrives)
      let unshown = 0;
      const resume = () => {
        if (gated && unshown < 4 && runNow()) {
          unshown++;
          return wakeUp();
        }
        unshown = 0;
        if (!gated) return channel.port2.postMessage(0);
        let done = false;
        const go = (t) => {
          if (done) return;
          done = true;
          if (t !== undefined) noteFrame(t);
          channel.port2.postMessage(0);
        };
        requestAnimationFrame(go);
        setTimeout(() => {
          if (done) return;
          gated = false;
          requestAnimationFrame(() => (gated = true));
          go();
        }, 100);
      };
      gpuImports.gpu_present = new WebAssembly.Suspending((clockMs, x, y, w, h, dw, dh) => {
        if (clockMs > shownClock) {
          steps.push(Math.min(100, clockMs - shownClock));
          if (steps.length > 8) steps.shift();
          frameStep = Math.min(...steps);
        }
        shownClock = clockMs;
        const frame = show(clockMs, x, y, w, h, dw, dh, guestTime());
        const r = onFrame ? onFrame(frame, gpu) : undefined;
        return Promise.resolve(r).then(
          () =>
            new Promise((res) => {
              wakeUp = res;
              resume();
            }),
        ).then(() => {
          resumed = performance.now();
        });
      });
    } else if (pace && pacing === 'clock') {
      if (ahead) gpuImports.gpu_slack = (clockMs) => due(clockMs) - performance.now();
      gpuImports.gpu_pace = new WebAssembly.Suspending((clockMs) => {
        const at = due(clockMs);
        // within a millisecond: carry on without suspending
        if (at - performance.now() < 1) return undefined;
        return wake(at).then(() => {
          resumed = performance.now();
        });
      });
      gpuImports.gpu_present = new WebAssembly.Suspending((clockMs, x, y, w, h, dw, dh) => {
        const frame = show(clockMs, x, y, w, h, dw, dh, guestTime());
        // the frame reaches the screen at the next wait; captures (tests) are awaited
        const r = onFrame ? onFrame(frame, gpu) : undefined;
        return r && typeof r.then === 'function' ? r.then(() => (resumed = performance.now(), undefined)) : undefined;
      });
    } else {
      gpuImports.gpu_present = new WebAssembly.Suspending(async (clockMs, x, y, w, h, dw, dh) => {
        const busy = guestTime();
        const step = prevClock === undefined ? 20 : Math.max(1, clockMs - prevClock);
        prevClock = clockMs;
        const at = pace ? due(clockMs) : 0;
        if (pace && pacing === 'jit') await wake(at);
        const frame = show(clockMs, x, y, w, h, dw, dh, busy);
        if (onFrame) await onFrame(frame, gpu);
        // Yield even when not waiting: the canvas shows the frame and input arrives.
        const resumeAt = !pace ? 0 : pacing === 'jit' ? at + step - Math.min(step, estimate() + MARGIN_MS) : at;
        await wake(resumeAt);
        resumed = performance.now();
      });
    }
  }

  // Key events the page writes to a shared ring as they happen (see index.html):
  // read at the guest's every message poll, also while the worker blocks in a wait.
  const keyRing = inputRing && { idx: new Int32Array(inputRing, 0, 2), ev: new Int32Array(inputRing, 8, 256 * 3), when: new Float64Array(inputRing, 8 + 256 * 12, 256) };
  let systemDll = null;
  // The save or load asked for: {resolve, reject, state (a load's)}.
  let stateWait = null;
  const guestBytes = () => new Uint8Array(glue.exports.mem.buffer);
  // Save states' helper threads (`stateThreads`): all, and those that have started.
  const stateThreadsAll = [];
  const stateHelpers = [];
  // The page numbers a save finds (kept for the next).
  let stateList = null;
  // The helpers map `bytes` of fresh buffer `buf` while this thread copies into it, a chunk
  // at a time: returns what to call before writing into chunk c (it takes those no helper
  // has, or waits for the helper's), or null without helpers.
  const mapAhead = (buf, bytes) => {
    if (!stateHelpers.length) return null;
    const n = Math.ceil(bytes / MAP_CHUNK);
    const st = new Int32Array(new SharedArrayBuffer(4 * Math.max(1, n)));
    for (const h of stateHelpers) h.postMessage({ buf, states: st.buffer, n, chunk: MAP_CHUNK });
    return (c) => {
      if (Atomics.compareExchange(st, c, 0, 2) === 1) while (Atomics.load(st, c) === 1) Atomics.wait(st, c, 1);
    };
  };
  const host = {
    // A dialog for the player (hle Host::prompt: GameMaker 8's message, question and text
    // dialogs, I wanna play online's input_box): `prompt(title, text, default | null, kind)`
    // (kind 0 a message, 1 text, 2 a yes/no question, 3 a game error: an answer ignores it,
    // null aborts the game) answers synchronously (worker.mjs
    // waits for the page), a string or null when cancelled. Without one it is cancelled.
    prompt(tPtr, tLen, xPtr, xLen, dPtr, dLen, kind, out, cap) {
      const str = (p, n) => dec.decode(mem().subarray(p, p + n));
      const title = str(tPtr, tLen);
      const text = str(xPtr, xLen);
      const answer = prompt ? prompt(title, text, kind === 1 ? str(dPtr, dLen) : null, kind) : null;
      if (!prompt) print(`prompt [${title}]: ${text}`);
      if (answer == null) return -1;
      const bytes = new TextEncoder().encode(answer);
      mem().set(bytes.subarray(0, cap), out);
      return bytes.length;
    },
    // A system DLL the import did not supply (hle Host::system_dll: one a DLL the game
    // writes at run time imports): the player's system32/ copy, fetched synchronously
    // (a worker may), its length or -1; system_dll_copy then writes it.
    system_dll_len(p, n) {
      const name = dec.decode(mem().subarray(p, p + n));
      try {
        const x = new XMLHttpRequest();
        x.open('GET', asset(`system32/${encodeURIComponent(name)}`), false);
        x.responseType = 'arraybuffer';
        x.send();
        if (x.status !== 200) return -1;
        // (a site that answers a missing file with its page, status 200: not a DLL)
        const bytes = new Uint8Array(x.response);
        if (bytes.length < 2 || bytes[0] !== 0x4d || bytes[1] !== 0x5a) return -1;
        systemDll = bytes;
        return systemDll.length;
      } catch {
        return -1;
      }
    },
    system_dll_copy(out) {
      mem().set(systemDll, out);
      systemDll = null;
    },
    input_drain() {
      if (!keyRing) return;
      const w = Atomics.load(keyRing.idx, 0);
      for (let r = Atomics.load(keyRing.idx, 1); r !== w; r = (r + 1) | 0) {
        const k = (r & 255) * 3;
        const down = keyRing.ev[k + 2];
        if (down) pendingInputs.push(keyRing.when[r & 255]);
        rt.exports.input_key(keyRing.ev[k], keyRing.ev[k + 1], down);
      }
      Atomics.store(keyRing.idx, 1, w);
    },
    // Save states (crates/web state_*, runtime::state): {meta, pages, chunks, frame}: the
    // runtime's part, the numbers of the guest memory's non-zero 4 KB pages over the ranges
    // it names, their bytes (in chunks), and the frame shown.
    state_saved(metaPtr, metaLen, rangesPtr, n) {
      const meta = mem().slice(metaPtr, metaPtr + metaLen);
      const ranges = new Uint32Array(rt.exports.memory.buffer, rangesPtr, 2 * n).slice();
      const g = guestBytes();
      // [first, end) page numbers
      const spans = [];
      let total = 0;
      for (let r = 0; r < n; r++) {
        const start = ranges[2 * r] >>> 0;
        const end = Math.min(start + (ranges[2 * r + 1] >>> 0), g.length);
        if (end <= start) continue;
        spans.push([Math.floor(start / 4096), Math.ceil(end / 4096)]);
        total += spans.at(-1)[1] - spans.at(-1)[0];
      }
      // The non-zero pages are found, then copied (pages in a row at once) into one buffer:
      // that of the state this one replaces where it is big enough, as writing into fresh
      // memory cost three times as much (each of its pages is mapped as it is first
      // written; the helpers map a fresh one's while this thread copies). Fresh ones are
      // shared memory: the store's worker compresses them while the game's keeps them to load.
      if (!stateList || stateList.length < total) stateList = new Uint32Array(Math.ceil(total * 1.125));
      const list = stateList;
      let count = 0;
      const zpage = glue.exports.zpage;
      for (const [first, end] of spans) {
        for (let p = zpage(first, end, 0); p < end; p = zpage(p, end, 0)) {
          const q = zpage(p, end, 1);
          while (p < q) list[count++] = p++;
        }
      }
      const bytes = count * 4096;
      const spare = Array.isArray(stateWait?.state) ? stateWait.state : [];
      let buf = spare.find((b) => b instanceof SharedArrayBuffer && b.byteLength >= bytes);
      let mapped = null;
      if (!buf) {
        buf = new SharedArrayBuffer(Math.max(4096, Math.ceil((bytes * 1.125) / 4096) * 4096));
        mapped = mapAhead(buf, bytes);
      }
      const out = new Uint8Array(buf, 0, bytes);
      let off = 0;
      for (let i = 0; i < count; ) {
        let j = i + 1;
        while (j < count && list[j] === list[j - 1] + 1) j++;
        let at = list[i] * 4096;
        const end = list[j - 1] * 4096 + 4096;
        while (at < end) {
          const n = mapped ? Math.min(end - at, MAP_CHUNK - (off % MAP_CHUNK)) : end - at;
          mapped?.(Math.floor(off / MAP_CHUNK));
          out.set(g.subarray(at, at + n), off);
          at += n;
          off += n;
        }
        i = j;
      }
      const st = { meta, pages: list.slice(0, count), chunks: [out], frame: gpu?.grab() ?? null };
      const w = stateWait;
      stateWait = null;
      w?.resolve(st);
    },
    state_meta_len() {
      return stateWait?.state ? stateWait.state.meta.length : -1;
    },
    state_meta_copy(out) {
      mem().set(stateWait.state.meta, out);
    },
    state_memory(clearPtr, n) {
      const clear = new Uint32Array(rt.exports.memory.buffer, clearPtr, 2 * n).slice();
      const g = guestBytes();
      const { pages, chunks } = stateWait.state;
      // What the guest used is zeroed where the state has no page (its pages, in address
      // order as the ranges are, go over the rest).
      let q = 0;
      for (let r = 0; r < n; r++) {
        let at = clear[2 * r] >>> 0;
        const end = Math.min(at + (clear[2 * r + 1] >>> 0), g.length);
        while (at < end) {
          while (q < pages.length && pages[q] * 4096 + 4096 <= at) q++;
          const next = q < pages.length ? pages[q] * 4096 : end;
          if (next > at) g.fill(0, at, Math.min(next, end));
          at = Math.max(at, next + 4096);
        }
      }
      // (pages in a row, from one chunk, at once)
      let c = 0;
      let off = 0;
      for (let i = 0; i < pages.length; ) {
        while (off >= chunks[c].length) {
          c++;
          off = 0;
        }
        let j = i + 1;
        const most = i + (chunks[c].length - off) / 4096;
        while (j < most && j < pages.length && pages[j] === pages[j - 1] + 1) j++;
        const at = pages[i] * 4096;
        const len = (j - i) * 4096;
        if (at + len > g.length) return 0;
        g.set(chunks[c].subarray(off, off + len), at);
        off += len;
        i = j;
      }
      return 1;
    },
    state_loaded(clockMs) {
      const anchor = () => {
        startReal = performance.now() - clockMs;
        prevClock = clockMs;
        lastClockMs = clockMs;
        clockMoved(clockMs);
      };
      anchor();
      const w = stateWait;
      stateWait = null;
      w?.resolve();
      // A standby copy (ctl.standby, player.mjs) stops here, at the state, until it takes
      // over (or loads another): the guest has run nothing of it yet. Its clock then
      // goes on from when it resumes.
      if (ctl?.standby && jspi) return new Promise((res) => (ctl.release = () => (anchor(), res())));
    },
    state_failed(ptr, len) {
      const w = stateWait;
      stateWait = null;
      w?.reject(new Error(dec.decode(mem().subarray(ptr, ptr + len))));
    },
    print(ptr, len) {
      print(dec.decode(mem().subarray(ptr, ptr + len)));
    },
    now_us() {
      return performance.now() * 1000;
    },
    soundfont_request() {
      fetch(soundfontUrl)
        .then((r) => (r.ok ? r.arrayBuffer() : Promise.reject(new Error(`HTTP ${r.status}`))))
        .then((buf) => {
          const b = new Uint8Array(buf);
          const p = rt.exports.alloc(b.length);
          mem().set(b, p);
          rt.exports.soundfont_add(p, b.length);
          print(`MIDI SoundFont loaded (${(b.length / 1048576).toFixed(1)} MB)`);
        })
        .catch((e) => print(`no MIDI SoundFont (${e.message ?? e}): MIDI music stays silent`));
    },
    fs_persist(ptr, len) {
      if (!onSave) return;
      const view = new DataView(rt.exports.memory.buffer, ptr, len);
      const bytes = mem();
      const changes = [];
      for (let o = 0; o < len; ) {
        const n = view.getUint32(o, true);
        const path = dec.decode(bytes.subarray(ptr + o + 4, ptr + o + 4 + n));
        o += 4 + n;
        const size = view.getInt32(o, true);
        o += 4;
        const data = size < 0 ? null : bytes.slice(ptr + o, ptr + o + size);
        o += Math.max(0, size);
        const mtime = view.getFloat64(o, true);
        o += 8;
        changes.push({ path, data, mtime });
      }
      onSave(changes);
    },
    instantiate(ptr, len, namesPtr, namesLen, out) {
      try {
        const t0 = performance.now();
        // (a view: the module takes its own copy, so no garbage is left here)
        const bytes = mem().subarray(ptr, ptr + len);
        const names = dec.decode(mem().subarray(namesPtr, namesPtr + namesLen)).split('\n');
        const module = new WebAssembly.Module(bytes);
        const t1 = performance.now();
        const inst = new WebAssembly.Instance(module, { cpu: cpuImports, rt: rtImports, x87: x87Imports });
        compileMs += t1 - t0;
        instantiateMs += performance.now() - t1;
        moduleBytes += len;
        const base = table.grow(names.length);
        const o = new Uint32Array(rt.exports.memory.buffer, out, names.length);
        names.forEach((n, i) => {
          table.set(base + i, inst.exports[n]);
          o[i] = base + i;
        });
        modules++;
        return 1;
      } catch (e) {
        print(`instantiate failed: ${e}`);
        return 0;
      }
    },
    table_copy(dst, src) {
      try {
        table.set(dst, table.get(src));
        return 1;
      } catch (e) {
        print(`table_copy failed: ${e}`);
        return 0;
      }
    },
  };

  // Sound: the runtime pushes finished mix samples; they go into the ring the page's
  // AudioWorklet plays (dropped when the ring is full, e.g. before audio started).
  const ring = audioRing && { idx: new Int32Array(audioRing.sab, 0, 2), data: new Float32Array(audioRing.sab, 8, audioRing.frames * 2), frames: audioRing.frames };
  const audioImports = {
    audio_push(ptr, len) {
      if (!ring || ctl?.muted) return;
      const src = new Float32Array(rt.exports.memory.buffer, ptr, len);
      const n = len >> 1;
      const w = Atomics.load(ring.idx, 0);
      const rd = Atomics.load(ring.idx, 1);
      const free = ring.frames - (w - rd);
      const take = Math.min(n, free);
      for (let i = 0; i < take; i++) {
        const k = ((w + i) % ring.frames) * 2;
        ring.data[k] = src[2 * i];
        ring.data[k + 1] = src[2 * i + 1];
      }
      Atomics.store(ring.idx, 0, w + take);
    },
  };

  // Guest sockets (hle/winsock.rs): each connection a WebSocket to the relay
  // (tools/relay.mjs), which makes the TCP connection or sends the UDP datagrams;
  // nothing connects without `relayUrl`.
  const conns = new Map(); // id -> {ws, udp, state (0 connecting, 1 open, 2 closed, 3 failed), chunks, pending}
  const netImports = {
    net_enabled: () => (relayUrl ? 1 : 0),
    net_open(id, udp, hostPtr, hostLen, port) {
      if (!relayUrl) return 0;
      const url = new URL(relayUrl, location.href);
      url.searchParams.set('proto', udp ? 'udp' : 'tcp');
      url.searchParams.set('host', dec.decode(mem().subarray(hostPtr, hostPtr + hostLen)));
      url.searchParams.set('port', String(port));
      let ws;
      try {
        ws = new WebSocket(url);
      } catch (e) {
        print(`relay ${url}: ${e.message}`);
        return 0;
      }
      ws.binaryType = 'arraybuffer';
      const c = { ws, udp: !!udp, state: 0, chunks: [], pending: [] };
      ws.onopen = () => {
        c.state = 1;
        for (const d of c.pending) ws.send(d);
        c.pending = [];
      };
      ws.onmessage = (e) => c.chunks.push(new Uint8Array(e.data));
      ws.onclose = () => (c.state = c.state === 0 ? 3 : 2);
      ws.onerror = () => c.state === 0 && (c.state = 3);
      conns.set(id, c);
      return 1;
    },
    net_send(id, ptr, len) {
      const c = conns.get(id);
      if (!c || c.state >= 2) return 0;
      const d = mem().slice(ptr, ptr + len);
      if (c.state === 0) c.pending.push(d);
      else c.ws.send(d);
      return 1;
    },
    net_recv(id, ptr, len) {
      const c = conns.get(id);
      if (!c) return 0;
      if (!c.chunks.length) return c.state >= 2 && !c.udp ? 0 : -1;
      if (c.udp) {
        const d = c.chunks.shift();
        const n = Math.min(len, d.length);
        mem().set(d.subarray(0, n), ptr);
        return n;
      }
      let n = 0;
      while (n < len && c.chunks.length) {
        const d = c.chunks[0];
        const take = Math.min(len - n, d.length);
        mem().set(d.subarray(0, take), ptr + n);
        n += take;
        if (take === d.length) c.chunks.shift();
        else c.chunks[0] = d.subarray(take);
      }
      return n;
    },
    net_available(id) {
      const c = conns.get(id);
      if (!c || !c.chunks.length) return 0;
      return c.udp ? Math.max(1, c.chunks[0].length) : c.chunks.reduce((a, d) => a + d.length, 0);
    },
    net_state(id) {
      return conns.get(id)?.state ?? 3;
    },
    net_close(id) {
      const c = conns.get(id);
      if (!c) return;
      try {
        c.ws.close();
      } catch {}
      conns.delete(id);
    },
  };

  // (a standby copy waits in state_loaded: the import may suspend)
  if (ctl && jspi) host.state_loaded = new WebAssembly.Suspending(host.state_loaded);
  const engineImports = { host, gpu: gpuImports, audio: audioImports, net: netImports };
  rt = runtimeBytes instanceof WebAssembly.Module ? await WebAssembly.instantiate(runtimeBytes, engineImports) : (await WebAssembly.instantiate(runtimeBytes, engineImports)).instance;
  const x = rt.exports;
  table = x.__indirect_function_table;

  // The cpu glue module: guest memory, CPU-state globals and their accessors.
  const glueLen = x.glue_build(1024);
  const glueBytes = mem().slice(x.glue_ptr(), x.glue_ptr() + glueLen);
  glue = new WebAssembly.Instance(new WebAssembly.Module(glueBytes), { rt: { memory: x.memory, table } });
  const glueBase = table.grow(GLUE_FUNCS.length);
  GLUE_FUNCS.forEach((n, i) => table.set(glueBase + i, glue.exports[n]));
  x.glue_install(glueBase);
  cpuImports = { fns: table };
  for (const [name, v] of Object.entries(glue.exports)) {
    if (!GLUE_FUNCS.includes(name)) cpuImports[name] = v;
  }
  rtImports = { resolve: x.rt_resolve, trap: x.rt_trap, fpu: x.rt_fpu, cpuid: x.rt_cpuid, rdtsc: x.rt_rdtsc };

  // The x87 fast paths translated modules import.
  const x87Len = x.x87_build();
  x87Imports = new WebAssembly.Instance(new WebAssembly.Module(mem().slice(x.glue_ptr(), x.glue_ptr() + x87Len)), {
    cpu: cpuImports,
    rt: { sin: x.x87_sin, cos: x.x87_cos },
  }).exports;

  // The no-op function `resolve` returns after an API call.
  const nopLen = x.nop_build();
  const nop = new WebAssembly.Instance(new WebAssembly.Module(mem().slice(x.glue_ptr(), x.glue_ptr() + nopLen)), {});
  const nopIndex = table.grow(1);
  table.set(nopIndex, nop.exports.nop);

  let total = 0;
  for (const [path, data, mtime = 0] of files) {
    const p = enc.encode(path.replaceAll('/', '\\'));
    const pp = x.alloc(p.length);
    mem().set(p, pp);
    const dp = x.alloc(data.length);
    mem().set(data, dp);
    x.fs_add(pp, p.length, dp, data.length, mtime);
    x.dealloc(pp, p.length);
    x.dealloc(dp, data.length);
    total += data.length;
  }
  print(`${files.length} files, ${(total / 1048576).toFixed(1)} MB`);

  const e = enc.encode(exe);
  const ep = x.alloc(e.length);
  mem().set(e, ep);
  if (canvas) gpu = new D3DGL(canvas, () => glue.exports.mem.buffer, () => x.memory.buffer, print);
  const stateAsk = (op, state) =>
    new Promise((resolve, reject) => {
      stateWait?.reject(new Error('superseded'));
      stateWait = { resolve, reject, state };
      x.state_request(op);
    });
  if (onReady) {
    onReady({
      key: (vk, scan, down, when) => {
        if (when && down) pendingInputs.push(when);
        x.input_key(vk, scan, down ? 1 : 0);
      },
      mouse: (mx, my, buttons, when) => {
        if (when) pendingInputs.push(when);
        x.input_mouse(mx | 0, my | 0, buttons);
      },
      pad: (i, connected, buttons, lt, rt, lx, ly, rx, ry) => x.input_pad(i, connected ? 1 : 0, buttons, lt | (rt << 8), lx, ly, rx, ry),
      // Files another copy of the game wrote ([{path (as stored), data: Uint8Array | null}]),
      // written at the guest's next input poll.
      files: (changes) => {
        for (const { path, data } of changes) {
          const p = enc.encode(path);
          const pp = x.alloc(p.length);
          mem().set(p, pp);
          const dp = data ? x.alloc(data.length) : 0;
          if (data) mem().set(data, dp);
          x.fs_put(pp, p.length, dp, data ? data.length : -1);
        }
      },
      // Save states, made and loaded at the end of the game's next frame: `save()` resolves to
      // a state (see `state_saved`), `load(state)` once the game runs from it; either rejects
      // with why not. One at a time: a new one cancels the one waiting.
      state: {
        save: (spare) => stateAsk(1, spare ?? []),
        // (a standby copy uploads its textures before it waits at the state)
        load: (state) => stateAsk(ctl?.standby ? 3 : 2, state),
        cancel: () => {
          x.state_request(0);
          const w = stateWait;
          stateWait = null;
          w?.reject(new Error('cancelled'));
        },
      },
    });
  }
  if (x.init(ep, e.length, frames, trace ? 1 : 0, epoch, nopIndex, gpu ? 1 : 0) !== 0) {
    throw new Error('init failed');
  }
  // (texture content stamps unlike any other copy's: runtime::state keeps textures by them)
  x.set_content_seed(crypto.getRandomValues(new Uint32Array(1))[0] || 1);
  if (monitor) x.set_monitor(1);
  for (let i = 0; i < stateThreads; i++) {
    const h = new Worker(new URL('./state-map.mjs', import.meta.url), { type: 'module' });
    h.onmessage = () => stateHelpers.push(h);
    stateThreadsAll.push(h);
  }
  // [kind, frame, clock, a, b, c]: a recorded session's input
  for (const [kind, f, t, a, b, c] of replay ?? []) x.replay_push(kind, f, t, a, b, c);
  for (let i = 0, n = x.module_count(); i < n; i++) startModules.push({ base: x.module_at(i, 0), size: x.module_at(i, 1), id: x.module_at(i, 2) >>> 0 });
  if (!profile && sharedProfile) profile = await sharedProfile(startModules);
  if (profile) for (let i = profile.length - 3; i >= 0; i -= 3) x.profile_push(profile[i], profile[i + 1], profile[i + 2]);
  const status = canvas && jspi ? await WebAssembly.promising(x.run)() : x.run();
  flushProfile();
  if (gpu) print(`draw calls: ${(gpuMs / 1000).toFixed(2)}s in all, ${(gpuMs / Math.max(1, gpu.frames)).toFixed(2)} ms per frame; ${(gpu.draws / Math.max(1, gpu.frames)).toFixed(1)} draws and ${(gpu.states / Math.max(1, gpu.frames)).toFixed(1)} state changes per frame`);
  if (slackStats.n) print(`slack: ${slackStats.n} asks, estimate ${(slackStats.est / slackStats.n).toFixed(2)} ms, frame left ${(slackStats.left / slackStats.n).toFixed(2)} ms`);
  print(`modules instantiated: ${modules} (${(moduleBytes / 1048576).toFixed(1)} MB; compile ${(compileMs / 1000).toFixed(2)}s, instantiate ${(instantiateMs / 1000).toFixed(2)}s)`);
  if (frameMs.length > 60) {
    // Steady state: skip the loading frames.
    const tail = frameMs.slice(Math.floor(frameMs.length / 2)).sort((a, b) => a - b);
    const q = (p) => tail[Math.min(tail.length - 1, Math.floor(p * tail.length))].toFixed(2);
    print(`frame cost (ms, second half of the run): median ${q(0.5)}  p95 ${q(0.95)}  p99 ${q(0.99)}  max ${q(1)}`);
  }
  for (const h of stateThreadsAll) h.terminate();
  if (onExit) onExit({ guestMemory: glue.exports.mem });
  return status;
}
