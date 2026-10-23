// Builds web/src/iwpo-gm8.mjs, the PlayOnline mod: I wanna play online 1.2.3's GameMaker
// 8 converter (entry.js) with its own files (GML templates, extension packages, fonts,
// sounds, language and per-game settings) inside, from a checkout of
// https://github.com/Pardocaninacting/i-wanna-play-online-1.2.3.
//   npm ci --prefix web/tools/iwpo && node web/tools/iwpo/build.mjs <checkout>
// Its http_dll_2_3.dll is a marker the runtime replaces with its port of that DLL
// (crates/runtime/src/hle/httpdll.rs).
import * as esbuild from 'esbuild';
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { join, relative, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const checkout = process.argv[2];
if (!checkout || !existsSync(join(checkout, 'modder', 'converterGM8.ts'))) {
  console.error('usage: node web/tools/iwpo/build.mjs <i-wanna-play-online-1.2.3 checkout>');
  process.exit(2);
}
const modder = join(checkout, 'modder');
const out = join(here, '..', '..', 'src', 'iwpo-gm8.mjs');

// The converter's own files, by path under modder/.
const assets = {};
const walk = (dir) => {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) walk(full);
    else assets[relative(modder, full).replaceAll('\\', '/')] = readFileSync(full).toString('base64');
  }
};
for (const dir of ['gml', 'lib', 'lang', 'games']) if (existsSync(join(modder, dir))) walk(join(modder, dir));
assets['lib/http_dll_2_3.dll'] = Buffer.from('GMR-HLE http_dll_2_3\0', 'latin1').toString('base64');

const shim = (f) => join(here, 'shims', f);
const assetsPlugin = {
  name: 'iwpo-assets',
  setup(b) {
    b.onResolve({ filter: /^iwpo-assets$/ }, () => ({ path: 'iwpo-assets', namespace: 'iwpo-assets' }));
    b.onLoad({ filter: /.*/, namespace: 'iwpo-assets' }, () => ({ contents: `export default ${JSON.stringify(assets)};`, loader: 'js' }));
    b.onResolve({ filter: /^iwpo-modder\// }, (a) => ({ path: join(modder, a.path.slice('iwpo-modder/'.length) + '.ts') }));
  },
};
const license = readFileSync(join(checkout, 'LICENSE'), 'utf8').trim().split('\n').map((l) => `// ${l}`).join('\n');
const commit = (() => {
  try {
    const head = readFileSync(join(checkout, '.git', 'HEAD'), 'utf8').trim();
    return head.startsWith('ref: ') ? readFileSync(join(checkout, '.git', head.slice(5)), 'utf8').trim().slice(0, 7) : head.slice(0, 7);
  } catch {
    return 'unknown';
  }
})();
await esbuild.build({
  entryPoints: [join(here, 'entry.js')],
  bundle: true,
  format: 'esm',
  platform: 'browser',
  target: 'es2022',
  outfile: out,
  minify: true,
  legalComments: 'none',
  nodePaths: [join(here, 'node_modules')],
  alias: {
    fs: shim('fs.js'),
    'fs-extra': shim('fs.js'),
    zlib: shim('zlib.js'),
    crypto: shim('crypto.js'),
    process: shim('process.js'),
    ncp: shim('ncp.js'),
    path: 'path-browserify',
    child_process: shim('empty.js'),
    readline: shim('empty.js'),
    http: shim('empty.js'),
    https: shim('empty.js'),
    rimraf: shim('empty.js'),
    '7zip-min': shim('empty.js'),
  },
  inject: [shim('globals.js')],
  define: { __dirname: '"/iwpo"' },
  plugins: [assetsPlugin],
  banner: { js: `// I wanna play online 1.2.3's GameMaker 8 converter, ${commit}, built by web/tools/iwpo/build.mjs.\n${license}` },
  logLevel: 'warning',
});
console.log(`${out}: ${(statSync(out).size / 1048576).toFixed(1)} MB (I wanna play online ${commit})`);
