// Mods: changes to a game's files made in the browser before it starts (crates/gmmod,
// served at /gmmod/gmmod.wasm), with each mod's own files from /mods/<id>/ (copied from a
// Fangame.ModLoader folder by tools/mods.sh). The page shows the mods a game's engine
// allows (`applies`, from the manifest's engine) and keeps each game's choices.

// Fangame.ModLoader edits GameMaker 8 games and GameMaker: Studio games that keep their
// bytecode (not YYC); these mods do GameMaker 8 and GameMaker: Studio 1.4 (crates/gmmod
// gms.rs; not GameMaker Studio 2 yet).
const gm8 = (e) => e?.kind === 'gm80' || e?.kind === 'gm81';
const gmsVm = (e) => e?.kind === 'gms' && !e.yyc && (e.version?.[0] ?? 1) < 2;
const modLoader = (e) => gm8(e) || gmsVm(e);

/**
 * id: the mod (crates/gmmod `apply`); options: {key, label, type 'bool' | 'number' | 'text',
 * default, applies?}; files: what it needs from /mods/<id>/.
 */
export const MODS = [
  {
    id: 'debug',
    name: 'Debug mode',
    about: "Turns on the game's debug switch (debugMode and the like), as patcher.rs does.",
    applies: (e) => e?.kind === 'gms',
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
    name: 'PlayOnline',
    about: 'Play online with others in the same game: you see each other, chat (Space) and see where they saved. The browser reaches the server through a WebSocket relay you run (web/tools/relay.mjs).',
    applies: gm8,
    options: [
      { key: 'RelayUrl', label: 'Relay (WebSocket URL)', type: 'text', default: 'ws://localhost:8790/' },
      { key: 'ServerIp', label: 'Server', type: 'text', default: 'iwannastartaparty.com' },
      { key: 'TcpPort', label: 'TCP port', type: 'number', default: 8002 },
      { key: 'UdpPort', label: 'UDP port', type: 'number', default: 8003 },
      { key: 'PlayerName', label: 'Your name', type: 'text', default: 'Player' },
      { key: 'Password', label: 'Password (players with the same one see each other)', type: 'text', default: '' },
      { key: 'RaceMode', label: 'Race mode (others’ saves are not shown)', type: 'bool', default: false },
    ],
    files: ["GML/Config/po_config.gml", "GML/GM8/po_asset_get_index.gml", "GML/GM8/po_load_assets.gml", "GML/GM8/po_sound_play.gml", "GML/po_chatbox_create.gml", "GML/po_chatbox_draw.gml", "GML/po_chatbox_endstep.gml", "GML/po_find_player.gml", "GML/po_get_player_grav.gml", "GML/po_get_player_xscale.gml", "GML/po_get_player_yscale.gml", "GML/po_onlineplayer_create.gml", "GML/po_onlineplayer_draw.gml", "GML/po_onlineplayer_endstep.gml", "GML/po_playersaved_draw.gml", "GML/po_playersaved_endstep.gml", "GML/po_set_player_grav.gml", "GML/po_someonesave_load.gml", "GML/po_someonesave_send.gml", "GML/po_temp_socketstate_load.gml", "GML/po_temp_socketstate_save.gml", "GML/po_temp_someonesave_load.gml", "GML/po_temp_someonesave_save.gml", "GML/po_world_create.gml", "GML/po_world_endstep.gml", "GML/po_world_gameend.gml", "po_http_dll_2_3.dll", "po_http_dll_2_3.gml", "po_snd_chatbox.wav", "po_snd_saved.wav"],
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
  if (!mods.length) return files;
  compiled ??= WebAssembly.compileStreaming(fetch('/gmmod/gmmod.wasm'));
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
      const r = await fetch(`/mods/${m.id}/${name.split('/').map(encodeURIComponent).join('/')}`);
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
