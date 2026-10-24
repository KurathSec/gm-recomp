// Mods: changes to a game's files made in the browser before it starts (crates/gmmod,
// gmmod/gmmod.wasm next to the page), with each mod's own files from mods/<id>/ (copied from a
// Fangame.ModLoader folder by tools/mods.sh). The page shows the mods a game's engine
// allows (`applies`, from the manifest's engine) and keeps each game's choices.

import { asset } from './asset.mjs';

// Fangame.ModLoader edits GameMaker 8 games and GameMaker: Studio games that keep their
// bytecode (not YYC); these mods do GameMaker 8 and GameMaker: Studio 1.4 (crates/gmmod
// gms.rs; not GameMaker Studio 2 yet).
const gm8 = (e) => e?.kind === 'gm80' || e?.kind === 'gm81';
const gmsVm = (e) => e?.kind === 'gms' && !e.yyc && (e.version?.[0] ?? 1) < 2;
const modLoader = (e) => gm8(e) || gmsVm(e);

/**
 * id: the mod (crates/gmmod `apply`); options: {key, label, type 'bool' | 'number' | 'text',
 * default, applies?}; files: what it needs from /mods/<id>/. A `page` mod changes no files:
 * the page around the game does it (player.mjs).
 */
export const MODS = [
  {
    id: 'SaveStates',
    name: 'Save states',
    about: 'A key saves everything the game is at that moment, another goes back to it exactly (up to 10 states per game, kept in this browser only; F7 and F8 at first, a button opens the list).',
    applies: (e) => !!e,
    page: true,
    options: [],
    files: [],
  },
  {
    id: 'debug',
    name: 'Debug mode',
    about: "Turns on the game's debug switch (debugMode and the like), as patcher.rs does.",
    applies: (e) => e?.kind === 'gms' && (e.version?.[0] ?? 1) < 2,
    options: [{ key: 'noDeath', label: 'Also debugNoDeath (the player cannot die)', type: 'bool', default: false, applies: (e) => !e.yyc }],
    files: [],
  },
  // Fangame.ModLoader's mods, in its order (they apply in this order)
  {
    id: 'AllowFullscreen',
    name: 'AllowFullscreen',
    about: 'F4 switches the game to full screen (a GameMaker 8 setting).',
    applies: gm8,
    options: [],
    files: [],
  },
  {
    id: 'CrimsonKid',
    name: 'CrimsonKid',
    about: 'The Crimson Kid skin: new player and bow sprites, the bow drawn behind the player.',
    applies: modLoader,
    options: [{ key: 'BowImageSpeed', label: 'Bow animation speed', type: 'number', default: 0.2 }],
    files: ['sprPlayerIdle.png', 'sprPlayerRunning.png', 'sprPlayerJump.png', 'sprPlayerFall.png', 'sprPlayerSliding.png', 'sprBow.png'],
  },
  {
    id: 'DbgHelper',
    name: 'DbgHelper',
    about: 'Fixes GameMaker lag issues: gm8x_fix 0.5.9’s patches to the GameMaker 8 runner; for GameMaker: Studio, the runner’s sleep margin (its DBGHELP.dll only sets Windows’ timer resolution, which the browser’s clock does not need).',
    applies: modLoader,
    options: [
      { key: 'InputLagPatch', label: 'Input lag patch', type: 'bool', default: true, applies: gm8 },
      { key: 'JoystickPatch', label: 'Joystick patch', type: 'bool', default: true, applies: gm8 },
      { key: 'SchedulerPatch', label: 'Scheduler patch', type: 'bool', default: true, applies: gm8 },
      { key: 'MemoryPatch', label: 'Memory patch', type: 'bool', default: true, applies: gm8 },
      { key: 'DirectPlayPatch', label: 'DirectPlay patch', type: 'bool', default: true, applies: gm8 },
      { key: 'SetSleepMargin', label: 'Set the sleep margin', type: 'bool', default: true, applies: gmsVm },
      { key: 'SleepMargin', label: 'Sleep margin (ms)', type: 'number', default: 1, applies: gmsVm },
    ],
    files: [],
  },
  {
    id: 'DotKid',
    name: 'DotKid',
    about: 'The player is drawn as its collision box (a dot), with an optional outline circle.',
    applies: modLoader,
    options: [{ key: 'ShowOutline', label: 'Outline circle', type: 'bool', default: true }],
    files: ['sprDotkidMask.png', 'sprEmpty32x32.png', 'sprDotkidCircle.png'],
  },
  {
    id: 'PlayOnline',
    name: 'Play online',
    about: 'I wanna play online 1.2.3 (github.com/Pardocaninacting/i-wanna-play-online-1.2.3): see the others playing the same game, chat and see where they saved. Its converter changes the game here before it starts; the browser reaches the server over a WebSocket (one the server offers, or web/tools/relay.mjs).',
    applies: gm8,
    options: [
      { key: 'Server', label: 'Server', type: 'text', default: '123.iwannaplay.online' },
      { key: 'TcpPort', label: 'TCP port', type: 'number', default: 8002 },
      { key: 'UdpPort', label: 'UDP port', type: 'number', default: 8003 },
      { key: 'RelayUrl', label: 'WebSocket URL', type: 'text', default: 'ws://localhost:8790/' },
      { key: 'Language', label: 'Menu language (en, zh-CN)', type: 'text', default: 'en' },
    ],
    files: [],
  },
  {
    id: 'GodMode',
    name: 'GodMode',
    about: 'The player cannot die (the kill script returns at once).',
    applies: modLoader,
    options: [],
    files: [],
  },
];

/** A mod the page does, not the files (`page`). */
export const pageMod = (id) => MODS.some((m) => m.id === id && m.page);

export function modsFor(engine) {
  return MODS.filter((m) => m.applies(engine));
}

const key = (slug) => `gmr.mods.${slug}`;

/** A game's choices: {id: {on, config: {key: value}}}. */
export function loadChoices(slug) {
  try {
    return JSON.parse(localStorage.getItem(key(slug)) ?? '{}') ?? {};
  } catch {
    return {};
  }
}

export function saveChoices(slug, choices) {
  try {
    localStorage.setItem(key(slug), JSON.stringify(choices));
  } catch {}
}

/** The mods turned on in `choices` that `engine` allows, with their settings: [{id, config}]. */
export function selectedMods(choices, engine) {
  return modsFor(engine)
    .filter((m) => choices[m.id]?.on)
    .map((m) => {
      const config = {};
      for (const o of m.options) {
        if (o.applies && !o.applies(engine)) continue;
        config[o.key] = choices[m.id].config?.[o.key] ?? o.default;
      }
      return { id: m.id, config };
    });
}

let compiled;
const same = (a, b) => a.replaceAll('\\', '/').toLowerCase() === b.replaceAll('\\', '/').toLowerCase();

/**
 * Applies `mods` ([{id, config}]) to a game's files ([path, bytes, mtime]); returns the new
 * list (changed files replaced, new ones added). `print` gets the mods' log.
 */
export async function applyMods(files, manifest, mods, print) {
  const online = mods.find((m) => m.id === 'PlayOnline');
  const patches = mods.filter((m) => m !== online);
  let out = patches.length ? await patchFiles(files, manifest, patches, print) : files;
  // after the others: it converts whatever exe they leave
  if (online) out = await playOnline(out, manifest, online.config, print);
  return out;
}

/**
 * PlayOnline: I wanna play online's GameMaker 8 converter (iwpo-gm8.mjs, built by
 * tools/iwpo/build.mjs) rewrites the exe and adds its files next to it. Its
 * http_dll_2_3.dll is a marker the runtime replaces with its port of that DLL
 * (crates/runtime hle/httpdll.rs), whose connections go to `RelayUrl`.
 */
async function playOnline(files, manifest, config, print) {
  const { convertGM8, VERSION } = await import('./iwpo-gm8.mjs');
  const at = files.findIndex(([p]) => same(p, manifest.entry));
  if (at < 0) throw new Error('PlayOnline: the game exe is missing');
  const cut = manifest.entry.lastIndexOf('/') + 1;
  const dir = manifest.entry.slice(0, cut);
  const r = await convertGM8(files[at][1], manifest.entry.slice(cut), { server: String(config.Server), tcp: Number(config.TcpPort), udp: Number(config.UdpPort), lang: config.Language || undefined });
  const next = files.slice();
  next[at] = [next[at][0], r.exe, next[at][2] ?? 0];
  for (const [p, data] of r.files) {
    const path = dir + p;
    const i = next.findIndex(([q]) => same(q, path));
    if (i >= 0) next[i] = [next[i][0], data, 0];
    else next.push([path, data, 0]);
  }
  print(`mod PlayOnline: converted by I wanna play online ${VERSION} (server ${config.Server}:${config.TcpPort}/${config.UdpPort})`);
  return next;
}

/** The mods crates/gmmod applies (gmmod/gmmod.wasm). */
async function patchFiles(files, manifest, mods, print) {
  compiled ??= WebAssembly.compileStreaming(fetch(asset('gmmod/gmmod.wasm')));
  const module = await compiled;
  const imports = {};
  for (const imp of WebAssembly.Module.imports(module)) (imports[imp.module] ??= {})[imp.name] = () => 0;
  const x = (await WebAssembly.instantiate(module, imports)).exports;
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  const mem = () => new Uint8Array(x.memory.buffer);
  const put = (u8) => {
    const p = x.alloc(u8.length);
    mem().set(u8, p);
    return [p, u8.length];
  };
  const out = () => dec.decode(mem().slice(x.out_ptr(), x.out_ptr() + x.out_len()));
  const call = (f, ...args) => {
    const bufs = args.map((a) => (typeof a === 'string' ? put(enc.encode(a)) : a instanceof Uint8Array ? put(a) : null));
    const flat = args.flatMap((a, i) => (bufs[i] ? bufs[i] : [a]));
    const r = f(...flat);
    for (const b of bufs) if (b) x.dealloc(b[0], b[1]);
    return r;
  };
  if (call(x.begin, JSON.stringify({ engine: manifest.engine, entry: manifest.entry, mods })) !== 0) throw new Error('mods: bad request');
  // The files a mod may change: the exe and the data file.
  const wanted = [manifest.entry, manifest.engine?.data_file].filter((p) => p && !p.startsWith('embedded@'));
  for (const [path, data] of files) if (wanted.some((w) => same(w, path))) call(x.add_file, path, data);
  for (const m of mods) {
    const def = MODS.find((d) => d.id === m.id);
    for (const name of def?.files ?? []) {
      const r = await fetch(asset(`mods/${m.id}/${name.split('/').map(encodeURIComponent).join('/')}`));
      if (!r.ok) throw new Error(`mod ${m.id}: ${name} is missing (HTTP ${r.status}; see web/tools/mods.sh)`);
      call(x.add_asset, m.id, name, new Uint8Array(await r.arrayBuffer()));
    }
  }
  const failed = x.run() !== 0;
  const result = JSON.parse(out() || '{}');
  if (failed) {
    x.finish();
    throw new Error(`mods: ${result.error ?? 'failed'}`);
  }
  for (const line of result.log ?? []) print(`mod ${line}`);
  const next = files.slice();
  for (let i = 0, n = x.changed_count(); i < n; i++) {
    x.changed_path(i);
    const path = out();
    const data = mem().slice(x.changed_ptr(i), x.changed_ptr(i) + x.changed_len(i));
    const at = next.findIndex(([p]) => same(p, path));
    if (at >= 0) next[at] = [next[at][0], data, next[at][2] ?? 0];
    else next.push([path, data, 0]);
  }
  x.finish();
  return next;
}
