// Host side of the browser runtime (crates/web): instantiates the engine, its cpu glue
// module and translated modules, and hands it the game files. Runs unchanged in a
// browser worker and in Node.

const GLUE_FUNCS = ['g2r', 'r2g', 'fill', 'get', 'set', 'size', 'grow'];

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
 */
export async function runGame({ runtimeBytes, files, exe, frames = 0, trace = false, epoch = Math.floor(Date.now() / 1000), print = console.log, onExit }) {
  const dec = new TextDecoder();
  const enc = new TextEncoder();
  let rt;
  let table;
  let cpuImports;
  let rtImports;
  let modules = 0;
  const mem = () => new Uint8Array(rt.exports.memory.buffer);

  const host = {
    print(ptr, len) {
      print(dec.decode(mem().subarray(ptr, ptr + len)));
    },
    now_us() {
      return performance.now() * 1000;
    },
    instantiate(ptr, len, namesPtr, namesLen, out) {
      try {
        const bytes = mem().slice(ptr, ptr + len);
        const names = dec.decode(mem().subarray(namesPtr, namesPtr + namesLen)).split('\n');
        const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes), { cpu: cpuImports, rt: rtImports });
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

  ({ instance: rt } = await WebAssembly.instantiate(runtimeBytes, { host }));
  const x = rt.exports;
  table = x.__indirect_function_table;

  // The cpu glue module: guest memory, CPU-state globals and their accessors.
  const glueLen = x.glue_build(1024);
  const glueBytes = mem().slice(x.glue_ptr(), x.glue_ptr() + glueLen);
  const glue = new WebAssembly.Instance(new WebAssembly.Module(glueBytes), { rt: { memory: x.memory } });
  const glueBase = table.grow(GLUE_FUNCS.length);
  GLUE_FUNCS.forEach((n, i) => table.set(glueBase + i, glue.exports[n]));
  x.glue_install(glueBase);
  cpuImports = { fns: table };
  for (const [name, v] of Object.entries(glue.exports)) {
    if (!GLUE_FUNCS.includes(name)) cpuImports[name] = v;
  }
  rtImports = { resolve: x.rt_resolve, trap: x.rt_trap, fpu: x.rt_fpu, cpuid: x.rt_cpuid, rdtsc: x.rt_rdtsc };

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
  if (x.init(ep, e.length, frames, trace ? 1 : 0, epoch, nopIndex) !== 0) {
    throw new Error('init failed');
  }
  const status = x.run();
  print(`modules instantiated: ${modules}`);
  if (onExit) onExit({ guestMemory: glue.exports.mem });
  return status;
}
