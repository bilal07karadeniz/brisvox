import { mock, test } from "node:test";

// Stand-ins for the browser objects BrisvoxCall uses (WebSocket, RTCPeerConnection, AudioContext,
// AudioWorkletNode, getUserMedia, fetch), passed in through the `platform` option. They record
// what the module does and let a test play the server's part.

export class FakeTrack {
  constructor() { this.enabled = true; this.stopped = false; }
  stop() { this.stopped = true; }
}

export class FakeStream {
  constructor(tracks) { this.tracks = tracks || [new FakeTrack()]; }
  getAudioTracks() { return this.tracks; }
  getTracks() { return this.tracks; }
}

class FakeNode {
  constructor() { this.connected = []; this.disconnected = false; }
  connect(target) { this.connected.push(target); return target; }
  disconnect() { this.disconnected = true; }
}

class FakeAnalyser extends FakeNode {
  constructor() { super(); this.fftSize = 2048; this.level = 128; }
  getByteTimeDomainData(data) { data.fill(this.level); }
}

class FakeWorkletNode extends FakeNode {
  constructor(ctx, name, options) {
    super();
    this.name = name;
    this.options = options;
    this.posted = [];
    this.port = { onmessage: null, postMessage: (message, transfer) => this.posted.push({ message, transfer }) };
    ctx.env.workletNodes.push(this);
  }
}

class FakeContext {
  constructor(env) {
    this.env = env;
    this.state = "running";
    this.closed = false;
    this.modules = [];
    this.destination = new FakeNode();
    this.audioWorklet = { addModule: async (url) => { this.modules.push(url); } };
    env.contexts.push(this);
  }
  createMediaStreamSource(stream) { const n = new FakeNode(); n.stream = stream; this.env.sources.push(n); return n; }
  createGain() { const n = new FakeNode(); n.gain = { value: 1 }; return n; }
  createAnalyser() { const a = new FakeAnalyser(); this.env.analysers.push(a); return a; }
  async resume() { this.state = "running"; }
  async close() { this.closed = true; }
}

class FakeWebSocket {
  constructor(env, url, protocols) {
    this.env = env;
    this.url = url;
    this.protocols = protocols;
    this.readyState = 0;
    this.sent = [];
    this.bufferedAmount = 0;
    this.closedWith = undefined;
    env.sockets.push(this);
    if (env.autoOpen) setTimeout(() => this.open(), 0);
  }
  // ---- the server's side, for tests
  open() { this.readyState = 1; if (this.onopen) this.onopen(); }
  serverSend(message) { if (this.onmessage) this.onmessage({ data: typeof message === "string" ? message : JSON.stringify(message) }); }
  serverAudio(bytes = 960) { if (this.onmessage) this.onmessage({ data: new ArrayBuffer(bytes) }); }
  serverClose(code = 1000) { this.readyState = 3; if (this.onclose) this.onclose({ code }); }
  /** The JSON messages the client sent. */
  json() { return this.sent.filter((m) => typeof m === "string").map((m) => JSON.parse(m)); }
  binary() { return this.sent.filter((m) => typeof m !== "string"); }
  // ---- the client's side
  send(data) {
    this.sent.push(data);
    if (this.env.autoStart && typeof data === "string" && JSON.parse(data).type === "start") {
      setTimeout(() => this.serverSend({ type: "started", version: 1, call_id: "c1", sample_rate: 24000, encoding: "pcm16" }), 0);
    }
  }
  close(code) { this.closedWith = code; this.readyState = 3; }
}

class FakePeer {
  constructor(env, config) {
    this.env = env;
    this.config = config;
    this.connectionState = "new";
    this.iceGatheringState = "complete";
    this.localDescription = null;
    this.tracks = [];
    this.closed = false;
    env.peers.push(this);
  }
  addTrack(track, stream) { this.tracks.push({ track, stream }); }
  async createOffer() { return { type: "offer", sdp: "v=0 fake offer" }; }
  async setLocalDescription(d) { this.localDescription = d; }
  async setRemoteDescription(d) {
    this.remote = d;
    if (this.env.webrtc === "connect") setTimeout(() => this.setConnectionState("connected"), 0);
    if (this.env.webrtc === "fail") setTimeout(() => this.setConnectionState("failed"), 0);
  }
  addEventListener() {}
  removeEventListener() {}
  close() { this.closed = true; this.connectionState = "closed"; }
  // ---- for tests
  setConnectionState(state) {
    this.connectionState = state;
    if (this.onconnectionstatechange) this.onconnectionstatechange();
  }
  /** The remote audio track arriving. */
  remoteTrack() { if (this.ontrack) this.ontrack({ track: { kind: "audio" }, streams: [] }); }
}

export function response(status, body = "", headers = {}) {
  return {
    ok: status >= 200 && status < 300,
    status,
    headers: { get: (name) => headers[name] ?? null },
    text: async () => body,
    json: async () => JSON.parse(body),
  };
}

/**
 * @param {object} [options]
 *   webrtc: "connect" (the peer connects after the answer), "fail" (it fails), "hang" (nothing happens)
 *   whip: the status of the POST (default 201)
 *   holdMic: getUserMedia stays pending until `env.releaseMic()`
 *   autoOpen / autoStart: the WebSocket opens, and answers `start` with `started`, by itself
 */
export function makeEnv(options = {}) {
  const env = {
    webrtc: options.webrtc ?? "connect",
    autoOpen: options.autoOpen ?? true,
    autoStart: options.autoStart ?? true,
    contexts: [], sources: [], analysers: [], workletNodes: [], sockets: [], peers: [],
    requests: [], streams: [], constraints: [], audioEls: [],
    micError: options.micError,
  };
  env.fetch = async (url, init = {}) => {
    const method = init.method || "GET";
    env.requests.push({ url, method, headers: init.headers || {}, body: init.body, keepalive: init.keepalive });
    if (options.fetch) {
      const custom = await options.fetch(url, init);
      if (custom) return custom;
    }
    if (method === "POST") {
      const status = options.whip ?? 201;
      return status === 201
        ? response(201, "v=0 fake answer", { Location: "/whip/session1" })
        : response(status, "refused");
    }
    if (method === "DELETE") return response(200);
    if (url.endsWith("/ice")) return response(200, JSON.stringify({ iceServers: options.iceServers || [] }));
    return response(404);
  };
  env.platform = {
    fetch: env.fetch,
    WebSocket: class extends FakeWebSocket { constructor(url, protocols) { super(env, url, protocols); } },
    RTCPeerConnection: options.noRtc ? undefined : class extends FakePeer { constructor(config) { super(env, config); } },
    AudioContext: class extends FakeContext { constructor() { super(env); } },
    AudioWorkletNode: class extends FakeWorkletNode {},
    MediaStream: class extends FakeStream { constructor(tracks) { super(tracks); } },
    Audio: class {
      constructor() { this.srcObject = null; this.paused = false; env.audioEls.push(this); }
      play() { return Promise.resolve(); }
      pause() { this.paused = true; }
    },
    getUserMedia: async (constraints) => {
      env.constraints.push(constraints);
      if (env.micError) throw env.micError;
      // holdMic: the permission prompt is still open until the test calls env.releaseMic().
      if (options.holdMic) await new Promise((resolve) => { env.releaseMic = resolve; });
      const stream = new FakeStream();
      env.streams.push(stream);
      return stream;
    },
  };
  env.ws = () => env.sockets[env.sockets.length - 1];
  env.methods = (method) => env.requests.filter((r) => r.method === method);
  return env;
}

/** Collects the events of a call. */
export function record(call) {
  const events = [];
  call.subscribe((e) => events.push(e));
  events.of = (type) => events.filter((e) => e.type === type);
  events.states = () => events.of("state").map((e) => e.state);
  return events;
}

// ---- time -------------------------------------------------------------------------------------
// The module's timers (fallback after 4 s, signaling timeout, ...) and the fakes' own setTimeout(0)
// run on node's mock timers, so a test advances time by calling `advance` / `drive` / `until`
// instead of waiting for it; a whole suite takes milliseconds and cannot flake on a slow machine.

/** Lets every promise continuation that can run without a timer run. */
export async function settle() {
  for (let i = 0; i < 40; i++) await Promise.resolve();
}

/** Moves the fake clock forward by `ms`, running what becomes due (and the promises that unlocks). */
export async function advance(ms) {
  for (let i = 0; i < ms; i++) {
    await settle();
    mock.timers.tick(1);
  }
  await settle();
}

/** Advances the fake clock until `promise` settles; returns or throws what it does. */
export async function drive(promise, maxMs = 60000) {
  let done = false;
  const result = Promise.resolve(promise).finally(() => { done = true; });
  result.catch(() => {});
  for (let i = 0; i < maxMs && !done; i++) {
    await settle();
    if (done) break;
    mock.timers.tick(1);
  }
  if (!done) throw new Error(`still pending after ${maxMs} ms of fake time`);
  return result;
}

/** Advances the fake clock until `predicate()` is true. */
export async function until(predicate, what = "condition", ms = 2000) {
  for (let i = 0; i < ms; i++) {
    await settle();
    if (predicate()) return;
    mock.timers.tick(1);
  }
  throw new Error(`timed out waiting for ${what}`);
}

/** `test()` with the fake clock switched on for its duration. */
export function ftest(name, fn) {
  return test(name, async (t) => {
    mock.timers.enable({ apis: ["setTimeout"] });
    try {
      await fn(t);
    } finally {
      mock.timers.reset();
    }
  });
}
