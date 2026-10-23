// I wanna play online 1.2.3's GameMaker 8 converter (modder/ of
// https://github.com/Pardocaninacting/i-wanna-play-online-1.2.3, MIT) in the browser:
// the game's exe and the converter's own files sit in an in-memory file system, the
// converter runs as on the command line, and what it wrote next to the exe comes back.
// Built into web/src/iwpo-gm8.mjs by build.mjs.
import { Buffer } from 'buffer';
import { ConverterGM8 } from 'iwpo-modder/converterGM8';
import { Utils } from 'iwpo-modder/utils';
import { vol } from './shims/fs.js';
import ASSETS from 'iwpo-assets';

export const VERSION = Utils.getVersion();

/** The [iwpo] keys of a per-game ini (games/<name>.ini), as `iwpo.<key>` defines. */
function gameDefines(text, defines) {
  let section = '';
  for (const line of text.split(/\r?\n/)) {
    const t = line.trim();
    if (t.startsWith('[') && t.endsWith(']')) section = t.slice(1, -1).toLowerCase();
    else if (section === 'iwpo' && t && !t.startsWith(';') && !t.startsWith('#') && t.includes('=')) {
      const i = t.indexOf('=');
      const value = t.slice(i + 1).trim();
      if (value) defines.set(`iwpo.${t.slice(0, i).trim()}`, value);
    }
  }
}

/**
 * Converts a GameMaker 8 game.
 * @param {Uint8Array} exe  the game's exe
 * @param {string} name  its file name
 * @param {{server: string, tcp: number, udp: number, lang?: string}} o  the server the
 *   game connects to, and the default language of its online menus
 * @returns {Promise<{exe: Uint8Array, files: [string, Uint8Array][], log: string[]}>}
 *   the converted exe, the files to put next to it, and what the converter printed
 */
export async function convertGM8(exe, name, o) {
  vol.reset();
  for (const [path, b64] of Object.entries(ASSETS)) vol.put(`/iwpo/${path}`, Buffer.from(b64, 'base64'));
  const gameName = name.replace(/\.exe$/i, '');
  const input = `/game/${gameName}.exe`;
  const output = `/game/${gameName}_online.exe`;
  vol.put(input, exe);
  const defines = new Map();
  const ini = vol.get(`/iwpo/games/${gameName}.ini`);
  if (ini) gameDefines(Buffer.from(ini).toString('utf8'), defines);
  // In the browser: no GM8 extension packages (their DLLs need Windows' icu.dll and the
  // MSVC 2015+ runtime, or open Windows dialogs; the converter writes stand-ins), GM8.0's
  // CJK text from the built-in atlas (as the packaged settings ship), and the menus in
  // `lang`. A game's own games/<name>.ini still wins.
  if (!defines.has('iwpo.extension_packages')) defines.set('iwpo.extension_packages', 'none');
  if (!defines.has('iwpo.cjk_atlas')) defines.set('iwpo.cjk_atlas', '1');
  if (o.lang) defines.set('iwpo.lang', o.lang);
  const log = [];
  const saved = { log: console.log, warn: console.warn, info: console.info };
  console.log = console.info = console.warn = (...a) => log.push(a.join(' '));
  try {
    await ConverterGM8(input, gameName, o.server, { tcp: o.tcp, udp: o.udp }, false, null, false, defines);
  } finally {
    Object.assign(console, saved);
  }
  const converted = vol.get(output);
  if (!converted) throw new Error('the converter wrote no exe');
  const files = vol
    .list()
    .filter((p) => p.startsWith('/game/') && p !== input && p !== output)
    .map((p) => [p.slice('/game/'.length), vol.get(p)]);
  return { exe: converted, files, log };
}
