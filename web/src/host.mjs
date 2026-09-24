// Host side of the browser runtime (crates/web): instantiates the engine, its cpu glue
// module and translated modules, and hands it the game files. Runs unchanged in a
// browser worker and in Node.

import { D3DGL } from './gpu.mjs';

const GLUE_FUNCS = ['g2r', 'r2g', 'fill', 'get', 'set', 'size', 'grow', 'vget', 'vset', 'call'];
const jspi = typeof WebAssembly.Suspending === 'function' && typeof WebAssembly.promising === 'function';

/**
 * @param {object} o
 * @param {BufferSource} o.runtimeBytes  web.wasm
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
 */
export async function runGame({ runtimeBytes, files, exe, frames = 0, trace = false, epoch = Math.floor(Date.now() / 1000), print = console.log, onExit, canvas, pace = true, pacing = 'clock', ahead = true, onFrame, onReady, onResize, audioRing, onSave, soundfontUrl = '/soundfont.sf2', onStats, monitor = false, onSlow, replay }) {
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
      // is the one being presented)
      if (onSlow && ((busy ?? 0) > 12 || (target > 0 && interval > target + 4))) onSlow(gpu.frames + 1, busy ?? 0, interval, clockMs);
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
  const due = (clockMs) => {
    const now = performance.now();
    // more than 250 ms behind (a long load, a hidden tab): start over from now
    if (startReal === undefined || now - (startReal + clockMs) > 250) startReal = now - clockMs;
    return startReal + clockMs;
  };
  const show = (clockMs, x, y, w, h, dw, dh, busy) => {
    collect(performance.now(), clockMs, busy);
    const resized = canvas.width !== dw || canvas.height !== dh;
    gpu.present(x, y, w, h, dw, dh);
    if (resized && onResize) onResize(dw, dh);
    return gpu.frames;
  };
  // Recent guest time per frame, for the next frame's start.
  const recent = [];
  const estimate = () => {
    if (!recent.length) return 4;
    const s = [...recent].sort((a, b) => a - b);
    return s[Math.floor(s.length * 0.9)];
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
    gpu_texture_upload: (id, level, w, h, fmt, pitch, bits) => gpu?.textureUpload(id, level, w, h, fmt, pitch, bits),
    gpu_texture_delete: (id) => gpu?.textureDelete(id),
    gpu_set_target: (id, w, h) => gpu?.setTarget(id, w, h),
    gpu_clear: (flags, color, z, stencil, x, y, w, h) => gpu?.clear(flags, color >>> 0, z, stencil, x, y, w, h),
    gpu_draw: (ptr) => gpu?.draw(ptr),
    gpu_draw9: (ptr) => gpu?.draw9(ptr),
    gpu_shader_create: (id, vertex, ptr, len) => gpu?.shaderCreate(id, !!vertex, dec.decode(mem().subarray(ptr, ptr + len))),
    gpu_shader_delete: (id) => gpu?.shaderDelete(id),
    gpu_blit: (src, sx, sy, sw, sh, dst, dx, dy, dw, dh, linear) => gpu?.blit(src, sx, sy, sw, sh, dst, dx, dy, dw, dh, !!linear),
    gpu_read_target: (id, w, h, dst, pitch) => gpu?.readTarget(id, w, h, dst, pitch),
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
      if (onStats && t > before) win.wake.push(performance.now() - t);
    };
    const guestTime = () => {
      const busy = resumed ? performance.now() - resumed : undefined;
      if (busy !== undefined) {
        frameMs.push(busy);
        recent.push(busy);
        if (recent.length > 60) recent.shift();
      }
      return busy;
    };
    if (pace && pacing === 'clock') {
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

  const host = {
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
        const bytes = mem().slice(ptr, ptr + len);
        const names = dec.decode(mem().subarray(namesPtr, namesPtr + namesLen)).split('\n');
        const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes), { cpu: cpuImports, rt: rtImports, x87: x87Imports });
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
  };

  // Sound: the runtime pushes finished mix samples; they go into the ring the page's
  // AudioWorklet plays (dropped when the ring is full, e.g. before audio started).
  const ring = audioRing && { idx: new Int32Array(audioRing.sab, 0, 2), data: new Float32Array(audioRing.sab, 8, audioRing.frames * 2), frames: audioRing.frames };
  const audioImports = {
    audio_push(ptr, len) {
      if (!ring) return;
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

  ({ instance: rt } = await WebAssembly.instantiate(runtimeBytes, { host, gpu: gpuImports, audio: audioImports }));
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
    });
  }
  if (x.init(ep, e.length, frames, trace ? 1 : 0, epoch, nopIndex, gpu ? 1 : 0) !== 0) {
    throw new Error('init failed');
  }
  if (monitor) x.set_monitor(1);
  // [kind, frame, clock, a, b, c]: a recorded session's input
  for (const [kind, f, t, a, b, c] of replay ?? []) x.replay_push(kind, f, t, a, b, c);
  const status = canvas && jspi ? await WebAssembly.promising(x.run)() : x.run();
  print(`modules instantiated: ${modules}`);
  if (frameMs.length > 60) {
    // Steady state: skip the loading frames.
    const tail = frameMs.slice(Math.floor(frameMs.length / 2)).sort((a, b) => a - b);
    const q = (p) => tail[Math.min(tail.length - 1, Math.floor(p * tail.length))].toFixed(2);
    print(`frame cost (ms, second half of the run): median ${q(0.5)}  p95 ${q(0.95)}  p99 ${q(0.99)}  max ${q(1)}`);
  }
  if (onExit) onExit({ guestMemory: glue.exports.mem });
  return status;
}
