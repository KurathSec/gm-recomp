// fs / fs-extra for I wanna play online's converter in the browser: an in-memory
// file system holding the converter's own files and the game's, with the calls the
// GM8 converter makes (sync, callback-free promise and fs-extra forms).
import { Buffer } from 'buffer';

const files = new Map(); // normalized path -> Uint8Array
const dirs = new Set(['/']);

const norm = (p) => {
  const parts = [];
  for (const s of String(p).replaceAll('\\', '/').split('/')) {
    if (!s || s === '.') continue;
    if (s === '..') parts.pop();
    else parts.push(s);
  }
  return '/' + parts.join('/');
};
const parentOf = (p) => p.slice(0, p.lastIndexOf('/')) || '/';
const addDirs = (p) => {
  for (let d = parentOf(p); !dirs.has(d); d = parentOf(d)) dirs.add(d);
};
const noent = (p) => Object.assign(new Error(`ENOENT: no such file or directory, '${p}'`), { code: 'ENOENT' });
const encode = (data, enc) => (typeof data === 'string' ? Buffer.from(data, typeof enc === 'string' ? enc : enc?.encoding ?? 'utf8') : Buffer.from(data.buffer ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : data));

export const vol = {
  reset() {
    files.clear();
    dirs.clear();
    dirs.add('/');
  },
  put(p, data) {
    const n = norm(p);
    files.set(n, data instanceof Uint8Array ? data : encode(data));
    addDirs(n);
  },
  get(p) {
    return files.get(norm(p));
  },
  list() {
    return [...files.keys()];
  },
};

function readFileSync(p, enc) {
  const d = files.get(norm(p));
  if (!d) throw noent(p);
  const b = Buffer.from(d);
  const e = typeof enc === 'string' ? enc : enc?.encoding;
  return e ? b.toString(e) : b;
}
function writeFileSync(p, data, enc) {
  vol.put(p, encode(data, enc));
}
function existsSync(p) {
  const n = norm(p);
  return files.has(n) || dirs.has(n);
}
function statSync(p) {
  const n = norm(p);
  if (dirs.has(n)) return { isDirectory: () => true, isFile: () => false, size: 0, mtimeMs: 0 };
  const d = files.get(n);
  if (!d) throw noent(p);
  return { isDirectory: () => false, isFile: () => true, size: d.length, mtimeMs: 0 };
}
function readdirSync(p) {
  const n = norm(p);
  if (!dirs.has(n)) throw noent(p);
  const pre = n === '/' ? '/' : n + '/';
  const out = new Set();
  for (const k of [...files.keys(), ...dirs]) if (k.startsWith(pre) && k !== n) out.add(k.slice(pre.length).split('/')[0]);
  return [...out].sort();
}
function mkdirSync(p) {
  const n = norm(p);
  addDirs(n + '/x');
}
function copyFileSync(a, b) {
  const d = files.get(norm(a));
  if (!d) throw noent(a);
  vol.put(b, d);
}
function removeSync(p) {
  const n = norm(p);
  for (const k of [...files.keys()]) if (k === n || k.startsWith(n + '/')) files.delete(k);
  for (const k of [...dirs]) if (k === n || k.startsWith(n + '/')) dirs.delete(k);
}
// file handles: the converter reads an exe's first bytes
const handles = new Map();
let nextHandle = 3;
function openSync(p) {
  if (!files.has(norm(p))) throw noent(p);
  handles.set(nextHandle, norm(p));
  return nextHandle++;
}
function readSync(fd, buffer, offset, length, position) {
  const d = files.get(handles.get(fd)) ?? new Uint8Array();
  const n = Math.max(0, Math.min(length, d.length - (position ?? 0)));
  buffer.set(d.subarray(position ?? 0, (position ?? 0) + n), offset);
  return n;
}

const later = (f) => (...a) => {
  try {
    return Promise.resolve(f(...a));
  } catch (e) {
    return Promise.reject(e);
  }
};
const fs = {
  readFileSync,
  writeFileSync,
  existsSync,
  statSync,
  readdirSync,
  mkdirSync,
  copyFileSync,
  removeSync,
  openSync,
  readSync,
  closeSync: (fd) => void handles.delete(fd),
  readFile: later(readFileSync),
  writeFile: later(writeFileSync),
  exists: (p) => Promise.resolve(existsSync(p)),
  pathExists: (p) => Promise.resolve(existsSync(p)),
  stat: later(statSync),
  readdir: later(readdirSync),
  mkdir: later(mkdirSync),
  mkdirp: later(mkdirSync),
  ensureDir: later(mkdirSync),
  copyFile: later(copyFileSync),
  copy: later((a, b) => copyDir(a, b)),
  remove: later(removeSync),
  open: later(openSync),
  read: later((fd, buffer, offset, length, position) => ({ bytesRead: readSync(fd, buffer, offset, length, position), buffer })),
  close: later((fd) => void handles.delete(fd)),
};

/** Copies a file or a directory tree (ncp's semantics: existing files are replaced). */
export function copyDir(a, b) {
  const src = norm(a);
  const dst = norm(b);
  if (files.has(src)) return vol.put(dst, files.get(src));
  for (const k of [...files.keys()]) if (k.startsWith(src + '/')) vol.put(dst + k.slice(src.length), files.get(k));
  mkdirSync(dst);
}

export default fs;
export const { readFile, writeFile, exists, pathExists, stat, readdir, mkdir, ensureDir, copyFile, remove, open, read, close } = fs;
export { readFileSync, writeFileSync, existsSync, statSync, readdirSync, mkdirSync, copyFileSync, openSync, readSync };
