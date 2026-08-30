// Plays the runtime's mix from a SharedArrayBuffer ring written by the worker.
// Ring layout: Int32 [write frame, read frame] then interleaved stereo Float32 frames.
// The worker writes a frame's worth of sound at each Present, so playback waits until
// `target` frames are queued (and again after running dry) to ride out that jitter.
class RingPlayer extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const { sab, frames, latency = 0.05 } = options.processorOptions;
    this.idx = new Int32Array(sab, 0, 2);
    this.data = new Float32Array(sab, 8, frames * 2);
    this.frames = frames;
    this.target = Math.floor(sampleRate * latency);
    // More than this queued means the producer ran ahead: drop back to the target.
    this.maxQueued = 3 * this.target;
    this.buffering = true;
    // Totals the page can ask for.
    this.stats = { played: 0, starved: 0, underruns: 0, skipped: 0, sumsq: 0, start: -1, events: [] };
    this.port.onmessage = () => this.port.postMessage(this.stats);
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const l = out[0];
    const r = out[1] ?? out[0];
    const st = this.stats;
    const w = Atomics.load(this.idx, 0);
    let rd = Atomics.load(this.idx, 1);
    if (w - rd > this.maxQueued) {
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
    }
    if (st.played > 0) st.starved += l.length - n;
    st.played += n;
    Atomics.store(this.idx, 1, rd + n);
    return true;
  }
}

registerProcessor('ring-player', RingPlayer);
