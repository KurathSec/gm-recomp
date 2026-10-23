// Key remapping done by the page, outside the game: a key you press reaches the game as
// another one (player.mjs `keymap`). A map is {your key: the game's key}, both
// KeyboardEvent.code; keys not in it reach the game as they are. One map serves every
// game, and a game can have its own instead. Kept in localStorage.

import { KEYS } from './keys.mjs';

const ALL = 'gmr.keys';
const key = (slug) => `gmr.keys.${slug}`;

function read(k) {
  try {
    const v = JSON.parse(localStorage.getItem(k) ?? 'null');
    return v && typeof v === 'object' ? v : null;
  } catch {
    return null;
  }
}

function write(k, map) {
  try {
    if (map) localStorage.setItem(k, JSON.stringify(map));
    else localStorage.removeItem(k);
  } catch {}
}

/** The map for `slug`: its own, else the one for every game. `own` says which. */
export function loadKeymap(slug) {
  const own = read(key(slug));
  return { map: own ?? read(ALL) ?? {}, own: !!own };
}

/** Stores `map` for `slug` only (`own`), or for every game (and drops the game's own). */
export function saveKeymap(slug, map, own) {
  if (own) write(key(slug), map);
  else {
    write(key(slug), null);
    write(ALL, map);
  }
}

/** The game keys the editor lists: what fangames use, then the rest on demand. */
export const COMMON_KEYS = ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'ShiftLeft', 'KeyZ', 'KeyX', 'KeyR', 'KeyS', 'KeyQ', 'KeyP', 'Space', 'Enter', 'Escape', 'F2'];

const NAMES = {
  ArrowLeft: '←', ArrowRight: '→', ArrowUp: '↑', ArrowDown: '↓', ShiftLeft: 'Left Shift', ShiftRight: 'Right Shift',
  ControlLeft: 'Left Ctrl', ControlRight: 'Right Ctrl', AltLeft: 'Left Alt', AltRight: 'Right Alt', Space: 'Space',
  Enter: 'Enter', Escape: 'Esc', Backspace: 'Backspace', Tab: 'Tab', CapsLock: 'Caps Lock', Backquote: '`', Minus: '-',
  Equal: '=', BracketLeft: '[', BracketRight: ']', Backslash: '\\', Semicolon: ';', Quote: "'", Comma: ',', Period: '.', Slash: '/',
};

/** A key's name for people: "Z", "←", "Left Shift", "Numpad 4". */
export function keyName(code) {
  if (NAMES[code]) return NAMES[code];
  if (code.startsWith('Key')) return code.slice(3);
  if (code.startsWith('Digit')) return code.slice(5);
  if (code.startsWith('Numpad')) return `Numpad ${code.slice(6)}`;
  return code;
}

/**
 * Edits `slug`'s key map in `dialog` (an empty <dialog>); resolves with {map, own} once
 * saved, or null if cancelled. `labels` names the text (for translation); the elements
 * carry classes for the page's styles: km-row, km-game, km-keys, km-key, km-add,
 * km-listen, km-scope, km-buttons, primary.
 */
export function editKeymap(dialog, slug, labels = {}) {
  const L = {
    title: 'Keys',
    about: 'Keys you press here reach the game as other keys. The game itself is not changed.',
    gameKey: 'Game key',
    yourKeys: 'Your keys',
    add: 'Add',
    press: 'Press a key…',
    none: 'as it is',
    other: 'Another game key…',
    pressGame: 'Press the game key…',
    thisGame: 'This game only',
    allGames: 'Every game',
    reset: 'Reset',
    cancel: 'Cancel',
    save: 'Save',
    ...labels,
  };
  const start = loadKeymap(slug);
  const map = { ...start.map };
  let own = start.own;
  const shown = new Set([...COMMON_KEYS, ...Object.values(map)]);
  let listening = null; // (code) => void, while waiting for a key

  return new Promise((resolve) => {
    const el = (tag, cls, text) => {
      const e = document.createElement(tag);
      if (cls) e.className = cls;
      if (text != null) e.textContent = text;
      return e;
    };
    const render = () => {
      dialog.replaceChildren();
      const form = el('form');
      form.method = 'dialog';
      form.append(el('h2', null, L.title), el('p', 'km-about', L.about));
      const table = el('div', 'km-table');
      const head = el('div', 'km-row km-head');
      head.append(el('span', 'km-game', L.gameKey), el('span', 'km-keys', L.yourKeys));
      table.append(head);
      for (const game of shown) {
        const row = el('div', 'km-row');
        const keys = el('span', 'km-keys');
        const yours = Object.keys(map).filter((k) => map[k] === game);
        if (!yours.length) keys.append(el('span', 'km-none', L.none));
        for (const k of yours) {
          const b = el('button', 'km-key', `${keyName(k)} ×`);
          b.type = 'button';
          b.onclick = () => {
            delete map[k];
            render();
          };
          keys.append(b);
        }
        const add = el('button', 'km-add', L.add);
        add.type = 'button';
        add.onclick = () => {
          add.textContent = L.press;
          add.classList.add('km-listen');
          listening = (code) => {
            // (one game key per key you press; a key mapped to itself is just dropped)
            if (code === game) delete map[code];
            else map[code] = game;
            render();
          };
        };
        keys.append(add);
        row.append(el('span', 'km-game', keyName(game)), keys);
        table.append(row);
      }
      const other = el('button', 'km-add', L.other);
      other.type = 'button';
      other.onclick = () => {
        other.textContent = L.pressGame;
        other.classList.add('km-listen');
        listening = (code) => {
          if (KEYS[code]) shown.add(code);
          render();
        };
      };
      table.append(other);
      form.append(table);

      const scope = el('div', 'km-scope');
      for (const [value, text] of [[true, L.thisGame], [false, L.allGames]]) {
        const label = el('label');
        const radio = el('input');
        radio.type = 'radio';
        radio.name = 'km-scope';
        radio.checked = own === value;
        radio.onchange = () => (own = value);
        label.append(radio, ` ${text}`);
        scope.append(label);
      }
      form.append(scope);

      const buttons = el('div', 'km-buttons');
      const reset = el('button', null, L.reset);
      reset.type = 'button';
      reset.onclick = () => {
        for (const k of Object.keys(map)) delete map[k];
        render();
      };
      const cancel = el('button', null, L.cancel);
      cancel.type = 'button';
      cancel.onclick = () => dialog.close('cancel');
      const save = el('button', 'primary', L.save);
      save.value = 'save';
      buttons.append(reset, cancel, save);
      form.append(buttons);
      dialog.append(form);
    };
    const onKey = (e) => {
      if (!listening) return;
      e.preventDefault();
      e.stopPropagation();
      const f = listening;
      listening = null;
      // (a key the runtime does not know leaves the map as it was)
      if (KEYS[e.code]) f(e.code);
      else render();
    };
    dialog.addEventListener('keydown', onKey, true);
    dialog.addEventListener(
      'close',
      () => {
        dialog.removeEventListener('keydown', onKey, true);
        if (dialog.returnValue !== 'save') return resolve(null);
        const result = Object.fromEntries(Object.entries(map).filter(([k, v]) => KEYS[k] && KEYS[v] && k !== v));
        saveKeymap(slug, result, own);
        resolve({ map: result, own });
      },
      { once: true },
    );
    render();
    dialog.returnValue = '';
    dialog.showModal();
  });
}
