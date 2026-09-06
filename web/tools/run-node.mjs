// Runs an imported game with the browser runtime under Node:
//   node web/tools/run-node.mjs web/public/games/<slug> [--frames N] [--trace] [--epoch S]
//        [--key frame:vk:down ...] (as the harness: vk in hex, down 1/0, after that frame)
import { readFile, readdir, stat, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { runGame } from '../src/host.mjs';

const args = process.argv.slice(2);
const gameDir = args[0];
const opt = (name, dflt) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : dflt;
};
const frames = Number(opt('--frames', 300));
const trace = args.includes('--trace');
// The harness's fixed start date, so runs of both hosts can be compared.
const epoch = Number(opt('--epoch', 1767225600));

async function walk(dir, rel, out) {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const r = rel ? `${rel}\\${e.name}` : e.name;
    if (e.isDirectory()) await walk(join(dir, e.name), r, out);
    else out.push([r, new Uint8Array(await readFile(join(dir, e.name))), (await stat(join(dir, e.name))).mtimeMs]);
  }
  return out;
}

const manifest = JSON.parse(await readFile(join(gameDir, 'manifest.json'), 'utf8'));
const files = await walk(join(gameDir, 'files'), '', []);
const runtimeBytes = await readFile(new URL('../../target/wasm/wasm32-unknown-unknown/release/web.wasm', import.meta.url));
const t = performance.now();
const dumpMem = opt('--dump-mem', null);
let guest;
const keys = args.flatMap((a, i) => (a === '--key' ? [args[i + 1].split(':')] : [])).map(([f, vk, d]) => [Number(f), parseInt(vk, 16), d === '1']);
let input;
const onFrame = (frame) => {
  for (const [f, vk, down] of keys) if (f === frame) input.key(vk, 0, down);
};
const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, trace, epoch, onFrame, onReady: (i) => (input = i), onExit: (s) => (guest = s.guestMemory) });
if (dumpMem) await writeFile(dumpMem, new Uint8Array(guest.buffer));
console.log(`status ${status}, ${((performance.now() - t) / 1000).toFixed(1)}s total`);
process.exit(status);
