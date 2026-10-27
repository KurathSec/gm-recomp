// A helper thread of the game's worker for save states (host.mjs `mapAhead`): maps the
// pages of a state's fresh buffer (writes each first, as the system then gives it memory)
// a chunk at a time, ahead of the game's thread copying the guest's memory into it, which
// cost three times as much as copying into mapped memory. {buf, states, n, chunk}: the
// buffer's first n chunks, each claimed in states (0 free, 1 being mapped, 2 done or the
// game's) by whichever thread comes first.
self.onmessage = ({ data: { buf, states, n, chunk } }) => {
  const bytes = new Uint8Array(buf);
  const st = new Int32Array(states);
  for (let c = 0; c < n; c++) {
    if (Atomics.compareExchange(st, c, 0, 1) !== 0) continue;
    const end = Math.min(bytes.length, (c + 1) * chunk);
    for (let p = c * chunk; p < end; p += 4096) bytes[p] = 0;
    Atomics.store(st, c, 2);
    Atomics.notify(st, c);
  }
};
self.postMessage('ready');
