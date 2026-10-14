// brisvox-call.js: the browser side of a Brisvox voice call, as one dependency-free ES module.
//
//   import { BrisvoxCall } from "./brisvox-call.js";
//   const call = new BrisvoxCall({ serverUrl: "https://voice.example.com", tokenProvider });
//   await call.start();     // asks for the microphone, then connects
//   ...
//   await call.hangup();
//
// It tries WebRTC first (one WHIP request, Opus both ways, the browser's own echo cancellation)
// and, when media cannot get through, falls back by itself to the generic Brisvox WebSocket audio
// protocol on the same HTTP(S) port (docs/WEBSOCKET_PROTOCOL.md): the microphone is resampled to
// 24 kHz PCM16 in an AudioWorklet and sent in 20 ms binary frames, the assistant's audio plays
// through a second worklet with a small jitter buffer, and `clear` (barge-in) empties that buffer
// at once. Types: brisvox-call.d.ts. Usage: README.md.

// ---------------------------------------------------------------------------------------------
// Constants

const WS_RATE = 24000;                      // Hz, both directions (what text-to-speech produces)
const WS_FRAME = WS_RATE / 50;              // samples in 20 ms
const WS_JITTER_MS = 80;                    // playback starts when this much audio is buffered
const WS_MAX_BUFFER_MS = 400;               // playback buffer bound: the oldest audio is dropped beyond it
const WS_MAX_BACKLOG = 128 * 1024;          // bytes: microphone frames are dropped while the socket is this far behind
const WS_MAX_PLAYBACK_BACKLOG = 96 * 1024;  // bytes posted to the playback worklet and not yet handled: audio beyond it is dropped
const OPEN = 1;                             // WebSocket.OPEN
const SUBPROTOCOL_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const DROP_PEER_WAIT_MS = 1500;             // how long a fallback waits for the DELETE that frees the WebRTC session
const ICE_FETCH_TIMEOUT_MS = 4000;
const ICE_GATHER_TIMEOUT_MS = 3000;

const DEFAULT_AUDIO = {
  echoCancellation: true,
  noiseSuppression: true,
  autoGainControl: true,
  channelCount: 1,
};

// ---------------------------------------------------------------------------------------------
// The AudioWorklet code. A string (not a function) so that a bundler or minifier cannot change
// what runs inside the worklet scope. It registers two processors: "brisvox-capture" (microphone
// to 20 ms PCM16 frames at the call's rate) and "brisvox-playback" (a bounded jitter buffer with
// marks and an instant clear in front of the speakers).

/** The source of the AudioWorklet module the WebSocket transport loads (a Blob URL by default). */
export const WORKLET_SOURCE = String.raw`"use strict";
const HALF = 8;       // resampler kernel half-width, in samples of the lower rate
const PHASES = 64;    // fractional positions the kernel is tabulated for

// Streaming windowed-sinc resampler (low-pass at the lower of the two Nyquist rates).
class Resampler {
  constructor(inRate, outRate) {
    this.step = inRate / outRate;               // input samples per output sample
    const scale = Math.max(1, this.step);       // widen the kernel when down-sampling
    this.half = Math.ceil(HALF * scale);
    const taps = 2 * this.half;
    this.taps = taps;
    this.table = new Float32Array(PHASES * taps);
    for (let ph = 0; ph < PHASES; ph++) {
      let sum = 0;
      for (let k = 0; k < taps; k++) {
        const d = (k - this.half + 1) - ph / PHASES;   // input distance from the output position
        const x = d / scale;
        const win = Math.abs(d) < HALF * scale ? 0.5 + 0.5 * Math.cos((Math.PI * d) / (HALF * scale)) : 0;
        const v = (x === 0 ? 1 : Math.sin(Math.PI * x) / (Math.PI * x)) * win;
        this.table[ph * taps + k] = v;
        sum += v;
      }
      for (let k = 0; k < taps; k++) this.table[ph * taps + k] /= sum;   // unit gain at DC
    }
    this.buf = new Float32Array(4096);
    this.reset();
  }
  reset() {
    this.buf.fill(0, 0, this.half);
    this.len = this.half;     // half samples of silence before the first input
    this.pos = this.half;     // input position of the next output sample
  }
  // Appends input and calls emit(sample) for every output sample that is ready.
  push(input, emit) {
    if (this.len + input.length > this.buf.length) {
      const bigger = new Float32Array(Math.max(this.buf.length * 2, this.len + input.length));
      bigger.set(this.buf.subarray(0, this.len));
      this.buf = bigger;
    }
    this.buf.set(input, this.len);
    this.len += input.length;
    for (;;) {
      let base = Math.floor(this.pos);
      let ph = Math.round((this.pos - base) * PHASES);
      if (ph === PHASES) { ph = 0; base += 1; }
      if (base + this.half >= this.len) break;
      const row = ph * this.taps, first = base - this.half + 1;
      let acc = 0;
      for (let k = 0; k < this.taps; k++) acc += this.buf[first + k] * this.table[row + k];
      emit(acc);
      this.pos += this.step;
    }
    const drop = Math.floor(this.pos) - this.half;   // keep the history the kernel still needs
    if (drop > 0) {
      this.buf.copyWithin(0, drop, this.len);
      this.len -= drop;
      this.pos -= drop;
    }
  }
}

// Microphone: resample to the call's rate and hand out 20 ms PCM16 frames.
class Capture extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions;
    this.rs = new Resampler(sampleRate, o.rate);
    this.frame = new Int16Array(o.frame);
    this.n = 0;
    this.emit = (v) => {
      const c = v < -1 ? -1 : v > 1 ? 1 : v;
      this.frame[this.n++] = c < 0 ? c * 32768 : c * 32767;
      if (this.n === this.frame.length) {
        const copy = this.frame.slice();
        this.port.postMessage(copy.buffer, [copy.buffer]);
        this.n = 0;
      }
    };
  }
  process(inputs) {
    const channel = inputs[0] && inputs[0][0];
    if (channel) this.rs.push(channel, this.emit);
    return true;
  }
}

// Speakers: a jitter buffer in front of the output. Audio is queued as it arrives; playback
// starts once jitterMs is buffered (or a mark or a short wait says no more is coming soon),
// and stops when the buffer runs dry. The buffer is bounded: past maxBufferMs the oldest
// audio is dropped. clear (barge-in) empties it at once. A mark is reported back
// when everything queued before it has been played.
class Playback extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const o = options.processorOptions;
    this.cap = Math.ceil((sampleRate * o.maxBufferMs) / 1000);
    this.target = Math.ceil((sampleRate * o.jitterMs) / 1000);
    this.maxWait = Math.ceil((sampleRate * 0.15) / 128);   // render quanta a short tail may wait
    this.ring = new Float32Array(this.cap);
    this.rs = new Resampler(o.rate, sampleRate);
    this.r = 0;            // samples played or dropped (counters never wrap)
    this.w = 0;            // samples queued
    this.playing = false;
    this.waited = 0;
    this.marks = [];
    this.view = null;      // the reused conversion buffer (frames have one size)
    this.handled = 0;      // PCM bytes taken from the main thread, reported back after every message
    this.put = (v) => {
      if (this.w - this.r >= this.cap) this.r += 1;       // full: drop the oldest sample
      this.ring[this.w % this.cap] = v;
      this.w += 1;
    };
    this.port.onmessage = (e) => this.handle(e.data);
  }
  handle(m) {
    if (m.pcm) {
      const pcm = new Int16Array(m.pcm), f = this.view && this.view.length === pcm.length ? this.view : (this.view = new Float32Array(pcm.length));
      for (let i = 0; i < pcm.length; i++) f[i] = pcm[i] / 32768;
      this.rs.push(f, this.put);
      this.handled += m.pcm.byteLength;
      // Every message, never every Nth: the page stops feeding while posted - handled is large, so a
      // report that lags by several big frames could leave the page waiting for one that never comes.
      this.port.postMessage({ handled: this.handled });
    } else if (m.clear) {
      this.r = this.w;
      this.marks.length = 0;
      this.playing = false;
      this.waited = 0;
      this.rs.reset();
    } else if (m.mark !== undefined) {
      this.marks.push({ at: this.w, id: m.mark });
      if (this.w > this.r) this.playing = true;         // the audio before a mark is all there is
    }
  }
  process(inputs, outputs) {
    const out = outputs[0][0], n = out.length, avail = this.w - this.r;
    if (!this.playing && avail > 0) {
      this.waited += 1;
      if (avail >= this.target || this.waited >= this.maxWait) this.playing = true;
    }
    let i = 0;
    if (this.playing) {
      const take = Math.min(avail, n);
      for (; i < take; i++) out[i] = this.ring[(this.r + i) % this.cap];
      this.r += take;
      if (take < n) { this.playing = false; this.waited = 0; }
    }
    for (; i < n; i++) out[i] = 0;
    for (let c = 1; c < outputs[0].length; c++) outputs[0][c].set(out);
    while (this.marks.length && this.marks[0].at <= this.r) {
      this.port.postMessage({ markPlayed: this.marks.shift().id });
    }
    return true;
  }
}

registerProcessor("brisvox-capture", Capture);
registerProcessor("brisvox-playback", Playback);`;

// ---------------------------------------------------------------------------------------------
// Errors

/**
 * Every failure of a call is a `BrisvoxCallError`. `code` is one of: `config`, `unsupported`,
 * `microphone`, `token`, `signaling`, `webrtc`, `websocket`, `timeout`, `connection`.
 * `status` is the HTTP status when the server answered one.
 */
export class BrisvoxCallError extends Error {
  constructor(message, code, details = {}) {
    super(message);
    this.name = "BrisvoxCallError";
    this.code = code;
    if (details.status !== undefined) this.status = details.status;
    if (details.cause !== undefined) this.cause = details.cause;
  }
}

// A failure the WebSocket fallback can answer (no WebRTC here, no way to reach the signaling
// endpoint, media that never connects). Anything else (a 401, a 503, no microphone) is final.
class Fallback extends Error {}
// The caller hung up while the call was still opening.
class Cancelled extends Error {}

function abortError() {
  const abort = new Error("the call was hung up before it connected");
  abort.name = "AbortError";
  return abort;
}

function stopStream(stream) {
  try { stream.getTracks().forEach((track) => track.stop()); } catch (_) { /* already stopped */ }
}

const messageOf = (error) => (error && error.message) || String(error);

// ---------------------------------------------------------------------------------------------
// Helpers

// Waits for `promise`, but not longer than `ms` (the timer is cleared as soon as it is not needed).
function within(promise, ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(resolve, ms);
    const done = () => { clearTimeout(timer); resolve(); };
    Promise.resolve(promise).then(done, done);
  });
}

// The offer carries every candidate (the server does not trickle): wait for gathering, but not
// longer than ICE_GATHER_TIMEOUT_MS, because host candidates arrive at once.
function gathered(peer) {
  return new Promise((resolve) => {
    if (peer.iceGatheringState === "complete") return resolve();
    const timer = setTimeout(() => {
      peer.removeEventListener("icegatheringstatechange", done);
      resolve();
    }, ICE_GATHER_TIMEOUT_MS);
    function done() {
      if (peer.iceGatheringState === "complete") {
        clearTimeout(timer);
        peer.removeEventListener("icegatheringstatechange", done);
        resolve();
      }
    }
    peer.addEventListener("icegatheringstatechange", done);
  });
}

function peakLevel(analyser, data) {
  analyser.getByteTimeDomainData(data);
  let peak = 0;
  for (let i = 0; i < data.length; i++) peak = Math.max(peak, Math.abs(data[i] - 128));
  return Math.min(1, (peak / 128) * 1.6);
}

function resolvePlatform(options) {
  const g = globalThis;
  const over = options.platform || {};
  const nav = g.navigator;
  const devices = nav && nav.mediaDevices;
  return {
    fetch: over.fetch || (typeof g.fetch === "function" ? g.fetch.bind(g) : undefined),
    WebSocket: over.WebSocket || g.WebSocket,
    RTCPeerConnection: over.RTCPeerConnection || g.RTCPeerConnection,
    AudioContext: over.AudioContext || g.AudioContext || g.webkitAudioContext,
    AudioWorkletNode: over.AudioWorkletNode || g.AudioWorkletNode,
    MediaStream: over.MediaStream || g.MediaStream,
    Audio: over.Audio || g.Audio,
    getUserMedia: over.getUserMedia
      || (devices && typeof devices.getUserMedia === "function" ? devices.getUserMedia.bind(devices) : undefined),
  };
}

// ---------------------------------------------------------------------------------------------
// The call

/**
 * One voice call. Make one object per call: `start()` once, `hangup()` to end it.
 *
 * `state` is `idle`, `connecting`, `connected`, `ended` or `failed`; `transport` is `null` until a
 * transport carries the call, then `webrtc` or `websocket`. Everything that happens is also
 * delivered as an event to `options.onEvent` and to listeners added with `subscribe()`.
 */
export class BrisvoxCall {
  constructor(options) {
    if (!options || typeof options.serverUrl !== "string") {
      throw new BrisvoxCallError("serverUrl is required (the http(s) address of the Brisvox server)", "config");
    }
    const transport = options.transport || "auto";
    if (transport !== "auto" && transport !== "webrtc" && transport !== "websocket") {
      throw new BrisvoxCallError(`transport must be "auto", "webrtc" or "websocket", not ${JSON.stringify(transport)}`, "config");
    }
    if (options.token !== undefined && options.tokenProvider !== undefined) {
      throw new BrisvoxCallError("give token or tokenProvider, not both", "config");
    }
    this._o = {
      serverUrl: options.serverUrl,
      token: options.token,
      tokenProvider: options.tokenProvider,
      transport,
      fallbackAfterMs: positive(options.fallbackAfterMs, 4000),
      signalingTimeoutMs: positive(options.signalingTimeoutMs, 10000),
      connectTimeoutMs: positive(options.connectTimeoutMs, 20000),
      startTimeoutMs: positive(options.startTimeoutMs, 8000),
      whipPath: options.whipPath === undefined ? "/whip" : options.whipPath,
      wsPath: options.wsPath === undefined ? "/ws" : options.wsPath,
      audio: { ...DEFAULT_AUDIO, ...(options.audio || {}) },
      audioElement: options.audioElement,
      workletUrl: options.workletUrl,
      metadata: options.metadata,
      platform: options.platform,
    };
    /** @type {"idle"|"connecting"|"connected"|"ended"|"failed"} */
    this.state = "idle";
    /** @type {null|"webrtc"|"websocket"} */
    this.transport = null;
    this._listeners = new Set();
    if (typeof options.onEvent === "function") this._listeners.add(options.onEvent);
    this._muted = false;
    this._started = false;
    this._hung = false;
    this._torn = null;      // the teardown promise, once it began
    this._tearing = false;
    this._r = {             // everything that needs cleaning up
      ctx: null, mic: null, audioEl: null, ownAudioEl: false, remote: null,
      peer: null, sessionUrl: null, sessionToken: undefined, ws: null,
      nodes: [], timers: new Set(), analysers: { mic: null, bot: null }, pagehide: null,
    };
  }

  /** Whether the microphone is muted (`setMuted`). */
  get muted() { return this._muted; }

  /** Adds an event listener; returns a function that removes it. */
  subscribe(listener) {
    this._listeners.add(listener);
    return () => { this._listeners.delete(listener); };
  }

  /**
   * Opens the microphone and connects. Resolves (with this object) once the call is connected over
   * some transport; rejects with a `BrisvoxCallError` (after an `error` event and `state` `failed`)
   * if it could not be, or with an error named `AbortError` if `hangup()` came first. Call it from
   * a click handler: browsers only let a user gesture start audio.
   */
  async start() {
    if (this._started) {
      throw new BrisvoxCallError("start() was already called: make a new BrisvoxCall for each call", "config");
    }
    this._started = true;
    if (this._hung) {
      // hangup() came first: nothing is opened, not even an AudioContext.
      this._setState("ended", { reason: "hung up" });
      throw abortError();
    }
    this._setState("connecting");
    try {
      await this._connect();
    } catch (error) {
      if (error instanceof Cancelled || this._hung) {
        await this._teardown();
        // Whatever an in-flight step opened after the first teardown ran (a WHIP session whose
        // answer arrived late, ...) is freed now.
        await this._sweep();
        this._setState("ended", { reason: "hung up" });
        throw abortError();
      }
      const failure = error instanceof BrisvoxCallError
        ? error
        : new BrisvoxCallError(messageOf(error), "connection", { cause: error });
      this._emit({ type: "error", error: failure, code: failure.code, message: failure.message, fatal: true });
      await this._teardown();
      this._setState("failed", { reason: failure.message, error: failure });
      throw failure;
    }
    return this;
  }

  /**
   * Ends the call and frees everything: the WHIP session is deleted, the sockets are closed, the
   * microphone tracks are stopped, the audio context is closed. Safe to call at any time and more
   * than once.
   */
  async hangup() {
    this._hung = true;
    const wasLive = this.state === "connecting" || this.state === "connected";
    await this._teardown();
    if (wasLive) this._setState("ended", { reason: "hung up" });
  }

  /** Mutes or unmutes the microphone (the call stays open). */
  setMuted(muted) {
    this._muted = Boolean(muted);
    const mic = this._r.mic;
    if (mic) mic.getAudioTracks().forEach((track) => { track.enabled = !this._muted; });
    this._emit({ type: "muted", muted: this._muted });
  }

  /** Current microphone and assistant levels, each 0 to 1 (for a level meter; poll it). */
  getLevels() {
    const a = this._r.analysers;
    return { mic: a.mic ? peakLevel(a.mic.node, a.mic.data) : 0, bot: a.bot ? peakLevel(a.bot.node, a.bot.data) : 0 };
  }

  /** The AudioContext of the call (null before start or after hangup). */
  get audioContext() { return this._r.ctx; }

  // ---- events -------------------------------------------------------------------------------

  _emit(event) {
    for (const listener of [...this._listeners]) {
      try { listener(event); } catch (error) {
        if (typeof console !== "undefined") console.error("BrisvoxCall listener failed:", error);
      }
    }
  }

  _setState(state, extra) {
    if (this.state === state) return;
    const previous = this.state;
    this.state = state;
    this._emit({ type: "state", state, previous, ...(extra || {}) });
  }

  _log(text) { this._emit({ type: "log", text }); }

  _live() {
    if (this._hung) throw new Cancelled();
  }

  _timer(fn, ms) {
    const id = setTimeout(() => { this._r.timers.delete(id); fn(); }, ms);
    this._r.timers.add(id);
    return id;
  }

  _clearTimer(id) {
    clearTimeout(id);
    this._r.timers.delete(id);
  }

  // ---- connecting ---------------------------------------------------------------------------

  async _token() {
    const o = this._o;
    if (o.tokenProvider) {
      try {
        const token = await o.tokenProvider();
        return token ? String(token) : undefined;
      } catch (error) {
        throw new BrisvoxCallError(`the token provider failed: ${messageOf(error)}`, "token", { cause: error });
      }
    }
    return o.token ? String(o.token) : undefined;
  }

  _urls() {
    const o = this._o;
    let base;
    try { base = new URL(o.serverUrl); } catch (_) {
      throw new BrisvoxCallError(`serverUrl is not a valid URL: ${o.serverUrl}`, "config");
    }
    if (base.protocol !== "http:" && base.protocol !== "https:") {
      throw new BrisvoxCallError("serverUrl must start with http:// or https://", "config");
    }
    let whip = null, ws = null;
    if (o.whipPath) whip = new URL(o.whipPath, base).href;
    if (o.wsPath) {
      const u = new URL(o.wsPath, base);
      u.protocol = base.protocol === "https:" ? "wss:" : "ws:";
      ws = u.href;
    }
    return { whip, ws };
  }

  async _connect() {
    const o = this._o;
    const p = (this._p = resolvePlatform(o));
    const urls = this._urls();
    if (o.transport === "webrtc" && !urls.whip) throw new BrisvoxCallError("transport \"webrtc\" needs a whipPath", "config");
    if (o.transport === "websocket" && !urls.ws) throw new BrisvoxCallError("transport \"websocket\" needs a wsPath", "config");
    if (!p.getUserMedia) {
      throw new BrisvoxCallError("no microphone access: open the page on http://localhost or over HTTPS", "unsupported");
    }
    if (typeof p.fetch !== "function" && o.transport !== "websocket") {
      throw new BrisvoxCallError("fetch is not available", "unsupported");
    }

    // The AudioContext is made first, synchronously, while the click that started the call still
    // counts as a user gesture. It carries the level meters and the WebSocket transport.
    if (p.AudioContext) {
      try { this._r.ctx = new p.AudioContext(); } catch (error) { this._log(`no AudioContext: ${messageOf(error)}`); }
    }
    this._installPagehide();

    let mic;
    try {
      mic = await p.getUserMedia({ audio: o.audio, video: false });
    } catch (error) {
      const denied = error && (error.name === "NotAllowedError" || error.name === "SecurityError");
      throw new BrisvoxCallError(
        denied ? "microphone permission was denied" : `could not open the microphone: ${messageOf(error)}`,
        "microphone",
        { cause: error },
      );
    }
    if (this._hung) {
      // Hung up while the browser was asking: the teardown has already run and found no microphone,
      // so this stream is ours to stop (the permission prompt can outlive the call).
      stopStream(mic);
      throw new Cancelled();
    }
    this._r.mic = mic;
    if (this._muted) mic.getAudioTracks().forEach((track) => { track.enabled = false; });
    this._meter("mic", (ctx) => ctx.createMediaStreamSource(mic));

    if (o.transport === "websocket") {
      await this._openWebSocket("transport set to websocket", urls, false);
      return;
    }
    try {
      await this._openWebRtc(urls.whip);
    } catch (error) {
      if (!(error instanceof Fallback)) throw error;
      if (o.transport === "webrtc" || !urls.ws) {
        throw new BrisvoxCallError(error.message, "webrtc", { cause: error });
      }
      this._live();
      this._emit({ type: "fallback", from: "webrtc", to: "websocket", reason: error.message });
      this._log(`WebSocket fallback: ${error.message}`);
      await this._dropPeer();
      this._live();
      await this._openWebSocket(error.message, urls, true);
    }
  }

  // Level meters: an analyser on the microphone and one on what the assistant says.
  _meter(which, makeSource) {
    const ctx = this._r.ctx;
    if (!ctx || typeof ctx.createAnalyser !== "function") return;
    try {
      const node = ctx.createAnalyser();
      node.fftSize = 512;
      makeSource(ctx).connect(node);
      this._r.analysers[which] = { node, data: new Uint8Array(node.fftSize) };
    } catch (error) {
      this._log(`no ${which} level meter: ${messageOf(error)}`);
    }
  }

  // ---- WebRTC -------------------------------------------------------------------------------

  async _fetchIceServers(url, token) {
    // TURN servers for this call, from the server (GET <whip>/ice): short-lived credentials, asked
    // for per call and never stored. Any failure (404 from an older server, no relay configured, a
    // slow answer) gives an empty list: the call then connects directly.
    const headers = {};
    if (token) headers.Authorization = "Bearer " + token;
    const abort = new AbortController();
    const timer = setTimeout(() => abort.abort(), ICE_FETCH_TIMEOUT_MS);
    try {
      const res = await this._p.fetch(url, { headers, cache: "no-store", signal: abort.signal });
      if (!res.ok) return [];
      const body = await res.json();
      return Array.isArray(body.iceServers) ? body.iceServers : [];
    } catch (_) {
      return [];
    } finally {
      clearTimeout(timer);
    }
  }

  async _openWebRtc(whip) {
    const p = this._p;
    const o = this._o;
    const r = this._r;
    if (typeof p.RTCPeerConnection !== "function") throw new Fallback("RTCPeerConnection is not available");
    const token = await this._token();
    this._live();
    r.sessionToken = token;
    const iceServers = await this._fetchIceServers(whip + "/ice", token);
    this._live();
    if (iceServers.length) this._log(`using ${iceServers.length} ICE server entr${iceServers.length === 1 ? "y" : "ies"} from the server`);
    let peer;
    try {
      peer = new p.RTCPeerConnection({ iceServers });
    } catch (error) {
      throw new Fallback(`RTCPeerConnection failed: ${messageOf(error)}`);
    }
    r.peer = peer;
    r.mic.getAudioTracks().forEach((track) => peer.addTrack(track, r.mic));
    peer.ontrack = (e) => this._onRemoteTrack(e);

    // The attempt settles once: connected, or an error. After that the same handler watches the
    // call (steady state).
    let settle;
    const attempt = new Promise((resolve, reject) => { settle = { resolve, reject, done: false }; });
    attempt.catch(() => {});
    const settled = (fn) => { if (!settle.done) { settle.done = true; fn(); } };
    r.abortAttempt = () => settled(() => settle.reject(new Cancelled()));   // a hangup ends the wait
    let timer = 0;
    peer.onconnectionstatechange = () => {
      if (r.peer !== peer) return;                       // dropped for the fallback
      const state = peer.connectionState;
      this._log(`connection ${state}`);
      if (state === "connected") {
        this._clearTimer(timer);
        settled(() => settle.resolve());
        if (this.state === "connecting") this._markConnected("webrtc", false);
      } else if (state === "disconnected") {
        // May recover by itself (the server waits a few seconds too).
        if (this.state === "connected") this._setState("connecting", { transport: "webrtc", reason: "disconnected" });
      } else if (state === "failed") {
        if (!settle.done) {
          const error = o.transport === "auto"
            ? new Fallback("WebRTC could not connect")
            : new BrisvoxCallError("WebRTC could not connect (ICE or DTLS failed)", "webrtc");
          settled(() => settle.reject(error));
        } else {
          this._terminate("failed", new BrisvoxCallError("the WebRTC connection failed", "connection"));
        }
      } else if (state === "closed") {
        if (!settle.done) settled(() => settle.reject(new Fallback("the WebRTC connection closed")));
        else this._terminate("ended", null, "the WebRTC connection closed");
      }
    };

    try {
      await peer.setLocalDescription(await peer.createOffer());
      await gathered(peer);
    } catch (error) {
      throw new Fallback(`WebRTC offer failed: ${messageOf(error)}`);
    }
    this._live();

    const headers = { "Content-Type": "application/sdp" };
    if (token) headers.Authorization = "Bearer " + token;
    const abort = new AbortController();
    const signalingTimer = setTimeout(() => abort.abort(), o.signalingTimeoutMs);
    let res, answer;
    try {
      res = await p.fetch(whip, { method: "POST", headers, body: peer.localDescription.sdp, signal: abort.signal });
      if (res.status === 201) answer = await res.text();
    } catch (error) {
      throw new Fallback(`the signaling request failed: ${messageOf(error)}`);
    } finally {
      clearTimeout(signalingTimer);
    }
    // The session exists on the server as soon as it answers 201: remember it before anything can
    // throw, so a hangup that landed during the POST still deletes it (the teardown's own sweep ran
    // before the URL was known; start() sweeps again).
    if (res.status === 201) {
      const location = res.headers && res.headers.get && res.headers.get("Location");
      r.sessionUrl = location ? new URL(location, whip).href : null;
    }
    this._live();
    if (res.status !== 201) {
      let detail = "";
      try { detail = (await res.text()).slice(0, 200); } catch (_) { /* no body */ }
      throw new BrisvoxCallError(`server answered ${res.status}: ${detail}`, "signaling", { status: res.status });
    }

    // From here the server holds a session for this offer. WebRTC gets `fallbackAfterMs` to
    // connect; a forced WebRTC call gets the longer `connectTimeoutMs` and then fails.
    if (o.transport === "auto") {
      timer = this._timer(() => {
        if (r.peer === peer && peer.connectionState !== "connected") {
          settled(() => settle.reject(new Fallback(`WebRTC did not connect within ${o.fallbackAfterMs / 1000} s`)));
        }
      }, o.fallbackAfterMs);
    } else {
      timer = this._timer(() => {
        settled(() => settle.reject(new BrisvoxCallError(`WebRTC did not connect within ${o.connectTimeoutMs / 1000} s`, "timeout")));
      }, o.connectTimeoutMs);
    }
    try {
      await peer.setRemoteDescription({ type: "answer", sdp: answer });
    } catch (error) {
      this._clearTimer(timer);
      throw new Fallback(`the server's answer was refused: ${messageOf(error)}`);
    }
    this._log(`session ${r.sessionUrl}`);
    await attempt;
    this._clearTimer(timer);
  }

  _onRemoteTrack(e) {
    const p = this._p;
    const r = this._r;
    // An answer without an a=msid line fires ontrack with no streams: wrap the track.
    const stream = (e.streams && e.streams[0]) || (p.MediaStream ? new p.MediaStream([e.track]) : null);
    if (!stream) return;
    r.remote = stream;
    let el = this._o.audioElement;
    if (!el && typeof p.Audio === "function") { el = new p.Audio(); r.ownAudioEl = true; }
    if (el) {
      r.audioEl = el;
      el.autoplay = true;
      el.srcObject = stream;
      const playing = el.play && el.play();
      if (playing && playing.catch) playing.catch(() => { /* autoplay rules: the element has autoplay set */ });
    }
    this._meter("bot", (ctx) => ctx.createMediaStreamSource(stream));
  }

  // Closes the WebRTC attempt and frees its session on the server. The DELETE also gives a session
  // token back (the server does so for a session that never connected), so it is awaited, for a
  // moment, before the WebSocket asks for a token.
  async _dropPeer() {
    const r = this._r;
    const peer = r.peer;
    r.peer = null;
    if (peer) {
      peer.onconnectionstatechange = null;
      peer.ontrack = null;
      try { peer.close(); } catch (_) { /* closed */ }
    }
    if (r.audioEl) r.audioEl.srcObject = null;
    r.remote = null;
    r.analysers.bot = null;
    await within(this._deleteSession(), DROP_PEER_WAIT_MS);
  }

  // Frees the server's slot with one DELETE; clearing the URL first makes every exit path hang up
  // exactly once.
  _deleteSession() {
    const r = this._r;
    const url = r.sessionUrl;
    r.sessionUrl = null;
    if (!url || typeof this._p.fetch !== "function") return Promise.resolve();
    const headers = {};
    if (r.sessionToken) headers.Authorization = "Bearer " + r.sessionToken;
    try {
      return Promise.resolve(this._p.fetch(url, { method: "DELETE", headers, keepalive: true })).catch(() => { /* gone anyway */ });
    } catch (_) {
      return Promise.resolve();
    }
  }

  // ---- WebSocket ----------------------------------------------------------------------------

  async _openWebSocket(reason, urls, afterWebRtc) {
    const p = this._p;
    const o = this._o;
    const r = this._r;
    const ctx = r.ctx;
    if (typeof p.WebSocket !== "function") throw new BrisvoxCallError("WebSocket is not available in this browser", "unsupported");
    if (!ctx || !ctx.audioWorklet || typeof p.AudioWorkletNode !== "function") {
      throw new BrisvoxCallError("this browser has no AudioWorklet (it needs HTTPS or http://localhost)", "unsupported");
    }
    if (!urls.ws) throw new BrisvoxCallError("the WebSocket fallback is switched off (no wsPath)", "unsupported");
    // A fresh token for this attempt: tokens are single use.
    const token = await this._token();
    this._live();

    if (!ctx.brisvoxWorklet) {
      if (o.workletUrl) {
        await ctx.audioWorklet.addModule(o.workletUrl);
      } else {
        const blob = URL.createObjectURL(new Blob([WORKLET_SOURCE], { type: "text/javascript" }));
        try { await ctx.audioWorklet.addModule(blob); } finally { URL.revokeObjectURL(blob); }
      }
      ctx.brisvoxWorklet = true;
      this._live();
    }
    const capture = new p.AudioWorkletNode(ctx, "brisvox-capture", {
      numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1],
      channelCount: 1, channelCountMode: "explicit",           // the browser mixes the microphone down
      processorOptions: { rate: WS_RATE, frame: WS_FRAME },
    });
    const playback = new p.AudioWorkletNode(ctx, "brisvox-playback", {
      numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [1],
      processorOptions: { rate: WS_RATE, jitterMs: WS_JITTER_MS, maxBufferMs: WS_MAX_BUFFER_MS },
    });
    const source = ctx.createMediaStreamSource(r.mic);
    const silent = ctx.createGain();       // keeps the capture node in the audio graph, makes no sound
    silent.gain.value = 0;
    source.connect(capture);
    capture.connect(silent);
    silent.connect(ctx.destination);
    playback.connect(ctx.destination);
    r.nodes.push(source, capture, silent, playback);
    this._meter("bot", () => playback);
    if (ctx.state === "suspended") Promise.resolve(ctx.resume()).catch(() => {});

    // A browser cannot set headers on a WebSocket: the token travels as a subprotocol (the server
    // selects "brisvox"), or in the query when it has characters a subprotocol cannot carry.
    const protocols = ["brisvox"];
    let target = urls.ws;
    if (token) {
      if (SUBPROTOCOL_TOKEN.test(token)) protocols.push("brisvox-token." + token);
      else target += (target.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
    }
    const ws = new p.WebSocket(target, protocols);
    ws.binaryType = "arraybuffer";
    r.ws = ws;

    const session = { started: false, ended: false, droppedFrames: 0, droppedAudio: 0, posted: 0, handled: 0, fatal: null };
    await new Promise((resolve, reject) => {
      let timer = 0;
      // The socket is over (before or after the call started). `error` is set for a failure.
      const finish = (why, error) => {
        if (session.ended) return;
        session.ended = true;
        this._clearTimer(timer);
        capture.port.onmessage = null;
        playback.port.onmessage = null;
        if (session.droppedFrames) this._log(`${session.droppedFrames} microphone frames dropped (the connection was slow)`);
        if (session.droppedAudio) this._log(`${session.droppedAudio} audio frames dropped (the page could not play them fast enough)`);
        if (!session.started) reject(error || new BrisvoxCallError(why, "websocket"));
        else if (error) this._terminate("failed", error);
        else this._terminate("ended", null, why);
      };
      r.wsFinish = finish;
      timer = this._timer(
        () => finish("the server did not start the call in time", new BrisvoxCallError("the server did not start the call in time", "timeout")),
        o.startTimeoutMs,
      );

      capture.port.onmessage = (e) => {
        if (!session.started || ws.readyState !== OPEN) return;
        if (ws.bufferedAmount > WS_MAX_BACKLOG) { session.droppedFrames += 1; return; }
        ws.send(e.data);
      };
      playback.port.onmessage = (e) => {
        if (e.data.handled !== undefined) { session.handled = e.data.handled; return; }
        if (e.data.markPlayed !== undefined && ws.readyState === OPEN) {
          ws.send(JSON.stringify({ type: "mark", id: e.data.markPlayed }));
        }
      };
      ws.onopen = () => {
        const metadata = { client: "brisvox-web", ...(o.metadata || {}) };
        ws.send(JSON.stringify({ type: "start", version: 1, sample_rate: WS_RATE, encoding: "pcm16", metadata }));
      };
      ws.onmessage = (e) => {
        if (typeof e.data !== "string") {
          if (!session.started) return;
          // The worklet's message queue is not bounded by the browser: stop feeding it while it lags.
          if (session.posted - session.handled > WS_MAX_PLAYBACK_BACKLOG) { session.droppedAudio += 1; return; }
          session.posted += e.data.byteLength;
          playback.port.postMessage({ pcm: e.data }, [e.data]);
          return;
        }
        let m;
        try { m = JSON.parse(e.data); } catch (_) { return; }
        if (!m || typeof m !== "object") return;
        if (m.type === "started") {
          if (m.sample_rate !== WS_RATE || m.encoding !== "pcm16") {
            const why = `the server chose ${m.sample_rate} Hz ${m.encoding}, not ${WS_RATE} Hz pcm16`;
            finish(why, new BrisvoxCallError(why, "websocket"));
            return;
          }
          session.started = true;
          this._clearTimer(timer);
          this._markConnected("websocket", afterWebRtc, reason);
          this._emit({ type: "started", callId: m.call_id, sampleRate: m.sample_rate, encoding: m.encoding });
          resolve();
        } else if (m.type === "clear") {
          playback.port.postMessage({ clear: true });             // barge-in: stop playing at once
          this._emit({ type: "clear", epoch: m.epoch });
        } else if (m.type === "mark") {
          playback.port.postMessage({ mark: String(m.id) });
          this._emit({ type: "mark", id: String(m.id) });
        } else if (m.type === "stop") {
          this._emit({ type: "stop", reason: m.reason || "ended" });
          finish(`the assistant ended the call (${m.reason || "ended"})`);
        } else if (m.type === "error") {
          if (m.fatal) session.fatal = m;
          this._emit({ type: "notice", code: m.code, message: m.message, fatal: Boolean(m.fatal) });
        } else {
          // Anything else the server may send later (transcripts, for one) reaches the page as it is.
          this._emit({ type: "message", message: m });
        }
      };
      ws.onerror = () => { /* the browser hides the cause; onclose follows */ };
      ws.onclose = (e) => {
        if (session.started) {
          finish(session.fatal ? `server error ${session.fatal.code}: ${session.fatal.message}` : `connection closed (${e.code})`);
          return;
        }
        // A browser hides the HTTP status. After a WebRTC attempt with a fixed session token, the
        // usual cause of a refusal is a token the server already counts as used.
        const reused = afterWebRtc && !o.tokenProvider && /^v1\./.test(token || "")
          ? ". If the server answered 403, the session token was already used: use a tokenProvider so every attempt gets a new one"
          : "";
        const fatal = session.fatal ? ` (server error ${session.fatal.code}: ${session.fatal.message})` : "";
        const why = `could not open the WebSocket (close code ${e.code}): wrong token, origin or path, or the server is full${fatal}${reused}`;
        finish(why, new BrisvoxCallError(why, "websocket"));
      };
    });
  }

  // ---- steady state and teardown --------------------------------------------------------------

  _markConnected(transport, fallback, reason) {
    if (this.transport !== transport) {
      this.transport = transport;
      this._emit({ type: "transport", transport, fallback, ...(reason ? { reason } : {}) });
    }
    this._setState("connected", { transport });
  }

  // The call ended or broke on its own (server hang-up, socket closed, media lost).
  _terminate(state, error, reason) {
    if (this._tearing || this.state === "ended" || this.state === "failed") return;
    if (error) this._emit({ type: "error", error, code: error.code, message: error.message, fatal: true });
    this._teardown().then(() => {
      this._setState(state, { reason: reason || (error && error.message) || "ended", ...(error ? { error } : {}) });
    });
  }

  _installPagehide() {
    const g = globalThis;
    if (typeof g.addEventListener !== "function") return;
    const handler = () => { this.hangup(); };
    this._r.pagehide = handler;
    g.addEventListener("pagehide", handler);
  }

  // Frees everything, once: later callers get the same promise.
  _teardown() {
    if (this._torn) return this._torn;
    this._tearing = true;
    this._torn = this._sweep();
    return this._torn;
  }

  // Frees whatever is open right now. Every step is guarded (one failing must not leave the rest
  // open) and safe to repeat: start() sweeps again after a hangup, to free what a step that was
  // still in flight (the microphone prompt, the WHIP POST) opened after the teardown had run.
  _sweep() {
    const r = this._r;
    const attempt = (fn) => { try { fn(); } catch (_) { /* already closed */ } };
    return (async () => {
      for (const id of r.timers) clearTimeout(id);
      r.timers.clear();
      if (r.pagehide) { attempt(() => globalThis.removeEventListener("pagehide", r.pagehide)); r.pagehide = null; }
      // WebSocket: tell the server, then close (it holds back reading the close frame until it
      // has answered the stop).
      if (r.abortAttempt) { const abort = r.abortAttempt; r.abortAttempt = null; abort(); }
      const ws = r.ws;
      r.ws = null;
      if (ws) {
        ws.onmessage = null;
        ws.onerror = null;
        ws.onclose = null;
        if (r.wsFinish) attempt(() => r.wsFinish("hung up"));
        attempt(() => { if (ws.readyState === OPEN) ws.send(JSON.stringify({ type: "stop" })); });
        attempt(() => ws.close(1000));
      }
      const peer = r.peer;
      r.peer = null;
      if (peer) {
        peer.onconnectionstatechange = null;
        peer.ontrack = null;
        attempt(() => peer.close());
      }
      const deleted = this._deleteSession();
      if (r.audioEl) attempt(() => { r.audioEl.srcObject = null; if (r.ownAudioEl && r.audioEl.pause) r.audioEl.pause(); });
      r.audioEl = null;
      r.remote = null;
      for (const node of r.nodes) {
        if (node.port) attempt(() => { node.port.onmessage = null; });
        attempt(() => node.disconnect());
      }
      r.nodes = [];
      r.analysers = { mic: null, bot: null };
      if (r.mic) { const mic = r.mic; r.mic = null; attempt(() => stopStream(mic)); }
      if (r.ctx) {
        const ctx = r.ctx;
        r.ctx = null;
        attempt(() => { const closing = ctx.close(); if (closing && closing.catch) closing.catch(() => {}); });
      }
      await within(deleted, DROP_PEER_WAIT_MS);
    })();
  }
}

function positive(value, fallback) {
  return typeof value === "number" && Number.isFinite(value) && value > 0 ? value : fallback;
}

/**
 * Makes a call and connects it: `const call = await connect({ serverUrl, tokenProvider })`.
 * Resolves once the call is connected; rejects like `BrisvoxCall.start()`.
 */
export async function connect(options) {
  const call = new BrisvoxCall(options);
  await call.start();
  return call;
}
