// Plays the runtime's mix from a SharedArrayBuffer ring written by the worker.
// Ring layout: Int32 [write frame, read frame] then interleaved stereo Float32 frames.
// The worker writes sound as the game's clock moves (about every millisecond while it
// waits, a frame's worth at worst), so playback waits until `target` frames are
// queued (and again after running dry) to ride out that jitter. The target starts at
// what the system's audio period needs, grows a little after each underrun during
// play and shrinks back after quiet stretches, so sound stays as close as it can.
class RingPlayer extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { sab, frames, latency = 0.02 } = options.processorOptions;
    this.idx = new Int32Array(sab, 0, 2);
    this.data = new Float32Array(sab, 8, frames * 2);
    this.frames = frames;
    this.base = Math.floor(sampleRate * latency);
    this.target = this.base;
    this.calm = 0; // frames played since the last underrun
    this.buffering = true;
    // Totals the page can ask for.
    this.stats = { played: 0, starved: 0, underruns: 0, skipped: 0, sumsq: 0, start: -1, events: [], target: 0 };
    this.port.onmessage = () => this.port.postMessage({ ...this.stats, target: this.target / sampleRate });
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const l = out[0];
    const r = out[1] ?? out[0];
    const st = this.stats;
    const w = Atomics.load(this.idx, 0);
    let rd = Atomics.load(this.idx, 1);
    // More than this queued means the producer ran ahead: drop back to the target.
    if (w - rd > this.target + sampleRate * 0.04) {
      st.skipped += w - rd - this.target;
      rd = w - this.target;
    }
    if (this.buffering && w - rd >= this.target) {
      this.buffering = false;
      if (st.start < 0) st.start = currentTime;
    }
    const n = this.buffering ? 0 : Math.min(l.length, w - rd);
    for (let i = 0; i < n; i++) {
      const k = ((rd + i) % this.frames) * 2;
      l[i] = this.data[k];
      r[i] = this.data[k + 1];
      st.sumsq += l[i] * l[i] + r[i] * r[i];
    }
    for (let i = n; i < l.length; i++) l[i] = r[i] = 0;
    if (!this.buffering && n < l.length) {
      this.buffering = true;
      st.underruns++;
      if (st.events.length < 64) st.events.push(+currentTime.toFixed(2));
      this.target = Math.min(this.target + Math.floor(sampleRate * 0.005), Math.floor(sampleRate * 0.08));
      this.calm = 0;
    } else if (n > 0) {
      // a second without running dry: back off 1 ms toward the base
      this.calm += n;
      if (this.calm >= sampleRate && this.target > this.base) {
        this.target = Math.max(this.base, this.target - Math.floor(sampleRate * 0.001));
        this.calm = 0;
      }
    }
    if (st.played > 0) st.starved += l.length - n;
    st.played += n;
    Atomics.store(this.idx, 1, rd + n);
    return true;
  }
}

registerProcessor('ring-player', RingPlayer);
