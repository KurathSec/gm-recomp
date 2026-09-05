// Host side of the browser runtime (crates/web): instantiates the engine, its cpu glue
// module and translated modules, and hands it the game files. Runs unchanged in a
// browser worker and in Node.

import { D3DGL } from './gpu.mjs';

const GLUE_FUNCS = ['g2r', 'r2g', 'fill', 'get', 'set', 'size', 'grow', 'vget', 'vset'];
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
 * @param {(frame: number, gpu: D3DGL) => void|Promise<void>} [o.onFrame]  after each Present
 * @param {(input: {key: Function, mouse: Function, pad: Function}) => void} [o.onReady]  input entry points,
 *   usable while the guest is suspended in Present
 * @param {(w: number, h: number) => void} [o.onResize]  the shown frame changed size
 * @param {{sab: SharedArrayBuffer, frames: number}} [o.audioRing]  where the sound mix goes
 * @param {(changes: {path: string, data: Uint8Array|null, mtime: number}[]) => void} [o.onSave]
 *   files the game wrote or removed (its saves; not the temp directory)
 */
export async function runGame({ runtimeBytes, files, exe, frames = 0, trace = false, epoch = Math.floor(Date.now() / 1000), print = console.log, onExit, canvas, pace = true, onFrame, onReady, onResize, audioRing, onSave }) {
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
  // Wall-clock cost of each frame (Present to Present), for the summary.
  const frameMs = [];
  let lastPresent = 0;
  const present = (clockMs, x, y, w, h, dw, dh) => {
    const t = performance.now();
    if (lastPresent) frameMs.push(t - lastPresent);
    const resized = canvas.width !== dw || canvas.height !== dh;
    gpu.present(x, y, w, h, dw, dh);
    if (resized && onResize) onResize(dw, dh);
    const frame = gpu.frames;
    // The guest runs on virtual time; hold each frame until real time catches up.
    const now = performance.now();
    if (startReal === undefined || now - (startReal + clockMs) > 250) startReal = now - clockMs;
    const wait = startReal + clockMs - now;
    lastPresent = performance.now() + Math.max(0, pace ? wait : 0);
    return { frame, wait };
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
    gpu_present: (clockMs, x, y, w, h, dw, dh) => {
      if (!gpu) return;
      const { frame } = present(clockMs, x, y, w, h, dw, dh);
      if (onFrame) onFrame(frame, gpu);
    },
  };
  if (canvas && jspi) {
    // Present suspends the guest: the worker's event loop runs, the canvas shows the
    // frame, and input messages arrive.
    gpuImports.gpu_present = new WebAssembly.Suspending(async (clockMs, x, y, w, h, dw, dh) => {
      const { frame, wait } = present(clockMs, x, y, w, h, dw, dh);
      if (onFrame) await onFrame(frame, gpu);
      await new Promise((r) => setTimeout(r, pace ? Math.max(0, wait) : 0));
    });
  }

  const host = {
    print(ptr, len) {
      print(dec.decode(mem().subarray(ptr, ptr + len)));
    },
    now_us() {
      return performance.now() * 1000;
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
  glue = new WebAssembly.Instance(new WebAssembly.Module(glueBytes), { rt: { memory: x.memory } });
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
  x87Imports = new WebAssembly.Instance(new WebAssembly.Module(mem().slice(x.glue_ptr(), x.glue_ptr() + x87Len)), { cpu: cpuImports }).exports;

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
      key: (vk, scan, down) => x.input_key(vk, scan, down ? 1 : 0),
      mouse: (mx, my, buttons) => x.input_mouse(mx | 0, my | 0, buttons),
      pad: (i, connected, buttons, lt, rt, lx, ly, rx, ry) => x.input_pad(i, connected ? 1 : 0, buttons, lt | (rt << 8), lx, ly, rx, ry),
    });
  }
  if (x.init(ep, e.length, frames, trace ? 1 : 0, epoch, nopIndex, gpu ? 1 : 0) !== 0) {
    throw new Error('init failed');
  }
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
