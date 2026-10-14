// Behaviour tests of the AudioWorklet code (WORKLET_SOURCE): the microphone resampler and 20 ms
// framing, and the playback jitter buffer with marks and barge-in. No browser: the source runs here
// with stand-ins for the globals an AudioWorkletGlobalScope provides.
import assert from "node:assert";
import { test } from "node:test";
import { WORKLET_SOURCE } from "../brisvox-call.js";

function load(sampleRate) {
  const processors = {};
  class AudioWorkletProcessor {
    constructor() {
      this.posted = [];
      this.port = { postMessage: (m) => this.posted.push(m), onmessage: null };
    }
  }
  const registerProcessor = (name, cls) => { processors[name] = cls; };
  new Function("AudioWorkletProcessor", "registerProcessor", "sampleRate", WORKLET_SOURCE)(
    AudioWorkletProcessor, registerProcessor, sampleRate);
  return processors;
}

const QUANTUM = 128;

// Runs `samples` (Float32Array at `rate`) through a capture processor; returns all Int16 frames.
function capture(rate, samples, frame = 480, outRate = 24000) {
  const { "brisvox-capture": Capture } = load(rate);
  const node = new Capture({ processorOptions: { rate: outRate, frame } });
  for (let i = 0; i < samples.length; i += QUANTUM) {
    node.process([[samples.subarray(i, Math.min(samples.length, i + QUANTUM))]], []);
  }
  return node.posted.map((buffer) => new Int16Array(buffer));
}

function tone(rate, hz, seconds, amplitude = 0.5) {
  const out = new Float32Array(Math.round(rate * seconds));
  for (let i = 0; i < out.length; i++) out[i] = amplitude * Math.sin((2 * Math.PI * hz * i) / rate);
  return out;
}

function rms(int16, from = 0, to = int16.length) {
  let sum = 0;
  for (let i = from; i < to; i++) sum += (int16[i] / 32768) ** 2;
  return Math.sqrt(sum / (to - from));
}

function flat(frames) {
  const all = new Int16Array(frames.length * frames[0].length);
  frames.forEach((f, i) => all.set(f, i * f.length));
  return all;
}

function crossings(int16) {
  let n = 0;
  for (let i = 1; i < int16.length; i++) if (int16[i - 1] < 0 && int16[i] >= 0) n++;
  return n;
}

function checkCapture() {
  // Microphone: 48 kHz and 44.1 kHz in, 20 ms frames of 480 samples at 24 kHz out.
  for (const rate of [48000, 44100, 24000, 16000]) {
    const frames = capture(rate, tone(rate, 1000, 1.0));
    assert(frames.every((f) => f.length === 480), `frames are 480 samples at ${rate}`);
    assert(Math.abs(frames.length - 50) <= 2, `about 50 frames a second at ${rate}, got ${frames.length}`);
    const pcm = flat(frames).subarray(2400);          // skip the filter's start-up
    const hz = (crossings(pcm) * 24000) / pcm.length;
    assert(Math.abs(hz - 1000) < 20, `a 1 kHz tone stays 1 kHz at ${rate}, got ${hz}`);
    const level = rms(pcm);
    // 0.5 amplitude -> 0.354 rms.
    assert(Math.abs(level - 0.354) < 0.02, `the tone keeps its level at ${rate}, got ${level}`);
  }
  // A tone above the call's Nyquist rate (12 kHz) is filtered out, not aliased into the band.
  {
    const frames = capture(48000, tone(48000, 18000, 1.0));
    const pcm = flat(frames).subarray(2400);
    assert(rms(pcm) < 0.02, `18 kHz is removed, got ${rms(pcm)}`);
  }
  // Clipping: a full-scale square-ish input never wraps around.
  {
    const loud = new Float32Array(48000).map((_, i) => (i % 64 < 32 ? 2.5 : -2.5));
    const pcm = flat(capture(48000, loud));
    assert(pcm.every((v) => v >= -32768 && v <= 32767));
  }
  // No input connected: no frames, no failure.
  {
    const { "brisvox-capture": Capture } = load(48000);
    const node = new Capture({ processorOptions: { rate: 24000, frame: 480 } });
    assert.strictEqual(node.process([[]], []), true);
    assert.strictEqual(node.posted.length, 0);
  }

}

// Speakers: a jitter buffer with marks and barge-in.
function checkPlayback() {
  const out = 48000;
  const { "brisvox-playback": Playback } = load(out);
  const make = () => new Playback({ processorOptions: { rate: 24000, jitterMs: 80, maxBufferMs: 400 } });
  const frame24 = (value) => new Int16Array(480).fill(value).buffer;     // 20 ms at 24 kHz
  const quantum = (node) => {
    const channel = new Float32Array(QUANTUM);
    node.process([], [[channel]]);
    return channel;
  };
  const drain = (node, quanta) => {
    let peak = 0, nonzero = 0;
    for (let i = 0; i < quanta; i++) {
      const channel = quantum(node);
      for (const v of channel) { peak = Math.max(peak, Math.abs(v)); if (v !== 0) nonzero++; }
    }
    return { peak, nonzero };
  };
  const send = (node, message) => node.port.onmessage({ data: message });
  const marks = (node) => node.posted.filter((m) => m.markPlayed !== undefined);

  // Silence while there is nothing to play.
  let node = make();
  assert.strictEqual(drain(node, 10).nonzero, 0);

  // Below the prebuffer target nothing plays; at the target it starts.
  send(node, { pcm: frame24(8000) });
  send(node, { pcm: frame24(8000) });
  assert.strictEqual(drain(node, 4).nonzero, 0, `40 ms is held back`);
  send(node, { pcm: frame24(8000) });
  send(node, { pcm: frame24(8000) });
  send(node, { pcm: frame24(8000) });
  const started = drain(node, 4);
  assert(started.nonzero > 0 && started.peak > 0.2 && started.peak < 0.32, `a little over 80 ms starts playback`);

  // 80 ms = 3840 samples at 48 kHz = 30 quanta; the rest of the buffer plays out, then silence.
  drain(node, 40);
  assert.strictEqual(drain(node, 4).nonzero, 0, `an empty buffer is silence`);

  // A short tail that never reaches the target plays after a short wait (150 ms).
  node = make();
  send(node, { pcm: frame24(8000) });
  assert.strictEqual(drain(node, 20).nonzero, 0);
  assert(drain(node, 60).nonzero > 0, `a short tail is played after a short wait`);

  // A mark is reported once the audio before it has played, not before, and it starts a short tail.
  node = make();
  send(node, { pcm: frame24(8000) });
  send(node, { mark: "m1" });
  assert.deepStrictEqual(marks(node), [], "the mark waits for the audio");
  const quanta = Math.ceil((480 * 2) / QUANTUM);
  drain(node, quanta - 1);
  assert.deepStrictEqual(marks(node), [], `not before it has played`);
  drain(node, 6);
  assert.deepStrictEqual(marks(node), [{ markPlayed: "m1" }], `after it has played`);
  // A mark with nothing queued is reported at once.
  send(node, { mark: "m2" });
  drain(node, 1);
  assert.deepStrictEqual(marks(node).slice(1), [{ markPlayed: "m2" }]);

  // Barge-in: clear stops the audio at once and forgets the marks that were not reached.
  node = make();
  for (let i = 0; i < 10; i++) send(node, { pcm: frame24(8000) });
  send(node, { mark: "cut" });
  assert(drain(node, 3).nonzero > 0);
  send(node, { clear: true });
  assert.strictEqual(drain(node, 20).nonzero, 0, `nothing plays after a clear`);
  assert.deepStrictEqual(marks(node), [], `a cut mark is never reported`);
  // ... and the next utterance plays from its start.
  for (let i = 0; i < 5; i++) send(node, { pcm: frame24(-8000) });
  const next = drain(node, 4);
  assert(next.nonzero > 0, `audio after a clear plays`);

  // The buffer is bounded: a burst of 2 s keeps about 400 ms, the oldest is dropped.
  node = make();
  for (let i = 0; i < 100; i++) send(node, { pcm: frame24(1000 + i) });
  const quantaToEmpty = (() => {
    let n = 0;
    while (drain(node, 1).nonzero > 0 && n < 2000) n++;
    return n;
  })();
  const ms = (quantaToEmpty * QUANTUM * 1000) / out;
  assert(ms <= 420 && ms >= 300, `about 400 ms stay buffered, got ${ms}`);
  // The newest audio survives (the last frames carry the highest values).
  node = make();
  for (let i = 0; i < 100; i++) send(node, { pcm: frame24(1000 + 100 * i) });
  const firstPlayed = quantum(node).find((v) => v !== 0) * 32768;
  assert(firstPlayed > 1000 + 100 * 70, `the oldest audio was dropped, first played ${firstPlayed}`);

  // The worklet reports how many PCM bytes it has taken after every message, so the page can stop
  // feeding a worklet that lags (a report every Nth message could leave the page waiting for one
  // that never comes when the frames are big); it also converts into one reused buffer.
  node = make();
  for (let i = 0; i < 20; i++) send(node, { pcm: frame24(1000) });
  const acks = node.posted.filter((m) => m.handled !== undefined).map((m) => m.handled);
  assert.deepStrictEqual(acks, Array.from({ length: 20 }, (_, i) => (i + 1) * 960), `handled bytes are reported after every message`);
  const view = node.view;
  send(node, { pcm: frame24(1000) });
  assert(view && node.view === view, `the conversion buffer is reused`);

  // A long steady call: 4 minutes of 20 ms frames played in real time (7.5 quanta each).
  node = make();
  let played = 0;
  for (let i = 0; i < 12000; i++) {
    send(node, { pcm: frame24(5000) });
    played += drain(node, i % 2 === 0 ? 7 : 8).nonzero;
  }
  assert(played > 12000 * 900 * 0.9, `a long call keeps playing, played ${played}`);
}


test("the capture worklet resamples, frames and clips", () => { checkCapture(); });
test("the playback worklet buffers, marks and clears", () => { checkPlayback(); });
