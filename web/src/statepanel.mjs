// The save-state list a page shows over its game (player.mjs `saveStates`): a floating
// panel with the game's slots (picture, when, Save, Load, Delete), the keys that save to
// and load the chosen slot, and a line over the game when a key did either. The page
// styles it (classes st-*; gm-recomp's index.html and the archive's play.css).

import { SLOTS } from './state-store.mjs';

const KEYS_KEY = 'gmr.states.keys';
const slotKey = (game) => `gmr.states.slot.${game}`;
const read = (k, fallback) => {
  try {
    return JSON.parse(localStorage.getItem(k) ?? 'null') ?? fallback;
  } catch {
    return fallback;
  }
};
const write = (k, v) => {
  try {
    localStorage.setItem(k, JSON.stringify(v));
  } catch {}
};

export const STATE_LABELS = {
  title: 'Save states',
  about: 'Kept in this browser only. {save} saves to the chosen slot, {load} goes back to it.',
  slot: 'Slot {n}',
  empty: 'Empty',
  save: 'Save',
  load: 'Load',
  delete: 'Delete',
  close: 'Close',
  saveKey: 'Save key',
  loadKey: 'Load key',
  press: 'Press a key…',
  saving: 'Saving to slot {n}…',
  saved: 'Saved to slot {n}',
  loading: 'Loading slot {n}…',
  loaded: 'Loaded slot {n}',
  failed: 'Slot {n}: {error}',
  isEmpty: 'Slot {n} is empty',
  notStored: 'Slot {n} could not be kept: {error}',
};

const fill = (s, vars) => s.replace(/\{(\w+)\}/g, (_, k) => vars[k] ?? '');
const keyName = (code) => code.replace(/^Key|^Digit/, '');

/**
 * @param {object} o
 * @param {{states: object}} o.player  playGame's result, started with `saveStates`
 * @param {string} o.game  the game's slug (as given to playGame)
 * @param {HTMLElement} o.mount  where the panel and the notices go (over the game)
 * @param {Partial<typeof STATE_LABELS>} [o.labels]
 * @returns {{toggle(): void, open(): void, close(): void, destroy(): void}}
 */
export function statesPanel({ player, game, mount, labels = {} }) {
  const L = { ...STATE_LABELS, ...labels };
  const states = player.states;
  let keys = { save: 'F7', load: 'F8', ...read(KEYS_KEY, {}) };
  let chosen = Math.min(SLOTS, Math.max(1, read(slotKey(game), 1)));
  let listening = null; // 'save' | 'load' while a key is being picked
  let urls = [];
  let busy = false;

  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };

  const panel = el('div', 'st-panel');
  panel.hidden = true;
  const head = el('div', 'st-head');
  const title = el('span', 'st-title', L.title);
  const closeBtn = el('button', 'st-close', '×');
  closeBtn.type = 'button';
  closeBtn.title = L.close;
  closeBtn.onclick = () => close();
  head.append(title, closeBtn);
  const about = el('div', 'st-about');
  const keyRow = el('div', 'st-keys');
  const list = el('div', 'st-list');
  panel.append(head, about, keyRow, list);
  const notice = el('div', 'st-notice');
  notice.hidden = true;
  mount.append(panel, notice);

  let noticeTimer = 0;
  const say = (text, error = false) => {
    notice.textContent = text;
    notice.classList.toggle('st-error', error);
    notice.hidden = false;
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(() => (notice.hidden = true), error ? 4000 : 1500);
  };

  const drawKeys = () => {
    about.textContent = fill(L.about, { save: keyName(keys.save), load: keyName(keys.load) });
    keyRow.replaceChildren();
    for (const which of ['save', 'load']) {
      const lab = el('label', 'st-key');
      lab.append(el('span', null, which === 'save' ? L.saveKey : L.loadKey));
      const b = el('button', listening === which ? 'st-listen' : null, listening === which ? L.press : keyName(keys[which]));
      b.type = 'button';
      b.onclick = () => {
        listening = listening === which ? null : which;
        drawKeys();
      };
      lab.append(b);
      keyRow.append(lab);
    }
  };

  const draw = async () => {
    let have = [];
    try {
      have = await states.list();
    } catch (e) {
      say(String(e.message ?? e), true);
    }
    for (const u of urls) URL.revokeObjectURL(u);
    urls = [];
    const bySlot = new Map(have.map((s) => [s.slot, s]));
    list.replaceChildren();
    for (let n = 1; n <= SLOTS; n++) {
      const s = bySlot.get(n);
      const row = el('div', 'st-row' + (n === chosen ? ' st-chosen' : '') + (s ? '' : ' st-empty'));
      row.onclick = (e) => {
        if (e.target.closest('button')) return;
        chosen = n;
        write(slotKey(game), n);
        draw();
      };
      const pic = el('div', 'st-pic');
      if (s?.thumb) {
        const img = el('img');
        const u = URL.createObjectURL(s.thumb);
        urls.push(u);
        img.src = u;
        img.alt = '';
        pic.append(img);
      }
      const info = el('div', 'st-info');
      info.append(el('div', 'st-name', fill(L.slot, { n })));
      info.append(el('div', 'st-when', s ? `${new Date(s.time).toLocaleString()} · ${(s.stored / 1048576).toFixed(1)} MB` : L.empty));
      const btns = el('div', 'st-btns');
      const btn = (text, f, disabled = false) => {
        const b = el('button', null, text);
        b.type = 'button';
        b.disabled = disabled;
        b.onclick = f;
        btns.append(b);
      };
      btn(L.save, () => save(n));
      btn(L.load, () => load(n), !s);
      btn(L.delete, () => states.remove(n).then(draw, (e) => say(String(e.message ?? e), true)), !s);
      row.append(pic, info, btns);
      list.append(row);
    }
  };

  const save = async (n) => {
    if (busy) return;
    busy = true;
    chosen = n;
    write(slotKey(game), n);
    say(fill(L.saving, { n }));
    try {
      await states.save(n);
      say(fill(L.saved, { n }));
    } catch (e) {
      say(fill(L.failed, { n, error: e.message ?? e }), true);
    }
    busy = false;
  };
  const load = async (n) => {
    if (busy) return;
    busy = true;
    say(fill(L.loading, { n }));
    try {
      await states.load(n);
      say(fill(L.loaded, { n }));
    } catch (e) {
      const msg = String(e.message ?? e);
      say(msg === 'empty slot' ? fill(L.isEmpty, { n }) : fill(L.failed, { n, error: msg }), true);
    }
    busy = false;
  };
  // the list shows a state once it is stored
  const stored = (slot, error) => {
    if (error) say(fill(L.notStored, { n: slot, error }), true);
    if (!panel.hidden) draw();
  };

  // The keys: before the player's own listener (it would send them to the game).
  const typing = (e) => e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement || e.target instanceof HTMLSelectElement || (e.target instanceof Element && e.target.closest('dialog[open]'));
  const onKey = (e) => {
    if (typing(e)) return;
    if (listening && e.type === 'keydown') {
      e.preventDefault();
      e.stopImmediatePropagation();
      if (e.code !== 'Escape') {
        keys = { ...keys, [listening]: e.code };
        write(KEYS_KEY, keys);
      }
      listening = null;
      drawKeys();
      return;
    }
    const which = e.code === keys.save ? 'save' : e.code === keys.load ? 'load' : null;
    if (!which) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    if (e.type !== 'keydown' || e.repeat) return;
    if (which === 'save') save(chosen).then(() => !panel.hidden && draw());
    else load(chosen);
  };
  addEventListener('keydown', onKey, true);
  addEventListener('keyup', onKey, true);

  const open = () => {
    panel.hidden = false;
    drawKeys();
    draw();
  };
  const close = () => {
    panel.hidden = true;
    listening = null;
  };
  return {
    toggle: () => (panel.hidden ? open() : close()),
    open,
    close,
    /** For playGame's onStateStored. */
    stored,
    destroy() {
      removeEventListener('keydown', onKey, true);
      removeEventListener('keyup', onKey, true);
      panel.remove();
      notice.remove();
      for (const u of urls) URL.revokeObjectURL(u);
    },
  };
}
