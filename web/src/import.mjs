// Importing a game in the browser from its archive (zip, 7z or RAR, as games are stored):
// webimport.wasm (crates/webimport) unpacks it, unwraps an IExpress package, picks the
// entry exe and lists the system DLLs to add; those come from /system32/ (see
// tools/system32.sh). Imported games live in IndexedDB (saves.mjs `storeGame`) and
// play from there (?game=<slug>&local).

let modulePromise;

function instantiate() {
  modulePromise ??= WebAssembly.compileStreaming(fetch('/import/webimport.wasm'));
  return modulePromise.then((mod) => {
    // (wasm-bindgen placeholders a dependency brings along; nothing calls them)
    const imports = {};
    for (const imp of WebAssembly.Module.imports(mod)) {
      imports[imp.module] ??= {};
      imports[imp.module][imp.name] = () => 0;
    }
    return new WebAssembly.Instance(mod, imports);
  });
}

/**
 * @param {Uint8Array} bytes  the archive
 * @param {string} name  its file name (the title when it has no single top folder)
 * @param {(text: string) => void} [progress]
 * @returns {Promise<{manifest: object, files: [string, Uint8Array][]}>}
 */
export async function importArchive(bytes, name, progress = () => {}) {
  const inst = await instantiate();
  const x = inst.exports;
  const mem = () => new Uint8Array(x.memory.buffer);
  const put = (u8) => {
    const p = x.alloc(u8.length);
    mem().set(u8, p);
    return [p, u8.length];
  };
  const out = () => new TextDecoder().decode(mem().slice(x.out_ptr(), x.out_ptr() + x.out_len()));
  progress(`unpacking ${name} (${(bytes.length / 1048576).toFixed(1)} MB)`);
  const [ap, al] = put(bytes);
  const [np, nl] = put(new TextEncoder().encode(name));
  const failed = x.import_zip(ap, al, np, nl);
  x.dealloc(ap, al);
  x.dealloc(np, nl);
  if (failed) throw new Error(out());
  // system DLLs, until nothing new can be had (they may need others)
  const tried = new Set();
  for (;;) {
    x.needs();
    const want = JSON.parse(out()).filter((n) => !tried.has(n));
    if (!want.length) break;
    for (const dll of want) {
      tried.add(dll);
      const r = await fetch(`/system32/${encodeURIComponent(dll)}`);
      if (!r.ok) continue;
      progress(`adding ${dll}`);
      const [pp, pl] = put(new TextEncoder().encode(`C/Windows/system32/${dll}`));
      const [dp, dl] = put(new Uint8Array(await r.arrayBuffer()));
      x.add_file(pp, pl, dp, dl);
      x.dealloc(pp, pl);
      x.dealloc(dp, dl);
    }
  }
  x.manifest();
  const manifest = JSON.parse(out());
  const files = [];
  for (let i = 0; i < x.file_count(); i++) {
    x.file_path(i);
    const path = out();
    const p = x.file_ptr(i);
    files.push([path, mem().slice(p, p + x.file_len(i))]);
  }
  x.finish();
  if (!manifest.entry) throw new Error('no exe found in the archive');
  return { manifest, files };
}
