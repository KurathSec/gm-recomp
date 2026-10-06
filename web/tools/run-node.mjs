// Runs an imported game with the browser runtime under Node:
//   node web/tools/run-node.mjs web/public/games/<slug> [--frames N] [--trace] [--epoch S]
//        [--key frame:vk:down ...] (as the harness: vk in hex, down 1/0, after that frame)
//        [--replay target/monitor/<session>.log] (a session the page recorded: its date,
//        saves and input; `node --cpu-prof` then profiles it)
//        [--profile from:to [--profile-out file.cpuprofile]] (a CPU profile of these frames only)
//        [--translation-profile file] (read, then extended with what this run needed)
import { readFile, readdir, stat, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { writeFileSync } from 'node:fs';
import inspector from 'node:inspector';
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
let epoch = Number(opt('--epoch', 1767225600));
// a recorded session: date, saves (file path -> bytes or deleted), input
const replayLog = opt('--replay', null);
const replay = { saves: [], input: [] };
if (replayLog) {
  const unhex = (h) => Uint8Array.from({ length: h.length >> 1 }, (_, i) => parseInt(h.substr(2 * i, 2), 16));
  for (const line of (await readFile(replayLog, 'utf8')).split('\n')) {
    const w = line.split(' ');
    if (w[0] === 'start') epoch = Number((w.find((x) => x.startsWith('epoch=')) ?? 'epoch=0').slice(6)) || epoch;
    else if (w[0] === 'save') replay.saves.push([w[1].replaceAll('%20', ' '), unhex(w[2] ?? '')]);
    else if (w[0] === 'deleted') replay.saves.push([w[1].replaceAll('%20', ' '), null]);
    else if (w[0] === 'input' && (w[3] === 'key' || w[3] === 'mouse')) replay.input.push([w[3] === 'key' ? 0 : 1, Number(w[1]), Number(w[2]), ...w.slice(4, 7).map(Number)]);
  }
}

async function walk(dir, rel, out) {
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const r = rel ? `${rel}\\${e.name}` : e.name;
    if (e.isDirectory()) await walk(join(dir, e.name), r, out);
    else out.push([r, new Uint8Array(await readFile(join(dir, e.name))), (await stat(join(dir, e.name))).mtimeMs]);
  }
  return out;
}

const manifest = JSON.parse(await readFile(join(gameDir, 'manifest.json'), 'utf8'));
let files = await walk(join(gameDir, 'files'), '', []);
if (replayLog) {
  const key = (p) => p.replaceAll('/', '\\').toLowerCase();
  const byPath = new Map(files.map((f) => [key(f[0]), f]));
  for (const [path, data] of replay.saves) {
    if (data) byPath.set(key(path), [path, data, 0]);
    else byPath.delete(key(path));
  }
  files = [...byPath.values()];
}
const runtimeBytes = await readFile(new URL('../../target/wasm/wasm32-unknown-unknown/release/web.wasm', import.meta.url));
const t = performance.now();
const dumpMem = opt('--dump-mem', null);
let guest;
const keys = args.flatMap((a, i) => (a === '--key' ? [args[i + 1].split(':')] : [])).map(([f, vk, d]) => [Number(f), parseInt(vk, 16), d === '1']);
let input;
const [profFrom, profTo] = (opt('--profile', '') || '-1:-1').split(':').map(Number);
const profOut = opt('--profile-out', 'frames.cpuprofile');
let session;
const onFrame = (frame) => {
  for (const [f, vk, down] of keys) if (f === frame) input.key(vk, 0, down);
  // the inspector answers a session of this thread synchronously
  if (frame === profFrom) {
    session = new inspector.Session();
    session.connect();
    session.post('Profiler.enable');
    session.post('Profiler.setSamplingInterval', { interval: 100 });
    session.post('Profiler.start');
  } else if (frame === profTo && session) {
    session.post('Profiler.stop', (err, { profile }) => writeFileSync(profOut, JSON.stringify(profile)));
    session.disconnect();
  }
};
const tpFile = opt('--translation-profile', null);
let tp = null;
if (tpFile) {
  try {
    const b = await readFile(tpFile);
    tp = new Uint32Array(b.buffer, b.byteOffset, b.byteLength >> 2);
  } catch {}
}
const onProfile = (entries) => {
  const merged = new Uint32Array((tp?.length ?? 0) + entries.length);
  if (tp) merged.set(tp);
  merged.set(entries, tp?.length ?? 0);
  tp = merged;
};
const status = await runGame({ runtimeBytes, files, exe: manifest.entry, frames, trace, epoch, onFrame, onReady: (i) => (input = i), onExit: (s) => (guest = s.guestMemory), replay: replay.input, profile: tp, onProfile: tpFile ? onProfile : undefined });
if (tpFile && tp) await writeFile(tpFile, new Uint8Array(tp.buffer, tp.byteOffset, tp.byteLength));
if (dumpMem) await writeFile(dumpMem, new Uint8Array(guest.buffer));
console.log(`status ${status}, ${((performance.now() - t) / 1000).toFixed(1)}s total`);
process.exit(status);
