// Merges the per-module translation profiles of several runs (run-node.mjs --module-profiles)
// into the ones the page starts a game's first session from (web/public/profiles):
//   node web/tools/module-profiles.mjs <out dir> <run dir>...
// A function goes in once per module, with the first frame any run needed it in, except that
// it is translated before the start (frame 0) only when every run that needed it did so
// before its first frame: what one game needs at its start, another may never need.
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const [out, ...runs] = process.argv.slice(2);
const byModule = new Map(); // file name -> Map(address -> {hash, frame, early})
for (const run of runs) {
  for (const name of await readdir(run)) {
    if (!/^module-[0-9a-f]+-[0-9a-f]+\.bin$/.test(name)) continue;
    const b = await readFile(join(run, name));
    const e = new Uint32Array(b.buffer, b.byteOffset, b.byteLength >> 2);
    if (!byModule.has(name)) byModule.set(name, new Map());
    const m = byModule.get(name);
    for (let i = 0; i < e.length; i += 3) {
      const [addr, hash, frame] = [e[i], e[i + 1], e[i + 2]];
      const have = m.get(addr);
      if (!have) m.set(addr, { hash, frame: Math.max(frame, 1), early: frame === 0 });
      else if (have.hash === hash) {
        if (frame > 0) have.frame = Math.min(have.frame, frame);
        have.early &&= frame === 0;
      }
    }
  }
}
await mkdir(out, { recursive: true });
for (const [name, m] of byModule) {
  const rows = [...m].map(([addr, { hash, frame, early }]) => [addr, hash, early ? 0 : frame]).sort((a, b) => a[2] - b[2] || a[0] - b[0]);
  await writeFile(join(out, name), new Uint8Array(Uint32Array.from(rows.flat()).buffer));
  const early = rows.filter((r) => r[2] === 0).length;
  console.log(`${name}: ${rows.length} functions, ${early} before the start`);
}
